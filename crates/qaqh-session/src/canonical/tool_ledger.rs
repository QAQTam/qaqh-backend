//! Durable tool lifecycle ledger over canonical `events.jsonl`.
//!
//! This is the P3-5 core only: it enforces one `ToolIntent` and one
//! `ToolFinished` per `call_id`, rebuilds its index from committed facts, and
//! classifies open intents for recovery. SessionActor/ToolRuntime wiring and
//! cancel/resume CAS remain a later slice.

use std::collections::HashMap;
use std::path::Path;

use thiserror::Error;

use crate::session_fact_v2::{
    ContentRef, EventId, ExecutionId, FactPayload, FactSchema, InteractionExpired, InteractionId,
    InteractionRequested, InteractionResolved, RecoveryRef, SessionFact, SessionId, ToolCallId,
    ToolError, ToolFinished, ToolIntent, ToolMetrics, ToolReplayCapability, ToolTerminalStatus,
    TurnId,
};

use super::{
    CanonicalError, CanonicalIdentityError, CanonicalSessionStore, CommittedFactReader, WriterId,
    WriterLease, generate_ulid,
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolLedgerEntry {
    intent: Option<SessionFact>,
    finished: Option<SessionFact>,
}

impl ToolLedgerEntry {
    pub fn intent(&self) -> Option<&ToolIntent> {
        self.intent.as_ref().and_then(tool_intent_payload)
    }

    pub fn finished(&self) -> Option<&ToolFinished> {
        self.finished.as_ref().and_then(tool_finished_payload)
    }

    pub fn intent_fact(&self) -> Option<&SessionFact> {
        self.intent.as_ref()
    }

    pub fn finished_fact(&self) -> Option<&SessionFact> {
        self.finished.as_ref()
    }

    pub fn is_open(&self) -> bool {
        self.intent.is_some() && self.finished.is_none()
    }
}

/// Recovery disposition for a call that has an intent but no terminal fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolRecoveryDisposition {
    Finished {
        terminal_status: ToolTerminalStatus,
    },
    ReplayAllowed {
        execution_id: ExecutionId,
    },
    ReconcileRequired {
        execution_id: ExecutionId,
        probe_ref: ContentRef,
    },
    IndeterminateRequired {
        execution_id: ExecutionId,
    },
}

#[derive(Debug, Error)]
pub enum ToolLedgerError {
    #[error(transparent)]
    Canonical(#[from] CanonicalError),

    #[error(transparent)]
    Identity(#[from] CanonicalIdentityError),

    #[error("tool call {call_id} already has an intent")]
    DuplicateIntent { call_id: ToolCallId },

    #[error("tool call {call_id} already has a finished terminal")]
    DuplicateFinished { call_id: ToolCallId },

    #[error(
        "tool call {call_id} already has a conflicting intent: existing execution {existing_execution_id}, incoming {incoming_execution_id}"
    )]
    IntentConflict {
        call_id: ToolCallId,
        existing_execution_id: ExecutionId,
        incoming_execution_id: ExecutionId,
    },

    #[error("tool call {call_id} already has a finished terminal {terminal_status:?}")]
    IntentAfterFinished {
        call_id: ToolCallId,
        terminal_status: ToolTerminalStatus,
    },

    #[error("tool call {call_id} already has a conflicting finished terminal")]
    FinishedConflict { call_id: ToolCallId },

    #[error("tool call {call_id} has no durable intent")]
    IntentMissing { call_id: ToolCallId },

    #[error("tool call {call_id} terminal {terminal_status:?} requires an intent")]
    IntentRequired {
        call_id: ToolCallId,
        terminal_status: ToolTerminalStatus,
    },

    #[error(
        "tool call {call_id} execution mismatch: intent {intent_execution_id}, finished {finished_execution_id}"
    )]
    ExecutionMismatch {
        call_id: ToolCallId,
        intent_execution_id: ExecutionId,
        finished_execution_id: ExecutionId,
    },

    #[error("tool call {call_id} has an intent and cannot finish without execution_id")]
    IntentPresentForExecutionlessTerminal { call_id: ToolCallId },

    #[error("interaction {interaction_id} already has a conflicting request")]
    InteractionRequestConflict { interaction_id: InteractionId },

    #[error("interaction {interaction_id} already has a conflicting terminal")]
    InteractionTerminalConflict { interaction_id: InteractionId },
}

#[derive(Debug)]
pub struct ToolLedger {
    store: CanonicalSessionStore,
    lease: WriterLease,
    session_id: SessionId,
    log_id: crate::session_fact_v2::LogId,
    entries: HashMap<ToolCallId, ToolLedgerEntry>,
    interaction_requests: HashMap<InteractionId, SessionFact>,
    interaction_terminals: HashMap<InteractionId, SessionFact>,
}

impl ToolLedger {
    /// Open the canonical log and acquire a writer lease for this ledger owner.
    pub fn open(
        session_dir: impl AsRef<Path>,
        session_id: SessionId,
        log_id: crate::session_fact_v2::LogId,
        writer_id: WriterId,
        now_ms: i64,
        lease_duration_ms: i64,
    ) -> Result<Self, ToolLedgerError> {
        let session_dir = session_dir.as_ref().to_path_buf();
        let mut store = CanonicalSessionStore::open(&session_dir, session_id, log_id)?;
        let lease = store.acquire_writer(writer_id, now_ms, lease_duration_ms)?;
        Self::from_store(session_dir, store, lease)
    }

    /// Reuse an already-owned canonical store/lease.
    pub fn from_store(
        session_dir: impl AsRef<Path>,
        store: CanonicalSessionStore,
        lease: WriterLease,
    ) -> Result<Self, ToolLedgerError> {
        let session_dir = session_dir.as_ref().to_path_buf();
        let session_id = store.session_id().clone();
        let log_id = store.log_id().clone();
        let reader = CommittedFactReader::open(&session_dir, session_id.clone(), log_id.clone())?;
        let mut entries = HashMap::new();
        let mut interaction_requests = HashMap::new();
        let mut interaction_terminals = HashMap::new();
        for fact in reader.read_all()? {
            index_fact(
                &mut entries,
                &mut interaction_requests,
                &mut interaction_terminals,
                fact,
            )?;
        }
        Ok(Self {
            store,
            lease,
            session_id,
            log_id,
            entries,
            interaction_requests,
            interaction_terminals,
        })
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn log_id(&self) -> &crate::session_fact_v2::LogId {
        &self.log_id
    }

    pub fn lease(&self) -> &WriterLease {
        &self.lease
    }

    pub fn renew_lease(
        &mut self,
        now_ms: i64,
        lease_duration_ms: i64,
    ) -> Result<(), ToolLedgerError> {
        self.lease = self
            .store
            .renew_writer(&self.lease, now_ms, lease_duration_ms)?;
        Ok(())
    }

    /// Keep the actor's writer lease alive across idle gaps.
    ///
    /// `renew_writer` deliberately rejects an expired fence. A long-lived actor
    /// can legitimately outlive its lease between tool calls, so an expired
    /// lease is reacquired under the same writer id; an active lease is renewed
    /// without rotating fence identity.
    pub fn ensure_lease(
        &mut self,
        now_ms: i64,
        lease_duration_ms: i64,
    ) -> Result<(), ToolLedgerError> {
        if self.lease.lease_expires_at_ms > now_ms {
            self.renew_lease(now_ms, lease_duration_ms)
        } else {
            self.lease = self.store.acquire_writer(
                self.lease.writer_id.clone(),
                now_ms,
                lease_duration_ms,
            )?;
            Ok(())
        }
    }

    pub fn get(&self, call_id: &ToolCallId) -> Option<&ToolLedgerEntry> {
        self.entries.get(call_id)
    }

    /// All calls with a durable intent but no terminal fact.
    pub fn open_intents(&self) -> Vec<ToolLedgerEntry> {
        let mut entries: Vec<_> = self
            .entries
            .values()
            .filter(|entry| entry.is_open())
            .cloned()
            .collect();
        entries.sort_by(|left, right| {
            let left = left.intent().map(|intent| intent.call_id.as_str());
            let right = right.intent().map(|intent| intent.call_id.as_str());
            left.cmp(&right)
        });
        entries
    }

    pub fn recovery_disposition(&self, call_id: &ToolCallId) -> Option<ToolRecoveryDisposition> {
        let entry = self.entries.get(call_id)?;
        if let Some(finished) = entry.finished() {
            return Some(ToolRecoveryDisposition::Finished {
                terminal_status: finished.terminal_status,
            });
        }
        let intent = entry.intent()?;
        Some(match &intent.replay_capability {
            ToolReplayCapability::NoReplay => ToolRecoveryDisposition::IndeterminateRequired {
                execution_id: intent.execution_id.clone(),
            },
            ToolReplayCapability::IdempotentReplay => ToolRecoveryDisposition::ReplayAllowed {
                execution_id: intent.execution_id.clone(),
            },
            ToolReplayCapability::Reconcile { probe_ref } => {
                ToolRecoveryDisposition::ReconcileRequired {
                    execution_id: intent.execution_id.clone(),
                    probe_ref: probe_ref.clone(),
                }
            }
        })
    }

    pub fn interaction_terminal(&self, interaction_id: &InteractionId) -> Option<&SessionFact> {
        self.interaction_terminals.get(interaction_id)
    }

    /// Append the canonical request for an interaction.
    pub fn append_interaction_requested(
        &mut self,
        event_id: EventId,
        call_id: Option<ToolCallId>,
        payload: InteractionRequested,
        now_ms: i64,
    ) -> Result<SessionFact, ToolLedgerError> {
        let interaction_id = payload.interaction_id.clone();
        if self.interaction_terminals.contains_key(&interaction_id) {
            return Err(ToolLedgerError::InteractionTerminalConflict { interaction_id });
        }
        if let Some(existing) = self.interaction_requests.get(&interaction_id) {
            if interaction_request_matches(&existing.payload, &payload) {
                return Ok(existing.clone());
            }
            return Err(ToolLedgerError::InteractionRequestConflict { interaction_id });
        }

        let turn_id = payload.turn_id.clone();
        let fact = self.build_interaction_fact(
            event_id,
            Some(turn_id),
            call_id,
            interaction_id.clone(),
            FactPayload::InteractionRequested(payload),
            now_ms,
        );
        let outcome = self.store.append(&self.lease, fact, now_ms)?;
        self.interaction_requests
            .insert(interaction_id, outcome.fact.clone());
        Ok(outcome.fact)
    }

    /// Append the first canonical resolution for an interaction.
    ///
    /// A repeated identical resolution is idempotent. Once a resolution or
    /// expiry is committed, any conflicting terminal is rejected so the
    /// canonical log preserves first-answer-wins.
    pub fn append_interaction_resolved(
        &mut self,
        event_id: EventId,
        turn_id: Option<TurnId>,
        call_id: Option<ToolCallId>,
        payload: InteractionResolved,
        now_ms: i64,
    ) -> Result<SessionFact, ToolLedgerError> {
        let interaction_id = payload.interaction_id.clone();
        if let Some(existing) = self.interaction_terminals.get(&interaction_id) {
            if interaction_resolution_matches(&existing.payload, &payload) {
                return Ok(existing.clone());
            }
            return Err(ToolLedgerError::InteractionTerminalConflict { interaction_id });
        }
        let (turn_id, call_id) =
            self.interaction_envelope_context(&interaction_id, turn_id, call_id);

        let fact = self.build_interaction_fact(
            event_id,
            turn_id,
            call_id,
            interaction_id.clone(),
            FactPayload::InteractionResolved(payload),
            now_ms,
        );
        let outcome = self.store.append(&self.lease, fact, now_ms)?;
        self.interaction_terminals
            .insert(interaction_id, outcome.fact.clone());
        Ok(outcome.fact)
    }

    /// Append the first canonical expiry for an interaction.
    pub fn append_interaction_expired(
        &mut self,
        event_id: EventId,
        turn_id: Option<TurnId>,
        call_id: Option<ToolCallId>,
        payload: InteractionExpired,
        now_ms: i64,
    ) -> Result<SessionFact, ToolLedgerError> {
        let interaction_id = payload.interaction_id.clone();
        if let Some(existing) = self.interaction_terminals.get(&interaction_id) {
            if interaction_expiry_matches(&existing.payload, &payload) {
                return Ok(existing.clone());
            }
            return Err(ToolLedgerError::InteractionTerminalConflict { interaction_id });
        }
        let (turn_id, call_id) =
            self.interaction_envelope_context(&interaction_id, turn_id, call_id);

        let fact = self.build_interaction_fact(
            event_id,
            turn_id,
            call_id,
            interaction_id.clone(),
            FactPayload::InteractionExpired(payload),
            now_ms,
        );
        let outcome = self.store.append(&self.lease, fact, now_ms)?;
        self.interaction_terminals
            .insert(interaction_id, outcome.fact.clone());
        Ok(outcome.fact)
    }

    /// Seal one open non-replayable intent as part of a recovery batch.
    ///
    /// This is the recovery counterpart to the live execution path: the
    /// resulting `ToolFinished::Indeterminate` carries the batch's canonical
    /// `RecoveryRef`, and the method is idempotent for repeated recovery runs.
    /// Idempotent replay and reconciliation intents remain open for their
    /// dedicated recovery step.
    pub fn seal_recovery_intent(
        &mut self,
        call_id: &ToolCallId,
        event_id: EventId,
        recovery_ref: RecoveryRef,
        now_ms: i64,
    ) -> Result<ToolRecoveryDisposition, ToolLedgerError> {
        let (intent, turn_id) = match self.entries.get(call_id) {
            Some(entry) => {
                if let Some(finished) = entry.finished() {
                    return Ok(ToolRecoveryDisposition::Finished {
                        terminal_status: finished.terminal_status,
                    });
                }
                let intent = entry
                    .intent()
                    .ok_or_else(|| ToolLedgerError::IntentMissing {
                        call_id: call_id.clone(),
                    })?
                    .clone();
                let turn_id = entry.intent_fact().and_then(|fact| fact.turn_id.clone());
                (intent, turn_id)
            }
            None => {
                return Err(ToolLedgerError::IntentMissing {
                    call_id: call_id.clone(),
                });
            }
        };

        match &intent.replay_capability {
            ToolReplayCapability::NoReplay => {
                let finished = recovery_indeterminate_finished(&intent, recovery_ref, now_ms);
                self.append_finished(event_id, turn_id, finished, now_ms)?;
                Ok(ToolRecoveryDisposition::Finished {
                    terminal_status: ToolTerminalStatus::Indeterminate,
                })
            }
            ToolReplayCapability::IdempotentReplay => Ok(ToolRecoveryDisposition::ReplayAllowed {
                execution_id: intent.execution_id,
            }),
            ToolReplayCapability::Reconcile { probe_ref } => {
                Ok(ToolRecoveryDisposition::ReconcileRequired {
                    execution_id: intent.execution_id,
                    probe_ref: probe_ref.clone(),
                })
            }
        }
    }

    /// Apply the recovery disposition to every open intent in call-id order.
    ///
    /// Non-replayable intents are sealed with one shared batch `RecoveryRef`.
    /// Replay/reconcile intents remain open and are returned to the caller for
    /// their dedicated recovery steps. The operation is idempotent across
    /// repeated recovery executions.
    pub fn recover_open_intents(
        &mut self,
        recovery_ref: RecoveryRef,
        now_ms: i64,
    ) -> Result<Vec<(ToolCallId, ToolRecoveryDisposition)>, ToolLedgerError> {
        let call_ids: Vec<ToolCallId> = self
            .open_intents()
            .into_iter()
            .filter_map(|entry| entry.intent().map(|intent| intent.call_id.clone()))
            .collect();
        let mut dispositions = Vec::with_capacity(call_ids.len());
        for call_id in call_ids {
            let disposition = self.seal_recovery_intent(
                &call_id,
                EventId::new(generate_ulid()),
                recovery_ref.clone(),
                now_ms,
            )?;
            dispositions.push((call_id, disposition));
        }
        Ok(dispositions)
    }

    /// Durably append the one intent for `payload.call_id`.
    ///
    /// Repeating the exact same intent is idempotent and returns the existing
    /// fact. Any conflicting intent, or an intent after terminal, is rejected.
    pub fn append_intent(
        &mut self,
        event_id: EventId,
        turn_id: Option<TurnId>,
        payload: ToolIntent,
        now_ms: i64,
    ) -> Result<SessionFact, ToolLedgerError> {
        let call_id = payload.call_id.clone();
        if let Some(entry) = self.entries.get(&call_id) {
            if let Some(finished) = entry.finished() {
                return Err(ToolLedgerError::IntentAfterFinished {
                    call_id,
                    terminal_status: finished.terminal_status,
                });
            }
            if let Some(existing) = entry.intent() {
                if existing == &payload {
                    return Ok(entry
                        .intent_fact()
                        .expect("intent payload implies intent fact")
                        .clone());
                }
                return Err(ToolLedgerError::IntentConflict {
                    call_id,
                    existing_execution_id: existing.execution_id.clone(),
                    incoming_execution_id: payload.execution_id.clone(),
                });
            }
        }

        let fact = self.build_fact(
            event_id,
            turn_id,
            call_id.clone(),
            FactPayload::ToolIntent(payload),
            now_ms,
        );
        let outcome = self.store.append(&self.lease, fact, now_ms)?;
        self.entries.entry(call_id).or_default().intent = Some(outcome.fact.clone());
        Ok(outcome.fact)
    }

    /// Durably append the one terminal for `payload.call_id`.
    ///
    /// Repeating the exact same terminal is idempotent. A conflicting terminal,
    /// an execution mismatch, or an execution-bearing terminal without intent
    /// is rejected.
    pub fn append_finished(
        &mut self,
        event_id: EventId,
        turn_id: Option<TurnId>,
        payload: ToolFinished,
        now_ms: i64,
    ) -> Result<SessionFact, ToolLedgerError> {
        let call_id = payload.call_id.clone();
        if let Some(entry) = self.entries.get(&call_id) {
            if let Some(existing) = entry.finished() {
                if existing == &payload {
                    return Ok(entry
                        .finished_fact()
                        .expect("finished payload implies finished fact")
                        .clone());
                }
                return Err(ToolLedgerError::FinishedConflict { call_id });
            }
            if let Some(intent) = entry.intent() {
                match &payload.execution_id {
                    Some(finished_execution_id)
                        if finished_execution_id != &intent.execution_id =>
                    {
                        return Err(ToolLedgerError::ExecutionMismatch {
                            call_id,
                            intent_execution_id: intent.execution_id.clone(),
                            finished_execution_id: finished_execution_id.clone(),
                        });
                    }
                    None => {
                        return Err(ToolLedgerError::IntentPresentForExecutionlessTerminal {
                            call_id,
                        });
                    }
                    Some(_) => {}
                }
            }
        } else if payload.execution_id.is_some() {
            return Err(ToolLedgerError::IntentMissing { call_id });
        } else if !executionless_terminal_allowed(payload.terminal_status) {
            return Err(ToolLedgerError::IntentRequired {
                call_id,
                terminal_status: payload.terminal_status,
            });
        }

        let fact = self.build_fact(
            event_id,
            turn_id,
            call_id.clone(),
            FactPayload::ToolFinished(payload),
            now_ms,
        );
        let outcome = self.store.append(&self.lease, fact, now_ms)?;
        self.entries.entry(call_id).or_default().finished = Some(outcome.fact.clone());
        Ok(outcome.fact)
    }

    fn interaction_envelope_context(
        &self,
        interaction_id: &InteractionId,
        turn_id: Option<TurnId>,
        call_id: Option<ToolCallId>,
    ) -> (Option<TurnId>, Option<ToolCallId>) {
        let Some(requested) = self.interaction_requests.get(interaction_id) else {
            return (turn_id, call_id);
        };
        (
            turn_id.or_else(|| requested.turn_id.clone()),
            call_id.or_else(|| requested.call_id.clone()),
        )
    }

    fn build_fact(
        &self,
        event_id: EventId,
        turn_id: Option<TurnId>,
        call_id: ToolCallId,
        payload: FactPayload,
        now_ms: i64,
    ) -> SessionFact {
        SessionFact {
            schema: FactSchema::v2(),
            session_id: self.session_id.clone(),
            log_id: self.log_id.clone(),
            fact_seq: 0,
            event_id,
            ts_ms: now_ms,
            causation_id: None,
            turn_id,
            call_id: Some(call_id),
            interaction_id: None,
            payload,
        }
    }

    fn build_interaction_fact(
        &self,
        event_id: EventId,
        turn_id: Option<TurnId>,
        call_id: Option<ToolCallId>,
        interaction_id: InteractionId,
        payload: FactPayload,
        now_ms: i64,
    ) -> SessionFact {
        SessionFact {
            schema: FactSchema::v2(),
            session_id: self.session_id.clone(),
            log_id: self.log_id.clone(),
            fact_seq: 0,
            event_id,
            ts_ms: now_ms,
            causation_id: None,
            turn_id,
            call_id,
            interaction_id: Some(interaction_id),
            payload,
        }
    }
}

fn index_fact(
    entries: &mut HashMap<ToolCallId, ToolLedgerEntry>,
    interaction_requests: &mut HashMap<InteractionId, SessionFact>,
    interaction_terminals: &mut HashMap<InteractionId, SessionFact>,
    fact: SessionFact,
) -> Result<(), ToolLedgerError> {
    match &fact.payload {
        FactPayload::ToolIntent(payload) => {
            let entry = entries.entry(payload.call_id.clone()).or_default();
            if let Some(finished) = entry.finished() {
                return Err(ToolLedgerError::IntentAfterFinished {
                    call_id: payload.call_id.clone(),
                    terminal_status: finished.terminal_status,
                });
            }
            if entry.intent.is_some() {
                return Err(ToolLedgerError::DuplicateIntent {
                    call_id: payload.call_id.clone(),
                });
            }
            entry.intent = Some(fact);
        }
        FactPayload::ToolFinished(payload) => {
            let entry = entries.entry(payload.call_id.clone()).or_default();
            if entry.finished.is_some() {
                return Err(ToolLedgerError::DuplicateFinished {
                    call_id: payload.call_id.clone(),
                });
            }
            if let Some(intent) = entry.intent() {
                match &payload.execution_id {
                    Some(finished_execution_id)
                        if finished_execution_id != &intent.execution_id =>
                    {
                        return Err(ToolLedgerError::ExecutionMismatch {
                            call_id: payload.call_id.clone(),
                            intent_execution_id: intent.execution_id.clone(),
                            finished_execution_id: finished_execution_id.clone(),
                        });
                    }
                    None => {
                        return Err(ToolLedgerError::IntentPresentForExecutionlessTerminal {
                            call_id: payload.call_id.clone(),
                        });
                    }
                    Some(_) => {}
                }
            } else if payload.execution_id.is_some() {
                return Err(ToolLedgerError::IntentMissing {
                    call_id: payload.call_id.clone(),
                });
            } else if !executionless_terminal_allowed(payload.terminal_status) {
                return Err(ToolLedgerError::IntentRequired {
                    call_id: payload.call_id.clone(),
                    terminal_status: payload.terminal_status,
                });
            }
            entry.finished = Some(fact);
        }
        FactPayload::InteractionRequested(payload) => {
            index_interaction_request(interaction_requests, payload.interaction_id.clone(), fact)?;
        }
        FactPayload::InteractionResolved(payload) => {
            index_interaction_terminal(
                interaction_terminals,
                payload.interaction_id.clone(),
                fact,
            )?;
        }
        FactPayload::InteractionExpired(payload) => {
            index_interaction_terminal(
                interaction_terminals,
                payload.interaction_id.clone(),
                fact,
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn interaction_resolution_matches(existing: &FactPayload, incoming: &InteractionResolved) -> bool {
    matches!(
        existing,
        FactPayload::InteractionResolved(existing)
            if existing.interaction_id == incoming.interaction_id
                && existing.decision_ref == incoming.decision_ref
                && existing.resolved_by == incoming.resolved_by
                && existing.resolution_seq == incoming.resolution_seq
    )
}

fn interaction_expiry_matches(existing: &FactPayload, incoming: &InteractionExpired) -> bool {
    matches!(
        existing,
        FactPayload::InteractionExpired(existing)
            if existing.interaction_id == incoming.interaction_id
                && existing.reason == incoming.reason
                && existing.recovery_ref == incoming.recovery_ref
    )
}

fn interaction_request_matches(existing: &FactPayload, incoming: &InteractionRequested) -> bool {
    matches!(
        existing,
        FactPayload::InteractionRequested(existing)
            if existing.interaction_id == incoming.interaction_id
                && existing.turn_id == incoming.turn_id
                && existing.call_id == incoming.call_id
                && existing.kind == incoming.kind
    )
}

fn index_interaction_request(
    interaction_requests: &mut HashMap<InteractionId, SessionFact>,
    interaction_id: InteractionId,
    fact: SessionFact,
) -> Result<(), ToolLedgerError> {
    if interaction_requests.contains_key(&interaction_id) {
        return Err(ToolLedgerError::InteractionRequestConflict { interaction_id });
    }
    interaction_requests.insert(interaction_id, fact);
    Ok(())
}

fn index_interaction_terminal(
    interaction_terminals: &mut HashMap<InteractionId, SessionFact>,
    interaction_id: InteractionId,
    fact: SessionFact,
) -> Result<(), ToolLedgerError> {
    if interaction_terminals.contains_key(&interaction_id) {
        return Err(ToolLedgerError::InteractionTerminalConflict { interaction_id });
    }
    interaction_terminals.insert(interaction_id, fact);
    Ok(())
}

fn recovery_indeterminate_finished(
    intent: &ToolIntent,
    recovery_ref: RecoveryRef,
    now_ms: i64,
) -> ToolFinished {
    ToolFinished {
        call_id: intent.call_id.clone(),
        execution_id: Some(intent.execution_id.clone()),
        terminal_status: ToolTerminalStatus::Indeterminate,
        output_ref: None,
        error: Some(ToolError {
            code: "indeterminate_after_crash".into(),
            message: "non-idempotent execution was not replayed".into(),
            retryable: false,
            details_ref: None,
        }),
        metrics: ToolMetrics {
            started_at_ms: intent.intent_at_ms,
            finished_at_ms: now_ms,
            retry_count: 0,
            output_bytes: 0,
            progress_bytes_total: 0,
        },
        reconciled: false,
        evidence_ref: None,
        evidence_fact_seq: None,
        evidence_event_id: None,
        recovery_ref: Some(recovery_ref),
        finished_at_ms: now_ms,
    }
}

fn tool_intent_payload(fact: &SessionFact) -> Option<&ToolIntent> {
    match &fact.payload {
        FactPayload::ToolIntent(payload) => Some(payload),
        _ => None,
    }
}

fn tool_finished_payload(fact: &SessionFact) -> Option<&ToolFinished> {
    match &fact.payload {
        FactPayload::ToolFinished(payload) => Some(payload),
        _ => None,
    }
}

fn executionless_terminal_allowed(status: ToolTerminalStatus) -> bool {
    matches!(
        status,
        ToolTerminalStatus::Denied | ToolTerminalStatus::Cancelled
    )
}
