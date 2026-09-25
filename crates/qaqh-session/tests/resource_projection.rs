//! ResourceProjection apply/rebuild contract.

use qaqh_session::projection::{Projection, ResourceProjection};
use qaqh_session::session_fact_v2::{
    ContentHash, ContentRef, EventId, FactPayload, ResourceDelta, ResourceId, ResourceKind,
    SessionFact, SessionId, SubagentFinished, SubagentSpawned, SubagentTerminalStatus, ToolCallId,
    WorkspaceResourceChanged,
};

const ENVELOPE_FIXTURE: &str = include_str!("fixtures/session_fact_v2/envelope.jsonl");

fn fact(ordinal: u64) -> SessionFact {
    let mut fact: SessionFact =
        serde_json::from_str(ENVELOPE_FIXTURE.trim()).expect("parse envelope fixture");
    fact.fact_seq = ordinal;
    fact.event_id = EventId::new(format!("01J{ordinal:023}"));
    fact
}

fn with_payload(mut fact: SessionFact, payload: FactPayload) -> SessionFact {
    fact.payload = payload;
    fact
}

fn resource_id() -> ResourceId {
    ResourceId::new("res_01J00000000000000000000000")
}

fn summary_ref(seed: u8) -> ContentRef {
    ContentRef::new(ContentHash::new(format!("sha256:{seed:064x}")))
}

fn workspace(
    ordinal: u64,
    resource_kind: ResourceKind,
    revision: u64,
    deleted: bool,
) -> SessionFact {
    with_payload(
        fact(ordinal),
        FactPayload::WorkspaceResourceChanged(WorkspaceResourceChanged {
            resource_kind,
            resource_id: resource_id(),
            source_call_id: Some(ToolCallId::new("call_01J00000000000000000000000")),
            revision,
            summary_ref: summary_ref(revision as u8),
            deleted,
        }),
    )
}

fn child_id() -> SessionId {
    SessionId::new("0198f1a0-0000-7000-8000-000000000003")
}

fn spawned(ordinal: u64) -> SessionFact {
    with_payload(
        fact(ordinal),
        FactPayload::SubagentSpawned(SubagentSpawned {
            child_session_id: child_id(),
            parent_call_id: ToolCallId::new("call_01J00000000000000000000001"),
            parent_agent_path: None,
            child_agent_path: None,
            role: Some("worker".into()),
            spawn_config: None,
            spawned_at_ms: 1_789_830_000_004,
        }),
    )
}

fn finished(ordinal: u64) -> SessionFact {
    with_payload(
        fact(ordinal),
        FactPayload::SubagentFinished(SubagentFinished {
            child_session_id: child_id(),
            parent_call_id: ToolCallId::new("call_01J00000000000000000000001"),
            status: SubagentTerminalStatus::Completed,
            result_ref: None,
            finished_at_ms: 1_789_830_000_005,
            recovery_ref: None,
        }),
    )
}

#[test]
fn resource_projection_applies_and_rebuilds_equivalently() {
    let facts = vec![
        workspace(1, ResourceKind::Todo, 1, false),
        workspace(2, ResourceKind::File, 1, false),
        workspace(3, ResourceKind::Todo, 2, true),
        spawned(4),
        finished(5),
        fact(6),
    ];

    let mut projection = ResourceProjection::default();
    let deltas: Vec<ResourceDelta> = facts
        .iter()
        .filter_map(|fact| projection.apply(fact))
        .collect();

    assert_eq!(deltas.len(), 5);
    assert!(matches!(
        &deltas[0],
        ResourceDelta::WorkspaceResourceChanged {
            revision: 1,
            resource_kind: ResourceKind::Todo,
            deleted: false,
            ..
        }
    ));
    assert!(matches!(
        &deltas[1],
        ResourceDelta::WorkspaceResourceChanged {
            revision: 2,
            resource_kind: ResourceKind::File,
            ..
        }
    ));
    assert!(matches!(
        &deltas[2],
        ResourceDelta::WorkspaceResourceChanged {
            revision: 3,
            resource_kind: ResourceKind::Todo,
            deleted: true,
            ..
        }
    ));
    assert!(matches!(
        &deltas[3],
        ResourceDelta::GraphEdge {
            revision: 4,
            status: None,
            ..
        }
    ));
    assert!(matches!(
        &deltas[4],
        ResourceDelta::GraphEdge {
            revision: 5,
            status: Some(SubagentTerminalStatus::Completed),
            ..
        }
    ));

    let snapshot = projection.snapshot();
    assert_eq!(snapshot.workspace.len(), 2);
    assert_eq!(snapshot.graph.len(), 1);
    let todo = snapshot
        .workspace
        .iter()
        .find(|state| state.resource_kind == ResourceKind::Todo)
        .expect("todo state");
    assert_eq!(todo.resource_revision, 2);
    assert!(todo.deleted);
    let file = snapshot
        .workspace
        .iter()
        .find(|state| state.resource_kind == ResourceKind::File)
        .expect("file state");
    assert_eq!(file.resource_revision, 1);
    assert!(!file.deleted);
    assert_eq!(
        snapshot.graph[0].status,
        Some(SubagentTerminalStatus::Completed)
    );
    assert_eq!(snapshot.revision, 5);
    assert_eq!(snapshot.last_fact_seq, 6);

    let rebuilt = ResourceProjection::rebuild(facts.into_iter());
    assert_eq!(snapshot, rebuilt.snapshot());
    assert_eq!(rebuilt.last_fact_seq(), 6);
}

#[test]
fn unrelated_fact_advances_progress_without_a_delta() {
    let mut projection = ResourceProjection::default();
    assert!(projection.apply(&fact(7)).is_none());
    assert_eq!(projection.last_fact_seq(), 7);
    assert_eq!(projection.snapshot().revision, 0);
}

#[test]
fn resource_snapshot_roundtrips() {
    let facts = vec![
        workspace(1, ResourceKind::Plan, 1, false),
        spawned(2),
        finished(3),
    ];
    let rebuilt = ResourceProjection::rebuild(facts.into_iter());
    let snapshot = rebuilt.snapshot();
    let encoded = serde_json::to_vec(&snapshot).expect("serialize resource snapshot");
    let decoded: qaqh_session::projection::ResourceSnapshot =
        serde_json::from_slice(&encoded).expect("deserialize resource snapshot");
    assert_eq!(snapshot, decoded);
}
