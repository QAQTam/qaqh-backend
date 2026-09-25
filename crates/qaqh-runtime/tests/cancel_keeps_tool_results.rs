//! 回归：批取消路径不得丢弃**已执行**工具结果（BUG-2026-09-13-08）。
//!
//! 触发链路：权限挂起（YieldToUser）→ 用户批准 → deferred 批执行中取消。
//! 工具副作用已经发生（ToolIntent 已 durable），但旧实现里取消分支
//! （`execute_admitted_batch` 并行 `let _ = handle.join(); continue;` / 串行
//! `if ctx.cancel.is_set() { return false; }`）把已 join 出来的结果直接丢弃
//! → store 里留下 open tool_use → 下轮模型重发同一 tool_use，工具被重复执行。
//!
//! 验收标准：取消后 store 内**没有 open tool_use**；已执行工具只执行一次。
//!
//! 取消点钉在批执行**中途**（而非批开始前）：spawn 前的检查返回 false 让批
//! 真正跑起来，随后的收割窗口才置位取消。

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use qaqh_message::MessageStore;
use qaqh_runtime::agent::engine_tool::ToolEngine;
use qaqh_runtime::agent::state::agent::AgentState;
use qaqh_runtime::agent::turn_lap_test_api::execute_admitted_batch;
use qaqh_runtime::agent::types::{Emitter, LoopPhase, PendingState, RingContext, StatsCollector};
use qaqh_types::{ContentBlock, Message};

fn tool_scope(call_id: &str, seed: &str) -> qaqh_workspace::runtime::ToolExecutionScope {
    qaqh_workspace::runtime::ToolExecutionScope::capture(
        qaqh_workspace::tool_api::ToolCallContext {
            call_id: call_id.to_string(),
            session_id: seed.to_string(),
            workspace_root: std::path::PathBuf::from(qaqh_workspace::current_workspace()),
            mode: qaqh_workspace::tool_api::AgentMode::Code,
            permission_level: qaqh_workspace::permission::PermissionLevel::Unrestricted,
            sandbox: qaqh_workspace::tool_api::SandboxMode::Main,
            sandbox_spec: qaqh_workspace::tool_api::SandboxSpec::workspace_write(
                std::path::PathBuf::from(qaqh_workspace::current_workspace()),
            ),
            exec_default_shell: None,
            timeout: Duration::ZERO,
            cancellation: qaqh_workspace::tool_api::CancellationToken::new(),
            progress: None,
            source: qaqh_workspace::tool_api::ToolCallSource::Model,
        },
    )
}

static SESSION_INIT: Once = Once::new();
/// workspace / tool manager 均为进程级单例：本文件用例串行。
static TEST_LOCK: Mutex<()> = Mutex::new(());

const TURN_ID: &str = "t-cancel-batch";
/// 4 个工具的并行批：取消点落在第 4 个仍可能在执行、前若干个已完成的窗口。
const CALL_IDS: [&str; 4] = [
    "cancel-batch-1",
    "cancel-batch-2",
    "cancel-batch-3",
    "cancel-batch-4",
];

// ── 取消 token ─────────────────────────────────────────

/// 取消点控制器：让取消在**批执行中途**到达。
///
/// `execute_admitted_batch` 的并行路径会 `drain_progress_external` 直到所有
/// 工作线程结束，因此「取消已到达」必须落在**仍有工具在执行**的窗口内，否则
/// 收割循环看到的结果就已经齐全（测不出缺陷）。
///
/// `is_set()` 判定规则：
/// - spawn 之前（`armed == false`）恒返回 false —— 让批真正跑起来；
/// - `arm()` 之后（由外部观察副作用日志触发）返回 true —— 收割窗口取消。
struct MidBatchCancel {
    armed: AtomicBool,
}

impl MidBatchCancel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            armed: AtomicBool::new(false),
        })
    }
    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }
    fn is_set(&self) -> bool {
        self.armed.load(Ordering::SeqCst)
    }
}

/// 把取消点钉在中途的 token：`arm()` 之前恒「未取消」，之后恒「已取消」。
fn mid_batch_cancel_token(hook: Arc<MidBatchCancel>) -> qaqh_runtime::agent::types::CancelToken {
    qaqh_runtime::agent::types::CancelToken::with_query_hook(Arc::new(move || hook.is_set()))
}

/// 恒取消 token（等价于取消在批开始前已到达）。
fn always_cancelled_token() -> qaqh_runtime::agent::types::CancelToken {
    qaqh_runtime::agent::types::CancelToken::with_query_hook(Arc::new(|| true))
}

// ── 测试夹具 ───────────────────────────────────────────

#[derive(Default)]
struct RecordingEmitter {
    domains: Mutex<Vec<qaqh_domain::DomainEvent>>,
    timelines: Mutex<Vec<qaqh_domain::TimelineIntent>>,
}

impl Emitter for RecordingEmitter {
    fn emit_domain(&self, event: qaqh_domain::DomainEvent) {
        self.domains.lock().expect("domain lock").push(event);
    }
    fn emit_timeline(&self, intent: qaqh_domain::TimelineIntent) {
        self.timelines.lock().expect("timeline lock").push(intent);
    }
}

/// 模拟「模型发出 4 个 tool_use + 权限挂起 + 用户全部批准」后的 store 状态。
///
/// 返回的 store 只含已批准的 assistant step：4 个 tool_use 尚无结果，
/// 正是 deferred 批执行前的状态。
fn store_with_pending_batch(seed: &str) -> MessageStore {
    let mut store = MessageStore::new(seed);
    store.push_user("run four commands");
    let mut assistant = Message {
        msg_id: None,
        role: Message::ROLE_ASSISTANT.into(),
        name: None,
        content: Vec::new(),
    };
    assistant.content = CALL_IDS
        .iter()
        .map(|id| ContentBlock::ToolUse {
            id: (*id).to_string(),
            name: "exec".into(),
            input: serde_json::json!({"command": "true"}),
        })
        .collect();
    let turn_completed = store.push_assistant(assistant);
    assert!(
        !turn_completed,
        "带 tool_use 的 assistant step 不得标记 turn 完成"
    );
    store
}

/// store 中「有 tool_use 但无对应 tool_result」的 call_id（= 会触发重发的洞）。
fn open_tool_use_ids(store: &MessageStore) -> Vec<String> {
    let mut open = Vec::new();
    for message in store.to_vec() {
        for block in &message.content {
            match block {
                ContentBlock::ToolUse { id, .. } => open.push(id.clone()),
                ContentBlock::ToolResult { tool_use_id, .. } => open.retain(|id| id != tool_use_id),
                _ => {}
            }
        }
    }
    open
}

/// 副作用计数：工具重复执行即写入多行。
fn executed_times(path: &std::path::Path, call_id: &str) -> usize {
    std::fs::read_to_string(path)
        .map(|text| text.lines().filter(|line| *line == call_id).count())
        .unwrap_or(0)
}

#[derive(Debug)]
struct BatchReport {
    /// `execute_admitted_batch` 返回值（false = 调用方应走取消收尾）。
    batch_ok: bool,
    /// store 里仍无结果的 tool_use。
    open: Vec<String>,
    /// 每个 call_id 的副作用发生次数。
    executed: Vec<usize>,
    /// ToolFinished 领域事件数。**本函数内恒为 0**：批执行只落 store（外加
    /// CodeChanged/DashboardUpdated），ringing 终态由批后的
    /// `turn_lap::backfill::emit_completed_tool_round` 逐项发。
    tool_finished: usize,
}

fn run_batch(cancel_before_batch: bool, label: &str) -> (BatchReport, tempfile::TempDir) {
    SESSION_INIT.call_once(|| qaqh_session::SessionManager::init(qaqh_types::platform::data_dir()));
    let temp = tempfile::tempdir().expect("tempdir");
    let workspace = temp.path().join("ws");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    let data = temp.path().join("data");
    std::fs::create_dir_all(&data).expect("data dir");
    // SAFETY: this test file serializes runtime/global setup with TEST_LOCK.
    unsafe {
        std::env::set_var("QAQH_DATA_DIR", &data);
        std::env::set_var("HOME", temp.path());
        std::env::set_var("USERPROFILE", temp.path());
    }
    qaqh_workspace::set_workspace(&workspace.to_string_lossy());
    qaqh_workspace::runtime::init_tools(label, &[], vec![]);
    qaqh_workspace::clear_cancel();

    let side_effects = workspace.join("side-effects.txt");
    let mut agent = AgentState::init("cancel-keeps-results-test", qaqh_config::Config::default());
    // 每次运行用独立 seed：canonical ledger 按 session 隔离，复用 seed 会让
    // 上一个用例的 intent 泄漏进本次（`seal_unexecuted_as_cancelled` 会据此跳过）。
    let seed = format!(
        "cancel-keeps-results-{label}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    agent.session.seed = seed.clone();
    agent.ephemeral = true;
    agent.config.permission_level = 4;
    agent.msg = store_with_pending_batch(&seed);

    let mut admitted = Vec::new();
    for (index, id) in CALL_IDS.iter().enumerate() {
        // 错峰结束：第 i 个工具在 0.9 + 0.7*i 秒后写入副作用。第 1 个工具写完
        // 时（≈0.9s），第 2..4 个仍在执行 —— 取消点因此必然落在**批执行中途**。
        let delay = format!("sleep {};", 0.9 + 0.7 * index as f64);
        let args = serde_json::json!({
            "command": format!("{delay} printf '%s\\n' \"$1\" >> \"$2\""),
            "args": [id, side_effects.to_string_lossy()],
            "shell": "sh",
            "timeout_secs": 30,
        });
        let admission = qaqh_workspace::authorize_call(&agent.session.seed, id, "exec", &args, 4);
        match admission {
            qaqh_workspace::Admission::Authorized(auth) => {
                admitted.push(qaqh_runtime::agent::types::AdmittedTool {
                    call_id: (*id).to_string(),
                    auth: Box::new(auth),
                    scope: tool_scope(id, &agent.session.seed),
                })
            }
            _ => panic!("call {index} ({id}) must be authorized"),
        }
    }

    // 取消点：always_cancelled = 批开始前；否则由 watcher 在线程「第 1 个工具
    // 已完成、其余仍在跑」的窗口里 arm()，即取消落在批执行中途。
    let cancel_hook = MidBatchCancel::new();
    let cancel = if cancel_before_batch {
        always_cancelled_token()
    } else {
        mid_batch_cancel_token(cancel_hook.clone())
    };
    if !cancel_before_batch {
        let watcher_hook = cancel_hook.clone();
        let watcher_path = side_effects.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                let done = std::fs::read_to_string(&watcher_path)
                    .map(|text| text.lines().count())
                    .unwrap_or(0);
                if done >= 1 {
                    watcher_hook.arm();
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
    }

    let emitter = RecordingEmitter::default();
    let mut phase = LoopPhase::ToolsRunning;
    let mut pending = PendingState::default();
    let writer_dead = Arc::new(AtomicBool::new(false));
    let mut stats = StatsCollector::new();
    let mut flow = qaqh_message::ContextFlow::new();
    let tool = ToolEngine::new();
    let tool_call_order: Vec<String> = CALL_IDS.iter().map(|id| (*id).to_string()).collect();
    let serial_call_ids = HashSet::new();

    let batch_ok = {
        let mut ctx = RingContext {
            agent: &mut agent,
            emitter: &emitter,
            cancel: &cancel,
            phase: &mut phase,
            pending: &mut pending,
            writer_dead: &writer_dead,
            stats: &mut stats,
            flow: &mut flow,
        };
        execute_admitted_batch(
            &mut ctx,
            &tool,
            admitted,
            &tool_call_order,
            &serial_call_ids,
            TURN_ID,
            0,
        )
    };

    let open = open_tool_use_ids(&agent.msg);
    let executed = CALL_IDS
        .iter()
        .map(|id| executed_times(&side_effects, id))
        .collect::<Vec<_>>();
    let tool_finished = {
        let domains = emitter.domains.lock().expect("domain lock");
        domains
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    qaqh_domain::DomainEvent::Tool(qaqh_domain::ToolEvent::ToolFinished { .. })
                )
            })
            .count()
    };

    (
        BatchReport {
            batch_ok,
            open,
            executed,
            tool_finished,
        },
        temp,
    )
}

// ── 回归用例 ───────────────────────────────────────────

/// 取消点落在工具批**中途**：已执行项必须落历史，不得留 open tool_use。
#[test]
fn cancel_mid_batch_keeps_executed_tool_results() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let (report, temp) = run_batch(false, "cancel-mid-batch");
    // 批确实跑起来了：4 个工具都产生了副作用（取消点在中途，不是批前）。
    let executed_count = report.executed.iter().filter(|count| **count > 0).count();
    assert!(
        report.executed.contains(&1),
        "取消点没有落在工具批中途（无任何工具执行）：{report:?}",
    );
    assert!(
        report.executed.iter().all(|count| *count <= 1),
        "已执行工具被重复执行：{:?}",
        report.executed
    );
    // 分工锁定：`execute_admitted_batch` 只落 store + CodeChanged/DashboardUpdated，
    // ringing 终态（ToolFinished）由批后的 `turn_lap::backfill::emit_completed_tool_round`
    // 逐项发。即使这里 4 个工具都真的执行了，本函数内也不得出现终态事件——
    // 否则就会与 backfill 双发、顺序错乱。
    assert_eq!(
        report.tool_finished, 0,
        "{executed_count} 个工具已执行，但终态事件必须留给批后 backfill：{report:?}"
    );
    assert!(
        report.open.is_empty(),
        "取消后 store 仍留有 open tool_use（下轮会被重发重跑）：{:?}（executed={:?}）",
        report.open,
        report.executed
    );
    assert!(
        !report.batch_ok,
        "取消路径必须返回 false，让调用方走取消终态收尾"
    );
    drop(temp);
}

/// 反向对照（防护栏）：取消发生在批开始前时，未执行的 tool_use 也必须被
/// 收尾为终态——否则 `abort_running_turn` 的 `remove_last_step_if_incomplete`
/// 会把整个 step（含已执行结果）一起丢掉，缺陷同样复现。
#[test]
fn cancel_before_batch_still_seals_every_tool_use() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let (report, temp) = run_batch(true, "cancel-before-batch");
    assert!(
        report.executed.iter().all(|count| *count == 0),
        "批开始前取消：工具不得执行，实际 {:?}",
        report.executed
    );
    assert!(
        report.open.is_empty(),
        "批开始前取消后 store 仍留有 open tool_use：{:?}",
        report.open
    );
    assert!(!report.batch_ok, "取消路径必须返回 false");
    drop(temp);
}

// ═══════════════════════════════════════════════════════
// 取消命令收尾：孤儿 tool_use 不得幸存到下一轮
// ═══════════════════════════════════════════════════════

/// 取消命令的收尾语义：先给「不会有结果」的 tool_use 补终态，再丢弃/保留
/// step —— 否则 store 留孤儿，下轮模型重发同一 tool_use，工具重复执行。
///
/// 这里直接对 store 断言（`ConversationCancel` 分派内部正是这两步），
/// 避免依赖模型流时序带来的抖动。
#[test]
fn cancel_command_seals_open_tool_use_before_dropping_step() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let mut store = store_with_pending_batch("cancel-command-seal");
    // 前置：4 个 tool_use 全部无结果 —— 正是取消瞬间 store 的形态。
    assert_eq!(
        open_tool_use_ids(&store).len(),
        4,
        "前置：4 个孤儿 tool_use"
    );

    let sealed = store.seal_pending_tools_as_cancelled();
    assert_eq!(sealed, 4, "4 个未执行 tool_use 都必须补取消终态");
    assert!(
        open_tool_use_ids(&store).is_empty(),
        "补终态后不得再有孤儿 tool_use"
    );

    // 补终态之后 step 已「完整」，remove_last_step_if_incomplete 不再丢弃它
    // （这正是旧实现丢结果、下轮重跑的根因）。
    let removed = store.remove_last_step_if_incomplete();
    assert!(
        !removed,
        "每个 tool_use 都有结果后，step 必须被保留而不是整体丢弃"
    );
    assert!(
        open_tool_use_ids(&store).is_empty(),
        "保留后的 step 仍然不得有孤儿"
    );
}

/// 已执行结果的 tool_use 不会被「取消补终态」覆盖。
#[test]
fn cancel_seal_never_overwrites_executed_results() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let mut store = store_with_pending_batch("cancel-seal-keeps-results");
    // 模拟取消收割已回填第 1 项真实结果。
    store.push_tool_result_canonical(
        CALL_IDS[0],
        &qaqh_types::ToolResult::ok("real-exec-output"),
        &[],
    );

    let sealed = store.seal_pending_tools_as_cancelled();
    assert_eq!(sealed, 3, "只应给尚未有结果的 3 项补终态");

    let results = store.last_step_tool_results();
    let first = results
        .iter()
        .find(|result| result.tool_call_id == CALL_IDS[0])
        .expect("第 1 项结果必须在");
    assert!(
        !matches!(first.result.status, qaqh_types::ToolStatus::Cancelled),
        "已执行的 canonical 结果不得被取消终态覆盖: {:?}",
        first.result.status
    );
}
