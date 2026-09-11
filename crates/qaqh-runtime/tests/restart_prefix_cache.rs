//! 跨 daemon 重启的前缀缓存一致性测试。
//!
//! 验证目标：同一会话跨越 N 个"进程生命周期"（daemon 重启 → resume），
//! `AgentState::build_context()` 渲染出的 LLM 请求上下文必须：
//!
//! 1. 无追加场景：逐字节一致（P0 fix：frozen annotation 从 meta.json 恢复，
//!    不重新生成——否则 `<today>`/file_state 变化会摧毁整个前缀）；
//! 2. 有追加场景：旧前缀逐字节不变 + 仅尾部追加（provider 前缀缓存的
//!    "append-only" 合同）；
//! 3. 中断恢复场景：孤儿 tool_use 的 [RESTORE] 修复只发生一次，其后
//!    前缀稳定，且修复本身跨重启持久。
//!
//! 走**真实磁盘**（SessionManager 单例 + meta.json + messages.jsonl），
//! 重启 = 重建 AgentState + `init_session(restore_seed)`——即
//! `lifecycle.rs` 里 P0（restore_frozen_annotation）/ P2
//! （align_skill_injection_watermark）钩子所在的生产恢复路径。
//!
//! 单例约束：SessionManager::init 会级联初始化 WorkspaceStore
//! （OnceLock，二次 init panic），因此三个场景合并在一个 #[test] 内
//! 顺序执行，共享一个临时数据根——语义上等价于一个 daemon 进程内
//! 顺序验证多个会话/多次 resume；"真实跨进程重启"由
//! "重建 AgentState + 全量磁盘恢复"这一同构路径代表。

use std::path::PathBuf;
use std::sync::Mutex;

use qaqh_config::Config;
use qaqh_runtime::agent::state::agent::AgentState;
use qaqh_runtime::agent::state::lifecycle::init_session;
use qaqh_types::message::{ContentBlock, Message};

/// 串行化触碰共享进程状态（workspace 全局 + file_state + CURRENT_WORKSPACE）。
static SERIAL: Mutex<()> = Mutex::new(());

fn temp_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "qaqh-restart-prefix-cache-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create temp data root");
    root
}

/// 冻结上下文为可比对的结构级字节串（逐消息 serde 序列化）。
fn freeze(context: &[Message]) -> Vec<String> {
    context
        .iter()
        .map(|m| serde_json::to_string(m).expect("serialize message"))
        .collect()
}

/// 带真实工具环的首轮对话：user → assistant(tool_use) → tool result。
/// 注意不要手动 push_system：`create_session` 已预置 backend prompt（lifecycle.rs:268），
/// 而 resume 重放时 `from_messages` 会丢弃第一条之后的非保护 system 消息
/// （store.rs:1155-1182 的历史 bug 防护）——多 push 的 system 会在恢复后消失。
fn seed_turn_1(agent: &mut AgentState) {
    agent.msg.push_user("hello, look up the deploy color");
    let assistant = Message {
        msg_id: None,
        role: "assistant".into(),
        name: None,
        content: vec![ContentBlock::ToolUse {
            id: "call-1".into(),
            name: "lookup".into(),
            input: serde_json::json!({ "key": "deploy-color" }),
        }],
    };
    // push_assistant 返回 `!has_tools`：携带 tool_use 的 assistant 步骤
    // 有意返回 false（表示“等待工具结果”）。push_tool_result 的布尔返回
    // 语义是“回合是否可结束”（对带 tool_use 的 step 恒为 false），
    // 都不代表存储失败 —— 用内容断言验证落存结果。
    let _ = agent.msg.push_assistant(assistant);
    let _ = agent
        .msg
        .push_tool_result("call-1", "value(deploy-color) = azure-falcon-42", true);
}

/// 一个"生命周期"：等价于 daemon 重启后 resume 指定会话。
fn resume_agent(root: &PathBuf, ws: &str, seed: &str) -> AgentState {
    unsafe { std::env::set_var("QAQH_DATA_DIR", root) };
    let mut agent = AgentState::init("restart-prefix-cache-test", Config::default());
    agent.ephemeral = false; // 必须走真实磁盘持久化
    qaqh_workspace::set_workspace(ws);
    assert!(
        init_session(&mut agent, Some(seed)),
        "resume must succeed for seed {seed}"
    );
    agent
}

/// 新建会话生命周期（首个生命周期，seed 由 create 路径分配）。
/// 注意：`init_session(_, None)` 直接返回 false（见 lifecycle.rs），
/// 创建会话必须走 `create_session`。
fn create_agent(root: &PathBuf, ws: &str) -> AgentState {
    unsafe { std::env::set_var("QAQH_DATA_DIR", root) };
    let mut agent = AgentState::init("restart-prefix-cache-test", Config::default());
    agent.ephemeral = false;
    qaqh_workspace::set_workspace(ws);
    qaqh_runtime::agent::state::lifecycle::create_session(&mut agent);
    assert!(
        !agent.session.seed.is_empty(),
        "create session must assign a seed"
    );
    agent
}

#[test]
fn prefix_cache_consistency_across_restarts() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let root = temp_root();
    unsafe { std::env::set_var("QAQH_DATA_DIR", &root) };
    // 单例只允许 init 一次（同进程语义 = 一个 daemon 生命周期）。
    qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());

    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("workspace dir");
    let ws = ws.to_string_lossy().into_owned();

    // ════ 场景 A：多次重启，无追加 → 上下文逐字节复现 ════
    {
        let mut agent = create_agent(&root, &ws);
        let seed = agent.session.seed.clone();
        seed_turn_1(&mut agent);
        // build_context 生成并冻结 [Environment] annotation；
        // flush_meta 把 pending_save 缓冲转为 PersistOp（真实循环中由
        // engine_turn.rs:1026 每回合调用）；drain_persist_ops 统一落盘
        // （含 PersistFrozenAnnotation meta op）并 checkpoint WAL。
        agent.build_context();
        agent
            .msg
            .flush_meta(&agent.config.model, &agent.config.reasoning_effort);
        agent.drain_persist_ops();
        let baseline = freeze(&agent.build_context());
        // Sanity：首轮上下文必须已含工具结果（call-1 配对完整，非孤儿）。
        assert!(
            baseline.iter().any(|m| m.contains("azure-falcon-42")),
            "first turn context must contain the tool result"
        );
        assert!(!baseline.is_empty());

        // ── 重启 #1 ──
        drop(agent);
        let mut agent = resume_agent(&root, &ws, &seed);
        let after1 = freeze(&agent.build_context());
        assert_eq!(
            baseline, after1,
            "restart #1 must reproduce the context byte-identically (annotation replayed, not regenerated)"
        );
        assert!(
            agent.session.frozen_annotation.is_some(),
            "resumed session must carry the persisted frozen annotation"
        );

        // ── 重启 #2：确认稳定性不是巧合 ──
        drop(agent);
        let mut agent = resume_agent(&root, &ws, &seed);
        let after2 = freeze(&agent.build_context());
        assert_eq!(baseline, after2, "restart #2 must also be byte-identical");

        drop(agent);
        let _ = qaqh_session::SessionManager::global().delete(&seed);
    }

    // ════ 场景 B：重启后追加新回合 → 前缀字节不变 + 仅尾部增长 ════
    {
        let mut agent = create_agent(&root, &ws);
        let seed = agent.session.seed.clone();
        seed_turn_1(&mut agent);
        agent.build_context();
        agent
            .msg
            .flush_meta(&agent.config.model, &agent.config.reasoning_effort);
        agent.drain_persist_ops();
        let before = freeze(&agent.build_context());
        let before_len = before.len();

        // ── 重启后追加第二个回合 ──
        drop(agent);
        let mut agent = resume_agent(&root, &ws, &seed);
        let after_restart = freeze(&agent.build_context());
        assert_eq!(before, after_restart, "resume must not mutate the prefix");

        agent.msg.push_user("second turn question");
        agent
            .msg
            .push_assistant(Message {
                msg_id: None,
                role: "assistant".into(),
                name: None,
                content: vec![ContentBlock::text("answered turn 2")],
            });
        agent.build_context();
        agent
            .msg
            .flush_meta(&agent.config.model, &agent.config.reasoning_effort);
        agent.drain_persist_ops();

        let grown = freeze(&agent.build_context());
        assert!(grown.len() > before_len, "append must grow the context");
        // 前缀合同：新上下文的前 N 条 == 重启后的快照。
        for (index, old) in after_restart.iter().enumerate() {
            assert_eq!(
                &grown[index], old,
                "prefix byte-stability violated at message {index}"
            );
        }

        // ── 再次重启：追加后的上下文同样逐字节复现 ──
        drop(agent);
        let mut agent = resume_agent(&root, &ws, &seed);
        let final_snapshot = freeze(&agent.build_context());
        assert_eq!(
            grown, final_snapshot,
            "second restart must reproduce the appended context byte-identically"
        );

        drop(agent);
        let _ = qaqh_session::SessionManager::global().delete(&seed);
    }

    // ════ 场景 C：中断恢复（孤儿 tool_use）→ 修复一次，其后稳定 ════
    {
        let mut agent = create_agent(&root, &ws);
        let seed = agent.session.seed.clone();
        agent.msg.push_user("run the tool");
        let assistant = Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: vec![ContentBlock::ToolUse {
                id: "call-orphan".into(),
                name: "lookup".into(),
                input: serde_json::json!({ "key": "k" }),
            }],
        };
        // 同上：tool_use 步骤返回 false 是正常语义。
        let _ = agent.msg.push_assistant(assistant);
        // "进程死亡"：tool result 从未写入即落盘重启。
        agent.build_context();
        agent
            .msg
            .flush_meta(&agent.config.model, &agent.config.reasoning_effort);
        agent.drain_persist_ops();
        let baseline = freeze(&agent.build_context());

        drop(agent);
        let mut agent = resume_agent(&root, &ws, &seed);
        // [RESTORE] 修复是**有意的恢复语义**：把丢失的结果物化为失败占位，
        // 因此此处不要求与崩溃前逐字节一致；要求的是：
        // 1) 修复不增删消息数量；
        // 2) 修复后（第二次 build_context 起）上下文逐字节稳定；
        // 3) 修复已持久化——再重启不再二次改写。
        let after_resume = freeze(&agent.build_context());
        // [RESTORE] 为孤儿 tool_use 补一条合成失败结果 → 恰好多一条消息。
        assert_eq!(
            after_resume.len(),
            baseline.len() + 1,
            "restore-repair must synthesize exactly one [RESTORE] result for the orphan tool_use"
        );
        assert!(
            after_resume.iter().any(|m| m.contains("[RESTORE]")),
            "the synthetic repair must carry the [RESTORE] marker"
        );
        let stabilized = freeze(&agent.build_context());
        assert_eq!(
            after_resume, stabilized,
            "post-repair context must be byte-stable across further rebuilds"
        );

        drop(agent);
        let mut agent = resume_agent(&root, &ws, &seed);
        let after_second_restart = freeze(&agent.build_context());
        assert_eq!(
            stabilized, after_second_restart,
            "persisted repair must survive another restart byte-identically"
        );

        drop(agent);
        let _ = qaqh_session::SessionManager::global().delete(&seed);
    }

    let _ = std::fs::remove_dir_all(root);
}
