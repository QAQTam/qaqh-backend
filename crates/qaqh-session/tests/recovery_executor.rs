//! Recovery executor contract over the durable ToolLedger.

use qaqh_session::canonical::{
    CommittedFactReader, RecoveryExecutionOutcome, RecoveryIntent, ToolLedger, ToolLedgerError,
    ToolReconciliationEvidence, WriterId, execute_recovery_intent, load_recovery_intent,
    persist_recovery_intent, sha256_content_hash,
};
use qaqh_session::session_fact_v2::{
    ContentHash, ContentRef, EventId, ExecutionId, FactPayload, LogId, PolicyDecisionRef,
    RecoveryId, SessionId, SideEffectClass, ToolCallId, ToolFinished, ToolIntent,
    ToolIntentPolicyOutcome, ToolMetrics, ToolReplayCapability, ToolTerminalStatus,
};

const NOW_MS: i64 = 1_789_830_000_000;
const LEASE_MS: i64 = 10_000;
const RECOVERY_ID: &str = "recovery_01J00000000000000000000001";
const RECOVERY_EVENT_ID: &str = "01J00000000000000000000099";

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

fn intent(
    call: &ToolCallId,
    execution: &ExecutionId,
    replay_capability: ToolReplayCapability,
) -> ToolIntent {
    ToolIntent {
        call_id: call.clone(),
        execution_id: execution.clone(),
        idempotency_key: matches!(replay_capability, ToolReplayCapability::IdempotentReplay)
            .then(|| format!("idem:{}", call.as_str())),
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

fn open_ledger(dir: &std::path::Path, writer: &str, now_ms: i64) -> ToolLedger {
    ToolLedger::open(
        dir,
        session_id(),
        log_id(),
        WriterId::new(writer),
        now_ms,
        LEASE_MS,
    )
    .expect("open tool ledger")
}

fn persist_plan(dir: &std::path::Path, open_ids: Vec<String>) -> RecoveryIntent {
    let facts = CommittedFactReader::open(dir, session_id(), log_id())
        .expect("open committed reader")
        .read_all()
        .expect("read committed facts");
    let last_good_fact_seq = facts.last().map(|fact| fact.fact_seq).unwrap_or(0);
    let plan = RecoveryIntent::new(
        RecoveryId::new(RECOVERY_ID),
        EventId::new(RECOVERY_EVENT_ID),
        log_id(),
        last_good_fact_seq,
        open_ids,
        sha256_content_hash(b""),
        sha256_content_hash(b"[]"),
        sha256_content_hash(b"plan"),
    )
    .expect("build recovery intent");
    persist_recovery_intent(dir, &plan, &facts).expect("persist recovery intent");
    plan
}

fn count_payload(
    facts: &[qaqh_session::session_fact_v2::SessionFact],
    predicate: impl Fn(&FactPayload) -> bool,
) -> usize {
    facts.iter().filter(|fact| predicate(&fact.payload)).count()
}

#[test]
fn non_replayable_open_intents_are_sealed_then_session_recovered() {
    let temp = tempfile::tempdir().expect("tempdir");
    let call_a = call_id(1);
    let call_b = call_id(2);
    let execution_a = execution_id(1);
    let execution_b = execution_id(2);

    let mut ledger = open_ledger(temp.path(), "writer-a", NOW_MS);
    ledger
        .append_intent(
            event_id(1),
            None,
            intent(&call_a, &execution_a, ToolReplayCapability::NoReplay),
            NOW_MS + 1,
        )
        .expect("append first intent");
    ledger
        .append_intent(
            event_id(2),
            None,
            intent(&call_b, &execution_b, ToolReplayCapability::NoReplay),
            NOW_MS + 2,
        )
        .expect("append second intent");
    drop(ledger);

    let plan = persist_plan(
        temp.path(),
        vec![call_a.as_str().to_owned(), call_b.as_str().to_owned()],
    );
    let outcome = execute_recovery_intent(
        temp.path(),
        session_id(),
        log_id(),
        WriterId::new("writer-b"),
        NOW_MS + LEASE_MS + 1,
        LEASE_MS,
    )
    .expect("execute recovery");
    let RecoveryExecutionOutcome::Recovered(recovered) = outcome else {
        panic!("expected recovered outcome");
    };
    assert!(recovered.intent_removed);
    assert_eq!(recovered.actions.len(), 2);
    assert!(
        load_recovery_intent(temp.path())
            .expect("load intent")
            .is_none()
    );

    let facts = CommittedFactReader::open(temp.path(), session_id(), log_id())
        .expect("open reader")
        .read_all()
        .expect("read facts");
    assert_eq!(
        count_payload(&facts, |payload| matches!(
            payload,
            FactPayload::ToolFinished(_)
        )),
        2
    );
    assert_eq!(
        count_payload(&facts, |payload| matches!(
            payload,
            FactPayload::SessionRecovered(_)
        )),
        1
    );
    for fact in &facts {
        if let FactPayload::ToolFinished(finished) = &fact.payload {
            assert_eq!(finished.recovery_ref.as_ref(), Some(&plan.recovery_ref));
            assert_eq!(finished.terminal_status, ToolTerminalStatus::Indeterminate);
        }
    }

    let second = execute_recovery_intent(
        temp.path(),
        session_id(),
        log_id(),
        WriterId::new("writer-c"),
        NOW_MS + 2 * LEASE_MS + 2,
        LEASE_MS,
    )
    .expect("second recovery");
    assert_eq!(second, RecoveryExecutionOutcome::NoIntent);
    let facts = CommittedFactReader::open(temp.path(), session_id(), log_id())
        .expect("open reader")
        .read_all()
        .expect("read facts");
    assert_eq!(
        count_payload(&facts, |payload| matches!(
            payload,
            FactPayload::SessionRecovered(_)
        )),
        1
    );
}

#[test]
fn replay_and_reconcile_keep_the_intent_pending_until_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let replay_call = call_id(1);
    let reconcile_call = call_id(2);
    let replay_execution = execution_id(1);
    let reconcile_execution = execution_id(2);
    let probe_ref = content_ref(7);

    let mut ledger = open_ledger(temp.path(), "writer-a", NOW_MS);
    ledger
        .append_intent(
            event_id(1),
            None,
            intent(
                &replay_call,
                &replay_execution,
                ToolReplayCapability::IdempotentReplay,
            ),
            NOW_MS + 1,
        )
        .expect("append replay intent");
    ledger
        .append_intent(
            event_id(2),
            None,
            intent(
                &reconcile_call,
                &reconcile_execution,
                ToolReplayCapability::Reconcile {
                    probe_ref: probe_ref.clone(),
                },
            ),
            NOW_MS + 2,
        )
        .expect("append reconcile intent");
    drop(ledger);

    let plan = persist_plan(
        temp.path(),
        vec![
            replay_call.as_str().to_owned(),
            reconcile_call.as_str().to_owned(),
        ],
    );
    let outcome = execute_recovery_intent(
        temp.path(),
        session_id(),
        log_id(),
        WriterId::new("writer-b"),
        NOW_MS + LEASE_MS + 1,
        LEASE_MS,
    )
    .expect("execute recovery");
    let RecoveryExecutionOutcome::Pending { dispositions } = outcome else {
        panic!("expected pending outcome");
    };
    assert_eq!(dispositions.len(), 2);
    assert!(
        load_recovery_intent(temp.path())
            .expect("load intent")
            .is_some()
    );

    let mut ledger = open_ledger(temp.path(), "writer-c", NOW_MS + 2 * LEASE_MS + 1);
    ledger
        .append_reconciled_finished(
            event_id(3),
            reconcile_call.clone(),
            probe_ref,
            plan.recovery_ref.clone(),
            ToolTerminalStatus::Succeeded,
            None,
            None,
            ToolReconciliationEvidence::Fact {
                fact_seq: 7,
                event_id: event_id(77),
            },
            NOW_MS + 2 * LEASE_MS + 2,
        )
        .expect("append reconciled terminal");
    drop(ledger);

    let outcome = execute_recovery_intent(
        temp.path(),
        session_id(),
        log_id(),
        WriterId::new("writer-d"),
        NOW_MS + 3 * LEASE_MS + 3,
        LEASE_MS,
    )
    .expect("execute after reconcile");
    let RecoveryExecutionOutcome::Pending { dispositions } = outcome else {
        panic!("expected replay to remain pending");
    };
    assert_eq!(dispositions.len(), 1);
    assert!(matches!(
        dispositions[0].1,
        qaqh_session::canonical::ToolRecoveryDisposition::ReplayAllowed { .. }
    ));

    let mut ledger = open_ledger(temp.path(), "writer-e", NOW_MS + 4 * LEASE_MS + 4);
    let replay_finished = ToolFinished {
        call_id: replay_call.clone(),
        execution_id: Some(replay_execution),
        terminal_status: ToolTerminalStatus::Succeeded,
        output_ref: None,
        error: None,
        metrics: ToolMetrics {
            started_at_ms: NOW_MS,
            finished_at_ms: NOW_MS + 4 * LEASE_MS + 5,
            retry_count: 0,
            output_bytes: 0,
            progress_bytes_total: 0,
        },
        reconciled: false,
        evidence_ref: None,
        evidence_fact_seq: None,
        evidence_event_id: None,
        recovery_ref: Some(plan.recovery_ref.clone()),
        finished_at_ms: NOW_MS + 4 * LEASE_MS + 5,
    };
    ledger
        .append_finished(
            event_id(4),
            None,
            replay_finished,
            NOW_MS + 4 * LEASE_MS + 5,
        )
        .expect("append replay terminal");
    drop(ledger);

    let outcome = execute_recovery_intent(
        temp.path(),
        session_id(),
        log_id(),
        WriterId::new("writer-f"),
        NOW_MS + 5 * LEASE_MS + 5,
        LEASE_MS,
    )
    .expect("execute final recovery");
    let RecoveryExecutionOutcome::Recovered(recovered) = outcome else {
        panic!("expected recovered outcome");
    };
    assert_eq!(recovered.actions.len(), 2);
    let facts = CommittedFactReader::open(temp.path(), session_id(), log_id())
        .expect("open reader")
        .read_all()
        .expect("read facts");
    let reconciled = facts
        .iter()
        .find_map(|fact| match &fact.payload {
            FactPayload::ToolFinished(finished) if finished.call_id == reconcile_call => {
                Some(finished)
            }
            _ => None,
        })
        .expect("reconciled terminal");
    assert!(reconciled.reconciled);
    assert_eq!(reconciled.evidence_fact_seq, Some(7));
    assert_eq!(reconciled.evidence_event_id, Some(event_id(77)));
}

#[test]
fn reconciliation_probe_mismatch_fails_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let call = call_id(1);
    let execution = execution_id(1);
    let probe_ref = content_ref(7);

    let mut ledger = open_ledger(temp.path(), "writer-a", NOW_MS);
    ledger
        .append_intent(
            event_id(1),
            None,
            intent(
                &call,
                &execution,
                ToolReplayCapability::Reconcile {
                    probe_ref: probe_ref.clone(),
                },
            ),
            NOW_MS + 1,
        )
        .expect("append reconcile intent");
    drop(ledger);
    let plan = persist_plan(temp.path(), vec![call.as_str().to_owned()]);

    let outcome = execute_recovery_intent(
        temp.path(),
        session_id(),
        log_id(),
        WriterId::new("writer-b"),
        NOW_MS + LEASE_MS + 1,
        LEASE_MS,
    )
    .expect("execute recovery");
    assert!(matches!(outcome, RecoveryExecutionOutcome::Pending { .. }));

    let mut ledger = open_ledger(temp.path(), "writer-c", NOW_MS + 2 * LEASE_MS + 1);
    let error = ledger
        .append_reconciled_finished(
            event_id(2),
            call,
            content_ref(8),
            plan.recovery_ref,
            ToolTerminalStatus::Succeeded,
            None,
            None,
            ToolReconciliationEvidence::ContentRef(content_ref(9)),
            NOW_MS + 2 * LEASE_MS + 2,
        )
        .expect_err("probe mismatch must fail closed");
    assert!(matches!(
        error,
        ToolLedgerError::ReconciliationProbeMismatch { .. }
    ));
}

#[test]
fn open_set_outside_the_plan_seals_unplanned_no_replay_intent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let planned = call_id(1);
    let unexpected = call_id(2);
    let execution = execution_id(1);

    let mut ledger = open_ledger(temp.path(), "writer-a", NOW_MS);
    ledger
        .append_intent(
            event_id(1),
            None,
            intent(&planned, &execution, ToolReplayCapability::NoReplay),
            NOW_MS + 1,
        )
        .expect("append planned intent");
    drop(ledger);
    persist_plan(temp.path(), vec![planned.as_str().to_owned()]);

    // Crash window: a second non-replayable intent lands after the plan was
    // written, so the durable plan no longer covers the open set.
    let mut ledger = open_ledger(temp.path(), "writer-b", NOW_MS + LEASE_MS + 1);
    ledger
        .append_intent(
            event_id(2),
            None,
            intent(&unexpected, &execution, ToolReplayCapability::NoReplay),
            NOW_MS + LEASE_MS + 2,
        )
        .expect("append unplanned intent");
    drop(ledger);

    let outcome = execute_recovery_intent(
        temp.path(),
        session_id(),
        log_id(),
        WriterId::new("writer-c"),
        NOW_MS + 2 * LEASE_MS + 2,
        LEASE_MS,
    )
    .expect("unplanned no-replay intent must be sealed, not rejected");
    let RecoveryExecutionOutcome::Recovered(recovered) = outcome else {
        panic!("expected recovered outcome");
    };
    assert_eq!(
        recovered.actions.len(),
        2,
        "the batch summary must list every terminal it sealed"
    );

    let facts = CommittedFactReader::open(temp.path(), session_id(), log_id())
        .expect("open reader")
        .read_all()
        .expect("read facts");
    for call in [&planned, &unexpected] {
        let finished = facts
            .iter()
            .find_map(|fact| match &fact.payload {
                FactPayload::ToolFinished(finished) if finished.call_id == *call => Some(finished),
                _ => None,
            })
            .unwrap_or_else(|| panic!("sealed terminal for {call}"));
        assert_eq!(finished.terminal_status, ToolTerminalStatus::Indeterminate);
    }
    assert!(
        load_recovery_intent(temp.path())
            .expect("load intent")
            .is_none(),
        "the batch must close once every open intent is sealed"
    );
}

#[test]
fn open_set_outside_the_plan_replayable_fails_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let planned = call_id(1);
    let unexpected = call_id(2);
    let execution = execution_id(1);

    let mut ledger = open_ledger(temp.path(), "writer-a", NOW_MS);
    ledger
        .append_intent(
            event_id(1),
            None,
            intent(&planned, &execution, ToolReplayCapability::NoReplay),
            NOW_MS + 1,
        )
        .expect("append planned intent");
    drop(ledger);
    persist_plan(temp.path(), vec![planned.as_str().to_owned()]);

    // A replay/reconcile call needs a planned step, so an unplanned one cannot
    // be sealed silently: the batch must fail closed without writing anything.
    let mut ledger = open_ledger(temp.path(), "writer-b", NOW_MS + LEASE_MS + 1);
    ledger
        .append_intent(
            event_id(2),
            None,
            intent(
                &unexpected,
                &execution,
                ToolReplayCapability::IdempotentReplay,
            ),
            NOW_MS + LEASE_MS + 2,
        )
        .expect("append unplanned replay intent");
    drop(ledger);

    let error = execute_recovery_intent(
        temp.path(),
        session_id(),
        log_id(),
        WriterId::new("writer-c"),
        NOW_MS + 2 * LEASE_MS + 2,
        LEASE_MS,
    )
    .expect_err("unplanned replay intent must fail closed");
    assert!(matches!(
        error,
        qaqh_session::canonical::RecoveryExecutionError::Canonical(
            qaqh_session::canonical::CanonicalError::RecoveryIntentConflict(_)
        )
    ));

    let facts = CommittedFactReader::open(temp.path(), session_id(), log_id())
        .expect("open reader")
        .read_all()
        .expect("read facts");
    assert_eq!(
        count_payload(&facts, |payload| matches!(
            payload,
            FactPayload::ToolFinished(_)
        )),
        0,
        "a failing batch must not leave partially sealed terminals behind"
    );
}

#[test]
fn unplanned_no_replay_intent_after_pending_recovery_does_not_wedge_session() {
    let temp = tempfile::tempdir().expect("tempdir");
    let read_call = call_id(1);
    let read_execution = execution_id(1);
    let write_call = call_id(2);
    let write_execution = execution_id(2);

    let mut ledger = open_ledger(temp.path(), "writer-a", NOW_MS);
    ledger
        .append_intent(
            event_id(1),
            None,
            intent(
                &read_call,
                &read_execution,
                ToolReplayCapability::IdempotentReplay,
            ),
            NOW_MS + 1,
        )
        .expect("append read intent");
    drop(ledger);
    let plan = persist_plan(temp.path(), vec![read_call.as_str().to_owned()]);

    // First restart: the replayable read keeps the batch pending, and the
    // intent file stays active (that part is by design).
    let outcome = execute_recovery_intent(
        temp.path(),
        session_id(),
        log_id(),
        WriterId::new("writer-b"),
        NOW_MS + LEASE_MS + 1,
        LEASE_MS,
    )
    .expect("execute recovery");
    assert!(matches!(outcome, RecoveryExecutionOutcome::Pending { .. }));

    // Crash window: a `write` (non-replayable) is interrupted while the read
    // still waits on its replay step, so a new open intent appears outside the
    // already-persisted plan.
    let mut ledger = open_ledger(temp.path(), "writer-c", NOW_MS + 2 * LEASE_MS + 1);
    ledger
        .append_intent(
            event_id(2),
            None,
            intent(
                &write_call,
                &write_execution,
                ToolReplayCapability::NoReplay,
            ),
            NOW_MS + 2 * LEASE_MS + 2,
        )
        .expect("append write intent");
    drop(ledger);

    // Every restart used to fail with RecoveryIntentConflict, leaving the write
    // intent unsealed forever. It must now be sealed and the batch must stay
    // pending only on the replay step.
    let outcome = execute_recovery_intent(
        temp.path(),
        session_id(),
        log_id(),
        WriterId::new("writer-d"),
        NOW_MS + 3 * LEASE_MS + 3,
        LEASE_MS,
    )
    .expect("a later unplanned no-replay intent must not wedge recovery");
    let RecoveryExecutionOutcome::Pending { dispositions } = outcome else {
        panic!("expected the read to remain pending on its replay step");
    };
    assert_eq!(dispositions.len(), 1);
    assert_eq!(dispositions[0].0, read_call);
    assert!(matches!(
        dispositions[0].1,
        qaqh_session::canonical::ToolRecoveryDisposition::ReplayAllowed { .. }
    ));

    let facts = CommittedFactReader::open(temp.path(), session_id(), log_id())
        .expect("open reader")
        .read_all()
        .expect("read facts");
    let write_finished = facts
        .iter()
        .find_map(|fact| match &fact.payload {
            FactPayload::ToolFinished(finished) if finished.call_id == write_call => Some(finished),
            _ => None,
        })
        .expect("the interrupted write must be sealed as indeterminate");
    assert_eq!(
        write_finished.terminal_status,
        ToolTerminalStatus::Indeterminate
    );
    assert_eq!(
        write_finished.recovery_ref.as_ref(),
        Some(&plan.recovery_ref),
        "the sealed terminal must carry the batch recovery ref"
    );

    // Once the replay step closes the read, the batch must recover and the
    // intent file must be removed.
    let mut ledger = open_ledger(temp.path(), "writer-e", NOW_MS + 4 * LEASE_MS + 4);
    let replay_finished = ToolFinished {
        call_id: read_call.clone(),
        execution_id: Some(read_execution),
        terminal_status: ToolTerminalStatus::Succeeded,
        output_ref: None,
        error: None,
        metrics: ToolMetrics {
            started_at_ms: NOW_MS,
            finished_at_ms: NOW_MS + 4 * LEASE_MS + 5,
            retry_count: 0,
            output_bytes: 0,
            progress_bytes_total: 0,
        },
        reconciled: false,
        evidence_ref: None,
        evidence_fact_seq: None,
        evidence_event_id: None,
        recovery_ref: Some(plan.recovery_ref.clone()),
        finished_at_ms: NOW_MS + 4 * LEASE_MS + 5,
    };
    ledger
        .append_finished(
            event_id(4),
            None,
            replay_finished,
            NOW_MS + 4 * LEASE_MS + 5,
        )
        .expect("append replay terminal");
    drop(ledger);

    let outcome = execute_recovery_intent(
        temp.path(),
        session_id(),
        log_id(),
        WriterId::new("writer-f"),
        NOW_MS + 5 * LEASE_MS + 5,
        LEASE_MS,
    )
    .expect("execute final recovery");
    let RecoveryExecutionOutcome::Recovered(recovered) = outcome else {
        panic!("expected recovered outcome");
    };
    assert_eq!(recovered.actions.len(), 2);
    assert!(recovered.intent_removed);
}

#[test]
fn stale_intent_is_cleaned_without_second_recovered_fact() {
    let temp = tempfile::tempdir().expect("tempdir");
    let call = call_id(1);
    let execution = execution_id(1);

    let mut ledger = open_ledger(temp.path(), "writer-a", NOW_MS);
    ledger
        .append_intent(
            event_id(1),
            None,
            intent(&call, &execution, ToolReplayCapability::NoReplay),
            NOW_MS + 1,
        )
        .expect("append intent");
    drop(ledger);
    let plan = persist_plan(temp.path(), vec![call.as_str().to_owned()]);

    execute_recovery_intent(
        temp.path(),
        session_id(),
        log_id(),
        WriterId::new("writer-b"),
        NOW_MS + LEASE_MS + 1,
        LEASE_MS,
    )
    .expect("first recovery");
    let facts = CommittedFactReader::open(temp.path(), session_id(), log_id())
        .expect("open reader")
        .read_all()
        .expect("read facts");
    persist_recovery_intent(temp.path(), &plan, &facts).expect("recreate stale intent");

    let outcome = execute_recovery_intent(
        temp.path(),
        session_id(),
        log_id(),
        WriterId::new("writer-c"),
        NOW_MS + 2 * LEASE_MS + 2,
        LEASE_MS,
    )
    .expect("stale recovery");
    let RecoveryExecutionOutcome::Recovered(recovered) = outcome else {
        panic!("expected stale recovery to return existing fact");
    };
    assert!(recovered.intent_removed);
    let facts = CommittedFactReader::open(temp.path(), session_id(), log_id())
        .expect("open reader")
        .read_all()
        .expect("read facts");
    assert_eq!(
        count_payload(&facts, |payload| matches!(
            payload,
            FactPayload::SessionRecovered(_)
        )),
        1
    );
}
