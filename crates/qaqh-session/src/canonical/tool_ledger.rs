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
    ContentRef, EventId, ExecutionId, FactPayload, FactSchema, SessionFact, SessionId, ToolCallId,
    ToolFinished, ToolIntent, ToolReplayCapability, ToolTerminalStatus, TurnId,
};

use super::{CanonicalError, CanonicalSessionStore, CommittedFactReader, WriterId, WriterLease};

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
}

#[derive(Debug)]
pub struct ToolLedger {
    store: CanonicalSessionStore,
    lease: WriterLease,
    session_id: SessionId,
    log_id: crate::session_fact_v2::LogId,
    entries: HashMap<ToolCallId, ToolLedgerEntry>,
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
        for fact in reader.read_all()? {
            index_fact(&mut entries, fact)?;
        }
        Ok(Self {
            store,
            lease,
            session_id,
            log_id,
            entries,
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
}

fn index_fact(
    entries: &mut HashMap<ToolCallId, ToolLedgerEntry>,
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
        _ => {}
    }
    Ok(())
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
