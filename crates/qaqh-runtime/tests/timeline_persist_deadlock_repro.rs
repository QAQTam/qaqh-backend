//! Regression guard: timeline persistence must never re-lock `timeline_store`.
//!
//! History (2026-09-12): commit `b6e1d96` added offload rehydration calls made
//! **while already holding** the `timeline_store` mutex; `rehydrate_offloaded_turns`
//! then locked the same mutex again on the same thread. `std::sync::Mutex` is not
//! reentrant, so both write paths parked forever while holding the lock:
//!
//! - `timeline_hub.rs::persist_timeline_sync` (sync path, reached by
//!   `flush_timeline_persistence`; at bug time also by `publish_timeline(TurnSealed)`,
//!   which since issue #28 only enqueues terminal persistence);
//! - the `qaqh-timeline-persist` worker thread (coalesced checkpoints).
//!
//! Symptom was a frozen session (the writer thread never published the sealed
//! turn) plus a wedged store mutex (bootstrap/restart recovery blocked behind it).
//!
//! Fix: `rehydrate_offloaded_turns` now takes the caller-held `&mut TimelineStore`
//! borrow, so re-entrancy is impossible by construction.
//! Keep this file as the regression guard: both tests below must pass.

use std::time::Duration;

use qaqh_domain::{
    TimelineBlockKind, TimelineIntent, TimelineTool, TimelineToolState, TimelineTurnState,
};
use qaqh_runtime::RingingHub;

fn temp_root(tag: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "qaqh-timeline-deadlock-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create temp root");
    root
}

fn open_turn(hub: &RingingHub, session_id: &str) {
    hub.publish_timeline(
        session_id,
        TimelineIntent::TurnOpened {
            turn_id: "t1".into(),
            user_text: "hello".into(),
        },
    )
    .expect("turn opened");
}

fn open_large_progress_tool(hub: &RingingHub, session_id: &str) {
    hub.publish_timeline(
        session_id,
        TimelineIntent::BlockOpened {
            turn_id: "t1".into(),
            round_num: 0,
            block_id: "tool".into(),
            kind: TimelineBlockKind::Tool,
            tool: Some(TimelineTool {
                exit_code: None,
                completed_at_ms: None,
                tool_call_id: "call-1".into(),
                name: "exec".into(),
                state: TimelineToolState::Running,
                summary: None,
                args_json: None,
                output: None,
                diff: None,
                progress: String::new(),
                progress_truncated: false,
                progress_stream: None,
                progress_bytes_total: 0,
                display: None,
                failure: None,
                permission: None,
            }),
        },
    )
    .expect("tool block opened");
    hub.publish_timeline(
        session_id,
        TimelineIntent::ToolProgress {
            turn_id: "t1".into(),
            round_num: 0,
            block_id: "tool".into(),
            chunk: "x".repeat(2 * 1024 * 1024),
            stream: None,
            bytes_total: 0,
        },
    )
    .expect("large progress accepted");
    hub.publish_timeline(
        session_id,
        TimelineIntent::BlockSealed {
            turn_id: "t1".into(),
            round_num: 0,
            block_id: "tool".into(),
        },
    )
    .expect("tool block sealed");
    hub.publish_timeline(
        session_id,
        TimelineIntent::RoundSealed {
            turn_id: "t1".into(),
            round_num: 0,
            is_final: true,
        },
    )
    .expect("round sealed");
}

/// Publish path: since issue #28 `TurnSealed` no longer persists synchronously
/// on the caller thread — it enqueues terminal persistence — but the publish
/// call must still return; with the bug the caller thread deadlocked.
#[test]
fn turn_sealed_sync_persist_returns() {
    let root = temp_root("sync");
    let hub = std::sync::Arc::new(RingingHub::with_persistence("epoch-deadlock-sync", &root));
    open_turn(&hub, "seed-sync");

    let (tx, rx) = std::sync::mpsc::channel();
    let caller_hub = std::sync::Arc::clone(&hub);
    std::thread::spawn(move || {
        let result = caller_hub.publish_timeline(
            "seed-sync",
            TimelineIntent::TurnSealed {
                turn_id: "t1".into(),
                state: TimelineTurnState::Completed,
                failure: None,
            },
        );
        let _ = tx.send(result.map(|_| ()));
    });

    assert!(
        rx.recv_timeout(Duration::from_secs(10)).is_ok(),
        "publish_timeline(TurnSealed) never returned within 10s: \
         the sync persist path self-deadlocks on `timeline_store` \
         (`persist_timeline_sync` -> `rehydrate_offloaded_turns`)"
    );
    std::mem::forget(hub); // Drop would join the (possibly stuck) persist worker
}

#[test]
fn turn_sealed_with_large_tool_progress_returns_and_flushes() {
    let root = temp_root("sync-large-progress");
    let hub = RingingHub::with_persistence("epoch-deadlock-large-progress", &root);
    open_turn(&hub, "seed-large-progress");
    open_large_progress_tool(&hub, "seed-large-progress");

    let (tx, rx) = std::sync::mpsc::channel();
    let caller_hub = std::sync::Arc::new(hub);
    let worker_hub = std::sync::Arc::clone(&caller_hub);
    std::thread::spawn(move || {
        let result = worker_hub.publish_timeline(
            "seed-large-progress",
            TimelineIntent::TurnSealed {
                turn_id: "t1".into(),
                state: TimelineTurnState::Completed,
                failure: None,
            },
        );
        if result.is_ok() {
            worker_hub.flush_timeline_persistence();
        }
        let _ = tx.send(result.map(|_| ()));
    });

    assert!(
        rx.recv_timeout(Duration::from_secs(10)).is_ok(),
        "TurnSealed with large tool progress never returned or flushed within 10s"
    );
    caller_hub.flush_timeline_persistence();
    let snapshot = caller_hub
        .timeline_snapshot("seed-large-progress")
        .expect("sealed snapshot");
    let tool = snapshot.turns[0].rounds[0].blocks[0]
        .tool
        .as_ref()
        .expect("tool block");
    assert!(snapshot.turns[0].offloaded, "sealed turn must be offloaded");
    assert!(tool.progress.len() <= 512);
    assert!(tool.progress_truncated);
    std::mem::forget(caller_hub);
}

/// Async path: the coalesced checkpoint worker must write
/// `ringing-timeline/{seed}.json` within one checkpoint window (1s) + margin.
#[test]
fn async_worker_writes_checkpoint() {
    let root = temp_root("async");
    let hub = RingingHub::with_persistence("epoch-deadlock-async", &root);
    open_turn(&hub, "seed-async"); // non-terminal intent -> requests a checkpoint

    let timeline_file = root.join("ringing-timeline").join("seed-async.json");
    let deadline = std::time::Instant::now() + Duration::from_secs(6);
    while std::time::Instant::now() < deadline {
        if timeline_file.exists() {
            std::mem::forget(hub);
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let message = format!(
        "async persist worker never wrote {}: \
         it self-deadlocks on `timeline_store` while draining a checkpoint",
        timeline_file.display()
    );
    std::mem::forget(hub);
    panic!("{message}");
}
