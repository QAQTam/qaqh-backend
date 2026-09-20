//! ContentClockRecord recovery and monotonic advance contract.

use std::fs;

use qaqh_session::canonical::{
    CONTENT_CLOCK_FILE, CONTENT_CLOCK_SCHEMA, ContentClock, ContentClockRecord,
};
use qaqh_session::session_fact_v2::{EventId, SessionFact};

const ENVELOPE_FIXTURE: &str = include_str!("fixtures/session_fact_v2/envelope.jsonl");
const ZERO_EVENT_ID: &str = "00000000000000000000000000";

fn fact(fact_seq: u64, ts_ms: i64) -> SessionFact {
    let mut fact: SessionFact =
        serde_json::from_str(ENVELOPE_FIXTURE.trim()).expect("parse envelope fixture");
    fact.fact_seq = fact_seq;
    fact.ts_ms = ts_ms;
    fact.event_id = EventId::new(format!("01J{fact_seq:023}"));
    fact
}

fn read_clock(dir: &std::path::Path) -> ContentClockRecord {
    let bytes = fs::read(dir.join(CONTENT_CLOCK_FILE)).expect("read content clock");
    serde_json::from_slice(&bytes).expect("parse content clock")
}

fn write_clock(dir: &std::path::Path, record: &ContentClockRecord) {
    fs::create_dir_all(dir.join("content")).expect("create content dir");
    fs::write(
        dir.join(CONTENT_CLOCK_FILE),
        serde_json::to_vec_pretty(record).expect("serialize clock"),
    )
    .expect("write clock");
}

#[test]
fn empty_log_writes_zero_clock() {
    let temp = tempfile::tempdir().expect("tempdir");
    let clock = ContentClock::open_or_recover(temp.path(), &[], None).expect("recover clock");

    assert_eq!(clock.logical_now_ms(), 0);
    let record = read_clock(temp.path());
    assert_eq!(record.schema, CONTENT_CLOCK_SCHEMA);
    assert_eq!(record.source_fact_seq, 0);
    assert_eq!(record.source_event_id.as_str(), ZERO_EVENT_ID);
}

#[test]
fn recovery_uses_largest_committed_timestamp_then_fact_seq() {
    let temp = tempfile::tempdir().expect("tempdir");
    let facts = vec![fact(1, 100), fact(2, 200), fact(3, 200)];
    let clock = ContentClock::open_or_recover(temp.path(), &facts, None).expect("recover clock");

    assert_eq!(clock.logical_now_ms(), 200);
    let record = read_clock(temp.path());
    assert_eq!(record.source_fact_seq, 3);
    assert_eq!(record.source_event_id, facts[2].event_id);
}

#[test]
fn recovery_ignores_clock_source_beyond_committed_high_water() {
    let temp = tempfile::tempdir().expect("tempdir");
    let stale = ContentClockRecord {
        schema: CONTENT_CLOCK_SCHEMA.into(),
        logical_now_ms: 900,
        source_fact_seq: 99,
        source_event_id: EventId::new("01J00000000000000000000099"),
        updated_at_ms: 900,
    };
    write_clock(temp.path(), &stale);

    let facts = vec![fact(1, 100), fact(2, 200)];
    let clock = ContentClock::open_or_recover(temp.path(), &facts, None).expect("recover clock");

    assert_eq!(clock.logical_now_ms(), 200);
    assert_eq!(read_clock(temp.path()).source_fact_seq, 2);
}

#[test]
fn recovery_keeps_ahead_clock_and_manifest_floor() {
    let temp = tempfile::tempdir().expect("tempdir");
    let ahead = ContentClockRecord {
        schema: CONTENT_CLOCK_SCHEMA.into(),
        logical_now_ms: 500,
        source_fact_seq: 2,
        source_event_id: EventId::new("01J00000000000000000000002"),
        updated_at_ms: 500,
    };
    write_clock(temp.path(), &ahead);

    let facts = vec![fact(1, 100), fact(2, 200)];
    let clock =
        ContentClock::open_or_recover(temp.path(), &facts, Some(600)).expect("recover clock");

    assert_eq!(clock.logical_now_ms(), 600);
    assert_eq!(read_clock(temp.path()).logical_now_ms, 600);
}

#[test]
fn corrupt_clock_is_rebuilt_from_committed_facts() {
    let temp = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(temp.path().join("content")).expect("create content dir");
    fs::write(temp.path().join(CONTENT_CLOCK_FILE), b"not json").expect("write corrupt clock");

    let facts = vec![fact(1, 123)];
    let clock = ContentClock::open_or_recover(temp.path(), &facts, None).expect("recover clock");

    assert_eq!(clock.logical_now_ms(), 123);
    assert_eq!(read_clock(temp.path()).source_fact_seq, 1);
}

#[test]
fn advance_is_monotonic_and_noops_for_older_facts() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut clock =
        ContentClock::open_or_recover(temp.path(), &[], None).expect("recover empty clock");

    assert!(clock.advance(&fact(1, 100)).expect("advance to first fact"));
    assert_eq!(clock.logical_now_ms(), 100);
    assert!(!clock.advance(&fact(2, 50)).expect("older fact no-op"));
    assert_eq!(clock.logical_now_ms(), 100);
    assert!(
        clock
            .advance(&fact(2, 200))
            .expect("advance to second fact")
    );
    assert_eq!(clock.logical_now_ms(), 200);
    assert_eq!(read_clock(temp.path()).source_fact_seq, 2);
}
