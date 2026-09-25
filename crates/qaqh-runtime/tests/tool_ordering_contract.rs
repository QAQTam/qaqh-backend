//! 工具编排契约（v1 表征测试）——`execute_admitted_batch` 的排序/并发/回填/取消语义。
//!
//! 为什么需要（v2.0 重构前置）：09-19 工具 SDK 补充 spec 要用
//! `ToolCapabilities` 替换 loop 里的"手工编排"（`serial_call_ids` 分区、
//! `tool_call_order` 顺序回放、skill effects 顺序应用）。动刀前必须先把
//! **现状语义**用测试钉住，否则重构无法证明行为等价（见
//! `docs/current/status.md`）。
//!
//! 本文件锁定的现状语义：
//! 1. 并行组先跑、串行组后跑：串行项严格在全部并行项结束之后才开始；
//! 2. 串行组按模型 `tool_call_order` 执行（与传入 `admitted` 顺序无关）；
//! 3. 并行组并发执行，但在途上限 `MAX_PARALLEL_TOOL_WORKERS = 4`（分批）；
//! 4. 结果回填顺序 = **执行阶段顺序**：并行组内按模型序回填（不按完成序），
//!    串行组随后；跨组不保证全局模型序（例如 [并行A, 串行B, 并行C] 回填为
//!    A, C, B）——这是 v1 现状，v2 若有意改为全局模型序需显式更新本契约；
//! 5. 串行路径取消：已执行项保留结果，未执行项补 `Cancelled` 终态，
//!    每个 tool_use 恰有一条 tool_result；
//! 6. skill effects 按模型序应用（不按完成序）。
//!
//! 观察手段：进程内探针工具 `test_probe`（注册进测试 ToolManager）记录
//! 每个调用的 start/end 与并发峰值——不依赖 exec 子进程与墙钟阈值，
//! 断言全部是相对顺序或并发计数。
//!
//! 隔离：本测试二进制独占 `QAQH_DATA_DIR`（临时目录），不触碰真实用户数据根；
//! 文件内用例经 `TEST_LOCK` 串行（tool manager / workspace 是进程级状态）。

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

use qaqh_message::MessageStore;
use qaqh_runtime::agent::engine_tool::ToolEngine;
use qaqh_runtime::agent::state::agent::AgentState;
use qaqh_runtime::agent::turn_lap_test_api::execute_admitted_batch;
use qaqh_runtime::agent::types::{
    CancelToken, Emitter, LoopPhase, PendingState, RingContext, StatsCollector,
};
use qaqh_types::{ContentBlock, Message, ToolStatus};
use qaqh_workspace::permission::ToolCategory;
use qaqh_workspace::{ToolCallCtx, ToolHandler, ToolManager, ToolResult, ToolRisk};

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

/// tool manager / workspace / 探针均为进程级状态：本文件用例串行。
static TEST_LOCK: Mutex<()> = Mutex::new(());
/// 本进程独占的数据根（会话/canonical ledger/审计落此）。
static DATA_ROOT: OnceLock<tempfile::TempDir> = OnceLock::new();
static SESSION_INIT: Once = Once::new();

// ── 探针工具（编排观察面） ─────────────────────────────

#[derive(Debug, Clone)]
struct ProbeEvent {
    call: String,
    phase: &'static str,
    at: Instant,
}

#[derive(Default)]
struct ProbeState {
    events: Vec<ProbeEvent>,
    active: usize,
    max_active: usize,
    /// 该 call 结束时置位 `CANCEL_ARMED`（把取消点钉在确定位置）。
    arm_cancel_after: Option<String>,
}

static PROBE: OnceLock<Mutex<ProbeState>> = OnceLock::new();
static CANCEL_ARMED: AtomicBool = AtomicBool::new(false);

fn probe_state() -> &'static Mutex<ProbeState> {
    PROBE.get_or_init(|| Mutex::new(ProbeState::default()))
}

fn lock_probe() -> std::sync::MutexGuard<'static, ProbeState> {
    probe_state()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

/// 测试探针：记录 start/end 与并发峰值，可选 sleep 与同批 rendezvous。
///
/// `rendezvous > 1` 时等待同批项到齐（或 3s 超时）——让"并发上限"断言
/// 不依赖调度时序：若实现串行执行，等待超时后 `max_active` 会停在 1，
/// 断言给出明确失败而不是偶发假红。
fn probe_handler(ctx: ToolCallCtx) -> ToolResult {
    let call = ctx.get_str("call").unwrap_or_default().to_string();
    let sleep_ms = ctx.get_u64("sleep_ms").unwrap_or(0);
    let rendezvous = ctx.get_u64("rendezvous").unwrap_or(0) as usize;
    {
        let mut state = lock_probe();
        state.active += 1;
        state.max_active = state.max_active.max(state.active);
        state.events.push(ProbeEvent {
            call: call.clone(),
            phase: "start",
            at: Instant::now(),
        });
    }
    if rendezvous > 1 {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if lock_probe().active >= rendezvous {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    if sleep_ms > 0 {
        std::thread::sleep(Duration::from_millis(sleep_ms));
    }
    {
        let mut state = lock_probe();
        state.events.push(ProbeEvent {
            call: call.clone(),
            phase: "end",
            at: Instant::now(),
        });
        state.active -= 1;
        if state.arm_cancel_after.as_deref() == Some(call.as_str()) {
            CANCEL_ARMED.store(true, Ordering::SeqCst);
        }
    }
    ToolResult::ok(format!("probe {call} done"))
}

fn register_probe(mgr: &mut ToolManager) {
    mgr.register(ToolHandler {
        key: "test_probe".to_string(),
        description: "编排契约探针（测试专用）",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "call": {"type": "string"},
                "sleep_ms": {"type": "integer"},
                "rendezvous": {"type": "integer"}
            },
            "required": ["call"]
        }),
        handler: probe_handler,
        risk: ToolRisk::ReadOnly,
        category: ToolCategory::Read,
        default_timeout: Duration::from_secs(15),
    });
}

// ── 夹具 ───────────────────────────────────────────────

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

/// 模型发出 N 个 tool_use、尚无结果的 store 状态（批执行前）。
fn store_with_tool_uses(seed: &str, calls: &[(&str, &str)]) -> MessageStore {
    let mut store = MessageStore::new(seed);
    store.push_user("run the ordering batch");
    let mut assistant = Message {
        msg_id: None,
        role: Message::ROLE_ASSISTANT.into(),
        name: None,
        content: Vec::new(),
    };
    assistant.content = calls
        .iter()
        .map(|(call_id, tool)| ContentBlock::ToolUse {
            id: (*call_id).to_string(),
            name: (*tool).to_string(),
            input: serde_json::json!({}),
        })
        .collect();
    let turn_completed = store.push_assistant(assistant);
    assert!(
        !turn_completed,
        "带 tool_use 的 assistant step 不得标记 turn 完成"
    );
    store
}

/// 本测试进程独占数据根（在任何 `data_dir()` 读取之前设置一次）。
fn init_process_data_root() {
    DATA_ROOT.get_or_init(|| {
        let dir = tempfile::tempdir().expect("data root tempdir");
        // SAFETY: 本文件所有用例经 TEST_LOCK 串行，且此处在任何 data_dir()
        // 读取之前执行；测试二进制独立进程，env 不外泄。
        unsafe {
            std::env::set_var("QAQH_DATA_DIR", dir.path());
        }
        dir
    });
    SESSION_INIT.call_once(|| qaqh_session::SessionManager::init(qaqh_types::platform::data_dir()));
}

#[derive(Debug)]
struct BatchOutcome {
    /// `execute_admitted_batch` 返回值（false = 调用方应走取消收尾）。
    batch_ok: bool,
    events: Vec<ProbeEvent>,
    max_active: usize,
    /// store 中 tool_result 的落地顺序。
    result_order: Vec<String>,
    /// 仍无结果的 tool_use（必须恒为空）。
    open: Vec<String>,
    cancelled: Vec<String>,
    ok_results: Vec<String>,
    /// skill 会话状态中的激活顺序（skills 用例用）。
    skill_entries: Vec<String>,
}

impl BatchOutcome {
    fn calls(&self, phase: &str) -> Vec<String> {
        self.events
            .iter()
            .filter(|event| event.phase == phase)
            .map(|event| event.call.clone())
            .collect()
    }
    fn at(&self, call: &str, phase: &str) -> Instant {
        self.events
            .iter()
            .find(|event| event.call == call && event.phase == phase)
            .map(|event| event.at)
            .unwrap_or_else(|| panic!("missing {phase} event for {call}: {:?}", self.events))
    }
}

/// 跑一批已授权工具，收集编排观察结果。
///
/// `calls` 为模型序；`admitted` 以**逆序**送入，验证批执行自行按
/// `tool_call_order` 归一。
fn run_batch<F: FnOnce(&Path)>(
    label: &str,
    calls: &[(&str, &str, serde_json::Value)],
    tool_call_order: &[&str],
    serial_call_ids: &[&str],
    arm_cancel_after: Option<&str>,
    setup_workspace: F,
) -> (BatchOutcome, tempfile::TempDir) {
    init_process_data_root();
    let temp = tempfile::tempdir().expect("tempdir");
    let workspace = temp.path().join("ws");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    setup_workspace(&workspace);
    qaqh_workspace::set_workspace(&workspace.to_string_lossy());

    // 探针状态与取消旗标每次归零。
    {
        let mut state = lock_probe();
        *state = ProbeState {
            arm_cancel_after: arm_cancel_after.map(str::to_string),
            ..ProbeState::default()
        };
    }
    CANCEL_ARMED.store(false, Ordering::SeqCst);

    let seed = format!(
        "tool-ordering-{label}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    );
    // 注意顺序：`TOOL_MANAGER` 是 `OnceLock`，`init_tools` 只有第一次生效。
    // 必须先注册探针，再 `AgentState::init`（其内部 init_tools 会成为 no-op）。
    qaqh_workspace::runtime::init_tools(label, &[register_probe], vec![]);
    qaqh_workspace::clear_cancel();
    let mut agent = AgentState::init("tool-ordering-test", qaqh_config::Config::default());
    agent.session.seed = seed.clone();
    agent.ephemeral = true;
    agent.config.permission_level = 4;
    let tool_uses: Vec<(&str, &str)> = calls
        .iter()
        .map(|(call_id, tool, _args)| (*call_id, *tool))
        .collect();
    agent.msg = store_with_tool_uses(&seed, &tool_uses);

    let mut admitted = Vec::new();
    for (call_id, tool, args) in calls {
        // 探针按 args 里的 `call` 记账：夹具统一注入 call_id，避免用例手抄。
        let mut args = args.clone();
        if *tool == "test_probe"
            && let Some(object) = args.as_object_mut()
        {
            object.insert("call".to_string(), serde_json::json!(call_id));
        }
        match qaqh_workspace::authorize_call(&seed, call_id, tool, &args, 4) {
            qaqh_workspace::Admission::Authorized(auth) => {
                admitted.push(qaqh_runtime::agent::types::AdmittedTool {
                    call_id: (*call_id).to_string(),
                    auth: Box::new(auth),
                    scope: tool_scope(call_id, &seed),
                });
            }
            _ => panic!("call {call_id} ({tool}) must be authorized at level 4"),
        }
    }
    admitted.reverse(); // 输入顺序 ≠ 模型序：由批执行自行归一。

    let cancel = CancelToken::with_query_hook(Arc::new(|| CANCEL_ARMED.load(Ordering::SeqCst)));
    let emitter = RecordingEmitter::default();
    let mut phase = LoopPhase::ToolsRunning;
    let mut pending = PendingState::default();
    let writer_dead = Arc::new(AtomicBool::new(false));
    let mut stats = StatsCollector::new();
    let mut flow = qaqh_message::ContextFlow::new();
    let tool = ToolEngine::new();
    let order: Vec<String> = tool_call_order.iter().map(|id| (*id).to_string()).collect();
    let serial: HashSet<String> = serial_call_ids.iter().map(|id| (*id).to_string()).collect();

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
        execute_admitted_batch(&mut ctx, &tool, admitted, &order, &serial, "t-ordering", 0)
    };

    let (events, max_active) = {
        let state = lock_probe();
        (state.events.clone(), state.max_active)
    };

    let mut result_order = Vec::new();
    let mut open = Vec::new();
    let mut cancelled = Vec::new();
    let mut ok_results = Vec::new();
    for message in agent.msg.to_vec() {
        for block in &message.content {
            match block {
                ContentBlock::ToolUse { id, .. } => open.push(id.clone()),
                ContentBlock::ToolResult {
                    tool_use_id,
                    result,
                } => {
                    open.retain(|id| id != tool_use_id);
                    result_order.push(tool_use_id.clone());
                    match result.status {
                        ToolStatus::Cancelled => cancelled.push(tool_use_id.clone()),
                        ToolStatus::Ok => ok_results.push(tool_use_id.clone()),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
    let skill_entries = agent
        .skills
        .session_state()
        .entries
        .iter()
        .map(|entry| entry.name.clone())
        .collect();

    (
        BatchOutcome {
            batch_ok,
            events,
            max_active,
            result_order,
            open,
            cancelled,
            ok_results,
            skill_entries,
        },
        temp,
    )
}

fn probe_call(sleep_ms: u64) -> serde_json::Value {
    serde_json::json!({"sleep_ms": sleep_ms})
}

// ── 契约用例 ───────────────────────────────────────────

/// 1. 混合批：串行项严格在并行组全部结束之后执行。
#[test]
fn mixed_batch_serial_group_runs_after_parallel_group() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let calls = [
        ("ord-a", "test_probe", probe_call(120)),
        ("ord-b", "test_probe", probe_call(0)),
        ("ord-c", "test_probe", probe_call(120)),
    ];
    let (outcome, _temp) = run_batch(
        "mixed",
        &calls,
        &["ord-a", "ord-b", "ord-c"],
        &["ord-b"],
        None,
        |_| {},
    );

    assert!(outcome.batch_ok, "batch must complete: {outcome:?}");
    assert_eq!(
        outcome.calls("start").len(),
        3,
        "all three calls must execute: {outcome:?}"
    );
    let serial_start = outcome.at("ord-b", "start");
    assert!(
        serial_start > outcome.at("ord-a", "end") && serial_start > outcome.at("ord-c", "end"),
        "serial item must start only after the whole parallel group finished: {outcome:?}"
    );
    // 回填 = 执行阶段顺序：并行组（a、c，组内模型序）先落，串行组（b）随后。
    // 跨组不是全局模型序——现状如此，本用例把它钉住。
    assert_eq!(
        outcome.result_order,
        vec!["ord-a", "ord-c", "ord-b"],
        "results must land in execution-phase order: {outcome:?}"
    );
    assert!(outcome.open.is_empty(), "no open tool_use: {outcome:?}");
}

/// 2. 串行组按模型序执行——与传入 admitted 的顺序无关（夹具固定逆序送入）。
#[test]
fn serial_group_keeps_model_order_not_input_order() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let calls = [
        ("ser-1", "test_probe", probe_call(40)),
        ("ser-2", "test_probe", probe_call(0)),
    ];
    let (outcome, _temp) = run_batch(
        "serial-order",
        &calls,
        &["ser-1", "ser-2"],
        &["ser-1", "ser-2"],
        None,
        |_| {},
    );

    assert!(outcome.batch_ok, "batch must complete: {outcome:?}");
    assert_eq!(
        outcome.calls("start"),
        vec!["ser-1", "ser-2"],
        "serial calls must start in model order: {outcome:?}"
    );
    assert!(
        outcome.at("ser-1", "end") <= outcome.at("ser-2", "start"),
        "serial calls must not overlap: {outcome:?}"
    );
    assert_eq!(outcome.result_order, vec!["ser-1", "ser-2"]);
}

/// 3. 并行组真并发，且分批上限 = 4（rendezvous 保证重叠可观测，不依赖时序）。
#[test]
fn parallel_group_is_bounded_and_actually_concurrent() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let ids: Vec<String> = (1..=6).map(|index| format!("par-{index}")).collect();
    let mut calls: Vec<(&str, &str, serde_json::Value)> = Vec::new();
    for (index, id) in ids.iter().enumerate() {
        // 前 4 项为第一批（rendezvous=4），后 2 项为第二批（rendezvous=2）。
        let rendezvous = if index < 4 { 4 } else { 2 };
        calls.push((
            id.as_str(),
            "test_probe",
            serde_json::json!({"sleep_ms": 30, "rendezvous": rendezvous}),
        ));
    }
    let order: Vec<&str> = ids.iter().map(String::as_str).collect();
    let (outcome, _temp) = run_batch("parallel-bound", &calls, &order, &[], None, |_| {});

    assert!(outcome.batch_ok, "batch must complete: {outcome:?}");
    assert_eq!(
        outcome.max_active, 4,
        "parallel batch size must be exactly MAX_PARALLEL_TOOL_WORKERS=4: {outcome:?}"
    );
    assert_eq!(
        outcome.result_order, order,
        "results must land in model order: {outcome:?}"
    );
    assert!(outcome.open.is_empty(), "no open tool_use: {outcome:?}");
}

/// 4. 结果按模型序回填——即使完成序与之相反（长睡眠项先启动、最后完成）。
#[test]
fn results_backfill_in_model_order_even_when_completion_order_differs() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let calls = [
        ("bf-1", "test_probe", probe_call(150)),
        ("bf-2", "test_probe", probe_call(60)),
        ("bf-3", "test_probe", probe_call(10)),
    ];
    let (outcome, _temp) = run_batch(
        "backfill",
        &calls,
        &["bf-1", "bf-2", "bf-3"],
        &[],
        None,
        |_| {},
    );

    assert!(outcome.batch_ok, "batch must complete: {outcome:?}");
    let end_order = outcome.calls("end");
    assert_eq!(
        end_order,
        vec!["bf-3", "bf-2", "bf-1"],
        "completion order must be reversed vs model order for this fixture: {outcome:?}"
    );
    assert_eq!(
        outcome.result_order,
        vec!["bf-1", "bf-2", "bf-3"],
        "results must backfill in model order, not completion order: {outcome:?}"
    );
}

/// 5. 串行路径取消：首项已执行保留结果，其余补 Cancelled 终态，无 open tool_use。
#[test]
fn cancel_after_first_serial_call_seals_the_rest() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let calls = [
        ("sc-1", "test_probe", probe_call(0)),
        ("sc-2", "test_probe", probe_call(0)),
        ("sc-3", "test_probe", probe_call(0)),
    ];
    let (outcome, _temp) = run_batch(
        "serial-cancel",
        &calls,
        &["sc-1", "sc-2", "sc-3"],
        &["sc-1", "sc-2", "sc-3"],
        Some("sc-1"),
        |_| {},
    );

    assert!(
        !outcome.batch_ok,
        "cancel must surface as false so the caller runs cancel finalization: {outcome:?}"
    );
    assert_eq!(
        outcome.calls("start"),
        vec!["sc-1"],
        "only the first serial call may execute: {outcome:?}"
    );
    assert_eq!(
        outcome.ok_results,
        vec!["sc-1"],
        "executed item keeps its real result: {outcome:?}"
    );
    assert_eq!(
        outcome.cancelled,
        vec!["sc-2", "sc-3"],
        "unexecuted items must be sealed as Cancelled: {outcome:?}"
    );
    assert_eq!(
        outcome.result_order,
        vec!["sc-1", "sc-2", "sc-3"],
        "results must land in model order: {outcome:?}"
    );
    assert!(
        outcome.open.is_empty(),
        "every tool_use must have exactly one tool_result: {outcome:?}"
    );
}

/// 6. skill effects 按模型序应用——即使完成序相反（串行项后完成）。
#[test]
fn skill_effects_apply_in_model_order() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let calls = [
        (
            "sk-alpha",
            "skills",
            serde_json::json!({"action": "activate", "name": "alpha"}),
        ),
        (
            "sk-beta",
            "skills",
            serde_json::json!({"action": "activate", "name": "beta"}),
        ),
    ];
    // alpha 串行（后执行）、beta 并行（先执行）：若按完成序应用 effects，
    // entries 会是 [beta, alpha]；契约要求按模型序 → [alpha, beta]。
    let (outcome, _temp) = run_batch(
        "skill-effects",
        &calls,
        &["sk-alpha", "sk-beta"],
        &["sk-alpha"],
        None,
        |workspace| {
            for name in ["alpha", "beta"] {
                let dir = workspace.join(format!(".agents/skills/{name}"));
                std::fs::create_dir_all(&dir).expect("skill dir");
                std::fs::write(
                    dir.join("SKILL.md"),
                    format!(
                        "---\nname: {name}\ndescription: ordering contract fixture.\n---\n\n# {name} instructions"
                    ),
                )
                .expect("skill file");
            }
        },
    );

    assert!(outcome.batch_ok, "batch must complete: {outcome:?}");
    // 结果回填 = 执行阶段顺序：beta（并行）先落，alpha（串行）随后。
    assert_eq!(
        outcome.result_order,
        vec!["sk-beta", "sk-alpha"],
        "results must land in execution-phase order: {outcome:?}"
    );
    // effects 则按模型序应用：若按完成序，entries 会是 [beta, alpha]。
    assert_eq!(
        outcome.skill_entries,
        vec!["alpha", "beta"],
        "skill effects must apply in model order, not completion order: {outcome:?}"
    );
}
