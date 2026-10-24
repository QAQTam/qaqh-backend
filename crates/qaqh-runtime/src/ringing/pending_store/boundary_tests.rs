//! Public-store fault boundaries; these do not prove HTTP dispatch exactly-once.

use std::sync::{Arc, Barrier, Mutex};

use super::{PendingCommandStore, RECEIPT_POLICY, ReceiptLoadError};
use qaqh_ringing::{RingingCommandState, RingingV2CommandResult, RingingV2ExistingResult};

const COMMAND: &str = "92455601-b53a-4f25-8df5-94124676055b";
const OWNER: &str = "lease-original";
const FINGERPRINT: &str = "payload-and-guards-hash";

fn reserve(store: &mut PendingCommandStore) -> bool {
    store
        .record_fingerprint_for_session(COMMAND, FINGERPRINT, OWNER)
        .expect("matching owner and payload")
}

fn assert_unknown(store: &PendingCommandStore, command_id: &str, owner: &str) {
    assert!(store.status_for_session(command_id, owner).is_none());
    assert!(store.v2_status_for_session(command_id, owner).is_none());
    assert!(
        store
            .existing_receipt_for_session(command_id, owner)
            .is_none()
    );
}

fn seed_receipt(path: &std::path::Path, accepted_at_ms: u64) {
    // Synthetic on-disk input exercises the public loader without wall-clock
    // sleeps, a global data-dir override, or mutations of private store fields.
    let saved = serde_json::json!({
        (COMMAND): {
            "fingerprint": FINGERPRINT,
            "client_session_id": OWNER,
            "accepted_at_ms": accepted_at_ms,
            "state": "running"
        }
    });
    std::fs::write(path, serde_json::to_vec(&saved).unwrap()).unwrap();
}

#[test]
fn receipt_policy_is_machine_readable_and_matches_store_defaults() {
    assert_eq!(
        serde_json::to_value(RECEIPT_POLICY).unwrap(),
        serde_json::json!({
            "ttl_secs": 300,
            "max_entries": 4096,
            "owner_scope": "client_session_id",
            "id_scope": "store",
            "persistence": "best_effort"
        })
    );
}

#[test]
fn concurrent_same_id_has_one_reservation_and_replays_the_rest() {
    let store = Arc::new(Mutex::new(PendingCommandStore::new()));
    let start = Arc::new(Barrier::new(16));
    let accepted = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..16)
            .map(|_| {
                let store = Arc::clone(&store);
                let start = Arc::clone(&start);
                scope.spawn(move || {
                    start.wait();
                    reserve(&mut store.lock().unwrap())
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|accepted| *accepted)
            .count()
    });
    assert_eq!(accepted, 1, "only the successful reservation may dispatch");
    assert_eq!(
        store
            .lock()
            .unwrap()
            .v2_status_for_session(COMMAND, OWNER)
            .unwrap()
            .state,
        RingingCommandState::Accepted
    );
}

#[test]
fn concurrent_conflicting_payloads_cannot_both_reserve_same_id() {
    let store = Arc::new(Mutex::new(PendingCommandStore::new()));
    let start = Arc::new(Barrier::new(2));
    let results = std::thread::scope(|scope| {
        let workers: Vec<_> = ["payload-a", "payload-b"]
            .into_iter()
            .map(|fingerprint| {
                let store = Arc::clone(&store);
                let start = Arc::clone(&start);
                scope.spawn(move || {
                    start.wait();
                    store.lock().unwrap().record_fingerprint_for_session(
                        COMMAND,
                        fingerprint,
                        OWNER,
                    )
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Ok(true)))
            .count(),
        1
    );
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
}

#[test]
fn lost_ack_status_and_replay_preserve_each_known_outcome() {
    for state in [
        RingingCommandState::Accepted,
        RingingCommandState::Running,
        RingingCommandState::Succeeded,
        RingingCommandState::Failed,
        RingingCommandState::Rejected,
    ] {
        let mut store = PendingCommandStore::new();
        assert!(reserve(&mut store));
        let terminal = !matches!(
            state,
            RingingCommandState::Accepted | RingingCommandState::Running
        );
        let result = terminal.then(|| RingingV2CommandResult::PlanReviewResolved {
            interaction_id: "interaction-synthetic".into(),
            approved: false,
        });
        let event_id = terminal.then(|| "terminal-event-synthetic".to_string());
        let error = matches!(
            state,
            RingingCommandState::Failed | RingingCommandState::Rejected
        )
        .then(|| "synthetic_failure".to_string());
        if state == RingingCommandState::Running {
            store.mark_running(COMMAND);
        } else if terminal {
            store.mark_terminal_with_result(
                COMMAND,
                state,
                event_id.clone(),
                error.clone(),
                result.clone(),
            );
        }
        assert!(
            !reserve(&mut store),
            "lost ACK must not create a new reservation"
        );
        let status = store.v2_status_for_session(COMMAND, OWNER).unwrap();
        assert_eq!(status.state, state);
        assert_eq!(status.payload_fingerprint, FINGERPRINT);
        assert_eq!(status.terminal_event_id, event_id);
        assert_eq!(status.error_code, error);
        assert_eq!(status.result, result);
        let replay = store.existing_receipt_for_session(COMMAND, OWNER).unwrap();
        assert_eq!(replay.payload_fingerprint, status.payload_fingerprint);
        assert_eq!(
            replay.into_existing(),
            RingingV2ExistingResult::CommandReceipt {
                state,
                terminal_event_id: status.terminal_event_id,
                error_code: status.error_code,
                result: status.result,
            }
        );
        assert!(
            store
                .record_fingerprint_for_session(COMMAND, "changed-guard-or-payload", OWNER)
                .is_err()
        );
    }
}

#[test]
fn lease_replacement_cannot_reconcile_or_take_over_receipts() {
    let mut store = PendingCommandStore::new();
    assert!(reserve(&mut store));
    store.mark_terminal(
        COMMAND,
        RingingCommandState::Succeeded,
        Some("event".into()),
        None,
    );
    let mut leases = super::super::lease_store::RingingLeaseStore::new();
    leases.open(OWNER.into(), "same-client-instance".into());
    leases.open("lease-reopened".into(), "same-client-instance".into());
    assert!(!leases.is_active_session(OWNER));
    assert!(leases.is_active_session("lease-reopened"));
    assert_unknown(&store, COMMAND, "lease-reopened");
    assert!(
        store
            .record_fingerprint_for_session(COMMAND, FINGERPRINT, "lease-reopened")
            .is_err()
    );
    // Storage retention does not grant the old lease HTTP authorization.
    assert!(store.v2_status_for_session(COMMAND, OWNER).is_some());
}

#[test]
fn capacity_eviction_means_unknown_and_allows_a_new_reservation() {
    let mut store = PendingCommandStore::new();
    assert!(reserve(&mut store));
    store.mark_terminal(
        COMMAND,
        RingingCommandState::Succeeded,
        Some("event".into()),
        None,
    );
    for index in 0..RECEIPT_POLICY.max_entries {
        assert!(
            store
                .record_fingerprint_for_session(&format!("filler-{index}"), "hash", OWNER)
                .unwrap()
        );
    }
    assert_unknown(&store, COMMAND, OWNER);
    assert!(!store.is_known(COMMAND));
    // This is why the SDK must not automatically retry an unknown side effect.
    assert!(reserve(&mut store));
    assert_eq!(
        store.v2_status_for_session(COMMAND, OWNER).unwrap().state,
        RingingCommandState::Accepted
    );
}

#[test]
fn ttl_and_future_timestamp_are_unknown_on_load_not_terminal_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("receipts.json");
    let now = super::unix_millis();
    for timestamp in [now - RECEIPT_POLICY.ttl_secs * 1000, now + 86_400_000] {
        seed_receipt(&path, timestamp);
        let mut store = PendingCommandStore::open_persistent_at(&path).unwrap();
        assert_unknown(&store, COMMAND, OWNER);
        assert!(!store.is_known(COMMAND));
        assert!(
            reserve(&mut store),
            "absent receipt does not protect a retry"
        );
    }
}

#[test]
fn loading_retains_original_age_instead_of_extending_ttl() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("receipts.json");
    let original_timestamp = super::unix_millis() - 60_000;
    seed_receipt(&path, original_timestamp);
    let mut store = PendingCommandStore::open_persistent_at(&path).unwrap();
    assert_eq!(
        store.v2_status_for_session(COMMAND, OWNER).unwrap().state,
        RingingCommandState::Running
    );
    assert!(!reserve(&mut store));
    store.mark_running(COMMAND); // Flush through the same persistence path.
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let written_timestamp = saved[COMMAND]["accepted_at_ms"].as_u64().unwrap();
    assert!(
        written_timestamp.abs_diff(original_timestamp) < 30_000,
        "loading/persisting must not reset the original acceptance age by a minute"
    );
}

#[test]
fn restart_keeps_nonterminal_receipts_nonterminal_and_terminal_payloads_replayable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("receipts.json");
    let mut store = PendingCommandStore::open_persistent_at(&path).unwrap();
    for (id, state) in [
        ("accepted", RingingCommandState::Accepted),
        ("running", RingingCommandState::Running),
        (COMMAND, RingingCommandState::Failed),
    ] {
        assert!(
            store
                .record_fingerprint_for_session(id, FINGERPRINT, OWNER)
                .unwrap()
        );
        if state == RingingCommandState::Running {
            store.mark_running(id);
        }
        if state == RingingCommandState::Failed {
            store.mark_terminal_with_result(
                id,
                state,
                Some("event".into()),
                Some("failure".into()),
                Some(RingingV2CommandResult::PermissionResolved {
                    interaction_id: "permission-synthetic".into(),
                    approved: false,
                }),
            );
        }
    }
    let before = store.v2_status_for_session(COMMAND, OWNER).unwrap();
    drop(store);
    let mut reloaded = PendingCommandStore::open_persistent_at(&path).unwrap();
    assert_eq!(
        reloaded.v2_status_for_session(COMMAND, OWNER).unwrap(),
        before
    );
    assert!(!reserve(&mut reloaded));
    for (id, state) in [
        ("accepted", RingingCommandState::Accepted),
        ("running", RingingCommandState::Running),
    ] {
        assert_eq!(
            reloaded.v2_status_for_session(id, OWNER).unwrap().state,
            state,
            "restart alone cannot infer execution outcome or redispatch"
        );
    }
    assert_unknown(&reloaded, COMMAND, "lease-after-restart");
}

#[test]
fn restart_rebuilds_causation_index_for_uuid_command_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("receipts.json");
    let mut store = PendingCommandStore::open_persistent_at(&path).unwrap();
    assert!(reserve(&mut store));
    store.mark_running(COMMAND);
    drop(store);
    let mut reloaded = PendingCommandStore::open_persistent_at(&path).unwrap();
    let event =
        super::tests::control_event(qaqh_session::canonical::causation_for_command(COMMAND));
    let event_id = event.event_id.0.clone();
    reloaded.observe_projection_events(&[event]);
    let status = reloaded.v2_status_for_session(COMMAND, OWNER).unwrap();
    assert_eq!(status.state, RingingCommandState::Succeeded);
    assert_eq!(status.terminal_event_id.as_deref(), Some(event_id.as_str()));
}

#[test]
fn persisted_rollback_and_missing_file_are_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("receipts.json");
    let mut store = PendingCommandStore::open_persistent_at(&path).unwrap();
    assert_unknown(&store, COMMAND, OWNER);
    assert!(reserve(&mut store));
    store.rollback(COMMAND);
    drop(store);
    let mut reloaded = PendingCommandStore::open_persistent_at(&path).unwrap();
    assert_unknown(&reloaded, COMMAND, OWNER);
    assert!(reserve(&mut reloaded));
}

#[test]
fn explicit_path_loader_reports_corruption_and_io_failures() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("receipts.json");
    for invalid in ["not json", "[]", "{\"receipt\":{}}"] {
        std::fs::write(&path, invalid).unwrap();
        assert!(matches!(
            PendingCommandStore::open_persistent_at(&path),
            Err(ReceiptLoadError::Decode(_))
        ));
    }
    assert!(matches!(
        PendingCommandStore::open_persistent_at(dir.path()),
        Err(ReceiptLoadError::Read(_))
    ));
}

#[test]
fn best_effort_persistence_does_not_promise_durable_ack() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("blocked-parent");
    let path = parent.join("receipts.json");
    let mut store = PendingCommandStore::open_persistent_at(&path).unwrap();
    std::fs::write(&parent, "not a directory").unwrap();
    assert!(
        reserve(&mut store),
        "current reservation success does not prove durable storage"
    );
    assert!(store.v2_status_for_session(COMMAND, OWNER).is_some());
    assert!(!path.exists());
    assert!(matches!(
        PendingCommandStore::open_persistent_at(&path),
        Err(ReceiptLoadError::Read(_))
    ));
}
