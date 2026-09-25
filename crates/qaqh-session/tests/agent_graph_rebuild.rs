//! Agent graph rebuild from the committed canonical prefix.

use qaqh_session::canonical::{CanonicalLog, CommittedFactReader, WriterId};
use qaqh_session::projection::{AgentGraphEdgeStatus, AgentGraphStore};
use qaqh_session::session_fact_v2::{
    AgentPath, EventId, FactPayload, FactSchema, LogId, SessionFact, SessionId, SubagentSpawned,
    ToolCallId,
};

const NOW_MS: i64 = 1_789_830_000_000;
const LEASE_MS: i64 = 10_000;

fn session_id() -> SessionId {
    SessionId::new("0198f1a0-0000-7000-8000-000000000001")
}

fn child_id() -> SessionId {
    SessionId::new("0198f1a0-0000-7000-8000-000000000003")
}

fn log_id() -> LogId {
    LogId::new("0198f1a0-0000-7000-8000-000000000002")
}

fn path(raw: &str) -> AgentPath {
    AgentPath::parse_absolute(raw).expect("valid agent path")
}

#[test]
fn rebuild_from_reader_restores_committed_spawn_edge() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut log = CanonicalLog::open(temp.path(), session_id(), log_id()).expect("open log");
    let lease = log
        .acquire_writer(WriterId::new("agent-graph-writer"), NOW_MS, LEASE_MS)
        .expect("acquire writer");
    let fact = SessionFact {
        schema: FactSchema::v2(),
        session_id: session_id(),
        log_id: log_id(),
        fact_seq: 0,
        event_id: EventId::new("01J00000000000000000000001"),
        ts_ms: NOW_MS + 1,
        causation_id: None,
        turn_id: None,
        call_id: None,
        interaction_id: None,
        payload: FactPayload::SubagentSpawned(SubagentSpawned {
            child_session_id: child_id(),
            parent_call_id: ToolCallId::new("call_01J00000000000000000000000"),
            parent_agent_path: Some(path("/root")),
            child_agent_path: Some(path("/root/review")),
            role: Some("worker".to_string()),
            spawned_at_ms: NOW_MS,
        }),
    };
    log.append(&lease, fact, NOW_MS + 1)
        .expect("append spawn fact");

    let reader =
        CommittedFactReader::open(temp.path(), session_id(), log_id()).expect("open reader");
    let graph = AgentGraphStore::rebuild_from_reader(&reader).expect("rebuild graph");
    let edge = graph.get_edge(&child_id()).expect("edge");
    assert_eq!(edge.status, AgentGraphEdgeStatus::Open);
    assert_eq!(
        graph.get_node(&child_id()).expect("child node").agent_path,
        path("/root/review")
    );
}

#[test]
fn rebuild_from_reader_fails_closed_without_paths() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut log = CanonicalLog::open(temp.path(), session_id(), log_id()).expect("open log");
    let lease = log
        .acquire_writer(WriterId::new("agent-graph-writer"), NOW_MS, LEASE_MS)
        .expect("acquire writer");
    let fact = SessionFact {
        schema: FactSchema::v2(),
        session_id: session_id(),
        log_id: log_id(),
        fact_seq: 0,
        event_id: EventId::new("01J00000000000000000000001"),
        ts_ms: NOW_MS + 1,
        causation_id: None,
        turn_id: None,
        call_id: None,
        interaction_id: None,
        payload: FactPayload::SubagentSpawned(SubagentSpawned {
            child_session_id: child_id(),
            parent_call_id: ToolCallId::new("call_01J00000000000000000000000"),
            parent_agent_path: None,
            child_agent_path: None,
            role: None,
            spawned_at_ms: NOW_MS,
        }),
    };
    log.append(&lease, fact, NOW_MS + 1)
        .expect("append legacy spawn fact");

    let reader =
        CommittedFactReader::open(temp.path(), session_id(), log_id()).expect("open reader");
    assert!(AgentGraphStore::rebuild_from_reader(&reader).is_err());
}
