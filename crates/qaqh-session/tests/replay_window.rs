//! Replay window persistence, floor computation and cursor expiry contract.

use std::fs;

use qaqh_session::canonical::{
    ContentClock, REPLAY_WINDOW_FILE, REPLAY_WINDOW_SCHEMA, ReplayWindowConfig,
    ReplayWindowManifest, ReplayWindowReason, SnapshotStatus, load_replay_window_manifest,
    recover_replay_window_manifest, sha256_content_hash,
};
use qaqh_session::projection::{ReplayOutcome, replay_reliable};
use qaqh_session::session_fact_v2::{
    ContentHash, END_OF_FACT, EventId, FactPayload, LogId, ReliableCursor, ResetReason,
    SessionFact, SessionTitleChanged, TitleSource,
};

const ENVELOPE_FIXTURE: &str = include_str!("fixtures/session_fact_v2/envelope.jsonl");

fn log_id() -> LogId {
    LogId::new("0198f1a0-0000-7000-8000-000000000002")
}

fn fact(fact_seq: u64, ts_ms: i64) -> SessionFact {
    let mut fact: SessionFact =
        serde_json::from_str(ENVELOPE_FIXTURE.trim()).expect("parse envelope fixture");
    fact.log_id = log_id();
    fact.fact_seq = fact_seq;
    fact.event_id = EventId::new(format!("01J{fact_seq:023}"));
    fact.ts_ms = ts_ms;
    fact.payload = FactPayload::SessionTitleChanged(SessionTitleChanged {
        title: format!("title-{fact_seq}"),
        source: TitleSource::Auto,
        changed_at_ms: ts_ms,
    });
    fact
}

fn cursor(fact_seq: u64, projection_index: u16) -> ReliableCursor {
    ReliableCursor {
        log_id: log_id(),
        fact_seq,
        projection_index,
    }
}

fn config() -> ReplayWindowConfig {
    ReplayWindowConfig::default()
}

fn snapshot_manifest(
    snapshot_fact_seq: u64,
    snapshot_bytes: &[u8],
    created_at: i64,
    expires_at: i64,
    logical_now: i64,
) -> ReplayWindowManifest {
    ReplayWindowManifest {
        schema: REPLAY_WINDOW_SCHEMA.into(),
        log_id: log_id(),
        generation: 1,
        earliest_available_fact_seq: 1,
        latest_fact_seq: 3,
        earliest_cursor: cursor(1, 0),
        snapshot_cursor: Some(cursor(snapshot_fact_seq, END_OF_FACT)),
        snapshot_generation: 1,
        snapshot_hash: sha256_content_hash(snapshot_bytes),
        snapshot_path: "snapshot-1.json".into(),
        snapshot_fact_seq: Some(snapshot_fact_seq),
        snapshot_created_at_logical_ms: Some(created_at),
        snapshot_expires_at_logical_ms: Some(expires_at),
        logical_now_ms: logical_now,
        retained_from_ms: 100,
        retained_until_ms: 300,
        window_capacity_facts: 100,
        window_capacity_bytes: 64 * 1024 * 1024,
        retained_facts: 3,
        retained_bytes: 3,
        reason: ReplayWindowReason::SnapshotRotated,
    }
}

fn write_manifest(dir: &std::path::Path, manifest: &ReplayWindowManifest) {
    fs::write(
        dir.join(REPLAY_WINDOW_FILE),
        serde_json::to_vec_pretty(manifest).expect("serialize manifest"),
    )
    .expect("write manifest");
}

fn write_snapshot(dir: &std::path::Path, bytes: &[u8]) {
    fs::create_dir_all(dir.join("snapshots")).expect("create snapshots dir");
    fs::write(dir.join("snapshots/snapshot-1.json"), bytes).expect("write snapshot");
}

#[test]
fn empty_log_writes_a_valid_empty_window() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (manifest, clock) =
        recover_replay_window_manifest(temp.path(), &log_id(), &[], config()).expect("recover");

    assert_eq!(clock.logical_now_ms(), 0);
    assert_eq!(manifest.generation, 1);
    assert_eq!(manifest.latest_fact_seq, 0);
    assert_eq!(manifest.earliest_available_fact_seq, 1);
    assert_eq!(manifest.retained_facts, 0);
    assert_eq!(manifest.retained_bytes, 0);
    assert_eq!(manifest.snapshot_cursor, None);
    assert_eq!(manifest.reason, ReplayWindowReason::Manual);
    assert!(temp.path().join(REPLAY_WINDOW_FILE).exists());
    assert!(!temp.path().join("replay-window.tmp").exists());
}

#[test]
fn fact_capacity_sets_the_sequence_floor() {
    let temp = tempfile::tempdir().expect("tempdir");
    let facts = (1..=5)
        .map(|seq| fact(seq, seq as i64 * 10))
        .collect::<Vec<_>>();
    let config = ReplayWindowConfig {
        window_capacity_facts: 2,
        ..config()
    };

    let (manifest, _) =
        recover_replay_window_manifest(temp.path(), &log_id(), &facts, config).expect("recover");

    assert_eq!(manifest.earliest_available_fact_seq, 4);
    assert_eq!(manifest.retained_facts, 2);
    assert_eq!(manifest.reason, ReplayWindowReason::CapacityFacts);
}

#[test]
fn byte_capacity_sets_the_sequence_floor_and_retained_bytes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let facts = (1..=3)
        .map(|seq| fact(seq, seq as i64 * 10))
        .collect::<Vec<_>>();
    let second = serde_json::to_vec(&facts[1])
        .expect("serialize second fact")
        .len() as u64;
    let third = serde_json::to_vec(&facts[2])
        .expect("serialize third fact")
        .len() as u64;
    let config = ReplayWindowConfig {
        window_capacity_bytes: second + third,
        ..config()
    };

    let (manifest, _) =
        recover_replay_window_manifest(temp.path(), &log_id(), &facts, config).expect("recover");

    assert_eq!(manifest.earliest_available_fact_seq, 2);
    assert_eq!(manifest.retained_facts, 2);
    assert_eq!(manifest.retained_bytes, second + third);
    assert_eq!(manifest.reason, ReplayWindowReason::CapacityBytes);
}

#[test]
fn retention_uses_the_content_clock_and_can_expire_all_facts() {
    let temp = tempfile::tempdir().expect("tempdir");
    let facts = (1..=3)
        .map(|seq| fact(seq, seq as i64 * 100))
        .collect::<Vec<_>>();
    let _clock =
        ContentClock::open_or_recover(temp.path(), &facts, Some(1_000)).expect("advance clock");
    let config = ReplayWindowConfig {
        replay_window_retention_ms: 100,
        ..config()
    };

    let (manifest, clock) =
        recover_replay_window_manifest(temp.path(), &log_id(), &facts, config).expect("recover");

    assert_eq!(clock.logical_now_ms(), 1_000);
    assert_eq!(manifest.logical_now_ms, 1_000);
    assert_eq!(manifest.earliest_available_fact_seq, 3);
    assert_eq!(manifest.retained_facts, 1);
    assert_eq!(manifest.reason, ReplayWindowReason::Retention);
}

#[test]
fn manifest_generation_changes_only_when_the_window_changes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (first, _) =
        recover_replay_window_manifest(temp.path(), &log_id(), &[], config()).expect("first");
    let (second, _) =
        recover_replay_window_manifest(temp.path(), &log_id(), &[], config()).expect("second");
    assert_eq!(first, second);
    assert_eq!(second.generation, 1);

    let facts = vec![fact(1, 100)];
    let (third, _) =
        recover_replay_window_manifest(temp.path(), &log_id(), &facts, config()).expect("third");
    assert_eq!(third.generation, 2);
}

#[test]
fn manifest_clock_floor_is_recovered_into_the_content_clock() {
    let temp = tempfile::tempdir().expect("tempdir");
    let facts = vec![fact(1, 100), fact(2, 200)];
    let manifest = ReplayWindowManifest {
        schema: REPLAY_WINDOW_SCHEMA.into(),
        log_id: log_id(),
        generation: 1,
        earliest_available_fact_seq: 1,
        latest_fact_seq: 2,
        earliest_cursor: cursor(1, 0),
        snapshot_cursor: None,
        snapshot_generation: 0,
        snapshot_hash: sha256_content_hash(b""),
        snapshot_path: String::new(),
        snapshot_fact_seq: None,
        snapshot_created_at_logical_ms: None,
        snapshot_expires_at_logical_ms: None,
        logical_now_ms: 1_000,
        retained_from_ms: 100,
        retained_until_ms: 200,
        window_capacity_facts: 100,
        window_capacity_bytes: 64 * 1024 * 1024,
        retained_facts: 2,
        retained_bytes: 2,
        reason: ReplayWindowReason::Manual,
    };
    write_manifest(temp.path(), &manifest);

    let (recovered, clock) =
        recover_replay_window_manifest(temp.path(), &log_id(), &facts, config()).expect("recover");

    assert_eq!(recovered.logical_now_ms, 1_000);
    assert_eq!(clock.logical_now_ms(), 1_000);
}

#[test]
fn valid_snapshot_sets_the_snapshot_floor_and_cursor() {
    let temp = tempfile::tempdir().expect("tempdir");
    let facts = (1..=3)
        .map(|seq| fact(seq, seq as i64 * 100))
        .collect::<Vec<_>>();
    let snapshot_bytes = b"snapshot-bytes";
    write_snapshot(temp.path(), snapshot_bytes);
    write_manifest(
        temp.path(),
        &snapshot_manifest(2, snapshot_bytes, 100, 1_000, 300),
    );
    let config = ReplayWindowConfig {
        window_capacity_facts: 1,
        ..config()
    };

    let (manifest, _) =
        recover_replay_window_manifest(temp.path(), &log_id(), &facts, config).expect("recover");

    assert_eq!(manifest.earliest_available_fact_seq, 3);
    assert_eq!(manifest.snapshot_cursor, Some(cursor(2, END_OF_FACT)));
    assert_eq!(
        manifest
            .snapshot_status(temp.path(), config)
            .expect("snapshot status"),
        SnapshotStatus::Valid
    );
    let reset = manifest
        .reset_for_cursor(temp.path(), &cursor(1, 0), &facts, config)
        .expect("reset check")
        .expect("expired cursor");
    assert_eq!(reset.reason, ResetReason::CursorExpired);
    assert_eq!(reset.snapshot_cursor, Some(cursor(2, END_OF_FACT)));

    let outcome = replay_reliable(Vec::new(), Some(&cursor(1, 0)), &manifest.replay_window())
        .expect("replay harness");
    let ReplayOutcome::ResetRequired(reset) = outcome else {
        panic!("expected reset");
    };
    assert_eq!(reset.reason, ResetReason::CursorExpired);
    assert_eq!(reset.snapshot_cursor, Some(cursor(2, END_OF_FACT)));
}

#[test]
fn missing_snapshot_returns_snapshot_missing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let facts = (1..=3)
        .map(|seq| fact(seq, seq as i64 * 100))
        .collect::<Vec<_>>();
    write_manifest(
        temp.path(),
        &snapshot_manifest(2, b"missing", 100, 1_000, 300),
    );
    let config = ReplayWindowConfig {
        window_capacity_facts: 1,
        ..config()
    };

    let (manifest, _) =
        recover_replay_window_manifest(temp.path(), &log_id(), &facts, config).expect("recover");
    assert_eq!(manifest.snapshot_cursor, None);
    assert_eq!(
        manifest
            .snapshot_status(temp.path(), config)
            .expect("snapshot status"),
        SnapshotStatus::Missing
    );
    let reset = manifest
        .reset_for_cursor(temp.path(), &cursor(1, 0), &facts, config)
        .expect("reset check")
        .expect("expired cursor");
    assert_eq!(reset.reason, ResetReason::SnapshotMissing);
}

#[test]
fn snapshot_hash_mismatch_and_expiry_are_distinct_resets() {
    let temp = tempfile::tempdir().expect("tempdir");
    let facts = (1..=3)
        .map(|seq| fact(seq, seq as i64 * 100))
        .collect::<Vec<_>>();
    write_snapshot(temp.path(), b"actual");
    write_manifest(
        temp.path(),
        &snapshot_manifest(2, b"declared", 100, 1_000, 300),
    );
    let config = ReplayWindowConfig {
        window_capacity_facts: 1,
        ..config()
    };

    let (manifest, _) =
        recover_replay_window_manifest(temp.path(), &log_id(), &facts, config).expect("recover");
    assert_eq!(
        manifest
            .snapshot_status(temp.path(), config)
            .expect("snapshot status"),
        SnapshotStatus::HashMismatch
    );
    let reset = manifest
        .reset_for_cursor(temp.path(), &cursor(1, 0), &facts, config)
        .expect("reset check")
        .expect("expired cursor");
    assert_eq!(reset.reason, ResetReason::SnapshotHashMismatch);

    let snapshot_bytes = b"expired";
    write_snapshot(temp.path(), snapshot_bytes);
    write_manifest(
        temp.path(),
        &snapshot_manifest(2, snapshot_bytes, 100, 200, 300),
    );
    let (manifest, _) =
        recover_replay_window_manifest(temp.path(), &log_id(), &facts, config).expect("recover");
    assert_eq!(
        manifest
            .snapshot_status(temp.path(), config)
            .expect("snapshot status"),
        SnapshotStatus::Expired
    );
    let reset = manifest
        .reset_for_cursor(temp.path(), &cursor(1, 0), &facts, config)
        .expect("reset check")
        .expect("expired cursor");
    assert_eq!(reset.reason, ResetReason::SnapshotExpired);
}

#[test]
fn log_id_mismatch_fails_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut manifest = snapshot_manifest(2, b"x", 100, 1_000, 300);
    manifest.log_id = LogId::new("0198f1a0-0000-7000-8000-000000000099");
    manifest.earliest_cursor.log_id = manifest.log_id.clone();
    manifest.snapshot_cursor = None;
    manifest.snapshot_fact_seq = None;
    manifest.snapshot_generation = 0;
    manifest.snapshot_hash =
        ContentHash::new("sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    manifest.snapshot_path.clear();
    manifest.snapshot_created_at_logical_ms = None;
    manifest.snapshot_expires_at_logical_ms = None;
    write_manifest(temp.path(), &manifest);

    let error = recover_replay_window_manifest(temp.path(), &log_id(), &[], config())
        .expect_err("log mismatch must fail closed");
    assert!(matches!(
        error,
        qaqh_session::canonical::CanonicalError::IdentityMismatch {
            field: "replay_window.log_id"
        }
    ));
}

#[test]
fn cursor_validation_distinguishes_unknown_and_high_projection_index() {
    let temp = tempfile::tempdir().expect("tempdir");
    let facts = vec![fact(1, 100), fact(3, 300)];
    let (manifest, _) =
        recover_replay_window_manifest(temp.path(), &log_id(), &facts, config()).expect("recover");

    assert_eq!(
        manifest
            .reset_for_cursor(temp.path(), &cursor(1, 0), &facts, config())
            .expect("valid cursor"),
        None
    );
    let reset = manifest
        .reset_for_cursor(temp.path(), &cursor(2, 0), &facts, config())
        .expect("unknown fact check")
        .expect("unknown fact reset");
    assert_eq!(reset.reason, ResetReason::UnknownFact);

    let reset = manifest
        .reset_for_cursor(temp.path(), &cursor(3, u16::MAX - 1), &facts, config())
        .expect("projection index check")
        .expect("projection index reset");
    assert_eq!(reset.reason, ResetReason::CursorExpired);
}

#[test]
fn malformed_manifest_fails_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    fs::write(temp.path().join(REPLAY_WINDOW_FILE), b"not json").expect("write corrupt manifest");

    let error =
        load_replay_window_manifest(temp.path()).expect_err("malformed manifest must fail closed");
    assert!(matches!(
        error,
        qaqh_session::canonical::CanonicalError::Json(_)
    ));
}
