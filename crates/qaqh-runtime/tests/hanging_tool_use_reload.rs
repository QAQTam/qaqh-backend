//! #13 端到端回归：**中断流的悬挂 ToolUse 不进历史回放**。
//!
//! 事故形状：流被中断（读错误 / 空闲超时）时，抢救路径会把未完成的工具调用
//! 一起落盘——`id:""` / `name:""` / `input:Null`。这些调用不可执行，
//! 也不该出现在任何回放面上；进历史回放后前端会渲染出永远 running 的幽灵工具卡。
//!
//! 本文件锁住两条恢复路径的历史面（reload 语义 = 新进程 + 持久化归档重建：
//! timeline 文件缺失时从 `messages.jsonl` 重建，conversation 快照直接投影归档）：
//! 1. timeline 重建结果里没有 tool block（更不会出现无名工具卡）；
//! 2. conversation 快照的 `turns` 里没有悬挂 `tool_calls`；
//! 3. 同一回合里的正文**不丢**（只丢不可执行的调用）；
//! 4. 完整工具环（control）不受影响，仍照常回放。
//!
//! `SessionManager` 是进程级单例（`init` 只能调用一次），全部用例共用一个
//! data root，各自使用独立 seed。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use qaqh_domain::TimelineBlockKind;
use qaqh_runtime::RingingHub;
use qaqh_session::SessionManager;
use qaqh_types::{ContentBlock, Message};

fn shared_root() -> PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root = std::env::temp_dir().join(format!(
            "qaqh-hanging-tooluse-reload-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create temp root");
        SessionManager::init(root.clone());
        root
    })
    .clone()
}

/// 中断抢救落盘的归档：正文保留，尾部带一个悬挂 ToolUse。
fn interrupted_archive() -> Vec<Message> {
    let assistant = Message {
        msg_id: None,
        role: Message::ROLE_ASSISTANT.to_string(),
        name: None,
        content: vec![
            ContentBlock::Text {
                text: "partial answer".to_string(),
            },
            ContentBlock::ToolUse {
                id: String::new(),
                name: String::new(),
                input: serde_json::Value::Null,
            },
        ],
    };
    vec![
        Message::system("system"),
        Message::user("interrupted"),
        assistant,
    ]
}

/// 完整工具环的归档（control 组）：user → assistant(tool_use) → tool result。
fn complete_tool_archive() -> Vec<Message> {
    let assistant = Message {
        msg_id: None,
        role: Message::ROLE_ASSISTANT.to_string(),
        name: None,
        content: vec![ContentBlock::ToolUse {
            id: "call-1".to_string(),
            name: "read".to_string(),
            input: serde_json::json!({ "path": "/tmp/a" }),
        }],
    };
    vec![
        Message::system("system"),
        Message::user("go"),
        assistant,
        Message::tool("call-1", "ok", true),
    ]
}

/// reload = 新的 RingingHub（空 timeline 缓存）+ 同一持久化归档。
fn reload_hub(root: &Path, epoch: &str) -> RingingHub {
    RingingHub::with_persistence(epoch, root.join("ringing"))
        .with_sessions(SessionManager::global())
}

#[test]
fn interrupted_reload_history_has_no_hanging_tool_use() {
    let root = shared_root();
    let session_id = "hanging-tooluse-reload";
    let control_session = "hanging-tooluse-reload-control";
    SessionManager::global().save_append(
        session_id,
        &interrupted_archive(),
        "test-model",
        None,
        0,
        1,
    );
    SessionManager::global().save_append(
        control_session,
        &complete_tool_archive(),
        "test-model",
        None,
        0,
        1,
    );

    // ── 中断会话：reload（首次装载 → timeline 缺失 → 从 messages.jsonl 重建）──
    let hub = reload_hub(&root, "epoch-hanging-1");
    let snapshot = hub
        .timeline_snapshot(session_id)
        .expect("timeline must be rebuilt from messages.jsonl");
    assert_eq!(snapshot.turns.len(), 1);
    let round_blocs = &snapshot.turns[0].rounds[0].blocks;
    assert!(
        !round_blocs
            .iter()
            .any(|b| b.kind == TimelineBlockKind::Tool),
        "interrupted reload must not surface a tool block: {round_blocs:?}"
    );
    assert!(
        !round_blocs
            .iter()
            .any(|b| b.text.contains("null") && b.text.contains("tool")),
        "no serialized hanging tool args in history: {round_blocs:?}"
    );
    // 正文不丢：只丢不可执行的悬挂调用。
    assert!(
        snapshot.turns[0].rounds[0]
            .blocks
            .iter()
            .any(|b| b.kind == TimelineBlockKind::Text && b.text == "partial answer")
    );

    // conversation 快照（前端 bootstrap 的 turns）同样不得出现悬挂 tool_calls。
    let conversation = hub.conversation_snapshot(session_id);
    let turns = conversation.state["turns"]
        .as_array()
        .expect("persisted turns in conversation snapshot")
        .clone();
    let tool_calls = turns
        .iter()
        .flat_map(|t| t["rounds"].as_array().cloned().unwrap_or_default())
        .flat_map(|r| r["tool_calls"].as_array().cloned().unwrap_or_default())
        .collect::<Vec<_>>();
    assert!(
        tool_calls.is_empty(),
        "conversation reload must not surface hanging tool calls: {tool_calls:?}"
    );

    // ── 第二次 reload：结果必须与首次一致（幂等，不因重建而变）──
    let reopened = reload_hub(&root, "epoch-hanging-2")
        .timeline_snapshot(session_id)
        .expect("timeline present after rebuild");
    assert_eq!(reopened, snapshot, "second reload must be identical");

    // ── control 组：完整工具环照常回放，过滤不误伤 ──
    let control = reload_hub(&root, "epoch-hanging-3")
        .timeline_snapshot(control_session)
        .expect("control timeline present");
    let control_tools = control.turns[0].rounds[0]
        .blocks
        .iter()
        .filter(|b| b.kind == TimelineBlockKind::Tool)
        .count();
    assert_eq!(
        control_tools, 1,
        "well-formed tool call must still replay: {:?}",
        control.turns[0].rounds[0].blocks
    );

    drop(hub);
}
