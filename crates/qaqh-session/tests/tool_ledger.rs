//! Durable ToolLedger core contract.

use qaqh_session::{
    canonical::{
        CommittedFactReader, FactCausation, ToolLedger, ToolLedgerError, ToolRecoveryDisposition,
        WriterId, causation_for_command, generate_ulid, stable_workspace_resource_id,
        ulid_from_text,
    },
    session_fact_v2::{
        ActorKind, ActorRef, AgentPath, ContentHash, ContentRef, EventId, ExecutionId, FactPayload,
        InputAccepted, InputId, InputKind, InputPurpose, InterAgentCommunication,
        InterAgentContent, InterAgentDelivery, InteractionDecision, InteractionExpired,
        InteractionExpiryReason, InteractionId, InteractionKind, InteractionRequested,
        InteractionResolved, LogId, MessageId, PolicyDecisionRef, RecoveryId, RecoveryRef,
        ResourceId, ResourceKind, SessionId, SideEffectClass, SubagentFinished, SubagentSpawned,
        SubagentTerminalStatus, ToolCallId, ToolError, ToolFinished, ToolIntent,
        ToolIntentPolicyOutcome, ToolMetrics, ToolReplayCapability, ToolTerminalStatus, TurnId,
    },
};

const NOW_MS: i64 = 1_789_830_000_000;
const LEASE_MS: i64 = 10_000;

fn session_id() -> SessionId {
    SessionId::new("0198f1a0-0000-7000-8000-000000000001")
}

fn log_id() -> LogId {
    LogId::new("0198f1a0-0000-7000-8000-000000000002")
}

fn call_id(ordinal: u64) -> ToolCallId {
    ToolCallId::new(format!("call_{ordinal:026}"))
}

fn execution_id(ordinal: u64) -> ExecutionId {
    ExecutionId::new(format!("exec_{ordinal:026}"))
}

fn event_id(ordinal: u64) -> EventId {
    EventId::new(format!("01J{ordinal:023}"))
}

fn content_hash(session_id: u8) -> ContentHash {
    ContentHash::new(format!("sha256:{session_id:064x}"))
}

fn content_ref(session_id: u8) -> ContentRef {
    ContentRef::new(content_hash(session_id))
}

fn recovery_ref() -> RecoveryRef {
    RecoveryRef {
        recovery_id: RecoveryId::new("recovery_01J00000000000000000000000"),
        recovery_event_id: EventId::new("01J00000000000000000000099"),
        recovery_input_fingerprint: content_hash(9),
    }
}

fn interaction_id() -> InteractionId {
    InteractionId::new("int_01J00000000000000000000001")
}

fn interaction_requested() -> InteractionRequested {
    InteractionRequested {
        interaction_id: interaction_id(),
        call_id: Some(call_id(50)),
        turn_id: TurnId::new("turn_01J00000000000000000000050"),
        kind: InteractionKind::Permission,
        request_ref: content_ref(5),
        expires_at_ms: None,
        requested_at_ms: NOW_MS + 39,
    }
}

fn interaction_resolved() -> InteractionResolved {
    InteractionResolved {
        interaction_id: interaction_id(),
        decision_ref: content_ref(4),
        decision: Some(InteractionDecision::Approved),
        resolved_by: ActorRef {
            kind: ActorKind::User,
            id: "user-1".into(),
            display_name: None,
        },
        resolution_seq: 1,
        resolved_at_ms: NOW_MS + 40,
    }
}

fn interaction_expired(reason: InteractionExpiryReason) -> InteractionExpired {
    InteractionExpired {
        interaction_id: interaction_id(),
        reason,
        recovery_ref: None,
        expired_at_ms: NOW_MS + 41,
    }
}

fn input_accepted(text: &str) -> InputAccepted {
    InputAccepted {
        input_id: InputId::new("input_01J00000000000000000000001"),
        input_kind: InputKind::UserText,
        input_purpose: InputPurpose::TriggerTurn,
        content_ref: None,
        inline_text: Some(text.to_string()),
        attachments: vec![],
        actor: ActorRef {
            kind: ActorKind::User,
            id: "user-1".into(),
            display_name: None,
        },
        client_request_id: Some("msg_01J00000000000000000000001".into()),
    }
}

fn communication() -> InterAgentCommunication {
    InterAgentCommunication {
        message_id: MessageId::new("msg_01J00000000000000000000001"),
        root_session_id: session_id(),
        author: AgentPath::root(),
        recipient: AgentPath::parse_absolute("/root/review").expect("recipient"),
        other_recipients: vec![],
        task_id: None,
        content: InterAgentContent::Inline {
            text: "review the diff".into(),
        },
        reply_to: None,
        causation_id: None,
        delivery: InterAgentDelivery::Trigger,
        created_at_ms: NOW_MS + 42,
    }
}

fn intent(
    call: &ToolCallId,
    execution: &ExecutionId,
    replay_capability: ToolReplayCapability,
) -> ToolIntent {
    let idempotency_key = match &replay_capability {
        ToolReplayCapability::IdempotentReplay => Some("idem-tool-ledger".to_string()),
        ToolReplayCapability::NoReplay | ToolReplayCapability::Reconcile { .. } => None,
    };
    ToolIntent {
        call_id: call.clone(),
        execution_id: execution.clone(),
        idempotency_key,
        replay_capability,
        policy_decision: PolicyDecisionRef {
            outcome: ToolIntentPolicyOutcome::Allow,
            rule_id: "allow_readonly".into(),
            decided_at_ms: NOW_MS,
            reason_ref: None,
        },
        effective_args_ref: None,
        effective_args_hash: None,
        sandbox_spec_hash: content_hash(2),
        side_effect_class: SideEffectClass::ReadOnly,
        intent_at_ms: NOW_MS,
    }
}

fn finished(
    call: &ToolCallId,
    execution: Option<&ExecutionId>,
    terminal_status: ToolTerminalStatus,
    finished_at_ms: i64,
) -> ToolFinished {
    ToolFinished {
        call_id: call.clone(),
        execution_id: execution.cloned(),
        terminal_status,
        output_ref: None,
        error: (terminal_status == ToolTerminalStatus::Failed).then(|| ToolError {
            code: "tool_failed".into(),
            message: "failed".into(),
            retryable: false,
            details_ref: None,
        }),
        metrics: ToolMetrics {
            started_at_ms: NOW_MS,
            finished_at_ms,
            retry_count: 0,
            output_bytes: 0,
            progress_bytes_total: 0,
        },
        reconciled: false,
        evidence_ref: None,
        evidence_fact_seq: None,
        evidence_event_id: None,
        recovery_ref: None,
        finished_at_ms,
    }
}

fn open_ledger(dir: &std::path::Path) -> ToolLedger {
    ToolLedger::open(
        dir,
        session_id(),
        log_id(),
        WriterId::new("writer-a"),
        NOW_MS,
        LEASE_MS,
    )
    .expect("open tool ledger")
}

#[test]
fn input_and_communication_appends_are_idempotent_and_rebuildable() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut ledger = open_ledger(temp.path());

    let input = input_accepted("hello");
    let first_input = ledger
        .append_input_accepted(event_id(70), input.clone(), NOW_MS + 70)
        .expect("append input");
    let duplicate_input = ledger
        .append_input_accepted(event_id(71), input.clone(), NOW_MS + 71)
        .expect("duplicate input is idempotent");
    assert_eq!(duplicate_input.fact_seq, first_input.fact_seq);

    let mut conflicting_input = input;
    conflicting_input.inline_text = Some("different".into());
    assert!(matches!(
        ledger.append_input_accepted(event_id(72), conflicting_input, NOW_MS + 72),
        Err(ToolLedgerError::InputAcceptedConflict { .. })
    ));

    let message = communication();
    let first_communication = ledger
        .append_inter_agent_communication(event_id(73), message.clone(), NOW_MS + 73)
        .expect("append communication");
    let duplicate_communication = ledger
        .append_inter_agent_communication(event_id(74), message.clone(), NOW_MS + 74)
        .expect("duplicate communication is idempotent");
    assert_eq!(
        duplicate_communication.fact_seq,
        first_communication.fact_seq
    );

    let mut conflicting_communication = message;
    conflicting_communication.delivery = InterAgentDelivery::Queue;
    assert!(matches!(
        ledger.append_inter_agent_communication(
            event_id(75),
            conflicting_communication,
            NOW_MS + 75
        ),
        Err(ToolLedgerError::InterAgentCommunicationConflict { .. })
    ));

    drop(ledger);
    let mut reopened = ToolLedger::open(
        temp.path(),
        session_id(),
        log_id(),
        WriterId::new("writer-b"),
        NOW_MS + LEASE_MS + 1,
        LEASE_MS,
    )
    .expect("reopen ledger");
    assert_eq!(
        reopened
            .append_input_accepted(event_id(76), input_accepted("hello"), NOW_MS + 76)
            .expect("rebuilt input index")
            .fact_seq,
        first_input.fact_seq
    );
    assert_eq!(
        reopened
            .append_inter_agent_communication(event_id(77), communication(), NOW_MS + 77)
            .expect("rebuilt communication index")
            .fact_seq,
        first_communication.fact_seq
    );
}

#[test]
fn intent_and_finished_round_trip_after_reopen() {
    let temp = tempfile::tempdir().expect("tempdir");
    let call = call_id(1);
    let execution = execution_id(1);
    let mut ledger = open_ledger(temp.path());

    let intent_fact = ledger
        .append_intent(
            event_id(1),
            None,
            intent(&call, &execution, ToolReplayCapability::NoReplay),
            NOW_MS + 1,
        )
        .expect("append intent");
    assert_eq!(intent_fact.fact_seq, 1);
    assert!(ledger.get(&call).expect("entry").is_open());

    let finished_fact = ledger
        .append_finished(
            event_id(2),
            None,
            finished(
                &call,
                Some(&execution),
                ToolTerminalStatus::Succeeded,
                NOW_MS + 2,
            ),
            NOW_MS + 2,
        )
        .expect("append finished");
    assert_eq!(finished_fact.fact_seq, 2);
    drop(ledger);

    let reopened = ToolLedger::open(
        temp.path(),
        session_id(),
        log_id(),
        WriterId::new("writer-b"),
        NOW_MS + LEASE_MS + 1,
        LEASE_MS,
    )
    .expect("reopen ledger");
    let entry = reopened.get(&call).expect("reopened entry");
    assert_eq!(entry.intent().expect("intent").execution_id, execution);
    assert_eq!(
        entry.finished().expect("finished").terminal_status,
        ToolTerminalStatus::Succeeded
    );
    assert!(!entry.is_open());
}

#[test]
fn failed_finished_requires_stable_snake_case_error_code() {
    // 回归（2026-10-02）：exec/grep 失败曾带 legacy 大写码 `TOOL_ERROR`，
    // canonical 校验拒收导致 tool_finished 永远落盘失败（tool_execution_failed:
    // canonical fact validation failed）。锁死契约：系统实际产出的 snake_case 码
    // 必须全部可追加；非 conforming 码必须被校验拒绝。
    let conforming = [
        "execution",
        "tool_error",
        "partial",
        "cancelled",
        "timeout",
        "not_found",
        "stale_file",
        "audit_quarantined",
        "mcp_tool_error",
        "ledger_blocked",
        "tool_denied",
        "custom_unspecified",
    ];
    for code in conforming {
        let temp = tempfile::tempdir().expect("tempdir");
        let call = call_id(7);
        let execution = execution_id(7);
        let mut ledger = open_ledger(temp.path());
        ledger
            .append_intent(
                event_id(1),
                None,
                intent(&call, &execution, ToolReplayCapability::NoReplay),
                NOW_MS + 1,
            )
            .expect("append intent");
        let mut fact = finished(
            &call,
            Some(&execution),
            ToolTerminalStatus::Failed,
            NOW_MS + 2,
        );
        fact.error.as_mut().expect("error").code = code.to_owned();
        ledger
            .append_finished(event_id(2), None, fact, NOW_MS + 2)
            .unwrap_or_else(|error| panic!("code {code} 必须可追加 canonical fact: {error}"));
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let call = call_id(8);
    let execution = execution_id(8);
    let mut ledger = open_ledger(temp.path());
    ledger
        .append_intent(
            event_id(1),
            None,
            intent(&call, &execution, ToolReplayCapability::NoReplay),
            NOW_MS + 1,
        )
        .expect("append intent");
    let mut fact = finished(
        &call,
        Some(&execution),
        ToolTerminalStatus::Failed,
        NOW_MS + 2,
    );
    fact.error.as_mut().expect("error").code = "TOOL_ERROR".to_owned();
    let appended = ledger.append_finished(event_id(2), None, fact, NOW_MS + 2);
    assert!(
        appended.is_err(),
        "非 conforming 大写码必须被 canonical 校验拒绝"
    );
}

#[test]
fn duplicate_intent_is_idempotent_and_conflict_is_rejected() {
    let temp = tempfile::tempdir().expect("tempdir");
    let call = call_id(2);
    let execution = execution_id(2);
    let payload = intent(&call, &execution, ToolReplayCapability::NoReplay);
    let mut ledger = open_ledger(temp.path());

    let first = ledger
        .append_intent(event_id(3), None, payload.clone(), NOW_MS + 3)
        .expect("first intent");
    let duplicate = ledger
        .append_intent(event_id(4), None, payload, NOW_MS + 4)
        .expect("duplicate intent is idempotent");
    assert_eq!(duplicate.fact_seq, first.fact_seq);

    let conflicting = intent(&call, &execution_id(22), ToolReplayCapability::NoReplay);
    let error = ledger
        .append_intent(event_id(5), None, conflicting, NOW_MS + 5)
        .expect_err("conflicting intent must fail");
    assert!(matches!(error, ToolLedgerError::IntentConflict { .. }));
}

#[test]
fn duplicate_finished_is_idempotent_and_conflict_is_rejected() {
    let temp = tempfile::tempdir().expect("tempdir");
    let call = call_id(3);
    let execution = execution_id(3);
    let payload = finished(
        &call,
        Some(&execution),
        ToolTerminalStatus::Succeeded,
        NOW_MS + 7,
    );
    let mut ledger = open_ledger(temp.path());
    ledger
        .append_intent(
            event_id(6),
            None,
            intent(&call, &execution, ToolReplayCapability::NoReplay),
            NOW_MS + 6,
        )
        .expect("intent");
    let first = ledger
        .append_finished(event_id(7), None, payload.clone(), NOW_MS + 7)
        .expect("first finished");
    let duplicate = ledger
        .append_finished(event_id(8), None, payload, NOW_MS + 8)
        .expect("duplicate finished is idempotent");
    assert_eq!(duplicate.fact_seq, first.fact_seq);

    let conflicting = finished(
        &call,
        Some(&execution),
        ToolTerminalStatus::Failed,
        NOW_MS + 9,
    );
    let error = ledger
        .append_finished(event_id(9), None, conflicting, NOW_MS + 9)
        .expect_err("conflicting terminal must fail");
    assert!(matches!(error, ToolLedgerError::FinishedConflict { .. }));

    let error = ledger
        .append_intent(
            event_id(10),
            None,
            intent(&call, &execution, ToolReplayCapability::NoReplay),
            NOW_MS + 10,
        )
        .expect_err("intent after terminal must fail");
    assert!(matches!(error, ToolLedgerError::IntentAfterFinished { .. }));
}

#[test]
fn execution_mismatch_and_intentless_success_are_rejected() {
    let temp = tempfile::tempdir().expect("tempdir");
    let call = call_id(4);
    let execution = execution_id(4);
    let mut ledger = open_ledger(temp.path());
    ledger
        .append_intent(
            event_id(10),
            None,
            intent(&call, &execution, ToolReplayCapability::NoReplay),
            NOW_MS + 10,
        )
        .expect("intent");

    let mismatch = finished(
        &call,
        Some(&execution_id(44)),
        ToolTerminalStatus::Succeeded,
        NOW_MS + 11,
    );
    let error = ledger
        .append_finished(event_id(11), None, mismatch, NOW_MS + 11)
        .expect_err("execution mismatch must fail");
    assert!(matches!(error, ToolLedgerError::ExecutionMismatch { .. }));

    let no_intent_call = call_id(5);
    let no_intent = finished(
        &no_intent_call,
        None,
        ToolTerminalStatus::Succeeded,
        NOW_MS + 12,
    );
    let error = ledger
        .append_finished(event_id(12), None, no_intent, NOW_MS + 12)
        .expect_err("success without intent must fail");
    assert!(matches!(error, ToolLedgerError::IntentRequired { .. }));
}

#[test]
fn executionless_denied_and_cancelled_are_allowed_without_intent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let denied_call = call_id(6);
    let cancelled_call = call_id(7);
    let mut ledger = open_ledger(temp.path());

    let denied = ledger
        .append_finished(
            event_id(13),
            None,
            finished(&denied_call, None, ToolTerminalStatus::Denied, NOW_MS + 13),
            NOW_MS + 13,
        )
        .expect("denied without intent");
    assert_eq!(denied.fact_seq, 1);
    assert_eq!(
        ledger
            .get(&denied_call)
            .and_then(|entry| entry.finished())
            .map(|finished| finished.terminal_status),
        Some(ToolTerminalStatus::Denied)
    );

    let cancelled = ledger
        .append_finished(
            event_id(14),
            None,
            finished(
                &cancelled_call,
                None,
                ToolTerminalStatus::Cancelled,
                NOW_MS + 14,
            ),
            NOW_MS + 14,
        )
        .expect("cancelled without intent");
    assert_eq!(cancelled.fact_seq, 2);
}

#[test]
fn open_intent_recovery_disposition_is_replay_capability_driven() {
    let temp = tempfile::tempdir().expect("tempdir");
    let no_replay_call = call_id(8);
    let replay_call = call_id(9);
    let reconcile_call = call_id(10);
    let mut ledger = open_ledger(temp.path());

    ledger
        .append_intent(
            event_id(15),
            None,
            intent(
                &no_replay_call,
                &execution_id(8),
                ToolReplayCapability::NoReplay,
            ),
            NOW_MS + 15,
        )
        .expect("no replay intent");
    ledger
        .append_intent(
            event_id(16),
            None,
            intent(
                &replay_call,
                &execution_id(9),
                ToolReplayCapability::IdempotentReplay,
            ),
            NOW_MS + 16,
        )
        .expect("idempotent replay intent");
    ledger
        .append_intent(
            event_id(17),
            None,
            intent(
                &reconcile_call,
                &execution_id(10),
                ToolReplayCapability::Reconcile {
                    probe_ref: content_ref(9),
                },
            ),
            NOW_MS + 17,
        )
        .expect("reconcile intent");

    assert_eq!(ledger.open_intents().len(), 3);
    assert_eq!(
        ledger.recovery_disposition(&no_replay_call),
        Some(ToolRecoveryDisposition::IndeterminateRequired {
            execution_id: execution_id(8),
        })
    );
    assert_eq!(
        ledger.recovery_disposition(&replay_call),
        Some(ToolRecoveryDisposition::ReplayAllowed {
            execution_id: execution_id(9),
        })
    );
    assert_eq!(
        ledger.recovery_disposition(&reconcile_call),
        Some(ToolRecoveryDisposition::ReconcileRequired {
            execution_id: execution_id(10),
            probe_ref: content_ref(9),
        })
    );
    drop(ledger);

    let mut ledger = ToolLedger::open(
        temp.path(),
        session_id(),
        log_id(),
        WriterId::new("writer-b"),
        NOW_MS + LEASE_MS + 1,
        LEASE_MS,
    )
    .expect("reopen ledger");
    assert_eq!(ledger.open_intents().len(), 3);
    assert_eq!(
        ledger.recovery_disposition(&replay_call),
        Some(ToolRecoveryDisposition::ReplayAllowed {
            execution_id: execution_id(9),
        })
    );

    ledger
        .append_finished(
            event_id(18),
            None,
            finished(
                &no_replay_call,
                Some(&execution_id(8)),
                ToolTerminalStatus::Indeterminate,
                NOW_MS + 18,
            ),
            NOW_MS + 18,
        )
        .expect("indeterminate terminal");
    assert_eq!(
        ledger.recovery_disposition(&no_replay_call),
        Some(ToolRecoveryDisposition::Finished {
            terminal_status: ToolTerminalStatus::Indeterminate,
        })
    );
    assert_eq!(ledger.open_intents().len(), 2);
}

#[test]
fn recovery_seal_carries_batch_provenance_and_is_idempotent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let call = call_id(30);
    let execution = execution_id(30);
    let recovery_ref = recovery_ref();
    let mut ledger = open_ledger(temp.path());

    ledger
        .append_intent(
            event_id(30),
            Some(qaqh_session::session_fact_v2::TurnId::new(
                "turn_01J00000000000000000000030",
            )),
            intent(&call, &execution, ToolReplayCapability::NoReplay),
            NOW_MS + 30,
        )
        .expect("append open intent");

    let first = ledger
        .seal_recovery_intent(&call, event_id(31), recovery_ref.clone(), NOW_MS + 31)
        .expect("seal recovery intent");
    assert_eq!(
        first,
        ToolRecoveryDisposition::Finished {
            terminal_status: ToolTerminalStatus::Indeterminate,
        }
    );

    let facts = CommittedFactReader::open(temp.path(), session_id(), log_id())
        .and_then(|reader| reader.read_all())
        .expect("read facts");
    let finished = facts
        .iter()
        .filter_map(|fact| match &fact.payload {
            FactPayload::ToolFinished(finished) => Some(finished),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].recovery_ref.as_ref(), Some(&recovery_ref));
    assert_eq!(
        finished[0].terminal_status,
        ToolTerminalStatus::Indeterminate
    );

    let second = ledger
        .seal_recovery_intent(&call, event_id(32), recovery_ref, NOW_MS + 32)
        .expect("repeat recovery seal");
    assert_eq!(second, first);
    assert_eq!(
        ledger
            .get(&call)
            .and_then(|entry| entry.finished())
            .map(|finished| finished.terminal_status),
        Some(ToolTerminalStatus::Indeterminate)
    );
}

#[test]
fn interaction_request_context_is_used_for_terminal_envelope() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut ledger = open_ledger(temp.path());
    let requested = interaction_requested();

    ledger
        .append_interaction_requested(
            event_id(39),
            Some(call_id(50)),
            requested.clone(),
            NOW_MS + 39,
        )
        .expect("append request");
    let repeated = ledger
        .append_interaction_requested(event_id(40), Some(call_id(50)), requested, NOW_MS + 40)
        .expect("repeat request is idempotent");
    assert_eq!(repeated.fact_seq, 1);

    let resolved = ledger
        .append_interaction_resolved(
            event_id(41),
            None,
            None,
            interaction_resolved(),
            NOW_MS + 41,
        )
        .expect("resolve request");
    assert_eq!(
        resolved.turn_id.as_ref().map(TurnId::as_str),
        Some("turn_01J00000000000000000000050")
    );
    assert_eq!(
        resolved.call_id.as_ref().map(ToolCallId::as_str),
        Some("call_00000000000000000000000050")
    );
}

#[test]
fn interaction_terminal_is_first_answer_wins_and_survives_reopen() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut ledger = open_ledger(temp.path());
    let resolved = interaction_resolved();

    let first = ledger
        .append_interaction_resolved(event_id(40), None, None, resolved.clone(), NOW_MS + 40)
        .expect("append resolution");
    assert_eq!(first.fact_seq, 1);
    assert!(matches!(first.payload, FactPayload::InteractionResolved(_)));

    let repeated = ledger
        .append_interaction_resolved(event_id(41), None, None, resolved, NOW_MS + 41)
        .expect("repeat resolution is idempotent");
    assert_eq!(repeated.fact_seq, 1);

    let conflict = ledger
        .append_interaction_expired(
            event_id(42),
            None,
            None,
            interaction_expired(InteractionExpiryReason::TurnCancelled),
            NOW_MS + 42,
        )
        .expect_err("expiry after resolution must not overwrite first answer");
    assert!(matches!(
        conflict,
        ToolLedgerError::InteractionTerminalConflict { interaction_id: id }
            if id == interaction_id()
    ));

    drop(ledger);
    let ledger = ToolLedger::open(
        temp.path(),
        session_id(),
        log_id(),
        WriterId::new("writer-b"),
        NOW_MS + LEASE_MS + 1,
        LEASE_MS,
    )
    .expect("reopen ledger");
    let terminal = ledger
        .interaction_terminal(&interaction_id())
        .expect("interaction terminal");
    assert_eq!(terminal.fact_seq, 1);
    assert!(matches!(
        terminal.payload,
        FactPayload::InteractionResolved(_)
    ));
}

#[test]
fn recovery_batch_seals_only_non_replayable_intents() {
    let temp = tempfile::tempdir().expect("tempdir");
    let no_replay = call_id(41);
    let replay = call_id(42);
    let reconcile = call_id(43);
    let recovery_ref = recovery_ref();
    let mut ledger = open_ledger(temp.path());

    ledger
        .append_intent(
            event_id(41),
            None,
            intent(
                &no_replay,
                &execution_id(41),
                ToolReplayCapability::NoReplay,
            ),
            NOW_MS + 41,
        )
        .expect("no replay intent");
    ledger
        .append_intent(
            event_id(42),
            None,
            intent(
                &replay,
                &execution_id(42),
                ToolReplayCapability::IdempotentReplay,
            ),
            NOW_MS + 42,
        )
        .expect("replay intent");
    ledger
        .append_intent(
            event_id(43),
            None,
            intent(
                &reconcile,
                &execution_id(43),
                ToolReplayCapability::Reconcile {
                    probe_ref: content_ref(8),
                },
            ),
            NOW_MS + 43,
        )
        .expect("reconcile intent");

    let dispositions = ledger
        .recover_open_intents(recovery_ref.clone(), NOW_MS + 44)
        .expect("recover open intents");
    assert_eq!(dispositions.len(), 3);
    assert_eq!(
        dispositions[0],
        (
            no_replay.clone(),
            ToolRecoveryDisposition::Finished {
                terminal_status: ToolTerminalStatus::Indeterminate,
            },
        )
    );
    assert_eq!(
        dispositions[1],
        (
            replay.clone(),
            ToolRecoveryDisposition::ReplayAllowed {
                execution_id: execution_id(42),
            },
        )
    );
    assert_eq!(
        dispositions[2],
        (
            reconcile.clone(),
            ToolRecoveryDisposition::ReconcileRequired {
                execution_id: execution_id(43),
                probe_ref: content_ref(8),
            },
        )
    );
    assert_eq!(ledger.open_intents().len(), 2);

    let finished = ledger
        .get(&no_replay)
        .and_then(|entry| entry.finished())
        .expect("recovered terminal");
    assert_eq!(finished.recovery_ref.as_ref(), Some(&recovery_ref));
    assert_eq!(finished.terminal_status, ToolTerminalStatus::Indeterminate);
}

#[test]
fn driver_seat_epoch_is_monotonic_and_survives_reopen() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut ledger = open_ledger(temp.path());
    assert_eq!(ledger.driver_state(), (None, 0));

    let claimed = ledger
        .claim_driver("cs-a", None, event_id(30), None, NOW_MS)
        .expect("claim");
    assert_eq!(
        claimed,
        qaqh_session::canonical::DriverClaimOutcome::Claimed { driver_epoch: 1 }
    );
    assert_eq!(ledger.driver_state(), (Some("cs-a".into()), 1));

    // Same holder re-claims: idempotent, no new fact, epoch unchanged.
    assert_eq!(
        ledger
            .claim_driver("cs-a", None, event_id(31), None, NOW_MS)
            .expect("re-claim"),
        qaqh_session::canonical::DriverClaimOutcome::AlreadyHeld { driver_epoch: 1 }
    );

    // Another holder without a stale handover is busy.
    assert_eq!(
        ledger
            .claim_driver("cs-b", None, event_id(32), None, NOW_MS)
            .expect("busy"),
        qaqh_session::canonical::DriverClaimOutcome::Busy {
            holder: "cs-a".into(),
            driver_epoch: 1,
        }
    );

    // Takeover is allowed only when the caller names the stale holder.
    assert_eq!(
        ledger
            .claim_driver("cs-b", Some("cs-a"), event_id(33), None, NOW_MS)
            .expect("takeover"),
        qaqh_session::canonical::DriverClaimOutcome::Claimed { driver_epoch: 2 }
    );

    // Non-holder release is rejected; holder release bumps the epoch.
    assert_eq!(
        ledger
            .release_driver("cs-a", None, event_id(34), None, NOW_MS)
            .expect("not driver"),
        qaqh_session::canonical::DriverReleaseOutcome::NotDriver {
            holder: Some("cs-b".into()),
            driver_epoch: 2,
        }
    );
    // A delayed reclaim carrying the pre-takeover epoch must be a no-op.
    assert_eq!(
        ledger
            .release_driver("cs-b", Some(1), event_id(35), None, NOW_MS)
            .expect("stale release"),
        qaqh_session::canonical::DriverReleaseOutcome::StaleEpoch { driver_epoch: 2 }
    );
    assert_eq!(ledger.driver_state(), (Some("cs-b".into()), 2));
    assert_eq!(
        ledger
            .release_driver("cs-b", Some(2), event_id(36), None, NOW_MS)
            .expect("release"),
        qaqh_session::canonical::DriverReleaseOutcome::Released { driver_epoch: 3 }
    );

    // Reopen rebuilds the seat from canonical facts (epoch does not reset).
    ledger
        .release_writer_lease(NOW_MS)
        .expect("release writer lease");
    drop(ledger);
    let reopened = ToolLedger::open(
        temp.path(),
        session_id(),
        log_id(),
        WriterId::new("writer-b"),
        NOW_MS + 1,
        LEASE_MS,
    )
    .expect("reopen");
    assert_eq!(reopened.driver_state(), (None, 3));
}

#[test]
fn dropping_a_ledger_releases_the_writer_fence() {
    let temp = tempfile::tempdir().expect("tempdir");
    let ledger = open_ledger(temp.path());
    // `open_ledger` stamps the fence with a synthetic `NOW_MS`; without a
    // release on drop the successor below would be `WriterBusy` until the
    // lease TTL elapsed on that same synthetic clock.
    drop(ledger);

    let reopened = ToolLedger::open(
        temp.path(),
        session_id(),
        log_id(),
        WriterId::new("writer-b"),
        NOW_MS,
        LEASE_MS,
    );
    assert!(
        reopened.is_ok(),
        "dropping a ledger must release its writer fence: {:?}",
        reopened.err()
    );
}

#[test]
fn ensure_lease_reacquires_after_idle_expiry() {
    let temp = tempfile::tempdir().expect("tempdir");
    let call = call_id(20);
    let execution = execution_id(20);
    let mut ledger = open_ledger(temp.path());

    ledger
        .ensure_lease(NOW_MS + LEASE_MS + 1, LEASE_MS)
        .expect("expired lease can be reacquired by the same writer");
    ledger
        .append_intent(
            event_id(20),
            None,
            intent(&call, &execution, ToolReplayCapability::NoReplay),
            NOW_MS + LEASE_MS + 2,
        )
        .expect("append after reacquire");
    assert!(ledger.get(&call).expect("entry").is_open());
}

#[test]
fn subagent_spawn_and_finish_edges_are_idempotent_and_rebuildable() {
    let temp = tempfile::tempdir().expect("tempdir");
    let parent_call = call_id(99);
    let child = SessionId::new("0198f1a0-0000-7000-8000-000000000099");
    let spawned = SubagentSpawned {
        child_session_id: child.clone(),
        parent_call_id: parent_call.clone(),
        parent_agent_path: Some(AgentPath::root()),
        child_agent_path: Some(
            AgentPath::parse_absolute("/root/review").expect("child agent path"),
        ),
        role: Some("review".to_string()),
        spawn_config: None,
        spawned_at_ms: NOW_MS,
    };
    let mut ledger = open_ledger(temp.path());
    let first = ledger
        .append_subagent_spawned(event_id(99), spawned.clone(), NOW_MS + 1)
        .expect("append spawn edge");
    assert_eq!(
        ledger
            .append_subagent_spawned(event_id(100), spawned.clone(), NOW_MS + 2)
            .expect("idempotent spawn edge"),
        first
    );

    let finished = SubagentFinished {
        child_session_id: child.clone(),
        parent_call_id: parent_call,
        status: SubagentTerminalStatus::Completed,
        result_ref: None,
        finished_at_ms: NOW_MS + 3,
        recovery_ref: None,
    };
    let terminal = ledger
        .append_subagent_finished(event_id(101), finished.clone(), NOW_MS + 3)
        .expect("append finish edge");
    assert_eq!(
        ledger
            .append_subagent_finished(event_id(102), finished.clone(), NOW_MS + 4)
            .expect("idempotent finish edge"),
        terminal
    );

    // Reopen from committed facts and replay both facts. The ledger index must
    // recover the edge instead of treating them as orphan facts.
    ledger
        .release_writer_lease(NOW_MS + 5)
        .expect("release writer");
    drop(ledger);
    let mut reopened = ToolLedger::open(
        temp.path(),
        session_id(),
        log_id(),
        WriterId::new("writer-b"),
        NOW_MS + 6,
        LEASE_MS,
    )
    .expect("reopen ledger");
    assert_eq!(
        reopened
            .append_subagent_spawned(event_id(103), spawned, NOW_MS + 7)
            .expect("replay spawn after reopen")
            .fact_seq,
        1
    );
    assert_eq!(
        reopened
            .append_subagent_finished(event_id(104), finished, NOW_MS + 8)
            .expect("replay finish after reopen")
            .fact_seq,
        2
    );
}

#[test]
fn compaction_applied_fact_records_the_canonical_head_as_the_replace_boundary() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut ledger = open_ledger(temp.path());

    // 压缩只发生在有历史之后：先落一条 InputAccepted，head ≥ 1 才满足
    // `replaces_through_fact_seq` 的非零校验（生产侧由 SessionCreated 保证）。
    let input = ledger
        .append_input_accepted(event_id(80), input_accepted("history"), NOW_MS + 80)
        .expect("append input");

    let applied = ledger
        .append_compaction_applied(
            event_id(81),
            qaqh_session::session_fact_v2::CheckpointId::new(
                "ckpt_01J00000000000000000000001".to_string(),
            ),
            content_ref(7),
            42,
            NOW_MS + 81,
        )
        .expect("append compaction fact");
    assert_eq!(applied.fact_seq, input.fact_seq + 1);
    match &applied.payload {
        FactPayload::CompactionApplied(payload) => {
            assert_eq!(
                payload.checkpoint_id.as_str(),
                "ckpt_01J00000000000000000000001"
            );
            assert_eq!(payload.replaces_through_fact_seq, input.fact_seq);
            assert_eq!(payload.context_revision, 42);
            assert_eq!(payload.summary_ref, content_ref(7));
            assert_eq!(payload.applied_at_ms, NOW_MS + 81);
        }
        other => panic!("expected CompactionApplied payload, got {other:?}"),
    }

    // 落盘可读回：CommittedFactReader 能重放该 fact（durable-before-publish）。
    let committed = CommittedFactReader::open(temp.path(), session_id(), log_id())
        .expect("open reader")
        .read_all()
        .expect("read facts");
    assert_eq!(committed.len(), 2);
    assert!(matches!(
        &committed[1].payload,
        FactPayload::CompactionApplied(_)
    ));
}

#[test]
fn compaction_applied_on_an_empty_log_is_rejected_by_validation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut ledger = open_ledger(temp.path());
    let result = ledger.append_compaction_applied(
        event_id(82),
        qaqh_session::session_fact_v2::CheckpointId::new(
            "ckpt_01J00000000000000000000002".to_string(),
        ),
        content_ref(8),
        1,
        NOW_MS + 82,
    );
    assert!(
        matches!(&result, Err(ToolLedgerError::Canonical(_))),
        "empty log has no replace boundary; validation must reject, got {result:?}"
    );
}

/// 2026-10-05 回归（`docs/bug-ringing-v2-commands-stuck-in-running.md`）：命令回执
/// 的终态折叠只认 canonical fact 上的 `causation_id`，而 writer 侧曾把它硬编码成
/// `None`——v1 事件链退役后，除交互应答外没有任何命令还能落到终态。
/// in-flight 命令 id 必须落到该次 dispatch 写出的每条 fact 上，出作用域后不再归因。
#[test]
fn in_flight_command_causation_lands_on_appended_facts() {
    let temp = tempfile::tempdir().expect("tempdir");
    let uuid_command = "92455601-b53a-4f25-8df5-94124676055b";
    let scope = FactCausation::new();
    scope.set(Some(uuid_command.to_string()));
    let mut ledger = open_ledger(temp.path());
    ledger.bind_causation(scope.clone());

    let call = call_id(40);
    let execution = execution_id(40);
    let intent_fact = ledger
        .append_intent(
            event_id(40),
            None,
            intent(&call, &execution, ToolReplayCapability::NoReplay),
            NOW_MS + 1,
        )
        .expect("append intent");
    let finished_fact = ledger
        .append_finished(
            event_id(41),
            None,
            finished(
                &call,
                Some(&execution),
                ToolTerminalStatus::Succeeded,
                NOW_MS + 2,
            ),
            NOW_MS + 2,
        )
        .expect("append finished");

    let encoded = ulid_from_text(uuid_command);
    for fact in [&intent_fact, &finished_fact] {
        assert_eq!(
            fact.causation_id.as_ref().map(|id| id.as_str()),
            Some(encoded.as_str()),
            "facts written while a command is in flight must name it"
        );
        fact.validate()
            .expect("a client UUID command id must land on the ULID causation lane");
    }

    scope.set(None);
    let idle_call = call_id(41);
    let idle_execution = execution_id(41);
    ledger
        .append_intent(
            event_id(42),
            None,
            intent(&idle_call, &idle_execution, ToolReplayCapability::NoReplay),
            NOW_MS + 3,
        )
        .expect("append idle intent");
    let idle_fact = ledger
        .append_finished(
            event_id(43),
            None,
            finished(
                &idle_call,
                Some(&idle_execution),
                ToolTerminalStatus::Succeeded,
                NOW_MS + 4,
            ),
            NOW_MS + 4,
        )
        .expect("append idle finished");
    assert_eq!(
        idle_fact.causation_id, None,
        "no command is in flight outside the dispatch scope"
    );
}

/// 命令 id 是客户端自由值（桌面提交 ULID、移动端提交 UUID），canonical 侧只有
/// 一条 ULID 因果通道：原生 ULID 原样透传，其余一律 `ulid_from_text` 派生，
/// 客户端选择的字符串永不原样落盘。
#[test]
fn causation_for_command_normalises_client_ids_onto_the_ulid_lane() {
    let native = generate_ulid();
    assert_eq!(
        causation_for_command(&native).map(|id| id.0),
        Some(native.clone()),
        "a ULID command id keeps its own form (matches the driver_changed facts already on disk)"
    );
    let uuid = "92455601-b53a-4f25-8df5-94124676055b";
    assert_eq!(
        causation_for_command(uuid).map(|id| id.0),
        Some(ulid_from_text(uuid)),
        "a UUID command id derives deterministically"
    );
    assert_eq!(
        causation_for_command(&"x".repeat(4096)).map(|id| id.0),
        Some(ulid_from_text(&"x".repeat(4096))),
        "an oversized client id still fits the 26-char lane"
    );
    assert_eq!(causation_for_command(""), None);
}

#[test]
fn workspace_resource_fact_persists_summary_and_revisions_survive_reopen() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut ledger = open_ledger(temp.path());
    let resource_id = stable_workspace_resource_id(session_id().as_str(), ResourceKind::Todo);

    let first = ledger
        .append_workspace_resource_changed(
            event_id(901),
            None,
            Some(call_id(901)),
            ResourceKind::Todo,
            resource_id.clone(),
            b"todo-v1",
            Some(call_id(901)),
            false,
            NOW_MS + 1,
        )
        .expect("append first resource fact");
    let FactPayload::WorkspaceResourceChanged(first_payload) = &first.payload else {
        panic!("expected WorkspaceResourceChanged payload");
    };
    assert_eq!(first_payload.revision, 1);
    assert_eq!(first_payload.resource_id, resource_id);
    assert_eq!(
        ledger.get_blob(&first_payload.summary_ref).expect("blob"),
        b"todo-v1"
    );
    first.validate().expect("first fact validates");

    let second = ledger
        .append_workspace_resource_changed(
            event_id(902),
            None,
            None,
            ResourceKind::Todo,
            resource_id.clone(),
            b"todo-v2",
            None,
            false,
            NOW_MS + 2,
        )
        .expect("append second resource fact");
    let FactPayload::WorkspaceResourceChanged(second_payload) = &second.payload else {
        panic!("expected WorkspaceResourceChanged payload");
    };
    assert_eq!(second_payload.revision, 2);

    // Reopen: revision continuity is rebuilt from committed facts and the
    // appended summary stays resolvable (I5).
    drop(ledger);
    let mut reopened = ToolLedger::open(
        temp.path(),
        session_id(),
        log_id(),
        WriterId::new("writer-b"),
        NOW_MS + LEASE_MS + 1,
        LEASE_MS,
    )
    .expect("reopen ledger");
    let third = reopened
        .append_workspace_resource_changed(
            event_id(903),
            None,
            None,
            ResourceKind::Todo,
            resource_id,
            b"todo-v3",
            None,
            false,
            NOW_MS + LEASE_MS + 2,
        )
        .expect("append after reopen");
    let FactPayload::WorkspaceResourceChanged(third_payload) = &third.payload else {
        panic!("expected WorkspaceResourceChanged payload");
    };
    assert_eq!(third_payload.revision, 3);
    assert_eq!(
        reopened.get_blob(&third_payload.summary_ref).expect("blob"),
        b"todo-v3"
    );
    third.validate().expect("third fact validates");
}
