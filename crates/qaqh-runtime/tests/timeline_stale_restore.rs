//! 回归：**合法但落后**的 timeline 快照不得被当作权威装载。
//!
//! 事故形状（2026-09-12，seed 6ffdb118 实测）：持久化在「首回合刚开、尚无内容」
//! 那一刻中断（先冻结、后文件永久停留在该状态），磁盘上的
//! `ringing-timeline/{seed}.json` 于是是 **合法 JSON**：`watermark=2`、
//! 单回合、`rounds=[]`。此后每次 daemon 启动都把它当权威 restore，客户端
//! （TUI 在断线重连 / 重启后 re-baseline）只剩第一条 user 消息——
//! `messages.jsonl` 里后续 3 个回合永久不可见。
//!
//! 契约（本文件锁住）：
//! 1. 快照尾部与会话归档不一致 = 落后 → 从 `messages.jsonl` 重建后再装载；
//! 2. 自愈必须落盘（下次进程读到的是修好的快照，不再依赖重复自愈）；
//! 3. 尾部一致的窗口化快照**不得**被重建（否则长会话每次装载都重写 → 自激）。
//!
//! `SessionManager` 是进程级单例（`init` 只能调用一次），因此本文件所有测试
//! 共用一个 data root，各自使用独立 seed。

use std::path::PathBuf;
use std::sync::OnceLock;

use qaqh_runtime::RingingHub;
use qaqh_session::SessionManager;
use qaqh_types::{ContentBlock, Message};

/// 进程级共享 data root（只 `init` 一次；不清理，避免与并行测试互相拔根）。
fn shared_root() -> PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root = std::env::temp_dir().join(format!(
            "qaqh-timeline-stale-restore-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create temp root");
        SessionManager::init(root.clone());
        root
    })
    .clone()
}

fn assistant_message(text: &str) -> Message {
    Message {
        msg_id: None,
        role: Message::ROLE_ASSISTANT.to_string(),
        name: None,
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
    }
}

/// 三个完整回合（各一问一答）的归档消息 + turn_count=3。
fn three_turn_messages() -> (Vec<Message>, usize) {
    let mut messages = vec![Message::system("system")];
    for turn in 1..=3 {
        messages.push(Message::user(&format!("question {turn}")));
        messages.push(assistant_message(&format!("answer {turn}")));
    }
    (messages, 3)
}

fn timeline_path(root: &std::path::Path, session_id: &str) -> PathBuf {
    root.join("ringing")
        .join("ringing-timeline")
        .join(format!("{session_id}.json"))
}

fn write_timeline_record(path: &std::path::Path, body: serde_json::Value) {
    std::fs::create_dir_all(path.parent().expect("timeline dir")).expect("create timeline dir");
    std::fs::write(path, serde_json::to_vec(&body).expect("serialize record"))
        .expect("write timeline record");
}

#[test]
fn stale_timeline_snapshot_is_rebuilt_from_messages_instead_of_restored() {
    let root = shared_root();
    let session_id = "stale-timeline-seed";
    let (messages, turn_count) = three_turn_messages();
    SessionManager::global().save_append(session_id, &messages, "test-model", None, 0, turn_count);

    // 事故快照形状：合法 JSON、单回合、空 rounds、watermark=2。
    let path = timeline_path(&root, session_id);
    write_timeline_record(
        &path,
        serde_json::json!({
            "session_id": session_id,
            "snapshot": {
                "watermark": 2,
                "turns": [{
                    "turn_id": "t1",
                    "created_seq": 1,
                    "user_text": "question 1",
                    "sealed": true,
                    "state": "completed",
                    "rounds": []
                }]
            },
            "journal": []
        }),
    );

    let hub = RingingHub::with_persistence("epoch-stale", root.join("ringing"))
        .with_sessions(SessionManager::global());
    let snapshot = hub.timeline_snapshot(session_id).expect("timeline present");
    assert_eq!(
        snapshot.turns.len(),
        3,
        "落后快照必须被归档投影替换（否则前端只剩第一条 user 消息）"
    );
    assert_eq!(
        snapshot.turns.last().map(|turn| turn.user_text.as_str()),
        Some("question 3"),
        "最新回合必须重新可见"
    );

    // 自愈落盘：磁盘记录不再是那条被冻结的单回合快照。
    let healed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("healed record readable"))
            .expect("healed record is valid json");
    assert_eq!(
        healed["snapshot"]["turns"].as_array().map(Vec::len),
        Some(3),
        "自愈后的快照必须写回磁盘"
    );

    // 下一次启动（新 hub）直接读到修好的快照：回合数与尾部都保持正确。
    drop(hub);
    let hub = RingingHub::with_persistence("epoch-stale-2", root.join("ringing"))
        .with_sessions(SessionManager::global());
    let reopened = hub.timeline_snapshot(session_id).expect("timeline present");
    assert_eq!(reopened.turns.len(), 3);
    assert_eq!(
        reopened.turns.last().map(|turn| turn.user_text.as_str()),
        Some("question 3")
    );
}

#[test]
fn windowed_but_tail_consistent_snapshot_is_restored_without_rebuild() {
    let root = shared_root();
    let session_id = "fresh-timeline-seed";
    let (messages, turn_count) = three_turn_messages();
    SessionManager::global().save_append(session_id, &messages, "test-model", None, 0, turn_count);

    // 窗口化快照：只有最后两个回合（回合数 < turn_count），但尾部与归档一致。
    // watermark 是"未被重建"的硬证据：重建会重新分配 seq（并重新物化新 turn）。
    let path = timeline_path(&root, session_id);
    write_timeline_record(
        &path,
        serde_json::json!({
            "session_id": session_id,
            "snapshot": {
                "watermark": 4242,
                "turns": [
                    {"turn_id": "t2", "created_seq": 10, "user_text": "question 2",
                     "sealed": true, "state": "completed", "rounds": []},
                    {"turn_id": "t3", "created_seq": 20, "user_text": "question 3",
                     "sealed": true, "state": "completed", "rounds": []}
                ]
            },
            "journal": []
        }),
    );

    let hub = RingingHub::with_persistence("epoch-fresh", root.join("ringing"))
        .with_sessions(SessionManager::global());
    let snapshot = hub.timeline_snapshot(session_id).expect("timeline present");
    assert_eq!(
        snapshot.watermark, 4242,
        "尾部一致的快照必须原样装载（重建会自激重写长会话）"
    );
    assert_eq!(snapshot.turns.len(), 2);
    assert_eq!(
        snapshot.turns.last().map(|turn| turn.user_text.as_str()),
        Some("question 3")
    );
}
