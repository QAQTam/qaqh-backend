//! Recovery batch execution over the canonical tool ledger.
//!
//! The intent file is the durable plan. This executor seals every
//! non-replayable intent with the batch `RecoveryRef`, and only writes the
//! final `SessionRecovered` fact after no open tool intent remains. Replay and
//! reconciliation dispositions are returned to the caller for their dedicated
//! steps; the intent stays active until those steps close the calls.
//!
//! The plan is a snapshot, so a later crash can leave an open intent it does
//! not cover. Such a call is sealed when it is non-replayable (there is no
//! replay step to plan for it) and fails closed when it needs a replay or
//! reconcile step — the fail-closed check is `validate_open_set`, and sealing
//! is done by `ToolLedger::recover_open_intents`.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use thiserror::Error;

use crate::session_fact_v2::{
    ContentHash, EventId, FactPayload, LogId, RecoveryAction, RecoveryId, RecoveryOutcome,
    RecoveryToolCompletion, SessionFact, SessionId, SessionRecovered, ToolCallId, ToolFinished,
    ToolReplayCapability,
};

use super::{
    CanonicalError, CanonicalSessionStore, CommittedFactReader, RecoveryIntent,
    RecoveryIntentStatus, ToolLedger, ToolLedgerError, ToolRecoveryDisposition, WriterId,
    generate_ulid, load_recovery_intent, remove_recovery_intent_if_stale, sha256_content_hash,
};

#[derive(Debug, Error)]
pub enum RecoveryExecutionError {
    #[error(transparent)]
    Canonical(#[from] CanonicalError),

    #[error(transparent)]
    ToolLedger(#[from] ToolLedgerError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryExecution {
    pub fact: SessionFact,
    pub actions: Vec<RecoveryAction>,
    pub intent_removed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryExecutionOutcome {
    Recovered(Box<RecoveryExecution>),
    Pending {
        dispositions: Vec<(ToolCallId, ToolRecoveryDisposition)>,
    },
    NoIntent,
}

/// Build a deterministic recovery intent from the current committed log.
///
/// The plan is based only on durable facts. An empty open set returns `None`
/// and must not create an intent file. `child_terminal_digest` is supplied by
/// the caller because subagent terminal inputs live outside this tool-ledger
/// slice; the tool plan hash is derived from the sorted open call IDs.
pub fn plan_recovery_intent(
    session_dir: impl AsRef<Path>,
    session_id: SessionId,
    log_id: LogId,
    recovery_id: RecoveryId,
    recovery_event_id: EventId,
    child_terminal_digest: ContentHash,
) -> Result<Option<RecoveryIntent>, CanonicalError> {
    let session_dir = session_dir.as_ref();
    let reader = CommittedFactReader::open(session_dir, session_id, log_id.clone())?;
    let facts = reader.read_all()?;
    let mut open_ids = open_tool_call_ids(&facts);
    if open_ids.is_empty() {
        return Ok(None);
    }
    open_ids.sort();
    open_ids.dedup();

    let events = match fs::read(reader.events_path()) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    let committed_offset = reader.committed().committed_offset;
    let torn_tail_bytes_hash = if (events.len() as u64) > committed_offset {
        sha256_content_hash(&events[committed_offset as usize..])
    } else {
        sha256_content_hash(b"")
    };
    let plan_bytes = serde_json::to_vec(&open_ids)?;
    let plan = RecoveryIntent::new(
        recovery_id,
        recovery_event_id,
        log_id,
        reader.committed().committed_fact_seq,
        open_ids,
        torn_tail_bytes_hash,
        child_terminal_digest,
        sha256_content_hash(&plan_bytes),
    )?;
    Ok(Some(plan))
}

/// Execute the durable recovery intent for one canonical session.
///
/// `NoIntent` means the batch was already finalized and its stale intent was
/// removed (or no recovery was pending). `Pending` means at least one
/// `IdempotentReplay` or `Reconcile` call still needs its dedicated recovery
/// step; non-replayable calls may already have been sealed and the operation
/// is safe to retry.
#[allow(clippy::too_many_arguments)]
pub fn execute_recovery_intent(
    session_dir: impl AsRef<Path>,
    session_id: SessionId,
    log_id: LogId,
    writer_id: WriterId,
    now_ms: i64,
    lease_duration_ms: i64,
) -> Result<RecoveryExecutionOutcome, RecoveryExecutionError> {
    let session_dir = session_dir.as_ref().to_path_buf();
    let Some(intent) = load_recovery_intent(&session_dir)? else {
        return Ok(RecoveryExecutionOutcome::NoIntent);
    };
    if intent.log_id != log_id {
        return Err(CanonicalError::RecoveryIntentConflict(
            "recovery intent log_id does not match the canonical log".into(),
        )
        .into());
    }

    let committed =
        CommittedFactReader::open(&session_dir, session_id.clone(), log_id.clone())?.read_all()?;
    if intent.status(&committed)? == RecoveryIntentStatus::Stale {
        let fact = recovered_fact(&committed, &intent).ok_or_else(|| {
            CanonicalError::RecoveryIntentConflict(
                "stale recovery intent has no matching SessionRecovered fact".into(),
            )
        })?;
        // A closed batch must not swallow an open intent that appeared *after* it
        // was closed: spec requires a new `RecoveryRef`/fingerprint for new open
        // state. Fail closed and keep the intent file so the next load can
        // re-plan a batch for the current open set.
        let uncovered: Vec<String> = open_tool_call_ids(&committed)
            .into_iter()
            .filter(|call_id| !intent.sorted_open_ids.contains(call_id))
            .collect();
        if !uncovered.is_empty() {
            return Err(CanonicalError::RecoveryIntentConflict(format!(
                "closed recovery batch does not cover open tool intent(s) {}; the current open set needs a new recovery batch",
                uncovered.join(", ")
            ))
            .into());
        }
        let actions = recovered_actions(&fact);
        let intent_removed = remove_recovery_intent_if_stale(&session_dir, &intent, &committed)?;
        return Ok(RecoveryExecutionOutcome::Recovered(Box::new(
            RecoveryExecution {
                fact,
                actions,
                intent_removed,
            },
        )));
    }

    let mut store = CanonicalSessionStore::open(&session_dir, session_id.clone(), log_id.clone())?;
    let lease = store.acquire_writer(writer_id, now_ms, lease_duration_ms)?;
    let mut ledger = ToolLedger::from_store(&session_dir, store, lease)?;

    validate_open_set(&ledger, &intent)?;
    let dispositions = ledger.recover_open_intents(intent.recovery_ref.clone(), now_ms)?;
    if dispositions
        .iter()
        .any(|(_, disposition)| !matches!(disposition, ToolRecoveryDisposition::Finished { .. }))
    {
        // 恢复只借用 fence：不释放的话，本会话自己的 ledger（另一个 writer id）
        // 会在 lease 到期前一直拿到 `WriterBusy`，整批工具被拒。
        ledger.release_writer_lease(now_ms)?;
        return Ok(RecoveryExecutionOutcome::Pending { dispositions });
    }

    let actions = recovery_actions(&ledger, &intent)?;
    let recovered = SessionRecovered {
        recovery_id: intent.recovery_ref.recovery_id.clone(),
        recovery_event_id: intent.recovery_ref.recovery_event_id.clone(),
        recovery_input_fingerprint: intent.recovery_ref.recovery_input_fingerprint.clone(),
        outcome: RecoveryOutcome::Writable,
        last_good_fact_seq: intent.last_good_fact_seq,
        torn_tail: intent.torn_tail_bytes_hash != sha256_content_hash(b""),
        torn_bytes: None,
        actions: actions.clone(),
        recovered_at_ms: now_ms,
    };
    let fact = ledger.append_session_recovered(EventId::new(generate_ulid()), recovered, now_ms)?;
    let committed_after =
        CommittedFactReader::open(&session_dir, session_id, log_id)?.read_all()?;
    let intent_removed = remove_recovery_intent_if_stale(&session_dir, &intent, &committed_after)?;
    ledger.release_writer_lease(now_ms)?;
    Ok(RecoveryExecutionOutcome::Recovered(Box::new(
        RecoveryExecution {
            fact,
            actions,
            intent_removed,
        },
    )))
}

/// Validate that the durable plan can still cover every open intent.
///
/// The intent file is a snapshot of the open set taken when the plan was
/// written, so a later crash can leave a *new* open intent behind — for example
/// a `write` interrupted while an earlier `read` is still waiting on its replay
/// step. Re-planning is not available for an active intent (the batch key must
/// stay stable), so the difference is reconciled by capability instead of
/// failing forever:
///
/// - a non-replayable call has no replay step to plan for, so
///   [`ToolLedger::recover_open_intents`] seals it as `Indeterminate` under the
///   batch `RecoveryRef` — exactly the outcome the batch would have produced had
///   the call been in the plan. That holds whether or not the call is in
///   `sorted_open_ids`, so planned-but-still-open `NoReplay` calls are covered
///   too;
/// - a replay/reconcile call *does* need a planned step, so an unplanned one
///   still fails closed — silently sealing it would drop its replay/reconcile
///   contract.
///
/// This runs before anything is sealed, so a batch that cannot be executed
/// leaves no partially sealed terminal behind.
fn validate_open_set(ledger: &ToolLedger, intent: &RecoveryIntent) -> Result<(), CanonicalError> {
    let planned: HashSet<&str> = intent.sorted_open_ids.iter().map(String::as_str).collect();
    for entry in ledger.open_intents() {
        let Some(open) = entry.intent() else {
            continue;
        };
        if planned.contains(open.call_id.as_str()) {
            continue;
        }
        match open.replay_capability {
            ToolReplayCapability::NoReplay => {}
            ToolReplayCapability::IdempotentReplay | ToolReplayCapability::Reconcile { .. } => {
                return Err(CanonicalError::RecoveryIntentConflict(format!(
                    "open tool intent {} requires its own recovery step but is not covered by the recovery plan",
                    open.call_id
                )));
            }
        }
    }
    Ok(())
}

/// Summarise every terminal the batch wrote.
///
/// The plan's calls come first (sorted, as planned); terminals that carry the
/// batch `RecoveryRef` without being in the plan follow in fact order. Those
/// are the unplanned non-replayable calls the executor sealed, including ones
/// sealed by an earlier run of the same batch.
fn recovery_actions(
    ledger: &ToolLedger,
    intent: &RecoveryIntent,
) -> Result<Vec<RecoveryAction>, CanonicalError> {
    let mut call_ids: Vec<&str> = intent.sorted_open_ids.iter().map(String::as_str).collect();
    for fact in ledger.finished_facts() {
        if finished_recovery_ref(fact) != Some(&intent.recovery_ref) {
            continue;
        }
        let Some(call_id) = fact.call_id.as_ref().map(ToolCallId::as_str) else {
            continue;
        };
        if !call_ids.contains(&call_id) {
            call_ids.push(call_id);
        }
    }

    let mut actions = Vec::with_capacity(call_ids.len());
    for call_id in call_ids {
        let fact = ledger
            .finished_facts()
            .into_iter()
            .find(|fact| {
                fact.call_id
                    .as_ref()
                    .is_some_and(|candidate| candidate.as_str() == call_id)
                    && finished_recovery_ref(fact) == Some(&intent.recovery_ref)
            })
            .ok_or_else(|| {
                CanonicalError::RecoveryIntentConflict(format!(
                    "recovery plan call {call_id} has no matching terminal fact"
                ))
            })?;
        actions.push(RecoveryAction::ToolFinished {
            completion: completion_from_fact(fact)?,
        });
    }
    Ok(actions)
}

fn completion_from_fact(fact: &SessionFact) -> Result<RecoveryToolCompletion, CanonicalError> {
    let Some(completion) = tool_finished(fact) else {
        return Err(CanonicalError::RecoveryIntentConflict(
            "recovery action fact is not a ToolFinished".into(),
        ));
    };
    let recovery_ref = completion.recovery_ref.clone().ok_or_else(|| {
        CanonicalError::RecoveryIntentConflict(
            "recovery ToolFinished is missing recovery_ref".into(),
        )
    })?;
    Ok(RecoveryToolCompletion {
        call_id: completion.call_id.clone(),
        execution_id: completion.execution_id.clone(),
        terminal_status: completion.terminal_status,
        output_ref: completion.output_ref.clone(),
        error: completion.error.clone(),
        metrics: completion.metrics.clone(),
        reconciled: completion.reconciled,
        recovery_ref,
        finished_at_ms: completion.finished_at_ms,
        evidence_ref: completion.evidence_ref.clone(),
        evidence_fact_seq: completion.evidence_fact_seq,
        evidence_event_id: completion.evidence_event_id.clone(),
    })
}

fn recovered_fact(facts: &[SessionFact], intent: &RecoveryIntent) -> Option<SessionFact> {
    facts
        .iter()
        .rev()
        .find(|fact| {
            matches!(
                &fact.payload,
                FactPayload::SessionRecovered(recovered)
                    if recovered.recovery_event_id == intent.recovery_ref.recovery_event_id
            )
        })
        .cloned()
}

fn recovered_actions(fact: &SessionFact) -> Vec<RecoveryAction> {
    match &fact.payload {
        FactPayload::SessionRecovered(recovered) => recovered.actions.clone(),
        _ => Vec::new(),
    }
}

fn open_tool_call_ids(facts: &[SessionFact]) -> Vec<String> {
    let mut intents = HashSet::new();
    let mut finished = HashSet::new();
    for fact in facts {
        match &fact.payload {
            FactPayload::ToolIntent(intent) => {
                intents.insert(intent.call_id.as_str().to_owned());
            }
            FactPayload::ToolFinished(finished_fact) => {
                finished.insert(finished_fact.call_id.as_str().to_owned());
            }
            _ => {}
        }
    }
    intents
        .into_iter()
        .filter(|call_id| !finished.contains(call_id))
        .collect()
}

fn finished_recovery_ref(fact: &SessionFact) -> Option<&crate::session_fact_v2::RecoveryRef> {
    tool_finished(fact).and_then(|finished| finished.recovery_ref.as_ref())
}

fn tool_finished(fact: &SessionFact) -> Option<&ToolFinished> {
    match &fact.payload {
        FactPayload::ToolFinished(finished) => Some(finished),
        _ => None,
    }
}
