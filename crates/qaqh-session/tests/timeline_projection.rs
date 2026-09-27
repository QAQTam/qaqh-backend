//! TimelineProjection apply/rebuild contract.

use qaqh_session::projection::{
    Projection, TimelineEntryKind, TimelineProjection, TimelineSnapshot,
};
use qaqh_session::session_fact_v2::{
    AssistantBlockKind, AssistantBlockSealed, BlockId, CompactionApplied, ContentHash, ContentRef,
    EventId, FactPayload, InputAccepted, InputId, InputKind, InputPurpose, InterruptReason,
    ModelRoundStarted, RecoveryId, RecoveryRef, SessionFact, TimelineDelta, ToolCallDeclared,
    ToolCallId, ToolFinished, ToolMetrics, ToolTerminalStatus, TurnFinished, TurnId,
    TurnInterrupted, TurnStarted, TurnTerminal,
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

fn input_id() -> InputId {
    InputId::new("input_01J00000000000000000000000")
}

fn turn_id() -> TurnId {
    TurnId::new("turn_01J00000000000000000000000")
}

fn block_id() -> BlockId {
    BlockId::new("block_01J00000000000000000000000")
}

fn call_id() -> ToolCallId {
    ToolCallId::new("call_01J00000000000000000000000")
}

fn content_ref(session_id: u8) -> ContentRef {
    ContentRef::new(ContentHash::new(format!("sha256:{session_id:064x}")))
}

fn recovery_ref() -> RecoveryRef {
    RecoveryRef {
        recovery_id: RecoveryId::new("recovery_01J00000000000000000000000"),
        recovery_event_id: EventId::new("01J00000000000000000000099"),
        recovery_input_fingerprint: ContentHash::new(
            "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        ),
    }
}

fn input_accepted(ordinal: u64) -> SessionFact {
    with_payload(
        fact(ordinal),
        FactPayload::InputAccepted(InputAccepted {
            input_id: input_id(),
            input_kind: InputKind::UserText,
            input_purpose: InputPurpose::TriggerTurn,
            content_ref: None,
            inline_text: Some("hello".into()),
            attachments: vec![content_ref(9)],
            actor: qaqh_session::session_fact_v2::ActorRef {
                kind: qaqh_session::session_fact_v2::ActorKind::User,
                id: "local".into(),
                display_name: None,
            },
            client_request_id: None,
        }),
    )
}

fn assistant_block(ordinal: u64) -> SessionFact {
    let turn_id = turn_id();
    let mut fact = with_payload(
        fact(ordinal),
        FactPayload::AssistantBlockSealed(AssistantBlockSealed {
            turn_id: turn_id.clone(),
            block_id: block_id(),
            kind: AssistantBlockKind::Answer,
            content_ref: content_ref(1),
            model: "deepseek-v4.1-flash".into(),
            usage: None,
        }),
    );
    fact.turn_id = Some(turn_id);
    fact
}

fn tool_call_declared(ordinal: u64) -> SessionFact {
    let turn_id = turn_id();
    let call_id = call_id();
    let mut fact = with_payload(
        fact(ordinal),
        FactPayload::ToolCallDeclared(ToolCallDeclared {
            turn_id: turn_id.clone(),
            call_id: call_id.clone(),
            tool_name: "read".into(),
            args_ref: content_ref(2),
            args_hash: content_ref(2).0,
        }),
    );
    fact.turn_id = Some(turn_id);
    fact.call_id = Some(call_id);
    fact
}

fn tool_finished(ordinal: u64) -> SessionFact {
    let call_id = call_id();
    let mut fact = with_payload(
        fact(ordinal),
        FactPayload::ToolFinished(ToolFinished {
            call_id: call_id.clone(),
            execution_id: None,
            terminal_status: ToolTerminalStatus::Succeeded,
            output_ref: Some(content_ref(3)),
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

fn turn_finished(ordinal: u64) -> SessionFact {
    let turn_id = turn_id();
    let mut fact = with_payload(
        fact(ordinal),
        FactPayload::TurnFinished(TurnFinished {
            turn_id: turn_id.clone(),
            terminal: TurnTerminal::Completed,
            usage: None,
            error: None,
            finished_at_ms: 1_789_830_000_006,
        }),
    );
    fact.turn_id = Some(turn_id);
    fact
}

fn turn_interrupted(ordinal: u64) -> SessionFact {
    let turn_id = turn_id();
    let mut fact = with_payload(
        fact(ordinal),
        FactPayload::TurnInterrupted(TurnInterrupted {
            turn_id: turn_id.clone(),
            reason: InterruptReason::CancelBeforeSeal,
            last_fact_seq: ordinal - 1,
            recovery_ref: recovery_ref(),
        }),
    );
    fact.turn_id = Some(turn_id);
    fact
}

fn compaction(ordinal: u64) -> SessionFact {
    with_payload(
        fact(ordinal),
        FactPayload::CompactionApplied(CompactionApplied {
            checkpoint_id: qaqh_session::session_fact_v2::CheckpointId::new(
                "ckpt_01J00000000000000000000000",
            ),
            replaces_through_fact_seq: ordinal - 1,
            summary_ref: content_ref(4),
            context_revision: 9,
            applied_at_ms: 1_789_830_000_007,
        }),
    )
}

fn unrelated_round(ordinal: u64) -> SessionFact {
    with_payload(
        fact(ordinal),
        FactPayload::ModelRoundStarted(ModelRoundStarted {
            turn_id: turn_id(),
            round: 0,
            request_hash: ContentHash::new(
                "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
            ),
            context_revision: 0,
        }),
    )
}

#[test]
fn timeline_projection_applies_and_rebuilds_equivalently() {
    let facts = vec![
        input_accepted(1),
        assistant_block(2),
        tool_call_declared(3),
        tool_finished(4),
        turn_finished(5),
        compaction(6),
    ];

    let mut projection = TimelineProjection::default();
    let deltas: Vec<TimelineDelta> = facts
        .iter()
        .filter_map(|fact| projection.apply(fact))
        .collect();

    assert_eq!(deltas.len(), 6);
    assert!(matches!(
        &deltas[0],
        TimelineDelta::Input {
            revision: 1,
            input_id: id,
            ..
        } if id == &input_id()
    ));
    assert!(matches!(
        &deltas[1],
        TimelineDelta::Block {
            revision: 2,
            block_id: id,
            block_kind: AssistantBlockKind::Answer,
            ..
        } if id == &block_id()
    ));
    assert!(matches!(
        &deltas[2],
        TimelineDelta::ToolCall {
            revision: 3,
            call_id: id,
            tool_name,
            ..
        } if id == &call_id() && tool_name == "read"
    ));
    assert!(matches!(
        &deltas[3],
        TimelineDelta::ToolResult {
            revision: 4,
            call_id: id,
            terminal_status: ToolTerminalStatus::Succeeded,
            ..
        } if id == &call_id()
    ));
    assert!(matches!(
        &deltas[4],
        TimelineDelta::TurnFinished {
            revision: 5,
            turn_id: id,
            terminal: TurnTerminal::Completed,
        } if id == &turn_id()
    ));
    assert!(matches!(
        &deltas[5],
        TimelineDelta::Compaction {
            revision: 6,
            checkpoint_id,
            ..
        } if checkpoint_id.as_str() == "ckpt_01J00000000000000000000000"
    ));

    let snapshot = projection.snapshot();
    assert_eq!(snapshot.session_id.as_ref(), Some(&facts[0].session_id));
    assert_eq!(snapshot.entries.len(), 5);
    assert_eq!(snapshot.revision, 6);
    assert_eq!(snapshot.last_fact_seq, 6);
    assert!(matches!(
        &snapshot.entries[0].kind,
        TimelineEntryKind::Input(state)
            if matches!(&state.content, qaqh_session::session_fact_v2::ContentValue::Inline { text } if text == "hello")
    ));
    assert!(matches!(
        &snapshot.entries[1].kind,
        TimelineEntryKind::AssistantBlock(state)
            if state.content == qaqh_session::session_fact_v2::ContentValue::Ref {
                content_ref: content_ref(1)
            }
    ));
    assert!(matches!(
        &snapshot.entries[2].kind,
        TimelineEntryKind::ToolCall(state)
            if state.result.as_ref().is_some_and(|result| {
                result.terminal_status == ToolTerminalStatus::Succeeded
            })
    ));
    assert!(matches!(
        &snapshot.entries[3].kind,
        TimelineEntryKind::TurnFinished(state)
            if state.turn_id == turn_id() && state.terminal == TurnTerminal::Completed
    ));
    assert!(matches!(
        &snapshot.entries[4].kind,
        TimelineEntryKind::Compaction(state)
            if state.checkpoint_id.as_str() == "ckpt_01J00000000000000000000000"
    ));

    let rebuilt = TimelineProjection::rebuild(facts.into_iter());
    assert_eq!(snapshot, rebuilt.snapshot());
    assert_eq!(rebuilt.last_fact_seq(), 6);
}

#[test]
fn stable_ids_upsert_without_reordering_transcript_entries() {
    let facts = vec![
        input_accepted(1),
        input_accepted(2),
        assistant_block(3),
        assistant_block(4),
        tool_call_declared(5),
        tool_finished(6),
    ];

    let rebuilt = TimelineProjection::rebuild(facts.into_iter());
    let snapshot = rebuilt.snapshot();

    assert_eq!(snapshot.entries.len(), 3);
    assert_eq!(snapshot.entries[0].source_fact_seq, 2);
    assert_eq!(snapshot.entries[1].source_fact_seq, 4);
    assert_eq!(snapshot.entries[2].source_fact_seq, 6);
    assert!(matches!(
        &snapshot.entries[2].kind,
        TimelineEntryKind::ToolCall(state)
            if state.tool_name.as_deref() == Some("read")
                && state.args.is_some()
                && state.result.is_some()
    ));
}

#[test]
fn compaction_appends_marker_without_pruning_transcript() {
    let facts = vec![input_accepted(1), assistant_block(2), compaction(3)];
    let rebuilt = TimelineProjection::rebuild(facts.into_iter());
    let snapshot = rebuilt.snapshot();

    assert_eq!(snapshot.entries.len(), 3);
    assert!(matches!(
        snapshot.entries[0].kind,
        TimelineEntryKind::Input(_)
    ));
    assert!(matches!(
        snapshot.entries[1].kind,
        TimelineEntryKind::AssistantBlock(_)
    ));
    assert!(matches!(
        snapshot.entries[2].kind,
        TimelineEntryKind::Compaction(_)
    ));
}

#[test]
fn unrelated_fact_advances_progress_without_a_delta() {
    let mut projection = TimelineProjection::default();
    assert!(projection.apply(&unrelated_round(7)).is_none());
    let snapshot = projection.snapshot();
    assert_eq!(snapshot.revision, 0);
    assert_eq!(snapshot.last_fact_seq, 7);
    assert!(snapshot.entries.is_empty());
}

#[test]
fn interrupted_turn_and_snapshot_roundtrip() {
    let facts = vec![turn_interrupted(1)];
    let rebuilt = TimelineProjection::rebuild(facts.into_iter());
    let snapshot = rebuilt.snapshot();

    assert!(matches!(
        &snapshot.entries[0].kind,
        TimelineEntryKind::TurnInterrupted(state)
            if state.turn_id == turn_id()
                && state.reason == InterruptReason::CancelBeforeSeal
    ));

    let encoded = serde_json::to_vec(&snapshot).expect("serialize timeline snapshot");
    let decoded: TimelineSnapshot =
        serde_json::from_slice(&encoded).expect("deserialize timeline snapshot");
    assert_eq!(snapshot, decoded);
}

#[test]
fn turn_started_alone_is_not_a_timeline_entry() {
    let turn_id = turn_id();
    let mut fact = with_payload(
        fact(1),
        FactPayload::TurnStarted(TurnStarted {
            turn_id: turn_id.clone(),
            input_id: input_id(),
            mode: qaqh_session::session_fact_v2::TurnMode::Normal,
            recovery_ref: None,
        }),
    );
    fact.turn_id = Some(turn_id);

    let mut projection = TimelineProjection::default();
    assert!(projection.apply(&fact).is_none());
    assert!(projection.snapshot().entries.is_empty());
    assert_eq!(projection.last_fact_seq(), 1);
}
