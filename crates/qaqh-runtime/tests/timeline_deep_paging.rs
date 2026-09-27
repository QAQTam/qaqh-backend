//! BUG-2026-09-15-05：timeline 深翻页——重建窗口之外、归档之内的回合必须可达。
//!
//! 独立测试文件是**必须**的：`SessionManager` 是进程级单例，`init` 只能调用一次
//! （第二次会 panic）。同文件里放第二条测试就会撞上这一点。

use std::path::PathBuf;

use qaqh_runtime::RingingHub;
use qaqh_session::SessionManager;
use qaqh_types::{ContentBlock, Message};

fn temp_root(tag: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("qaqh-timeline-deep-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create temp root");
    root
}

/// 造 n 轮「一问一答」的消息（每轮一条 user 起一个新回合）。
fn turns_messages(n: usize) -> Vec<Message> {
    let mut messages = vec![Message::system("system")];
    for i in 1..=n {
        messages.push(Message::user(&format!("q{i}")));
        messages.push(Message {
            msg_id: None,
            role: Message::ROLE_ASSISTANT.to_string(),
            name: None,
            content: vec![ContentBlock::Text {
                text: format!("a{i}"),
            }],
        });
    }
    messages
}

/// **BUG-2026-09-15-05 回归**：重建后常驻窗口只有最近 40 轮，但归档里更早的回合
/// 必须**可达**——否则长会话在 UI 上永久丢失历史（T-08 只让这件事「可见」，没让它
/// 「可达」）。
///
/// 同时钉住两件容易做错的事：
/// 1. **id 必须是全局的**。`projection::build_turns` 合成的是「已加载消息池内下标」，
///    池大小取决于读了多长，同一回合在不同池里会是不同的 `t{n}` —— 客户端按 id 拼页，
///    池内下标会让相邻两页对不上。
/// 2. 逐页往回翻能一直翻到第 1 轮，且每页的 `start` 单调下降。
#[test]
fn deep_paging_reaches_the_oldest_archived_turn() {
    let root = temp_root("paging");
    SessionManager::init(root.clone());
    let session_id = "deep-paging-seed";
    SessionManager::global().save_append(
        session_id,
        &turns_messages(60),
        "test-model",
        None,
        0,
        60,
    );

    let hub = RingingHub::with_persistence("epoch-deep", root.join("ringing"))
        .with_sessions(SessionManager::global());

    // 常驻窗口 = 最近 40 轮，id 由全局序号派生 → t21..t60
    let snapshot = hub
        .timeline_snapshot(session_id)
        .expect("rebuilt from archive");
    assert_eq!(
        snapshot.turns.len(),
        40,
        "重建窗口仍是 40 轮（Phase 2 的有界性不变）"
    );
    assert_eq!(
        snapshot.turns.first().unwrap().turn_id,
        "t21",
        "id 必须全局派生；池内下标会给出 t1"
    );
    assert_eq!(snapshot.turns.last().unwrap().turn_id, "t60");

    // 深翻页：从窗口最旧一端往回，一页一页翻到第 1 轮。
    let (page, start, capped) = hub
        .archive_turn_page(session_id, 21, 10)
        .expect("archive page");
    assert!(!capped, "60 轮远在 4000 条消息的上限之内");
    assert_eq!(start, 11);
    assert_eq!(page.first().unwrap().turn_id, "t12");
    assert_eq!(page.last().unwrap().turn_id, "t21");

    let (page, start, _) = hub
        .archive_turn_page(session_id, 11, 10)
        .expect("archive page");
    assert_eq!(start, 1);
    assert_eq!(page.first().unwrap().turn_id, "t2");

    let (page, start, capped) = hub
        .archive_turn_page(session_id, 1, 10)
        .expect("archive page");
    assert_eq!(start, 0, "翻到第 1 轮即历史起点");
    assert!(!capped);
    assert_eq!(page.first().unwrap().turn_id, "t1");
    assert_eq!(
        page.first().unwrap().user_text,
        "q1",
        "内容取自归档而非紧凑视图"
    );

    // 序号与 id 必须一致（客户端用序号当游标、用 id 拼页，两者错位会拼错回合）。
    for (offset, turn) in page.iter().enumerate() {
        assert_eq!(turn.turn_id, format!("t{}", start + offset + 1));
    }

    let _ = std::fs::remove_dir_all(root);
}
