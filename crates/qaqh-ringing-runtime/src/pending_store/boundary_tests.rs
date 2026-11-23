//! Public-store fault boundaries; these do not prove HTTP dispatch exactly-once.
//!
//! v4.4 P1-3：receipt 权威进 `command_receipts` 后，旧的
//! `open_persistent_at(path)` 文件夹具整体换成「独立 Db 句柄 = 重启」与
//! `receipts::upsert` 播种（过期年龄用同一套原语词表构造，不手改私有字段）。

use std::sync::{Arc, Barrier, Mutex};

use super::{PendingCommandStore, RECEIPT_POLICY};
use crate::pending_store::receipts;
use qaqh_ringing::{RingingCommandState, RingingV2CommandResult, RingingV2ExistingResult};
use qaqh_session::db::Db;

const COMMAND: &str = "92455601-b53a-4f25-8df5-94124676055b";
const OWNER: &str = "lease-original";
const FINGERPRINT: &str = "payload-and-guards-hash";

/// 独立 Db 句柄 = 一次「重启」：同一文件、新执行器、新内存态。
fn store_on(db: &Arc<Db>) -> PendingCommandStore {
    PendingCommandStore::new(Arc::clone(db))
}

fn new_store() -> (tempfile::TempDir, Arc<Db>, PendingCommandStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(Db::open(dir.path().join("qaqh.db")).expect("open db"));
    let store = store_on(&db);
    (dir, db, store)
}

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

/// 合成一条指定年龄的行（不经 wall-clock sleep）。
fn seed_row(db: &Arc<Db>, command_id: &str, accepted_at_ms: i64, state: &str) {
    let row = receipts::ReceiptRow {
        command_id: command_id.to_string(),
        fingerprint: FINGERPRINT.to_string(),
        client_session_id: Some(OWNER.to_string()),
        state: state.to_string(),
        terminal_event_id: None,
        error_code: None,
        result_json: None,
        sync_result_json: None,
        accepted_at_ms,
    };
    let commandless = row;
    db.write_blocking(move |conn| receipts::upsert(conn, &commandless))
        .expect("seed row");
}

#[test]
fn receipt_policy_is_machine_readable_and_matches_store_defaults() {
    assert_eq!(
        serde_json::to_value(RECEIPT_POLICY).unwrap(),
        serde_json::json!({
            "ttl_secs": 300,
            "max_entries": 4096,
            "owner_scope": "identity_instance_key",
            "id_scope": "store",
            "persistence": "best_effort"
        })
    );
}

#[test]
fn concurrent_same_id_has_one_reservation_and_replays_the_rest() {
    let (_dir, _db, store) = new_store();
    let store = Arc::new(Mutex::new(store));
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
    let (_dir, _db, store) = new_store();
    let store = Arc::new(Mutex::new(store));
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
        let (_dir, _db, mut store) = new_store();
        assert!(reserve(&mut store));
        let terminal = !matches!(
            state,
            RingingCommandState::Accepted | RingingCommandState::Running
        );
        let result = terminal.then(|| RingingV2CommandResult::PermissionResolved {
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
        assert!(!reserve(&mut store), "lost ACK must not create a new reservation");
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
                sync_result: status.sync_result,
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
fn receipts_reconcile_across_reopen_for_same_identity_but_not_other_identities() {
    let (_dir, db, mut store) = new_store();
    assert!(reserve(&mut store));
    store.mark_terminal(
        COMMAND,
        RingingCommandState::Succeeded,
        Some("event".into()),
        None,
    );
    // §3/§5：owner 是稳定身份键，同一设备的连接（含重开后的新 cs）并存且
    // 都能对账；不同设备的身份冒用同一 command_id 仍被指纹守卫拒绝。
    let mut leases = super::super::lease_store::RingingLeaseStore::new();
    leases.open("cs-1".into(), OWNER.into(), None);
    leases.open("cs-2".into(), OWNER.into(), None);
    assert!(leases.is_active_session("cs-1"), "同身份多连接并存（§3）");
    assert!(leases.is_active_session("cs-2"));
    // 「重启」后的 store 仍对同身份重复提交返回原回执（非错误）。
    let mut reloaded = store_on(&db);
    assert_eq!(
        reloaded
            .record_fingerprint_for_session(COMMAND, FINGERPRINT, OWNER)
            .expect("same identity reconciles"),
        false,
        "duplicate replay returns the stored receipt"
    );
    assert!(
        store
            .record_fingerprint_for_session(COMMAND, FINGERPRINT, "device-2")
            .is_err()
    );
    assert_unknown(&store, COMMAND, "device-2");
    // Storage retention does not grant a foreign identity authorization.
    assert!(store.v2_status_for_session(COMMAND, OWNER).is_some());
}

#[test]
fn capacity_eviction_means_unknown_and_allows_a_new_reservation() {
    let (_dir, _db, mut store) = new_store();
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
fn ttl_and_future_timestamp_are_unknown_not_terminal_failure() {
    let (_dir, db, _store0) = new_store();
    let now = super::unix_millis() as i64;
    for timestamp in [now - RECEIPT_POLICY.ttl_secs as i64 * 1000, now + 86_400_000] {
        seed_row(&db, COMMAND, timestamp, "running");
        let mut store = store_on(&db);
        assert_unknown(&store, COMMAND, OWNER);
        assert!(!store.is_known(COMMAND));
        assert!(
            reserve(&mut store),
            "absent receipt does not protect a retry"
        );
        // 清场，下一轮时间戳重新播种。
        store.rollback(COMMAND);
    }
}

#[test]
fn updates_retain_original_age_instead_of_extending_ttl() {
    let (_dir, db, _store0) = new_store();
    let original_timestamp = super::unix_millis() as i64 - 60_000;
    seed_row(&db, COMMAND, original_timestamp, "running");
    let mut store = store_on(&db);
    assert_eq!(
        store.v2_status_for_session(COMMAND, OWNER).unwrap().state,
        RingingCommandState::Running
    );
    assert!(!reserve(&mut store));
    store.mark_running(COMMAND); // Flush through the same persistence path.
    let written = store
        .db
        .read_blocking(|conn| {
            conn.query_row(
                "SELECT accepted_at_ms FROM command_receipts WHERE command_id = ?1",
                [COMMAND],
                |row| row.get::<_, i64>(0),
            )
        })
        .expect("read stored age");
    assert!(
        (written - original_timestamp).unsigned_abs() < 30_000,
        "state updates must not reset the original acceptance age by a minute"
    );
}

#[test]
fn restart_keeps_nonterminal_receipts_nonterminal_and_terminal_payloads_replayable() {
    let (_dir, db, _store0) = new_store();
    let mut store = store_on(&db);
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
    let mut reloaded = store_on(&db);
    assert_eq!(reloaded.v2_status_for_session(COMMAND, OWNER).unwrap(), before);
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
fn persisted_rollback_survives_restart() {
    let (_dir, db, _store0) = new_store();
    let mut store = store_on(&db);
    assert_unknown(&store, COMMAND, OWNER);
    assert!(reserve(&mut store));
    store.rollback(COMMAND);
    drop(store);
    let mut reloaded = store_on(&db);
    assert_unknown(&reloaded, COMMAND, OWNER);
    assert!(reserve(&mut reloaded));
}

#[test]
fn corrupt_row_vocabulary_is_unknown_not_fabricated() {
    // 旧形态用坏 JSON 文件测 Decode；DB 形态的对应物是坏 state 词表行。
    let (_dir, db, _store0) = new_store();
    seed_row(&db, "cmd-corrupt", super::unix_millis() as i64, "quantum");
    let store = store_on(&db);
    // 查询面：坏词表 = unknown（不伪装回执）。
    assert_unknown(&store, "cmd-corrupt", OWNER);
    // 闸门面：坏词表行不得给出「重复」放行——重放必须走 Mismatch。
    let mut gate = store_on(&db);
    assert!(
        gate.record_fingerprint_for_session("cmd-corrupt", FINGERPRINT, OWNER)
            .is_err(),
        "unparseable stored state must not be replayed as a duplicate"
    );
}

#[test]
fn reservation_is_durable_immediately_after_commit() {
    // 旧 `best_effort_persistence_does_not_promise_durable_ack` 的 DB 等价物
    // 反转为正证：reserve 返回 true 即已 COMMIT（WAL + synchronous=FULL），
    // 另一个 store 句柄（模拟重启）立刻能看到。文件时代「内存有、盘上没有」
    // 的窗口在这里不存在——持久性承诺升级，用例方向随之翻正。
    let (_dir, db, _store0) = new_store();
    let mut store = store_on(&db);
    assert!(reserve(&mut store));
    let witness = store_on(&db);
    assert!(witness.v2_status_for_session(COMMAND, OWNER).is_some());
}

// ── B05:同步命令结果的保存与重放 ─────────────────────────────────────────

use qaqh_ringing::RingingV2SyncResult;

fn sync_sample() -> RingingV2SyncResult {
    RingingV2SyncResult {
        operation: "session.create".into(),
        result: serde_json::json!({ "session_id": "0199a0f0-0000-7000-8000-000000000001" }),
        revision: None,
    }
}

#[test]
fn lost_sync_ack_restores_original_result_via_replay_and_status() {
    let (_dir, _db, mut store) = new_store();
    assert!(reserve(&mut store));
    let sample = sync_sample();
    store.mark_terminal_sync(COMMAND, sample.clone());

    // 同 ID 同指纹重放:不新建执行,existing 携带原 result/operation/revision。
    assert!(!reserve(&mut store), "replay must not re-execute");
    let replay = store.existing_receipt_for_session(COMMAND, OWNER).unwrap();
    assert_eq!(replay.state, RingingCommandState::Succeeded);
    assert_eq!(
        replay.sync_result.as_ref(),
        Some(&sample),
        "重放必须恢复首次执行的完整同步结果"
    );
    match replay.into_existing() {
        RingingV2ExistingResult::CommandReceipt { sync_result, .. } => {
            assert_eq!(sync_result.as_ref(), Some(&sample));
        }
        other => panic!("expected command receipt, got {other:?}"),
    }

    // status 轮询同样恢复原结果。
    let status = store.v2_status_for_session(COMMAND, OWNER).unwrap();
    assert_eq!(status.sync_result.as_ref(), Some(&sample));
}

#[test]
fn engine_succeeded_without_sync_result_is_not_fabricated() {
    let (_dir, _db, mut store) = new_store();
    assert!(reserve(&mut store));
    store.mark_terminal(COMMAND, RingingCommandState::Succeeded, None, None);
    let status = store.v2_status_for_session(COMMAND, OWNER).unwrap();
    assert_eq!(
        status.sync_result, None,
        "引擎类命令 succeeded 不得伪装出同步结果"
    );
    let replay = store.existing_receipt_for_session(COMMAND, OWNER).unwrap();
    assert_eq!(replay.sync_result, None);
}

#[test]
fn sync_result_survives_restart() {
    let (_dir, db, _store0) = new_store();
    let mut store = store_on(&db);
    assert!(reserve(&mut store));
    store.mark_terminal_sync(COMMAND, sync_sample());
    drop(store);

    let reloaded = store_on(&db);
    let status = reloaded.v2_status_for_session(COMMAND, OWNER).unwrap();
    assert_eq!(
        status.sync_result.map(|s| s.operation),
        Some("session.create".into()),
        "DB 权威必须带回同步结果"
    );
}

#[test]
fn conflicting_payload_still_rejected_after_sync_terminal() {
    let (_dir, _db, mut store) = new_store();
    assert!(reserve(&mut store));
    store.mark_terminal_sync(COMMAND, sync_sample());
    assert!(
        store
            .record_fingerprint_for_session(COMMAND, "different-payload", OWNER)
            .is_err(),
        "同 ID 不同 payload 在终态后仍被指纹守卫拒绝"
    );
}
