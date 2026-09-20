//! Upgrade fence read-only gate contract.

use std::fs;

use qaqh_session::canonical::{
    CanonicalError, CanonicalLog, UPGRADE_FENCE_FILE, UPGRADE_FENCE_SCHEMA, UpgradeFence,
    UpgradeState, WriterId,
};
use qaqh_session::session_fact_v2::{EventId, LogId, SessionFact, SessionId};

const ENVELOPE_FIXTURE: &str = include_str!("fixtures/session_fact_v2/envelope.jsonl");
const NOW_MS: i64 = 1_789_830_000_000;
const LEASE_MS: i64 = 10_000;

fn session_id() -> SessionId {
    SessionId::new("0198f1a0-0000-7000-8000-000000000001")
}

fn log_id() -> LogId {
    LogId::new("0198f1a0-0000-7000-8000-000000000002")
}

fn fact() -> SessionFact {
    let mut fact: SessionFact =
        serde_json::from_str(ENVELOPE_FIXTURE.trim()).expect("parse envelope fixture");
    fact.session_id = session_id();
    fact.log_id = log_id();
    fact.fact_seq = 0;
    fact.event_id = EventId::new("01J00000000000000000000099");
    fact
}

fn write_fence(dir: &std::path::Path, state: UpgradeState, log_id: LogId) {
    let fence = UpgradeFence {
        schema: UPGRADE_FENCE_SCHEMA.into(),
        log_id,
        generation: 1,
        state,
        last_recovery_id: None,
        updated_at_ms: NOW_MS,
    };
    fs::write(
        dir.join(UPGRADE_FENCE_FILE),
        serde_json::to_vec_pretty(&fence).expect("serialize upgrade fence"),
    )
    .expect("write upgrade fence");
}

fn open(dir: &std::path::Path) -> CanonicalLog {
    CanonicalLog::open(dir, session_id(), log_id()).expect("open canonical log")
}

#[test]
fn missing_upgrade_fence_preserves_writable_behavior() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut log = open(temp.path());
    log.acquire_writer(WriterId::new("writer-a"), NOW_MS, LEASE_MS)
        .expect("missing fence must remain writable");
    assert!(log.upgrade_fence().expect("read fence").is_none());
}

#[test]
fn writable_upgrade_fence_preserves_writable_behavior() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_fence(temp.path(), UpgradeState::Writable, log_id());
    let mut log = open(temp.path());
    log.acquire_writer(WriterId::new("writer-a"), NOW_MS, LEASE_MS)
        .expect("writable fence must allow writer");
    assert_eq!(
        log.upgrade_fence()
            .expect("read fence")
            .expect("fence")
            .state,
        UpgradeState::Writable
    );
}

#[test]
fn read_only_upgrade_fence_blocks_writer_and_append() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut writable = open(temp.path());
    let lease = writable
        .acquire_writer(WriterId::new("writer-a"), NOW_MS, LEASE_MS)
        .expect("acquire initial writer");
    drop(writable);

    write_fence(temp.path(), UpgradeState::ReadOnly, log_id());
    let events_before = fs::metadata(temp.path().join("events.jsonl"))
        .expect("events metadata")
        .len();
    let commit_before = fs::read(temp.path().join("events.commit.json")).expect("commit bytes");

    let mut read_only = open(temp.path());
    let error = read_only
        .acquire_writer(WriterId::new("writer-b"), NOW_MS + 1, LEASE_MS)
        .expect_err("read-only fence must block writer acquisition");
    assert!(matches!(error, CanonicalError::NotWritable(_)));

    let error = read_only
        .append(&lease, fact(), NOW_MS + 2)
        .expect_err("read-only fence must block append");
    assert!(matches!(error, CanonicalError::NotWritable(_)));
    assert_eq!(
        fs::metadata(temp.path().join("events.jsonl"))
            .expect("events metadata")
            .len(),
        events_before
    );
    assert_eq!(
        fs::read(temp.path().join("events.commit.json")).expect("commit bytes"),
        commit_before
    );
}

#[test]
fn upgrade_fence_log_id_mismatch_fails_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_fence(
        temp.path(),
        UpgradeState::Writable,
        LogId::new("0198f1a0-0000-7000-8000-000000000099"),
    );
    let error = CanonicalLog::open(temp.path(), session_id(), log_id())
        .expect_err("mismatched upgrade fence must fail closed");
    assert!(matches!(
        error,
        CanonicalError::IdentityMismatch {
            field: "upgrade_fence.log_id"
        }
    ));
}
