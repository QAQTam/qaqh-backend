use qaqh_session::projection::{Projection, ProjectionSet, TeamProjection};
use qaqh_session::session_fact_v2::{
    ActorKind, ActorRef, AgentPath, EventId, FactPayload, FactSchema, InputAccepted, InputId,
    InputKind, InputPurpose, InterAgentCommunication, InterAgentContent, InterAgentDelivery, LogId,
    MessageId, ProjectionPayload, ProjectionSlot, SessionCreated, SessionFact, SessionId,
    SubagentFinished, SubagentSpawned, SubagentTerminalStatus, TeamAgentResidency, TeamAgentStatus,
    TeamDelta, ToolCallId, TurnId,
};

const NOW_MS: i64 = 1_789_830_000_200;
const ROOT: &str = "0198f1a0-0000-7000-8000-000000000010";
const CHILD: &str = "0198f1a0-0000-7000-8000-000000000011";

fn id(raw: &str) -> SessionId {
    SessionId::new(raw)
}

fn log_id() -> LogId {
    LogId::new("0198f1a0-0000-7000-8000-000000000012")
}

fn path(raw: &str) -> AgentPath {
    AgentPath::parse_absolute(raw).expect("valid path")
}

fn fact(fact_seq: u64, payload: FactPayload) -> SessionFact {
    SessionFact {
        schema: FactSchema::v2(),
        session_id: id(ROOT),
        log_id: log_id(),
        fact_seq,
        event_id: EventId::new(format!("01J00000000000000000000{fact_seq:04}")),
        ts_ms: NOW_MS,
        causation_id: None,
        turn_id: None,
        call_id: None,
        interaction_id: None,
        payload,
    }
}

fn spawned(fact_seq: u64) -> SessionFact {
    fact(
        fact_seq,
        FactPayload::SubagentSpawned(SubagentSpawned {
            child_session_id: id(CHILD),
            parent_call_id: ToolCallId::new("call_01J00000000000000000000000"),
            parent_agent_path: Some(AgentPath::root()),
            child_agent_path: Some(path("/root/review")),
            role: Some("review".to_string()),
            spawn_config: None,
            spawned_at_ms: NOW_MS,
        }),
    )
}

#[test]
fn team_projection_tracks_roster_status_residency_and_inbox() {
    let mut team = TeamProjection::default();

    team.apply(&fact(
        1,
        FactPayload::SessionCreated(SessionCreated {
            created_at_ms: NOW_MS,
            cwd: "/".to_string(),
            model: "test".to_string(),
            parent_session_id: None,
            schema_caps: vec![],
        }),
    ));
    assert_eq!(team.snapshot().agents.len(), 1);

    assert!(matches!(
        team.apply(&spawned(2)),
        Some(TeamDelta::AgentJoined { .. })
    ));
    let spawned_child = team
        .snapshot()
        .agents
        .into_iter()
        .find(|agent| agent.agent_id == id(CHILD))
        .expect("spawned child roster entry");
    assert_eq!(
        spawned_child.residency,
        TeamAgentResidency::Unloaded,
        "canonical spawn alone must not claim a live worker"
    );
    assert!(matches!(
        team.apply_runtime_residency(&id(CHILD), TeamAgentResidency::Loaded),
        Some(TeamDelta::AgentResidencyChanged {
            residency: TeamAgentResidency::Loaded,
            ..
        })
    ));
    assert!(matches!(
        team.apply_runtime_residency(&id(CHILD), TeamAgentResidency::Unloaded),
        Some(TeamDelta::AgentResidencyChanged {
            residency: TeamAgentResidency::Unloaded,
            ..
        })
    ));
    assert!(
        team.apply_runtime_residency(&id(CHILD), TeamAgentResidency::Unloaded)
            .is_none(),
        "repeating the same residency must be idempotent"
    );
    assert!(matches!(
        team.apply(&fact(
            3,
            FactPayload::TurnStarted(qaqh_session::session_fact_v2::TurnStarted {
                turn_id: TurnId::new("t1"),
                input_id: InputId::new("input_01J00000000000000000000001"),
                mode: qaqh_session::session_fact_v2::TurnMode::Normal,
                recovery_ref: None,
            }),
        )),
        Some(TeamDelta::AgentStatusChanged {
            status: TeamAgentStatus::Running,
            ..
        })
    ));

    let message_id = MessageId::new("msg_01J00000000000000000000000");
    assert!(matches!(
        team.apply(&fact(
            4,
            FactPayload::InterAgentCommunication(InterAgentCommunication {
                message_id: message_id.clone(),
                root_session_id: id(ROOT),
                author: AgentPath::root(),
                recipient: path("/root/review"),
                other_recipients: vec![],
                task_id: Some("review".to_string()),
                content: InterAgentContent::Inline {
                    text: "review this".to_string(),
                },
                reply_to: None,
                causation_id: None,
                delivery: InterAgentDelivery::Queue,
                created_at_ms: NOW_MS,
            }),
        )),
        Some(TeamDelta::AgentMessageQueued { .. })
    ));
    assert_eq!(team.snapshot().unread_messages.len(), 1);

    assert!(matches!(
        team.apply(&fact(
            5,
            FactPayload::InputAccepted(InputAccepted {
                input_id: InputId::new("input_01J00000000000000000000000"),
                input_kind: InputKind::System,
                input_purpose: InputPurpose::QueueOnly,
                content_ref: None,
                inline_text: Some("review this".to_string()),
                attachments: vec![],
                actor: ActorRef {
                    kind: ActorKind::Subagent,
                    id: "/root".to_string(),
                    display_name: None,
                },
                client_request_id: Some(message_id.as_str().to_string()),
            }),
        )),
        Some(TeamDelta::AgentMessageDelivered { .. })
    ));
    assert!(team.snapshot().unread_messages.is_empty());

    assert!(matches!(
        team.apply(&fact(
            6,
            FactPayload::SubagentFinished(SubagentFinished {
                child_session_id: id(CHILD),
                parent_call_id: ToolCallId::new("call_01J00000000000000000000000"),
                status: SubagentTerminalStatus::Completed,
                result_ref: None,
                finished_at_ms: NOW_MS,
                recovery_ref: None,
            }),
        )),
        Some(TeamDelta::AgentCompleted {
            status: TeamAgentStatus::Completed,
            ..
        })
    ));
    let child = team
        .snapshot()
        .agents
        .into_iter()
        .find(|agent| agent.agent_id == id(CHILD))
        .expect("child roster entry");
    assert_eq!(child.status, TeamAgentStatus::Completed);
    assert_eq!(child.residency, TeamAgentResidency::Unloaded);
    assert_eq!(child.agent_path, path("/root/review"));
}

#[test]
fn projection_set_exposes_team_slot_and_snapshot() {
    let mut projections = ProjectionSet::default();
    let deltas = projections.apply(&spawned(1));
    let team = deltas
        .iter()
        .find(|delta| delta.slot == ProjectionSlot::Team)
        .expect("team delta");
    assert!(matches!(
        team.payload,
        ProjectionPayload::TeamDelta(TeamDelta::AgentJoined { .. })
    ));
    assert_eq!(projections.snapshot().team.agents.len(), 1);
}
