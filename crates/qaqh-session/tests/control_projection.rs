//! ControlProjection apply/rebuild contract.

use qaqh_session::projection::{ControlProjection, Projection};
use qaqh_session::session_fact_v2::{
    ActivityState, ActorKind, ActorRef, ContentHash, ContentRef, ControlDelta, EventId,
    ExecutionId, FactPayload, InputId, InteractionDecision, InteractionId, InteractionKind,
    InteractionRequested, InteractionResolved, ModelRoundStarted, PolicyDecisionRef, RecoveryId,
    RecoveryOutcome, SessionFact, SessionId, SessionTitleChanged, SideEffectClass,
    SubagentFinished, SubagentSpawned, SubagentTerminalStatus, TitleSource, ToolCallId,
    ToolFinished, ToolIntent, ToolIntentPolicyOutcome, ToolMetrics, ToolReplayCapability,
    ToolTerminalStatus, TurnFinished, TurnId, TurnMode, TurnTerminal,
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

fn turn_id() -> TurnId {
    TurnId::new("turn_01J00000000000000000000000")
}

fn call_id() -> ToolCallId {
    ToolCallId::new("call_01J00000000000000000000000")
}

fn execution_id() -> ExecutionId {
    ExecutionId::new("exec_01J00000000000000000000000")
}

fn interaction_id() -> InteractionId {
    InteractionId::new("int_01J00000000000000000000000")
}

fn child_id() -> SessionId {
    SessionId::new("0198f1a0-0000-7000-8000-000000000003")
}

fn content_ref(seed: u8) -> ContentRef {
    ContentRef::new(ContentHash::new(format!("sha256:{seed:064x}")))
}

fn turn_started(ordinal: u64) -> SessionFact {
    let turn_id = turn_id();
    let mut fact = with_payload(
        fact(ordinal),
        FactPayload::TurnStarted(qaqh_session::session_fact_v2::TurnStarted {
            turn_id: turn_id.clone(),
            input_id: InputId::new("input_01J00000000000000000000000"),
            mode: TurnMode::Normal,
            recovery_ref: None,
        }),
    );
    fact.turn_id = Some(turn_id);
    fact
}

fn round_started(ordinal: u64) -> SessionFact {
    let turn_id = turn_id();
    let mut fact = with_payload(
        fact(ordinal),
        FactPayload::ModelRoundStarted(ModelRoundStarted {
            turn_id: turn_id.clone(),
            round: 1,
            request_hash: ContentHash::new(
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
            context_revision: 3,
        }),
    );
    fact.turn_id = Some(turn_id);
    fact
}

fn tool_intent(ordinal: u64) -> SessionFact {
    let call_id = call_id();
    let mut fact = with_payload(
        fact(ordinal),
        FactPayload::ToolIntent(ToolIntent {
            call_id: call_id.clone(),
            execution_id: execution_id(),
            idempotency_key: None,
            replay_capability: ToolReplayCapability::NoReplay,
            policy_decision: PolicyDecisionRef {
                outcome: ToolIntentPolicyOutcome::Allow,
                rule_id: "allow_readonly".into(),
                decided_at_ms: 1_789_830_000_004,
                reason_ref: None,
            },
            effective_args_ref: None,
            effective_args_hash: None,
            sandbox_spec_hash: ContentHash::new(
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            ),
            side_effect_class: SideEffectClass::ReadOnly,
            intent_at_ms: 1_789_830_000_004,
        }),
    );
    fact.call_id = Some(call_id);
    fact
}

fn tool_finished(ordinal: u64) -> SessionFact {
    let call_id = call_id();
    let mut fact = with_payload(
        fact(ordinal),
        FactPayload::ToolFinished(ToolFinished {
            call_id: call_id.clone(),
            execution_id: Some(execution_id()),
            terminal_status: ToolTerminalStatus::Succeeded,
            output_ref: Some(content_ref(1)),
            error: None,
            metrics: ToolMetrics {
                started_at_ms: 1_789_830_000_004,
                finished_at_ms: 1_789_830_000_005,
                retry_count: 0,
                output_bytes: 12,
                progress_bytes_total: 12,
            },
            reconciled: false,
            evidence_ref: None,
            evidence_fact_seq: None,
            evidence_event_id: None,
            recovery_ref: None,
            finished_at_ms: 1_789_830_000_005,
        }),
    );
    fact.call_id = Some(call_id);
    fact
}

fn interaction_requested(ordinal: u64) -> SessionFact {
    let interaction_id = interaction_id();
    let turn_id = turn_id();
    let mut fact = with_payload(
        fact(ordinal),
        FactPayload::InteractionRequested(InteractionRequested {
            interaction_id: interaction_id.clone(),
            call_id: Some(call_id()),
            turn_id: turn_id.clone(),
            kind: InteractionKind::Ask,
            request_ref: content_ref(2),
            expires_at_ms: Some(1_789_830_000_100),
            requested_at_ms: 1_789_830_000_006,
        }),
    );
    fact.interaction_id = Some(interaction_id);
    fact.turn_id = Some(turn_id);
    fact
}

fn interaction_resolved(ordinal: u64) -> SessionFact {
    let interaction_id = interaction_id();
    let mut fact = with_payload(
        fact(ordinal),
        FactPayload::InteractionResolved(InteractionResolved {
            interaction_id: interaction_id.clone(),
            decision_ref: content_ref(3),
            decision: Some(InteractionDecision::Answered),
            resolved_by: ActorRef {
                kind: ActorKind::User,
                id: "local".into(),
                display_name: None,
            },
            resolution_seq: 1,
            resolved_at_ms: 1_789_830_000_007,
        }),
    );
    fact.interaction_id = Some(interaction_id);
    fact
}

fn turn_finished(ordinal: u64) -> SessionFact {
    let turn_id = turn_id();
    let mut fact = with_payload(
        fact(ordinal),
        FactPayload::TurnFinished(TurnFinished {
            turn_id: turn_id.clone(),
            terminal: TurnTerminal::Completed,
            usage: None,
            error: None,
            finished_at_ms: 1_789_830_000_008,
        }),
    );
    fact.turn_id = Some(turn_id);
    fact
}

fn subagent_spawned(ordinal: u64) -> SessionFact {
    with_payload(
        fact(ordinal),
        FactPayload::SubagentSpawned(SubagentSpawned {
            child_session_id: child_id(),
            parent_call_id: call_id(),
            parent_agent_path: None,
            child_agent_path: None,
            role: Some("worker".into()),
            spawned_at_ms: 1_789_830_000_009,
        }),
    )
}

fn subagent_finished(ordinal: u64) -> SessionFact {
    with_payload(
        fact(ordinal),
        FactPayload::SubagentFinished(SubagentFinished {
            child_session_id: child_id(),
            parent_call_id: call_id(),
            status: SubagentTerminalStatus::Completed,
            result_ref: Some(content_ref(4)),
            finished_at_ms: 1_789_830_000_010,
            recovery_ref: None,
        }),
    )
}

fn recovered(ordinal: u64) -> SessionFact {
    with_payload(
        fact(ordinal),
        FactPayload::SessionRecovered(qaqh_session::session_fact_v2::SessionRecovered {
            recovery_id: RecoveryId::new("recovery_01J00000000000000000000000"),
            recovery_event_id: EventId::new("01J00000000000000000000011"),
            recovery_input_fingerprint: ContentHash::new(
                "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            ),
            outcome: RecoveryOutcome::Writable,
            last_good_fact_seq: ordinal - 1,
            torn_tail: false,
            torn_bytes: None,
            actions: Vec::new(),
            recovered_at_ms: 1_789_830_000_011,
        }),
    )
}

fn title_changed(ordinal: u64) -> SessionFact {
    with_payload(
        fact(ordinal),
        FactPayload::SessionTitleChanged(SessionTitleChanged {
            title: "ignored by control".into(),
            source: TitleSource::User,
            changed_at_ms: 1_789_830_000_012,
        }),
    )
}

#[test]
fn control_projection_applies_and_rebuilds_equivalently() {
    let facts = vec![
        fact(1),
        turn_started(2),
        round_started(3),
        tool_intent(4),
        tool_finished(5),
        interaction_requested(6),
        interaction_resolved(7),
        turn_finished(8),
        subagent_spawned(9),
        subagent_finished(10),
        recovered(11),
        title_changed(12),
    ];

    let mut projection = ControlProjection::default();
    let deltas: Vec<ControlDelta> = facts
        .iter()
        .filter_map(|fact| projection.apply(fact))
        .collect();

    assert_eq!(deltas.len(), 11);
    assert!(matches!(
        &deltas[0],
        ControlDelta::SessionCreated { revision: 1, .. }
    ));
    assert!(matches!(
        &deltas[1],
        ControlDelta::Activity {
            revision: 2,
            state: ActivityState::Running,
            ..
        }
    ));
    assert!(matches!(
        &deltas[2],
        ControlDelta::Round { revision: 3, .. }
    ));
    assert!(matches!(
        &deltas[3],
        ControlDelta::ToolIntent { revision: 4, .. }
    ));
    assert!(matches!(
        &deltas[4],
        ControlDelta::ToolFinished {
            revision: 5,
            terminal_status: ToolTerminalStatus::Succeeded,
            ..
        }
    ));
    assert!(matches!(
        &deltas[5],
        ControlDelta::InteractionRequested { revision: 6, .. }
    ));
    assert!(matches!(
        &deltas[6],
        ControlDelta::InteractionResolved { revision: 7, .. }
    ));
    assert!(matches!(
        &deltas[7],
        ControlDelta::Activity {
            revision: 8,
            state: ActivityState::Idle,
            ..
        }
    ));
    assert!(matches!(
        &deltas[8],
        ControlDelta::SubagentSpawned { revision: 9, .. }
    ));
    assert!(matches!(
        &deltas[9],
        ControlDelta::SubagentFinished {
            revision: 10,
            status: SubagentTerminalStatus::Completed,
            ..
        }
    ));
    assert!(matches!(
        &deltas[10],
        ControlDelta::SessionRecovered {
            revision: 11,
            outcome: RecoveryOutcome::Writable,
            ..
        }
    ));

    let snapshot = projection.snapshot();
    assert_eq!(snapshot.activity, ActivityState::Idle);
    assert_eq!(snapshot.current_call_id, None);
    assert_eq!(snapshot.round.as_ref().map(|round| round.round), Some(1));
    assert_eq!(snapshot.tools.len(), 1);
    assert_eq!(
        snapshot.tools[0].terminal_status,
        Some(ToolTerminalStatus::Succeeded)
    );
    assert_eq!(snapshot.interactions.len(), 1);
    assert_eq!(
        snapshot.interactions[0]
            .resolution
            .as_ref()
            .and_then(|resolution| resolution.verdict),
        Some(InteractionDecision::Answered)
    );
    assert_eq!(snapshot.subagents.len(), 1);
    assert_eq!(
        snapshot.subagents[0].status,
        Some(SubagentTerminalStatus::Completed)
    );
    assert_eq!(snapshot.last_recovery, Some(RecoveryOutcome::Writable));
    assert_eq!(snapshot.revision, 11);
    assert_eq!(snapshot.last_fact_seq, 12);

    let rebuilt = ControlProjection::rebuild(facts.into_iter());
    assert_eq!(snapshot, rebuilt.snapshot());
    assert_eq!(rebuilt.last_fact_seq(), 12);
}

#[test]
fn unrelated_fact_advances_progress_without_a_delta() {
    let mut projection = ControlProjection::default();
    assert!(projection.apply(&title_changed(7)).is_none());
    assert_eq!(projection.last_fact_seq(), 7);
    assert_eq!(projection.snapshot().revision, 0);
}

#[test]
fn control_snapshot_roundtrips() {
    let facts = vec![fact(1), turn_started(2), tool_intent(3), tool_finished(4)];
    let rebuilt = ControlProjection::rebuild(facts.into_iter());
    let snapshot = rebuilt.snapshot();
    let encoded = serde_json::to_vec(&snapshot).expect("serialize control snapshot");
    let decoded: qaqh_session::projection::ControlSnapshot =
        serde_json::from_slice(&encoded).expect("deserialize control snapshot");
    assert_eq!(snapshot, decoded);
}
