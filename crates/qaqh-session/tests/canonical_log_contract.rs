//! P1 canonical log contract: writer fence, contiguous seq and durable commit.

use std::fs;
use std::io::Write;
use std::sync::{Arc, Barrier};
use std::thread;

use qaqh_session::canonical::{
    CanonicalError, CanonicalLog, EVENTS_COMMIT_FILE, EVENTS_FILE, EventsCommit, WriterId,
};
use qaqh_session::session_fact_v2::{EventId, LogId, SessionFact, SessionId};

const ENVELOPE_FIXTURE: &str = include_str!("fixtures/session_fact_v2/envelope.jsonl");
const NOW_MS: i64 = 1_789_830_000_000;
const LEASE_MS: i64 = 1_000;

fn session_id() -> SessionId {
    SessionId::new("0198f1a0-0000-7000-8000-000000000001")
}

fn log_id() -> LogId {
    LogId::new("0198f1a0-0000-7000-8000-000000000002")
}

fn fact(event_ordinal: u64) -> SessionFact {
    let mut fact: SessionFact =
        serde_json::from_str(ENVELOPE_FIXTURE.trim()).expect("parse envelope fixture");
    fact.fact_seq = 0;
    fact.event_id = EventId::new(format!("01J{event_ordinal:023}"));
    fact
}

fn open(dir: &std::path::Path) -> CanonicalLog {
    CanonicalLog::open(dir, session_id(), log_id()).expect("open canonical log")
}

fn read_commit(dir: &std::path::Path) -> EventsCommit {
    serde_json::from_slice(&fs::read(dir.join(EVENTS_COMMIT_FILE)).expect("read commit marker"))
        .expect("parse commit marker")
}

fn read_facts(dir: &std::path::Path) -> Vec<SessionFact> {
    fs::read_to_string(dir.join(EVENTS_FILE))
        .expect("read events")
        .lines()
        .map(|line| serde_json::from_str(line).expect("parse canonical fact"))
        .collect()
}

#[test]
fn concurrent_writers_cannot_both_hold_an_active_lease() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().to_path_buf();
    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();

    for writer in ["writer-a", "writer-b"] {
        let path = path.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            let mut log = CanonicalLog::open(path, session_id(), log_id()).expect("open log");
            barrier.wait();
            log.acquire_writer(WriterId::new(writer), NOW_MS, LEASE_MS)
        }));
    }

    barrier.wait();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("join writer"))
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(CanonicalError::WriterBusy { .. })))
            .count(),
        1
    );
}

#[test]
fn stale_writer_append_is_rejected_without_advancing_the_log() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut log = open(temp.path());
    let first = log
        .acquire_writer(WriterId::new("writer-a"), NOW_MS, LEASE_MS)
        .expect("acquire first writer");
    log.acquire_writer(WriterId::new("writer-b"), NOW_MS + LEASE_MS + 1, LEASE_MS)
        .expect("take over expired fence");

    let before_len = fs::metadata(log.events_path())
        .expect("events metadata")
        .len();
    let before_commit = read_commit(temp.path());
    let error = log
        .append(&first, fact(1), NOW_MS + LEASE_MS + 2)
        .expect_err("stale writer must be rejected");
    assert!(matches!(error, CanonicalError::StaleWriter(_)));
    assert_eq!(
        fs::metadata(log.events_path())
            .expect("events metadata")
            .len(),
        before_len
    );
    assert_eq!(read_commit(temp.path()), before_commit);
}

#[test]
fn expired_lease_is_rejected_without_writing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut log = open(temp.path());
    let lease = log
        .acquire_writer(WriterId::new("writer-a"), NOW_MS, LEASE_MS)
        .expect("acquire writer");

    let error = log
        .append(&lease, fact(1), NOW_MS + LEASE_MS + 1)
        .expect_err("expired lease must be rejected");
    assert!(matches!(error, CanonicalError::StaleWriter(_)));
    assert_eq!(
        fs::metadata(log.events_path())
            .expect("events metadata")
            .len(),
        0
    );
}

#[test]
fn tampered_lease_token_is_rejected_without_writing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut log = open(temp.path());
    let mut lease = log
        .acquire_writer(WriterId::new("writer-a"), NOW_MS, LEASE_MS)
        .expect("acquire writer");
    lease.fencing_token = lease.fencing_token.saturating_add(1);

    let error = log
        .append(&lease, fact(1), NOW_MS + 1)
        .expect_err("tampered token must be rejected");
    assert!(matches!(error, CanonicalError::StaleWriter(_)));
    assert_eq!(
        fs::metadata(log.events_path())
            .expect("events metadata")
            .len(),
        0
    );
}

#[test]
fn lease_log_id_mismatch_is_stale_writer() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut log = open(temp.path());
    let mut lease = log
        .acquire_writer(WriterId::new("writer-a"), NOW_MS, LEASE_MS)
        .expect("acquire writer");
    lease.log_id = LogId::new("0198f1a0-0000-7000-8000-000000000003");

    let error = log
        .append(&lease, fact(1), NOW_MS + 1)
        .expect_err("lease identity mismatch must be rejected");
    assert!(matches!(error, CanonicalError::StaleWriter(_)));
    assert_eq!(
        fs::metadata(log.events_path())
            .expect("events metadata")
            .len(),
        0
    );
}

#[test]
fn append_assigns_contiguous_fact_seq_and_advances_commit_high_water() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut log = open(temp.path());
    let lease = log
        .acquire_writer(WriterId::new("writer-a"), NOW_MS, LEASE_MS)
        .expect("acquire writer");

    for ordinal in 1..=3 {
        let appended = log
            .append(&lease, fact(ordinal), NOW_MS + ordinal as i64)
            .expect("append canonical fact");
        assert_eq!(appended.fact_seq, ordinal);
        appended.validate().expect("valid canonical fact");
    }

    let facts = read_facts(temp.path());
    assert_eq!(
        facts.iter().map(|fact| fact.fact_seq).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    let marker = read_commit(temp.path());
    assert_eq!(marker.committed_fact_seq, 3);
    assert_eq!(
        marker.committed_offset,
        fs::metadata(log.events_path())
            .expect("events metadata")
            .len()
    );
    assert_eq!(marker.commit_generation, 3);
    assert_eq!(
        marker.last_barrier_event_id.as_ref().map(EventId::as_str),
        Some("01J00000000000000000000003")
    );
}

#[test]
fn fact_identity_mismatch_is_rejected_without_writing() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut log = open(temp.path());
    let lease = log
        .acquire_writer(WriterId::new("writer-a"), NOW_MS, LEASE_MS)
        .expect("acquire writer");
    let mut wrong_log = fact(1);
    wrong_log.log_id = LogId::new("0198f1a0-0000-7000-8000-000000000003");

    let error = log
        .append(&lease, wrong_log, NOW_MS + 1)
        .expect_err("identity mismatch must fail");
    assert!(matches!(
        error,
        CanonicalError::IdentityMismatch { field: "log_id" }
    ));
    assert_eq!(
        fs::metadata(log.events_path())
            .expect("events metadata")
            .len(),
        0
    );
}

#[test]
fn missing_marker_for_non_empty_log_fails_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let line = serde_json::to_string(&fact(1)).expect("serialize fact");
    fs::write(temp.path().join(EVENTS_FILE), format!("{line}\n")).expect("write events");

    let error = CanonicalLog::open(temp.path(), session_id(), log_id())
        .expect_err("missing marker must fail closed");
    assert!(matches!(error, CanonicalError::CommitRecoveryRequired(_)));
}

#[test]
fn corrupt_marker_fails_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    fs::write(temp.path().join(EVENTS_COMMIT_FILE), b"{").expect("write corrupt marker");

    let error = CanonicalLog::open(temp.path(), session_id(), log_id())
        .expect_err("corrupt marker must fail closed");
    assert!(matches!(error, CanonicalError::CommitRecoveryRequired(_)));
}

#[test]
fn marker_log_id_mismatch_fails_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _log = open(temp.path());
    let mut marker = read_commit(temp.path());
    marker.log_id = LogId::new("0198f1a0-0000-7000-8000-000000000003");
    fs::write(
        temp.path().join(EVENTS_COMMIT_FILE),
        serde_json::to_vec(&marker).expect("serialize marker"),
    )
    .expect("rewrite marker");

    let error = CanonicalLog::open(temp.path(), session_id(), log_id())
        .expect_err("marker identity mismatch must fail closed");
    assert!(matches!(error, CanonicalError::CommitRecoveryRequired(_)));
}

#[test]
fn open_truncates_an_uncommitted_suffix() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut log = open(temp.path());
    let lease = log
        .acquire_writer(WriterId::new("writer-a"), NOW_MS, LEASE_MS)
        .expect("acquire writer");
    log.append(&lease, fact(1), NOW_MS + 1)
        .expect("append committed fact");
    let committed = read_commit(temp.path());

    let extra = serde_json::to_string(&fact(2)).expect("serialize extra fact");
    let mut events = fs::OpenOptions::new()
        .append(true)
        .open(log.events_path())
        .expect("open events for extra suffix");
    writeln!(events, "{extra}").expect("append uncommitted suffix");
    events.sync_all().expect("sync suffix");
    drop(events);

    let reopened = open(temp.path());
    assert_eq!(
        fs::metadata(reopened.events_path())
            .expect("events metadata")
            .len(),
        committed.committed_offset
    );
    assert_eq!(read_commit(temp.path()), committed);
    assert_eq!(read_facts(temp.path()).len(), 1);
}
