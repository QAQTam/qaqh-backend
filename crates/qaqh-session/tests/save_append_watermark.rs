//! BUG-2026-09-13-32 回归：`save_append` 的幂等判据（归档内最大 `msg_id`）
//! 必须与归档大小**解耦**——增量水位缓存 + 尾读重建，而非每批全量扫描。
//!
//! 反向测试（修复前必红）：把同一份大归档反复 append 小批次，单批耗时
//! 不得随归档体积线性增长；水位必须与全量扫描结果等价（幂等判据不退化）。
//!
//! 本测试是「契约级」回归而非微基准：用**归档体积比**与**扫描次数**这类
//! 规模无关的量断言，避免在 CI 机器上做易抖动的墙钟阈值判定。

use qaqh_session::{SessionManager, store};
use qaqh_types::Message;
use std::path::PathBuf;
use std::sync::Arc;

fn manager() -> (PathBuf, Arc<SessionManager>) {
    // 测试进程内 WorkspaceStore/水位缓存是单例：每个用例用独立目录，
    // 并清空水位缓存，避免用例间互相污染（缓存按目录路径键控）。
    store::reset_watermarks();
    let root = std::env::temp_dir().join(format!(
        "qaqh-session-wm-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let sessions_dir = root.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("create test sessions");
    SessionManager::init_for_test(root.join("data"));
    let manager = Arc::new(SessionManager::new_for_test(
        sessions_dir,
        root.join(".active_session"),
    ));
    (root, manager)
}

fn id_msg(id: u64, text: &str) -> Message {
    let mut msg = Message::user(text);
    msg.msg_id = Some(id);
    msg
}

/// 归档体积（字节）——水位是否退化为全量扫描的规模判据。
fn archive_bytes(dir: &std::path::Path) -> u64 {
    std::fs::metadata(dir.join("messages.jsonl"))
        .map(|m| m.len())
        .unwrap_or(0)
}

/// 内部一致性：`max_msg_id` 仍是权威（水位是其缓存视图）。
#[test]
fn max_msg_id_matches_full_scan() {
    let (root, _sm) = manager();
    let dir = root.join("sessions").join("seed");
    std::fs::create_dir_all(&dir).expect("seed dir");
    let batch: Vec<Message> = (1..=200).map(|i| id_msg(i, "x")).collect();
    store::append_messages(&dir, &batch).expect("append");

    assert_eq!(store::max_msg_id(&dir), 200, "全量扫描必须仍是唯一权威判据");
    assert_eq!(
        store::watermark_msg_id(&dir),
        200,
        "水位重建必须与全量扫描一致"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 反向测试（修复前红）：反复 append 的批次，不得每批做一次全量扫描。
#[test]
fn repeated_appends_do_not_rescan_the_archive() {
    let (root, sm) = manager();
    let session_id = "hot-seed";
    let dir = root.join("sessions").join(session_id);

    // 先落一份大归档（约 5 MB 级），再反复小批 append。
    let bulk: Vec<Message> = (1..=60_000)
        .map(|i| id_msg(i, "0123456789abcdef0123456789abcdef"))
        .collect();
    sm.save_append(session_id, &bulk, "m", None, 0, 1);
    // 逼出一次身份失效（外部 writer 追加）：水位必须自愈而非退化。
    store::append_one(&dir, &id_msg(60_001, "external-tail")).expect("external tail");
    let base = archive_bytes(&dir);
    assert!(base > 1_000_000, "前置归档需足够大，实测 {base} 字节");

    // 规模不变量：缓存命中路径（归档身份未变）不得触发任何全量扫描。
    //
    // 基线：外部追加让归档身份失效 → 首次 save_append 允许一次重建
    // （O(尾部)；尾部含 id），其后的批次必须零全量扫描。这正是
    // 「每批 O(历史)」→「身份变化时一次」的差别，也是本回归的判据。
    let scans_before = store::scan_count(&dir);
    let mut next_id = 60_002;
    for _ in 0..50 {
        sm.save_append(session_id, &[id_msg(next_id, "batch")], "m", None, 0, 2);
        let batch_index = next_id - 60_001;
        assert_eq!(
            store::scan_count(&dir) - scans_before,
            0,
            "第 {batch_index} 批 save_append 触发了全量扫描（归档 {} 字节）——水位未生效",
            archive_bytes(&dir)
        );
        // 权威等价性核对（调用本身是全量扫描，故只在末尾做一次）。
        next_id += 1;
    }
    assert_eq!(
        store::max_msg_id(&dir),
        next_id - 1,
        "50 批后水位必须与权威全量扫描一致（不得丢 id）"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 幂等判据不得退化：重复批仍须被去重（水位与全量扫描等价）。
#[test]
fn append_idempotency_survives_watermark_cache() {
    let (root, sm) = manager();
    let session_id = "idem";
    let dir = root.join("sessions").join(session_id);

    sm.save_append(
        session_id,
        &[id_msg(1, "a"), id_msg(2, "b")],
        "m",
        None,
        0,
        1,
    );
    let lines_before = std::fs::read_to_string(dir.join("messages.jsonl"))
        .expect("archive")
        .lines()
        .count();

    // 同一批重放（WAL replay 场景）：必须被过滤，不新增行。
    sm.save_append(
        session_id,
        &[id_msg(1, "a"), id_msg(2, "b")],
        "m",
        None,
        0,
        1,
    );
    let lines_after = std::fs::read_to_string(dir.join("messages.jsonl"))
        .expect("archive")
        .lines()
        .count();
    assert_eq!(lines_before, lines_after, "重复 msg_id 必须幂等过滤");

    // 新 id 仍能写入。
    sm.save_append(session_id, &[id_msg(3, "c")], "m", None, 0, 2);
    assert_eq!(
        std::fs::read_to_string(dir.join("messages.jsonl"))
            .expect("archive")
            .lines()
            .count(),
        3
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 归档被外部进程原地追加（另一 writer / 手工修复）后，水位必须自愈。
#[test]
fn external_tail_append_invalidates_watermark() {
    let (root, sm) = manager();
    let session_id = "external";
    let dir = root.join("sessions").join(session_id);

    sm.save_append(session_id, &[id_msg(1, "a")], "m", None, 0, 1);
    assert_eq!(store::watermark_msg_id(&dir), 1);

    // 绕过 SessionManager 直接追加一行（模拟第二 writer）。
    store::append_one(&dir, &id_msg(99, "external")).expect("external append");

    assert_eq!(
        store::watermark_msg_id(&dir),
        99,
        "归档长度变化必须作废缓存并重建水位"
    );
    // 幂等判据随之更新：重复的 99 不再落盘。
    sm.save_append(session_id, &[id_msg(99, "external")], "m", None, 0, 2);
    assert_eq!(
        std::fs::read_to_string(dir.join("messages.jsonl"))
            .expect("archive")
            .lines()
            .count(),
        2
    );

    // save_full（undo/compact 全量重写）后水位必须重基，不得残留旧高水位。
    sm.save_full(session_id, &[id_msg(1, "a")], "m", None, 0, 1);
    assert_eq!(store::watermark_msg_id(&dir), 1, "全量重写后水位重基");
    sm.save_append(session_id, &[id_msg(2, "b")], "m", None, 0, 2);
    assert_eq!(
        std::fs::read_to_string(dir.join("messages.jsonl"))
            .expect("archive")
            .lines()
            .count(),
        2,
        "save_full 后低 id 必须可再写入（水位被旧值抬高就丢了）"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 会话删除 + 同 seed 重建：水位不得跨生命周期泄漏。
#[test]
fn delete_then_recreate_starts_from_scratch() {
    let (root, sm) = manager();
    let session_id = "recycled";
    let dir = root.join("sessions").join(session_id);

    sm.save_append(session_id, &[id_msg(500, "old")], "m", None, 0, 1);
    sm.delete(session_id).expect("delete");
    sm.persist_new_session(session_id);
    sm.save_append(session_id, &[id_msg(1, "new")], "m", None, 0, 1);

    assert_eq!(
        std::fs::read_to_string(dir.join("messages.jsonl"))
            .expect("archive")
            .lines()
            .count(),
        1,
        "新建会话的首批消息必须写入（水位残留在旧高水位即被误判重复）"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// 尾部损坏行（崩溃写一半）不得把水位带偏。
#[test]
fn torn_tail_line_does_not_lower_watermark() {
    let (root, sm) = manager();
    let session_id = "torn";
    let dir = root.join("sessions").join(session_id);

    sm.save_append(
        session_id,
        &[id_msg(1, "a"), id_msg(2, "b")],
        "m",
        None,
        0,
        1,
    );
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("messages.jsonl"))
            .expect("open");
        file.write_all(b"{\"msg_id\":3,\"role\":\"user\",\"conte")
            .expect("torn write");
    }

    assert_eq!(
        store::watermark_msg_id(&dir),
        2,
        "残缺尾行忽略，水位取最后一条完整行的最大值"
    );
    let _ = std::fs::remove_dir_all(root);
}
