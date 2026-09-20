//! Recovery state observation and commit marker repair contract.

use std::fs;
use std::io::Write;

use qaqh_session::canonical::{
    CanonicalError, CanonicalLog, EVENTS_COMMIT_FILE, EVENTS_FILE, EVENTS_POISON_FILE,
    EventsCommit, UPGRADE_FENCE_FILE, UPGRADE_FENCE_SCHEMA, UpgradeFence, UpgradeState, WriterId,
    inspect_recovery_state, repair_commit_marker,
};
use qaqh_session::session_fact_v2::{
    DeleteReason, EventId, FactPayload, LogId, RecoveryOutcome, SessionDeleted, SessionFact,
    SessionId,
};

const ENVELOPE_FIXTURE: &str = include_str!("fixtures/session_fact_v2/envelope.jsonl");
const NOW_MS: i64 = 1_789_830_000_000;
const LEASE_MS: i64 = 10_000;

fn session_id() -> SessionId {
    SessionId::new("0198f1a0-0000-7000-8000-000000000001")
}

fn log_id() -> LogId {
    LogId::new("0198f1a0-0000-7000-8000-000000000002")
}

fn fact(event_ordinal: u64) -> SessionFact {
    let mut fact: SessionFact =
        serde_json::from_str(ENVELOPE_FIXTURE.trim()).expect("parse envelope fixture");
    fact.fact_seq = event_ordinal;
    fact.event_id = EventId::new(format!("01J{event_ordinal:023}"));
    fact
}

fn tombstone_fact(event_ordinal: u64) -> SessionFact {
    let mut fact = fact(event_ordinal);
    fact.payload = FactPayload::SessionDeleted(SessionDeleted {
        tombstone_at_ms: fact.ts_ms,
        reason: DeleteReason::User,
        purge_after_ms: None,
    });
    fact
}

fn read_commit(dir: &std::path::Path) -> EventsCommit {
    serde_json::from_slice(&fs::read(dir.join(EVENTS_COMMIT_FILE)).expect("read commit marker"))
        .expect("parse commit marker")
}

fn write_upgrade_fence(dir: &std::path::Path, state: UpgradeState) {
    let fence = UpgradeFence {
        schema: UPGRADE_FENCE_SCHEMA.into(),
        log_id: log_id(),
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

#[test]
fn writable_state_is_observed_for_a_valid_log() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _log = CanonicalLog::open(temp.path(), session_id(), log_id()).expect("open log");

    let state = inspect_recovery_state(temp.path(), &session_id(), &log_id())
        .expect("inspect recovery state");

    assert_eq!(state.outcome, RecoveryOutcome::Writable);
    assert_eq!(state.reason, None);
}

#[test]
fn missing_marker_requires_repair_then_rebuild_is_idempotent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let events = format!(
        "{}\n",
        serde_json::to_string(&fact(1)).expect("serialize fact")
    );
    fs::write(temp.path().join(EVENTS_FILE), &events).expect("write events");

    let state = inspect_recovery_state(temp.path(), &session_id(), &log_id())
        .expect("inspect missing marker");
    assert_eq!(state.outcome, RecoveryOutcome::CommitRecoveryRequired);

    let repaired = repair_commit_marker(temp.path(), &session_id(), &log_id())
        .expect("rebuild missing marker");
    assert!(repaired.marker_rebuilt);
    assert!(!repaired.commit_repaired);
    assert_eq!(repaired.previous_commit_generation, None);
    assert_eq!(repaired.commit.commit_generation, 0);
    assert_eq!(repaired.commit.committed_fact_seq, 1);
    assert_eq!(repaired.commit.committed_offset, events.len() as u64);

    let state = inspect_recovery_state(temp.path(), &session_id(), &log_id())
        .expect("inspect repaired marker");
    assert_eq!(state.outcome, RecoveryOutcome::Writable);

    let second = repair_commit_marker(temp.path(), &session_id(), &log_id())
        .expect("second repair is a no-op");
    assert!(!second.marker_rebuilt);
    assert!(!second.commit_repaired);
    assert_eq!(second.commit.commit_generation, 0);
    assert_eq!(second.commit, repaired.commit);
}

#[test]
fn torn_tail_blocks_marker_rebuild() {
    let temp = tempfile::tempdir().expect("tempdir");
    let original = format!(
        "{}\n{{\"partial\":",
        serde_json::to_string(&fact(1)).expect("serialize fact")
    );
    fs::write(temp.path().join(EVENTS_FILE), &original).expect("write torn events");

    let state =
        inspect_recovery_state(temp.path(), &session_id(), &log_id()).expect("inspect torn tail");
    assert_eq!(state.outcome, RecoveryOutcome::CommitRecoveryRequired);

    let error = repair_commit_marker(temp.path(), &session_id(), &log_id())
        .expect_err("torn tail is not rebuildable");
    assert!(matches!(error, CanonicalError::CommitRecoveryRequired(_)));
    assert_eq!(
        fs::read_to_string(temp.path().join(EVENTS_FILE)).expect("read events"),
        original
    );
    assert!(!temp.path().join(EVENTS_COMMIT_FILE).exists());
}

#[test]
fn poison_and_uncommitted_suffix_are_repaired_after_truncation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut log = CanonicalLog::open(temp.path(), session_id(), log_id()).expect("open log");
    let lease = log
        .acquire_writer(WriterId::new("writer-a"), NOW_MS, LEASE_MS)
        .expect("acquire writer");
    log.append(&lease, fact(1), NOW_MS + 1)
        .expect("append committed fact");
    let committed = read_commit(temp.path());
    let committed_len = fs::metadata(log.events_path())
        .expect("events metadata")
        .len();

    let extra = serde_json::to_string(&fact(2)).expect("serialize extra fact");
    let mut events = fs::OpenOptions::new()
        .append(true)
        .open(log.events_path())
        .expect("open events for suffix");
    writeln!(events, "{extra}").expect("append uncommitted suffix");
    events.sync_all().expect("sync suffix");
    drop(events);
    fs::write(temp.path().join(EVENTS_POISON_FILE), b"{}").expect("write poison marker");

    let repaired = repair_commit_marker(temp.path(), &session_id(), &log_id())
        .expect("repair poison and suffix");

    assert!(repaired.commit_repaired);
    assert!(!repaired.marker_rebuilt);
    assert_eq!(
        repaired.previous_commit_generation,
        Some(committed.commit_generation)
    );
    assert_eq!(
        repaired.commit.commit_generation,
        committed.commit_generation + 1
    );
    assert_eq!(repaired.commit.committed_fact_seq, 1);
    assert_eq!(repaired.commit.committed_offset, committed_len);
    assert!(repaired.truncated_bytes > 0);
    assert!(repaired.poison_cleared);
    assert!(!temp.path().join(EVENTS_POISON_FILE).exists());
    assert_eq!(
        fs::metadata(temp.path().join(EVENTS_FILE))
            .expect("events metadata")
            .len(),
        committed_len
    );
    let facts = fs::read_to_string(temp.path().join(EVENTS_FILE))
        .expect("read repaired events")
        .lines()
        .map(|line| serde_json::from_str::<SessionFact>(line).expect("parse repaired fact"))
        .collect::<Vec<_>>();
    assert_eq!(facts.len(), 1);
    assert!(matches!(facts[0].payload, FactPayload::SessionCreated(_)));

    let state = inspect_recovery_state(temp.path(), &session_id(), &log_id())
        .expect("inspect repaired state");
    assert_eq!(state.outcome, RecoveryOutcome::Writable);

    let second = repair_commit_marker(temp.path(), &session_id(), &log_id())
        .expect("second repair is a no-op");
    assert!(!second.commit_repaired);
    assert_eq!(second.commit, repaired.commit);
}

#[test]
fn valid_marker_beyond_events_fails_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut log = CanonicalLog::open(temp.path(), session_id(), log_id()).expect("open log");
    let lease = log
        .acquire_writer(WriterId::new("writer-a"), NOW_MS, LEASE_MS)
        .expect("acquire writer");
    log.append(&lease, fact(1), NOW_MS + 1)
        .expect("append committed fact");
    let marker_before = read_commit(temp.path());
    fs::write(temp.path().join(EVENTS_FILE), b"").expect("truncate events below marker");

    let error = repair_commit_marker(temp.path(), &session_id(), &log_id())
        .expect_err("marker beyond events is not repairable");
    assert!(matches!(error, CanonicalError::CommitRecoveryRequired(_)));
    assert_eq!(read_commit(temp.path()), marker_before);
}

#[test]
fn upgrade_read_only_state_is_observed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _log = CanonicalLog::open(temp.path(), session_id(), log_id()).expect("open log");
    write_upgrade_fence(temp.path(), UpgradeState::ReadOnly);

    let state = inspect_recovery_state(temp.path(), &session_id(), &log_id())
        .expect("inspect read-only state");

    assert_eq!(state.outcome, RecoveryOutcome::ReadOnlyUpgradeRequired);
}

#[test]
fn tombstone_state_is_terminal_and_blocks_writes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut log = CanonicalLog::open(temp.path(), session_id(), log_id()).expect("open log");
    let lease = log
        .acquire_writer(WriterId::new("writer-a"), NOW_MS, LEASE_MS)
        .expect("acquire writer");
    log.append(&lease, tombstone_fact(1), NOW_MS + 1)
        .expect("append tombstone");
    assert_eq!(log.recovery_state().outcome, RecoveryOutcome::Tombstone);
    drop(log);

    let state =
        inspect_recovery_state(temp.path(), &session_id(), &log_id()).expect("inspect tombstone");
    assert_eq!(state.outcome, RecoveryOutcome::Tombstone);

    let mut reopened = CanonicalLog::open(temp.path(), session_id(), log_id()).expect("reopen log");
    assert_eq!(
        reopened.recovery_state().outcome,
        RecoveryOutcome::Tombstone
    );
    let error = reopened
        .acquire_writer(WriterId::new("writer-b"), NOW_MS + 2, LEASE_MS)
        .expect_err("tombstone blocks writer acquisition");
    assert!(matches!(error, CanonicalError::NotWritable(_)));
}
