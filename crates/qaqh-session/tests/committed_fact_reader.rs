//! P1 canonical committed fact reader contract.

use std::fs;
use std::io::Write;

use qaqh_session::canonical::{
    CanonicalError, CanonicalLog, CommittedFactReader, EVENTS_COMMIT_FILE, EVENTS_FILE,
    EVENTS_POISON_FILE, EventsCommit, WriterId,
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
    fact.fact_seq = event_ordinal;
    fact.event_id = EventId::new(format!("01J{event_ordinal:023}"));
    fact
}

fn open_log(dir: &std::path::Path) -> CanonicalLog {
    CanonicalLog::open(dir, session_id(), log_id()).expect("open canonical log")
}

fn append_facts(dir: &std::path::Path, count: u64) -> Vec<SessionFact> {
    let mut log = open_log(dir);
    let lease = log
        .acquire_writer(WriterId::new("reader-contract-writer"), NOW_MS, LEASE_MS)
        .expect("acquire writer");
    (1..=count)
        .map(|ordinal| {
            log.append(&lease, fact(ordinal), NOW_MS + ordinal as i64)
                .expect("append fact")
        })
        .collect()
}

fn read_marker(dir: &std::path::Path) -> EventsCommit {
    serde_json::from_slice(&fs::read(dir.join(EVENTS_COMMIT_FILE)).expect("read commit marker"))
        .expect("parse commit marker")
}

fn write_marker(dir: &std::path::Path, marker: &EventsCommit) {
    fs::write(
        dir.join(EVENTS_COMMIT_FILE),
        serde_json::to_vec(marker).expect("serialize marker"),
    )
    .expect("write marker");
}

fn write_facts_and_marker(dir: &std::path::Path, facts: &[SessionFact]) {
    let mut bytes = Vec::new();
    for fact in facts {
        serde_json::to_writer(&mut bytes, fact).expect("serialize fact");
        bytes.push(b'\n');
    }
    fs::write(dir.join(EVENTS_FILE), &bytes).expect("write facts");

    let mut marker = read_marker(dir);
    marker.committed_fact_seq = facts.last().map_or(0, |fact| fact.fact_seq);
    marker.committed_offset = bytes.len() as u64;
    marker.last_barrier_event_id = facts.last().map(|fact| fact.event_id.clone());
    write_marker(dir, &marker);
}

fn open_reader(dir: &std::path::Path) -> Result<CommittedFactReader, CanonicalError> {
    CommittedFactReader::open(dir, session_id(), log_id())
}

fn seqs(facts: &[SessionFact]) -> Vec<u64> {
    facts.iter().map(|fact| fact.fact_seq).collect()
}

#[test]
fn reads_all_closed_range_and_after_cursor() {
    let temp = tempfile::tempdir().expect("tempdir");
    append_facts(temp.path(), 4);
    let reader = open_reader(temp.path()).expect("open reader");

    assert_eq!(
        seqs(&reader.read_all().expect("read all")),
        vec![1, 2, 3, 4]
    );
    assert_eq!(
        seqs(&reader.read_range(2, 4).expect("read range")),
        vec![2, 3, 4]
    );
    assert_eq!(seqs(&reader.read_after(2).expect("read after")), vec![3, 4]);
    assert_eq!(
        seqs(&reader.read_after(0).expect("read after zero")),
        vec![1, 2, 3, 4]
    );
    assert!(reader.read_after(4).expect("read at high-water").is_empty());
    assert!(
        reader
            .read_after(99)
            .expect("read beyond high-water")
            .is_empty()
    );

    for (start, end) in [(0, 1), (3, 2), (1, 5)] {
        assert!(matches!(
            reader.read_range(start, end),
            Err(CanonicalError::InvalidFactRange {
                start_fact_seq,
                end_fact_seq,
                committed_fact_seq: 4,
            }) if start_fact_seq == start && end_fact_seq == end
        ));
    }
}

#[test]
fn reader_ignores_uncommitted_suffix_without_mutating_it() {
    let temp = tempfile::tempdir().expect("tempdir");
    append_facts(temp.path(), 2);
    let committed_len = fs::metadata(temp.path().join(EVENTS_FILE))
        .expect("events metadata")
        .len();
    let extra = serde_json::to_string(&fact(3)).expect("serialize extra fact");
    let mut events = fs::OpenOptions::new()
        .append(true)
        .open(temp.path().join(EVENTS_FILE))
        .expect("open events");
    writeln!(events, "{extra}").expect("append uncommitted suffix");
    events.sync_all().expect("sync suffix");
    drop(events);

    let reader = open_reader(temp.path()).expect("open reader");
    assert_eq!(
        seqs(&reader.read_all().expect("read committed prefix")),
        vec![1, 2]
    );
    assert_eq!(
        fs::metadata(temp.path().join(EVENTS_FILE))
            .expect("events metadata")
            .len(),
        committed_len + extra.len() as u64 + 1
    );
    assert!(
        fs::read_to_string(temp.path().join(EVENTS_FILE))
            .expect("read raw events")
            .ends_with(&format!("{extra}\n"))
    );
}

#[test]
fn empty_committed_log_reads_empty() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _log = open_log(temp.path());
    let reader = open_reader(temp.path()).expect("open reader");

    assert!(reader.read_all().expect("read empty").is_empty());
    assert!(reader.read_after(0).expect("read after zero").is_empty());
    assert!(matches!(
        reader.read_range(1, 1),
        Err(CanonicalError::InvalidFactRange {
            start_fact_seq: 1,
            end_fact_seq: 1,
            committed_fact_seq: 0,
        })
    ));
}

#[test]
fn missing_marker_fails_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _log = open_log(temp.path());
    fs::remove_file(temp.path().join(EVENTS_COMMIT_FILE)).expect("remove marker");

    assert!(matches!(
        open_reader(temp.path()),
        Err(CanonicalError::CommitRecoveryRequired(_))
    ));
}

#[test]
fn poison_marker_fails_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _log = open_log(temp.path());
    fs::write(temp.path().join(EVENTS_POISON_FILE), b"{}").expect("write poison");

    assert!(matches!(
        open_reader(temp.path()),
        Err(CanonicalError::CommitRecoveryRequired(_))
    ));
}

#[test]
fn corrupt_marker_fails_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let _log = open_log(temp.path());
    fs::write(temp.path().join(EVENTS_COMMIT_FILE), b"{").expect("write corrupt marker");

    assert!(matches!(
        open_reader(temp.path()),
        Err(CanonicalError::CommitRecoveryRequired(_))
    ));
}

#[test]
fn identity_schema_and_sequence_errors_fail_closed() {
    let identity = tempfile::tempdir().expect("identity tempdir");
    let mut facts = append_facts(identity.path(), 1);
    facts[0].log_id = LogId::new("0198f1a0-0000-7000-8000-000000000003");
    write_facts_and_marker(identity.path(), &facts);
    assert!(matches!(
        open_reader(identity.path()),
        Err(CanonicalError::IdentityMismatch { field: "log_id" })
    ));

    let schema = tempfile::tempdir().expect("schema tempdir");
    let mut facts = append_facts(schema.path(), 1);
    facts[0].schema.version = 1;
    write_facts_and_marker(schema.path(), &facts);
    assert!(matches!(
        open_reader(schema.path()),
        Err(CanonicalError::CommitRecoveryRequired(_))
    ));

    let sequence = tempfile::tempdir().expect("sequence tempdir");
    let mut facts = append_facts(sequence.path(), 2);
    facts[1].fact_seq = 3;
    write_facts_and_marker(sequence.path(), &facts);
    assert!(matches!(
        open_reader(sequence.path()),
        Err(CanonicalError::CommitRecoveryRequired(_))
    ));
}

#[test]
fn marker_prefix_mismatch_fails_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    append_facts(temp.path(), 1);
    let mut marker = read_marker(temp.path());
    marker.committed_fact_seq = 2;
    write_marker(temp.path(), &marker);

    assert!(matches!(
        open_reader(temp.path()),
        Err(CanonicalError::CommitRecoveryRequired(_))
    ));
}

#[test]
fn short_torn_and_invalid_committed_prefixes_fail_closed() {
    let short = tempfile::tempdir().expect("short tempdir");
    let _log = open_log(short.path());
    fs::write(short.path().join(EVENTS_FILE), b"x").expect("write short events");
    let mut marker = read_marker(short.path());
    marker.committed_offset = 2;
    write_marker(short.path(), &marker);
    assert!(matches!(
        open_reader(short.path()),
        Err(CanonicalError::CommitRecoveryRequired(_))
    ));

    let torn = tempfile::tempdir().expect("torn tempdir");
    let _log = open_log(torn.path());
    let torn_bytes = serde_json::to_vec(&fact(1)).expect("serialize torn fact");
    fs::write(torn.path().join(EVENTS_FILE), &torn_bytes).expect("write torn fact");
    let mut marker = read_marker(torn.path());
    marker.committed_fact_seq = 1;
    marker.committed_offset = torn_bytes.len() as u64;
    marker.last_barrier_event_id = Some(fact(1).event_id);
    write_marker(torn.path(), &marker);
    assert!(matches!(
        open_reader(torn.path()),
        Err(CanonicalError::CommitRecoveryRequired(_))
    ));

    let invalid = tempfile::tempdir().expect("invalid tempdir");
    let _log = open_log(invalid.path());
    let invalid_bytes = b"{}\n";
    fs::write(invalid.path().join(EVENTS_FILE), invalid_bytes).expect("write invalid fact");
    let mut marker = read_marker(invalid.path());
    marker.committed_fact_seq = 1;
    marker.committed_offset = invalid_bytes.len() as u64;
    marker.last_barrier_event_id = Some(fact(1).event_id);
    write_marker(invalid.path(), &marker);
    assert!(matches!(
        open_reader(invalid.path()),
        Err(CanonicalError::CommitRecoveryRequired(_))
    ));
}
