//! Durable ToolLedger core contract.

use qaqh_session::canonical::{
    CommittedFactReader, ToolLedger, ToolLedgerError, ToolRecoveryDisposition, WriterId,
};
use qaqh_session::session_fact_v2::{
    ActorKind, ActorRef, ContentHash, ContentRef, EventId, ExecutionId, FactPayload,
    InteractionExpired, InteractionExpiryReason, InteractionId, InteractionResolved, LogId,
    PolicyDecisionRef, RecoveryId, RecoveryRef, SessionId, SideEffectClass, ToolCallId, ToolError,
    ToolFinished, ToolIntent, ToolIntentPolicyOutcome, ToolMetrics, ToolReplayCapability,
    ToolTerminalStatus,
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

fn content_hash(seed: u8) -> ContentHash {
    ContentHash::new(format!("sha256:{seed:064x}"))
}

fn content_ref(seed: u8) -> ContentRef {
    ContentRef::new(content_hash(seed))
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

fn interaction_resolved() -> InteractionResolved {
    InteractionResolved {
        interaction_id: interaction_id(),
        decision_ref: content_ref(4),
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
