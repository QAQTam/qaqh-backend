//! PR2 端到端：工具批边界必须把工作区净变更作为注入消息落盘，且排在整批
//! tool 结果之后。
//!
//! 为什么需要（设计依据 docs/plan-workspace-diff-injection.md）：exec 跑脚本改
//! 文件时，qaqh 的审计链只覆盖"工具自报的路径"（permission::extract_target_paths
//! 对 exec 只取 cwd），模型只能再 read/cat 自查。workspace_audit 在批边界做全量
//! 状态扫描补这个洞。
//!
//! 本文件锁定的契约：
//! 1. 批内有文件变更 => 恰好一条 role=user + name=workspace 注入，且位于该批
//!    全部 tool 结果之后（Chat Completions 的 tool 紧邻硬约束）；
//! 2. 注入必须真的进了 message store——只 submit 不 drain_turn_boundary 会让报告
//!    滞留在 ContextFlow::pending 里永不落盘（"落盘 != 传输"分叉的源头），而
//!    Loop::drain_injections 在总线无投递时提前返回，指望不上；
//! 3. 零变更批不注入（不占上下文）。
//!
//! 不声明 command_id 幂等：它只服务已退役的注入日志（injections.jsonl，
//! PLAN B1），ContextFlow 不按它去重，同批重放仍会各落一条。
//!
//! wire 形态（注入排在 tool 消息之后在三条 provider 通路各自产出什么）由
//! qaqh-gate 的 workspace_diff_injection_* 系列单测锁定，本文件不重复。
//!
//! 隔离：QAQH_DATA_DIR / QAQH_SPY_DIR 各指临时目录；tool manager 与 workspace 是
//! 进程级状态，用例经 TEST_LOCK 串行（RUST_TEST_THREADS=1 已全局强制）。

#![allow(clippy::unwrap_used)] // 测试代码豁免（仓库惯例，见 clippy.toml）

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::Duration;

use qaqh_message::MessageStore;
use qaqh_runtime::agent::engine_tool::ToolEngine;
use qaqh_runtime::agent::state::agent::AgentState;
use qaqh_runtime::agent::turn_lap_test_api::execute_admitted_batch;
use qaqh_runtime::agent::types::{
    AdmittedTool, CancelToken, Emitter, LoopPhase, PendingState, RingContext, StatsCollector,
};
use qaqh_types::{ContentBlock, Message, ToolStatus};
use qaqh_workspace::permission::ToolCategory;
use qaqh_workspace::probe::ProbeTool;
use qaqh_workspace::{ToolManager, ToolResult, ToolRisk};

static TEST_LOCK: Mutex<()> = Mutex::new(());
static DATA_ROOT: OnceLock<tempfile::TempDir> = OnceLock::new();
static SPY_ROOT: OnceLock<tempfile::TempDir> = OnceLock::new();
static INIT: Once = Once::new();

/// 进程级环境：数据根与 spy 存储根都指向临时目录，绝不碰真实用户数据。
fn init_env() {
    DATA_ROOT.get_or_init(|| {
        let dir = tempfile::tempdir().expect("data root tempdir");
        // SAFETY: 本测试二进制由 TEST_LOCK 串行，且在任何 data_dir() 读取之前执行。
        unsafe { std::env::set_var("QAQH_DATA_DIR", dir.path()) };
        dir
    });
    SPY_ROOT.get_or_init(|| {
        let dir = tempfile::tempdir().expect("spy root tempdir");
        // SAFETY: 同上；QAQH_SPY_DIR 只影响 spy 快照存储位置。
        unsafe { std::env::set_var("QAQH_SPY_DIR", dir.path()) };
        dir
    });
    INIT.call_once(|| {
        qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());
        // 注意顺序：TOOL_MANAGER 是 OnceLock，init_tools 只有第一次生效，
        // 必须先注册探针再 AgentState::init（其内部 init_tools 成为 no-op）。
        qaqh_workspace::runtime::init_tools(
            "workspace-spy",
            &[register_spy_probe, register_declared_probe],
            vec![],
        );
    });
}

/// 探针工具：往工作区写文件，模拟 exec 跑脚本的副作用（宿主侧无从声明）。
/// name=__quiet__ 时什么都不做，用于验证零变更批不注入。
fn spy_probe_handler(
    ctx: &qaqh_workspace::tool_api::ToolCallContext,
    args: serde_json::Value,
) -> ToolResult {
    let workspace = ctx.workspace_root.to_string_lossy().to_string();
    if workspace.is_empty() || workspace == "." {
        return ToolResult::error("spy_probe: no workspace configured");
    }
    let name = match args.get("name").and_then(serde_json::Value::as_str) {
        Some(name) => name,
        None => return ToolResult::error("spy_probe: missing 'name'"),
    };
    if name == "__quiet__" {
        return ToolResult::ok("no-op");
    }
    let content = args
        .get("content")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("line one\nline two\n");
    let path = Path::new(&workspace).join(name);
    match std::fs::write(&path, content) {
        Ok(()) => ToolResult::ok(format!("wrote {}", path.display())),
        Err(error) => ToolResult::error(format!("spy_probe write failed: {error}")),
    }
}

fn register_spy_probe(mgr: &mut ToolManager) {
    mgr.register_probe(ProbeTool {
        key: "spy_probe".to_string(),
        description: "workspace change-audit probe",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "content": {"type": "string"}
            },
            "required": ["name"]
        }),
        handler: spy_probe_handler,
        risk: ToolRisk::Write,
        category: ToolCategory::Write,
        default_timeout: Duration::from_secs(5),
    });
}

fn tool_scope(call_id: &str, session_id: &str) -> qaqh_workspace::runtime::ToolExecutionScope {
    let root = Path::new(&qaqh_workspace::current_workspace()).to_path_buf();
    qaqh_workspace::runtime::ToolExecutionScope::capture(
        qaqh_workspace::tool_api::ToolCallContext {
            call_id: call_id.to_string(),
            session_id: session_id.to_string(),
            workspace_root: root.clone(),
            mode: qaqh_workspace::tool_api::AgentMode::Code,
            permission_level: qaqh_workspace::permission::PermissionLevel::SkipPermissions,
            sandbox: qaqh_workspace::tool_api::SandboxMode::Main,
            sandbox_spec: qaqh_workspace::tool_api::SandboxSpec::workspace_write(root),
            exec_default_shell: None,
            timeout: Duration::ZERO,
            cancellation: qaqh_workspace::tool_api::CancellationToken::new(),
            progress: None,
            source: qaqh_workspace::tool_api::ToolCallSource::Model,
        },
    )
}

fn store_with_tool_use(
    session_id: &str,
    call_id: &str,
    tool: &str,
    args: serde_json::Value,
) -> MessageStore {
    let mut store = MessageStore::new(session_id);
    store.push_user("mutate the workspace");
    let assistant = Message {
        msg_id: None,
        role: Message::ROLE_ASSISTANT.into(),
        name: None,
        content: vec![ContentBlock::ToolUse {
            id: call_id.to_string(),
            name: tool.to_string(),
            input: args,
        }],
    };
    assert!(
        !store.push_assistant(assistant),
        "tool_use step must remain open"
    );
    store
}

/// 跑一批已授权工具，返回批后的消息序列。
/// flow 必须 register_all——生产 Loop 就是这么构造的，未注册源会让 submit 直接
/// 报 UnknownSource，注入静默丢失。
fn run_batch(
    workspace: &Path,
    session_id: &str,
    tool: &str,
    call_id: &str,
    args: serde_json::Value,
    turn_id: &str,
    round_num: u32,
) -> (Vec<Message>, Vec<Message>) {
    qaqh_workspace::set_workspace(&workspace.to_string_lossy());
    qaqh_workspace::clear_cancel();

    let mut agent = AgentState::init("workspace-spy-test", qaqh_config::Config::default());
    agent.session.session_id = session_id.to_string();
    agent.ephemeral = true;
    agent.config.permission_level = 3; // skip-permissions(三档制)
    agent.msg = store_with_tool_use(session_id, call_id, tool, args.clone());

    let auth = match qaqh_workspace::authorize_call(session_id, call_id, tool, &args, 3) {
        qaqh_workspace::Admission::Authorized(auth) => auth,
        qaqh_workspace::Admission::ApprovalRequired(_) => {
            panic!("{tool} must not need approval at level 3")
        }
        qaqh_workspace::Admission::Denied(reason) => {
            panic!("{tool} was denied: {reason}")
        }
    };
    let admitted = vec![AdmittedTool {
        call_id: call_id.to_string(),
        auth: Box::new(auth),
        scope: tool_scope(call_id, session_id),
    }];

    let emitter = NullEmitter;
    let mut phase = LoopPhase::ToolsRunning;
    let mut pending = PendingState::default();
    let writer_dead = Arc::new(AtomicBool::new(false));
    let mut stats = StatsCollector::new();
    let mut flow = qaqh_message::ContextFlow::new();
    qaqh_message::builtin::register_all(&mut flow);
    let tool = ToolEngine::new();
    let order = vec![call_id.to_string()];
    let serial = HashSet::new();

    {
        let mut ctx = RingContext {
            agent: &mut agent,
            emitter: &emitter,
            cancel: &CancelToken::new(),
            phase: &mut phase,
            pending: &mut pending,
            writer_dead: &writer_dead,
            stats: &mut stats,
            flow: &mut flow,
        };
        let _ = execute_admitted_batch(
            &mut ctx, &tool, admitted, &order, &serial, turn_id, round_num,
        );
    }
    // 第二项是 loop 真正交给 gate 的上下文视图——「落盘了但没进请求」这一类
    // 分叉只有在这一层断言才拦得住（见 docs/plan §3.7）。
    let gate_context = agent.msg.build_context_for_gate(&[]);
    (agent.msg.to_vec(), gate_context)
}

/// 只满足 Emitter trait，本用例不关心事件流。
struct NullEmitter;

impl Emitter for NullEmitter {
    fn emit_domain(&self, _event: qaqh_domain::DomainEvent) {}
    fn emit_timeline(&self, _intent: qaqh_domain::TimelineIntent) {}
}

/// 找出注入消息的下标（role=user + name=workspace）。
fn injection_indices(messages: &[Message]) -> Vec<usize> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == Message::ROLE_USER && m.name.as_deref() == Some("workspace"))
        .map(|(i, _)| i)
        .collect()
}

/// 找出某 call_id 的 tool_result 所在消息下标。
fn tool_result_index(messages: &[Message], call_id: &str) -> usize {
    messages
        .iter()
        .position(|m| {
            m.content.iter().any(|block| {
                matches!(block, ContentBlock::ToolResult { tool_use_id, .. }
                    if tool_use_id == call_id)
            })
        })
        .unwrap_or_else(|| panic!("missing tool result for {call_id}"))
}

fn text_of(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 契约 1 + 2：批内有文件变更 => 恰好一条注入，排在 tool 结果之后，且已落盘。
#[test]
fn batch_with_file_change_injects_report_after_tool_result() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    init_env();
    let temp = tempfile::tempdir().expect("workspace tempdir");
    let workspace = temp.path().join("ws");
    std::fs::create_dir_all(&workspace).expect("workspace dir");

    let session = "spy-inject-1";
    let (messages, gate_context) = run_batch(
        &workspace,
        session,
        "spy_probe",
        "call-1",
        serde_json::json!({"name": "generated.py", "content": "def f():\n    return 1\n"}),
        "turn-a",
        0,
    );

    // tool 结果本身必须照常存在且成功（注入不得干扰 canonical 结果）。
    let tr = tool_result_index(&messages, "call-1");
    assert!(
        messages[tr]
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { result, .. }
                if result.status == ToolStatus::Ok)),
        "tool result must stay Ok: {messages:#?}"
    );

    let injections = injection_indices(&messages);
    assert_eq!(
        injections.len(),
        1,
        "批内改了文件应恰好一条注入：{messages:#?}"
    );
    let idx = injections[0];
    assert!(
        idx > tr,
        "注入必须排在整批 tool 结果之后（Chat 的 tool 紧邻硬约束）：tr={tr} idx={idx}"
    );

    let body = text_of(&messages[idx]);
    assert!(
        body.contains("[workspace-changes"),
        "注入头必须自带来源标签（name 只在 Chat 活到 wire）：{body}"
    );
    assert!(
        body.contains("generated.py"),
        "报告必须点名被改的文件：{body}"
    );
    assert!(
        body.contains("mark=s"),
        "注入头必须带扫描边界 id，供 undo/restore 定位：{body}"
    );

    // ── PR3：exec 盲区必须在两条既有宿主设施里留下痕迹 ──
    let abs = workspace.join("generated.py").to_string_lossy().to_string();

    // 1) SMJ 审计链：模型能用已经存在的 journal 工具 query/replay 找回脚本改动。
    let steps = qaqh_workspace::journal::query(None, Some("generated.py"), None);
    assert!(
        !steps.is_empty(),
        "spy 发现的变更必须回填 SMJ，否则审计链仍看不见 exec 副作用"
    );
    let step = &steps[steps.len() - 1];
    assert_eq!(step.tool, "spy_probe", "单调用批应归因到真实工具名");
    assert_eq!(step.op, "scan_added", "op 必须标出这是扫描发现的变更");
    assert_eq!(step.result, "ok");
    assert!(step.after_sha.is_some(), "新增文件必须有 after 指纹");

    // 2) file_state 账本：脚本改过的文件指纹必须刷新，否则下一次 edit/write
    //    命中 stale_file、被迫重新 read——这正是本功能要消除的症状。
    assert!(
        qaqh_workspace::file_state::last_hash(&abs).is_some(),
        "批尾必须刷新账本指纹：{abs}"
    );

    // 3) 报告必须带可执行回滚命令（codespy README 宣称有、原实现里没有）。
    assert!(
        body.contains("journal action=replay"),
        "报告必须给出可直接执行的回滚命令：{body}"
    );
    assert!(
        body.contains("at=0"),
        "新增文件的回滚点应是本步之前（seq-1）：{body}"
    );

    // 3.5) 落盘 != 传输：注入必须出现在 loop 交给 gate 的上下文视图里，
    //      且仍排在 tool 消息之后（否则模型永远看不见它）。
    let ctx_injections: Vec<usize> = gate_context
        .iter()
        .enumerate()
        .filter(|(_, m)| m.name.as_deref() == Some("workspace"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        ctx_injections.len(),
        1,
        "注入必须进 gate 上下文视图：{gate_context:#?}"
    );
    let ctx_tool = gate_context
        .iter()
        .position(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
        })
        .expect("gate context must carry the tool result");
    assert!(
        ctx_injections[0] > ctx_tool,
        "gate 视图里注入也必须排在 tool 结果之后：ctx_tool={ctx_tool} inj={:?}",
        ctx_injections
    );
    let ctx_body = text_of(&gate_context[ctx_injections[0]]);
    assert!(ctx_body.contains("[workspace-changes"), "{ctx_body}");

    // 4) 回滚命令必须真的能用——报告里发出的 at=seq-1 若还原不了就是空头支票。
    //    直接走 journal 工具背后的同一个函数，验证语义与提示一致。

    let restored = qaqh_workspace::journal::replay_to_path(
        "generated.py",
        Some(step.seq - 1),
        Some(Path::new(&abs)),
    )
    .expect("replay must succeed");
    assert_eq!(
        restored, None,
        "新增文件回滚到本步之前应得 None（即应删除），实得 {restored:?}"
    );
    // replay_to_path 在内容为 None 时会删除已存在的文件（journal.rs 的 else 分支），
    // 所以「撤销新增」的真语义是：文件消失。
    assert!(
        !Path::new(&abs).exists(),
        "回滚新增文件必须把它删掉，{abs} 仍在"
    );
}

/// 契约 3：零变更批不注入——不占模型上下文。
#[test]
fn quiet_batch_injects_nothing() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    init_env();
    let temp = tempfile::tempdir().expect("workspace tempdir");
    let workspace = temp.path().join("ws");
    std::fs::create_dir_all(&workspace).expect("workspace dir");

    let (messages, _) = run_batch(
        &workspace,
        "spy-quiet",
        "spy_probe",
        "call-q",
        serde_json::json!({"name": "__quiet__"}),
        "turn-q",
        0,
    );

    assert!(
        injection_indices(&messages).is_empty(),
        "无文件变更时不得注入：{:#?}",
        messages.iter().map(text_of).collect::<Vec<_>>()
    );
    // 工具照常执行成功，只是没有副作用。
    tool_result_index(&messages, "call-q");
}

/// 自报写路径的探针：注册成 file 工具名，args 带 action=write + path。
/// conflict::file_write_paths 对 tool_name==file 会取 args.action 再收集 path，
/// 因此本调用会进入批的 declared_writes 集合。
fn declared_write_handler(
    ctx: &qaqh_workspace::tool_api::ToolCallContext,
    args: serde_json::Value,
) -> ToolResult {
    let workspace = ctx.workspace_root.to_string_lossy().to_string();
    let path = match args.get("path").and_then(serde_json::Value::as_str) {
        Some(path) => path,
        None => return ToolResult::error("declared_write: missing path"),
    };
    let content = args
        .get("content")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("declared content");
    let full = Path::new(&workspace).join(path);
    match std::fs::write(&full, content) {
        Ok(()) => ToolResult::ok(format!("declared wrote {path}")),
        Err(error) => ToolResult::error(format!("declared_write failed: {error}")),
    }
}

fn register_declared_probe(mgr: &mut ToolManager) {
    mgr.register_probe(ProbeTool {
        key: "file".to_string(),
        description: "self-declaring write probe",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string" },
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["action", "path"]
        }),
        handler: declared_write_handler,
        risk: ToolRisk::Write,
        category: ToolCategory::Write,
        default_timeout: Duration::from_secs(5),
    });
}

/// 去重契约：工具**自报过**的写路径，工具自己已经记过 SMJ 也刷新过账本，
/// spy 不得再记一条——否则 journal query 出现成对假记录、审计流水翻倍。
/// 报告本身仍应出现（它描述的是工作区状态，与是否重复记账无关）。
#[test]
fn self_declared_write_is_not_double_recorded_in_smj() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    init_env();
    let temp = tempfile::tempdir().expect("workspace tempdir");
    let workspace = temp.path().join("ws");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    let (messages, _) = run_batch(
        &workspace,
        "spy-dedupe",
        "file",
        "call-d",
        serde_json::json!({
            "action": "write",
            "path": "declared.txt",
            "content": "self declared",
        }),
        "turn-d",
        0,
    );
    // 报告里应当看到它（状态扫描确实测到了）。
    let injections = injection_indices(&messages);
    assert_eq!(injections.len(), 1, "仍应注入变更报告：{messages:#?}");
    let body = text_of(&messages[injections[0]]);

    // 但 SMJ 不得由 spy 再记一遍。
    let steps = qaqh_workspace::journal::query(None, Some("declared.txt"), None);
    let from_scan = steps
        .iter()
        .filter(|step| step.op.starts_with("scan_"))
        .count();
    assert_eq!(
        from_scan, 0,
        "自报写路径不得被 spy 重复记入 SMJ：{steps:#?}"
    );
}
