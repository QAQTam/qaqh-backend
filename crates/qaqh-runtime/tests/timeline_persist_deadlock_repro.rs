//! Regression guard: timeline persistence must never re-lock `timeline_store`.
//!
//! History (2026-09-12): commit `b6e1d96` added offload rehydration calls made
//! **while already holding** the `timeline_store` mutex; `rehydrate_offloaded_turns`
//! then locked the same mutex again on the same thread. `std::sync::Mutex` is not
//! reentrant, so both write paths parked forever while holding the lock:
//!
//! - `timeline_hub.rs::persist_timeline_sync` (sync path, reached by
//!   `publish_timeline(TurnSealed)` and `flush_timeline_persistence`);
//! - the `qaqh-timeline-persist` worker thread (coalesced checkpoints).
//!
//! Symptom was a frozen session (the writer thread never published the sealed
//! turn) plus a wedged store mutex (bootstrap/restart recovery blocked behind it).
//!
//! Fix: `rehydrate_offloaded_turns` now takes `&TimelineStore` — the caller passes
//! the borrow it already holds — so re-entrancy is impossible by construction.
//! Keep this file as the regression guard: both tests below must pass.

use std::time::Duration;

use qaqh_domain::{TimelineIntent, TimelineTurnState};
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

fn open_turn(hub: &RingingHub, seed: &str) {
    hub.publish_timeline(
        seed,
        TimelineIntent::TurnOpened {
            turn_id: "t1".into(),
            user_text: "hello".into(),
        },
    )
    .expect("turn opened");
}

/// Sync path: `TurnSealed` persists synchronously on the caller thread.
/// The call must return; with the bug the caller thread deadlocks.
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
