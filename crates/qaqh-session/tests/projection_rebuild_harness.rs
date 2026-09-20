//! P1 projection trait and rebuild harness contract.

use std::fs;
use std::io::Write;

use qaqh_session::canonical::{
    CanonicalError, CanonicalLog, CommittedFactReader, EVENTS_FILE, WriterId,
};
use qaqh_session::projection::{Projection, rebuild_from_reader};
use qaqh_session::session_fact_v2::{EventId, LogId, SessionFact, SessionId};
use serde::{Deserialize, Serialize};

const ENVELOPE_FIXTURE: &str = include_str!("fixtures/session_fact_v2/envelope.jsonl");
const NOW_MS: i64 = 1_789_830_000_000;
const LEASE_MS: i64 = 1_000;

fn session_id() -> SessionId {
    SessionId::new("0198f1a0-0000-7000-8000-000000000001")
}

fn log_id() -> LogId {
    LogId::new("0198f1a0-0000-7000-8000-000000000002")
}

fn fact(ordinal: u64) -> SessionFact {
    let mut fact: SessionFact =
        serde_json::from_str(ENVELOPE_FIXTURE.trim()).expect("parse envelope fixture");
    fact.fact_seq = ordinal;
    fact.event_id = EventId::new(format!("01J{ordinal:023}"));
    fact
}

fn append_facts(dir: &std::path::Path, count: u64) -> Vec<SessionFact> {
    let mut log = CanonicalLog::open(dir, session_id(), log_id()).expect("open canonical log");
    let lease = log
        .acquire_writer(WriterId::new("projection-rebuild-writer"), NOW_MS, LEASE_MS)
        .expect("acquire writer");
    (1..=count)
        .map(|ordinal| {
            log.append(&lease, fact(ordinal), NOW_MS + ordinal as i64)
                .expect("append fact")
        })
        .collect()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct CountingSnapshot {
    fact_seqs: Vec<u64>,
    last_fact_seq: u64,
}

#[derive(Debug, Default)]
struct CountingProjection {
    snapshot: CountingSnapshot,
}

impl Projection for CountingProjection {
    type Snapshot = CountingSnapshot;
    type Delta = u64;

    fn apply(&mut self, fact: &SessionFact) -> Option<Self::Delta> {
        self.snapshot.fact_seqs.push(fact.fact_seq);
        self.snapshot.last_fact_seq = fact.fact_seq;
        Some(fact.fact_seq)
    }

    fn snapshot(&self) -> Self::Snapshot {
        self.snapshot.clone()
    }

    fn last_fact_seq(&self) -> u64 {
        self.snapshot.last_fact_seq
    }
}

#[test]
fn incremental_apply_and_rebuild_are_equivalent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let facts = append_facts(temp.path(), 3);

    let mut incremental = CountingProjection::default();
    for fact in &facts {
        assert_eq!(incremental.apply(fact), Some(fact.fact_seq));
    }
    let rebuilt = CountingProjection::rebuild(facts.clone().into_iter());

    assert_eq!(incremental.snapshot(), rebuilt.snapshot());
    assert_eq!(rebuilt.last_fact_seq(), 3);
}

#[test]
fn rebuild_from_reader_uses_only_committed_prefix() {
    let temp = tempfile::tempdir().expect("tempdir");
    append_facts(temp.path(), 2);
    let reader =
        CommittedFactReader::open(temp.path(), session_id(), log_id()).expect("open reader");

    let extra = serde_json::to_string(&fact(3)).expect("serialize extra fact");
    let mut events = fs::OpenOptions::new()
        .append(true)
        .open(temp.path().join(EVENTS_FILE))
        .expect("open events");
    writeln!(events, "{extra}").expect("append uncommitted suffix");
    events.sync_all().expect("sync suffix");
    drop(events);

    let rebuilt: CountingProjection =
        rebuild_from_reader(&reader).expect("rebuild committed projection");
    assert_eq!(
        rebuilt.snapshot(),
        CountingSnapshot {
            fact_seqs: vec![1, 2],
            last_fact_seq: 2,
        }
    );
}

#[test]
fn rebuild_from_reader_propagates_reader_failures() {
    let temp = tempfile::tempdir().expect("tempdir");
    append_facts(temp.path(), 1);
    let reader =
        CommittedFactReader::open(temp.path(), session_id(), log_id()).expect("open reader");
    fs::write(temp.path().join(EVENTS_FILE), b"").expect("truncate events");

    assert!(matches!(
        rebuild_from_reader::<CountingProjection>(&reader),
        Err(CanonicalError::CommitRecoveryRequired(_))
    ));
}
