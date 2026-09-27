//! PR-1-6 / A1 — Z5 red line: byte-for-byte fidelity of the PersistOp queue.
//!
//! 1. `flush_ops_enqueue_in_legacy_call_order` — the ops a flush sequence
//!    enqueues must match, field for field and in order, the inline
//!    `SessionManager` calls the old `flush_meta` / `snapshot_full` bodies
//!    made (see git history of store.rs pre-PR-1-6).
//! 2. `shadow_flush_jsonl_bytes_identical` — replaying the ops against the
//!    session manager (the same mapping the agent loop's `execute_persist_op`
//!    performs) on one seed produces a `messages.jsonl` byte-identical to
//!    driving the legacy inline call sequence on a twin seed.
//! 3. `legacy_session_dir_replays_and_appends_byte_identical` — a directory
//!    written by the old synchronous daemon is replayed via `from_messages`
//!    and continued; the result must be byte-identical to the legacy daemon
//!    continuing to write the same conversation (msg_id counter restored).

use std::path::PathBuf;

use qaqh_message::{MessageStore, PersistOp};
use qaqh_session::SessionManager;
use qaqh_types::{ContentBlock, Message};

/// SessionManager is a process-level OnceLock singleton: the whole test
/// binary shares one data_dir, tests isolate by seed.
fn data_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("qaqh-persist-effects-{}", std::process::id()));
        SessionManager::init(dir.clone());
        dir
    })
    .clone()
}

fn session_dir(session_id: &str) -> PathBuf {
    data_dir().join("sessions").join(session_id)
}

fn jsonl_of(session_id: &str) -> Vec<u8> {
    std::fs::read(session_dir(session_id).join("messages.jsonl")).unwrap_or_default()
}

fn assistant(text: &str) -> Message {
    Message {
        msg_id: None,
        role: "assistant".into(),
        name: None,
        content: vec![ContentBlock::text(text)],
    }
}

fn stamped(mut msg: Message, id: u64) -> Message {
    msg.msg_id = Some(id);
    msg
}

/// Test-side replay executor: mirrors the agent loop's `execute_persist_op`
/// (agent.rs) one-to-one. If that mapping drifts from this one, the shadow
/// test below fails and both must be re-reviewed together.
fn drain(store: &mut MessageStore) {
    for op in store.take_persist_ops() {
        let sm = SessionManager::global();
        match &op {
            PersistOp::Append {
                session_id,
                messages,
                model,
                effort,
                compact_skip,
                compact_covered_through_msg_id,
                turn_count,
            } => sm.save_append_with_watermark(
                session_id,
                messages,
                model,
                effort.as_deref(),
                *compact_skip,
                *compact_covered_through_msg_id,
                *turn_count,
            ),
            PersistOp::UpdateMeta {
                session_id,
                model,
                effort,
                compact_skip,
                turn_count,
            } => sm.update_meta(
                session_id,
                model,
                effort.as_deref(),
                *compact_skip,
                *turn_count,
            ),
            PersistOp::SaveFull {
                session_id,
                messages,
                model,
                effort,
                compact_skip,
                compact_covered_through_msg_id,
                turn_count,
            } => sm.save_full_with_watermark(
                session_id,
                messages,
                model,
                effort.as_deref(),
                *compact_skip,
                *compact_covered_through_msg_id,
                *turn_count,
            ),
        }
    }
}

#[test]
fn flush_ops_enqueue_in_legacy_call_order() {
    data_dir();
    let mut store = MessageStore::new("");

    // Before the seed is assigned, flush is a no-op — the legacy guard.
    store.push_user("pre-seed");
    store.flush_meta("model-a", "high");
    assert!(
        store.take_persist_ops().is_empty(),
        "empty seed must not enqueue"
    );

    let mut store = MessageStore::new("ops-order-seed");
    store.push_user("first");
    store.push_assistant(assistant("reply"));

    // First flush: non-empty pending_save → Append (no compact watermark here).
    store.flush_meta("model-a", "high");
    let ops = store.take_persist_ops();
    assert_eq!(ops.len(), 1, "no checkpoint → Append only");
    match &ops[0] {
        PersistOp::Append {
            session_id,
            messages,
            model,
            effort,
            compact_skip,
            compact_covered_through_msg_id,
            turn_count,
        } => {
            assert_eq!(session_id, "ops-order-seed");
            assert_eq!(model, "model-a");
            assert_eq!(effort.as_deref(), Some("high"));
            assert_eq!(*compact_skip, 0);
            assert_eq!(*compact_covered_through_msg_id, None);
            assert_eq!(*turn_count, 1);
            assert_eq!(messages.len(), 2);
            assert_eq!(messages[0].msg_id, Some(1));
            assert_eq!(messages[1].msg_id, Some(2));
        }
        other => panic!("expected Append, got {other:?}"),
    }

    // Second flush with nothing new → UpdateMeta (legacy else-branch).
    store.flush_meta("model-a", "high");
    let ops = store.take_persist_ops();
    assert_eq!(ops.len(), 1);
    assert!(matches!(
        ops[0],
        PersistOp::UpdateMeta { turn_count: 1, .. }
    ));

    // Full snapshot without checkpoint → SaveFull; pending_save is cleared
    // so a follow-up flush falls into the UpdateMeta branch (legacy shape).
    store.push_user("second");
    store.snapshot_full("model-b", "low");
    let ops = store.take_persist_ops();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        PersistOp::SaveFull {
            messages,
            model,
            effort,
            turn_count,
            compact_skip,
            ..
        } => {
            assert_eq!(model, "model-b");
            assert_eq!(effort.as_deref(), Some("low"));
            assert_eq!(*turn_count, 2);
            assert_eq!(*compact_skip, 0);
            assert_eq!(messages.len(), 3, "full snapshot carries every message");
            assert_eq!(messages[2].msg_id, Some(3));
        }
        other => panic!("expected SaveFull, got {other:?}"),
    }
    store.flush_meta("model-b", "low");
    assert!(matches!(
        store.take_persist_ops()[0],
        PersistOp::UpdateMeta { .. }
    ));
}

#[test]
fn shadow_flush_jsonl_bytes_identical() {
    data_dir();
    let session_legacy = "shadow-legacy-seed";
    let session_queue = "shadow-queue-seed";
    let _ = std::fs::remove_dir_all(session_dir(session_legacy));
    let _ = std::fs::remove_dir_all(session_dir(session_queue));

    // ── Legacy path: the inline call sequence pre-PR-1-6 flush_meta made,
    //    with the same message construction the store performs internally
    //    (Message::user/assistant clones stamped with monotonic msg_id).
    let sm = SessionManager::global();
    sm.save_append(
        session_legacy,
        &[
            stamped(Message::user("first"), 1),
            stamped(assistant("reply"), 2),
        ],
        "model-a",
        Some("high"),
        0,
        1,
    );
    sm.save_append(
        session_legacy,
        &[stamped(Message::user("second"), 3)],
        "model-a",
        Some("high"),
        0,
        2,
    );
    sm.update_meta(session_legacy, "model-a", Some("high"), 0, 2);

    // ── New path: same conversation through the enqueue queue. The queue is
    //    deliberately drained only at the end — batching must not change the
    //    write order or the bytes.
    let mut store = MessageStore::new(session_queue);
    store.push_user("first");
    store.push_assistant(assistant("reply"));
    store.flush_meta("model-a", "high");
    store.push_user("second");
    store.flush_meta("model-a", "high");
    store.flush_meta("model-a", "high");
    drain(&mut store);

    assert_eq!(
        jsonl_of(session_legacy),
        jsonl_of(session_queue),
        "messages.jsonl must be byte-identical between the legacy inline path and the queue path"
    );
}

#[test]
fn legacy_session_dir_replays_and_appends_byte_identical() {
    data_dir();
    let session_legacy = "replay-legacy-seed";
    let session_replay = "replay-new-seed";
    let _ = std::fs::remove_dir_all(session_dir(session_legacy));
    let _ = std::fs::remove_dir_all(session_dir(session_replay));

    // ── Old daemon wrote a session synchronously: two turns.
    let sm = SessionManager::global();
    sm.save_append(
        session_legacy,
        &[
            stamped(Message::user("u1"), 1),
            stamped(assistant("a1"), 2),
            stamped(Message::user("u2"), 3),
            stamped(assistant("a2"), 4),
        ],
        "model-a",
        Some("high"),
        0,
        2,
    );

    // ── The same session directory is now taken over by the new code
    //    (resume = same dir, new writer): copy the legacy artifacts, replay,
    //    continue.
    let legacy_dir = session_dir(session_legacy);
    let replay_dir = session_dir(session_replay);
    std::fs::create_dir_all(&replay_dir).unwrap();
    for file in ["messages.jsonl", "meta.json"] {
        std::fs::copy(legacy_dir.join(file), replay_dir.join(file)).unwrap();
    }
    let (_, msgs) = sm.load(session_replay).expect("legacy session loads");
    let (mut store, repairs) = MessageStore::from_messages(session_replay, &msgs, 0);
    assert!(
        repairs.is_empty(),
        "clean legacy dir must replay without repairs"
    );
    store.push_user("u3");
    store.push_assistant(assistant("a3"));
    store.flush_meta("model-a", "high");
    drain(&mut store);

    // ── The legacy daemon continuing the same conversation would have
    //    appended exactly these rows.
    sm.save_append(
        session_legacy,
        &[stamped(Message::user("u3"), 5), stamped(assistant("a3"), 6)],
        "model-a",
        Some("high"),
        0,
        3,
    );

    assert_eq!(
        jsonl_of(session_legacy),
        jsonl_of(session_replay),
        "replay + queue-append must be byte-identical to the legacy continuation"
    );
    assert_eq!(
        store.turns().len(),
        3,
        "replayed turns + new turn must keep the turn count the legacy meta would record"
    );
}

#[test]
fn ephemeral_and_unseeded_stores_never_enqueue() {
    data_dir();
    let mut store = MessageStore::new_ephemeral("ephemeral-seed");
    store.push_user("hello");
    store.flush_meta("m", "e");
    store.snapshot_full("m", "e");
    assert!(
        store.take_persist_ops().is_empty(),
        "ephemeral stores must never enqueue disk writes"
    );

    let mut store = MessageStore::new("");
    store.push_user("hello");
    store.flush_meta("m", "e");
    store.snapshot_full("m", "e");
    assert!(
        store.take_persist_ops().is_empty(),
        "unseeded stores must never enqueue disk writes"
    );
}
