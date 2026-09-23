//! End-to-end BUG-006 regression: a missing timeline file must be rebuilt
//! from `messages.jsonl`, so deleting the projection never deletes history.

use std::path::PathBuf;

use qaqh_domain::{TimelineBlockKind, TimelineToolState, TimelineTurnState};
use qaqh_runtime::RingingHub;
use qaqh_session::SessionManager;
use qaqh_types::{ContentBlock, Message};

fn temp_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "qaqh-timeline-rebuild-{}-{}",
        std::process::id(),
        tag
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create temp root");
    root
}

fn session_messages() -> Vec<Message> {
    let assistant = Message {
        msg_id: None,
        role: Message::ROLE_ASSISTANT.to_string(),
        name: None,
        content: vec![
            ContentBlock::Reasoning {
                reasoning: "thinking".to_string(),
            },
            ContentBlock::ToolUse {
                id: "call-1".to_string(),
                name: "read".to_string(),
                input: serde_json::json!({ "path": "/tmp/a" }),
            },
            ContentBlock::Text {
                text: "answer".to_string(),
            },
        ],
    };
    vec![
        Message::system("system"),
        Message::user("question"),
        assistant,
        Message::tool("call-1", "line one\nline two", true),
    ]
}

#[test]
fn missing_or_corrupt_timeline_is_rebuilt_from_persisted_messages() {
    let root = temp_root("e2e");
    SessionManager::init(root.clone());
    let seed = "rebuilt-seed";
    SessionManager::global().save_append(seed, &session_messages(), "test-model", None, 0, 1);

    // 第二个会话模拟“timeline 文件存在但已损坏”：文件名仍能进懒加载索引，
    // 内容无法解析，恢复路径必须退回 messages.jsonl 重建而不是返回空快照。
    let corrupt_seed = "corrupt-timeline-seed";
    SessionManager::global().save_append(
        corrupt_seed,
        &session_messages(),
        "test-model",
        None,
        0,
        1,
    );
    let ringing_root = root.join("ringing");
    let corrupt_timeline = ringing_root
        .join("ringing-timeline")
        .join(format!("{corrupt_seed}.json"));
    std::fs::create_dir_all(corrupt_timeline.parent().expect("timeline dir"))
        .expect("create timeline dir");
    std::fs::write(&corrupt_timeline, b"{not-json").expect("write corrupt timeline");

    // `ringing/` 的其它 seed 没有任何 timeline 记录，
    // 相当于用户删除了整个 timeline 目录。
    let hub = RingingHub::with_persistence("epoch-rebuild", ringing_root)
        .with_sessions(SessionManager::global());
    let snapshot = hub
        .timeline_snapshot(seed)
        .expect("timeline must be rebuilt from messages.jsonl");

    assert_eq!(snapshot.turns.len(), 1);
    let turn = &snapshot.turns[0];
    assert_eq!(turn.user_text, "question");
    assert!(turn.sealed);
    assert!(turn.offloaded);
    assert_eq!(turn.state, TimelineTurnState::Completed);
    assert_eq!(turn.rounds.len(), 1);
    let round = &turn.rounds[0];
    assert!(round.sealed);
    assert!(round.is_final);
    assert_eq!(round.blocks.len(), 3);
    assert_eq!(round.blocks[0].kind, TimelineBlockKind::Reasoning);
    assert_eq!(round.blocks[0].text, "thinking");
    let tool = round.blocks[1].tool.as_ref().expect("tool block");
    assert_eq!(tool.state, TimelineToolState::Succeeded);
    assert!(tool.output.is_none());
    assert_eq!(round.blocks[2].text, "answer");

    let page = hub.rehydrate_timeline_page(seed, snapshot.turns.clone());
    assert_eq!(
        page[0].rounds[0].blocks[1]
            .tool
            .as_ref()
            .expect("restored tool block")
            .output
            .as_deref(),
        Some("line one\nline two")
    );

    // 第二次读取走的是刚写回的 timeline 文件，结果必须一致。
    let second = hub
        .timeline_snapshot(seed)
        .expect("persisted rebuilt timeline");
    assert_eq!(second, snapshot);

    // 损坏记录也必须从 messages.jsonl 重建。
    let corrupted = hub
        .timeline_snapshot(corrupt_seed)
        .expect("corrupt timeline rebuilt from messages.jsonl");
    assert_eq!(corrupted.turns.len(), 1);
    assert_eq!(corrupted.turns[0].user_text, "question");
    assert!(
        !std::fs::read(&corrupt_timeline)
            .expect("rebuilt file readable")
            .is_empty()
    );

    // ── #314 回归锁：新会话不得靠重建凭空占用 seq 空间 ──
    //
    // 起点是新会话首个回合：`messages.jsonl` 里已经有「正在进行的首条用户消息」，
    // 但没有任何**已完成**回合（`meta.turn_count == 0`），而 timeline 文件尚未落盘。
    // 修复前 `ensure_timeline_loaded` 的 `None` 分支会无条件重建，产出一个只有
    // `user_text`、没有任何 block 的**空回合**（seq 1 = TurnOpened / 2 = TurnSealed），
    // 该回合随即 seal、其 journal 条目又被 seal 裁剪删掉 → `restore(watermark=2,
    // journal=[])`：seq 1、2 被吃掉却既无 live 投递、也不在 journal 里；真正的 live
    // 回合只能从 seq 3 起，客户端 cursor=0 按 `cursor + 1` 判 gap → gap→快照恢复
    // 循环 → transcript 永不渲染。
    let fresh_seed = "fresh-in-progress-seed";
    SessionManager::global().save_append(
        fresh_seed,
        &[Message::system("system"), Message::user("hi")],
        "test-model",
        None,
        0,
        0, // turn_count = 0：没有任何**已完成**回合
    );
    assert!(
        hub.timeline_snapshot(fresh_seed).is_none(),
        "新会话（无已完成回合）不得靠重建凭空占用 seq 空间"
    );

    drop(hub);
    let _ = std::fs::remove_dir_all(root);
}
