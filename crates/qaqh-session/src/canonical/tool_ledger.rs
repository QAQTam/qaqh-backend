//! Durable tool lifecycle ledger over canonical `events.jsonl`.
//!
//! This is the P3-5 core only: it enforces one `ToolIntent` and one
//! `ToolFinished` per `call_id`, rebuilds its index from committed facts, and
//! classifies open intents for recovery. SessionActor/ToolRuntime wiring and
//! cancel/resume CAS remain a later slice.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::session_fact_v2::{
    ContentRef, DriverChanged, EventId, ExecutionId, FactPayload, FactSchema, InteractionExpired,
    InteractionId, InteractionRequested, InteractionResolved, RecoveryRef, SessionFact, SessionId,
    SessionRecovered, ToolCallId, ToolError, ToolFinished, ToolIntent, ToolMetrics,
    ToolReplayCapability, ToolTerminalStatus, TurnId,
};

use super::{
    AppendOutcome, CanonicalError, CanonicalIdentityError, CanonicalSessionStore,
    CommittedFactReader, WriterId, WriterLease, generate_ulid,
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

/// Durable evidence produced by a `Reconcile` probe.
///
/// The canonical `ToolFinished` schema can carry either a content-addressed
/// evidence blob or an exact fact/event pair. Recovery probes must provide one
/// of those two forms; a bare boolean is not evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolReconciliationEvidence {
    ContentRef(ContentRef),
    Fact { fact_seq: u64, event_id: EventId },
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

    #[error("tool call {call_id} is not open for reconciliation")]
    ReconciliationNotAllowed { call_id: ToolCallId },

    #[error("tool call {call_id} reconciliation probe does not match the durable intent")]
    ReconciliationProbeMismatch { call_id: ToolCallId },

    #[error("tool call {call_id} reconciliation terminal cannot be backgrounded")]
    ReconciliationTerminalInvalid { call_id: ToolCallId },
}

/// Outcome of a canonical driver-seat claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverClaimOutcome {
    /// Seat was free (or its recorded holder was reported stale by the caller).
    Claimed { driver_epoch: u64 },
    /// The caller already holds the seat; no fact is written.
    AlreadyHeld { driver_epoch: u64 },
    /// Another holder owns the seat.
    Busy { holder: String, driver_epoch: u64 },
}

/// Outcome of a canonical driver-seat release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverReleaseOutcome {
    Released {
        driver_epoch: u64,
    },
    NotDriver {
        holder: Option<String>,
        driver_epoch: u64,
    },
    /// `expected_epoch` CAS failed: the seat moved on, so the release is a no-op.
    StaleEpoch {
        driver_epoch: u64,
    },
}

impl Drop for ToolLedger {
    /// Release the exclusive writer fence when the ledger owner goes away.
    ///
    /// The fence is a lease with a TTL, so a crashed writer still recovers on
    /// expiry. But an *orderly* exit (actor shutdown, daemon restart, session
    /// switch, recovery batch) must not make the successor wait out a whole
    /// lease window: without this, every actor exit blocks the next writer with
    /// `WriterBusy` for `tool_ledger_lease_ms()` (30s by default), which the
    /// runtime surfaces as LEDGER_BLOCKED for every canonical write.
    ///
    /// Errors are logged and swallowed — `Drop` must not panic, and the fence
    /// still expires by TTL if this fails.
    ///
    /// The release stamp is the minimum `i64` rather than "now": a released
    /// fence must look expired to *every* reader, including callers that
    /// compare against a synthetic clock. Stamping the wall clock here could
    /// actually push the expiry forward relative to such a caller and
    /// resurrect the fence.
    fn drop(&mut self) {
        if let Err(error) = self.store.release_writer(&self.lease, i64::MIN) {
            log::warn!("[qaqh-session] failed to release canonical writer lease on drop: {error}");
        }
    }
}

#[derive(Debug)]
pub struct ToolLedger {
    store: CanonicalSessionStore,
    lease: WriterLease,
    session_dir: PathBuf,
    session_id: SessionId,
    log_id: crate::session_fact_v2::LogId,
    entries: HashMap<ToolCallId, ToolLedgerEntry>,
    interaction_requests: HashMap<InteractionId, SessionFact>,
    interaction_terminals: HashMap<InteractionId, SessionFact>,
    /// Canonical driver seat, rebuilt from `DriverChanged` facts on open.
    driver_holder: Option<String>,
    driver_epoch: u64,
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
        let mut driver_holder = None;
        let mut driver_epoch = 0;
        for fact in reader.read_all()? {
            if let FactPayload::DriverChanged(payload) = &fact.payload {
                driver_holder = payload.holder.clone();
                driver_epoch = payload.driver_epoch;
            }
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
            session_dir,
            session_id,
            log_id,
            entries,
            interaction_requests,
            interaction_terminals,
            driver_holder,
            driver_epoch,
        })
    }

    /// Current canonical driver seat (`holder`, `driver_epoch`).
    pub fn driver_state(&self) -> (Option<String>, u64) {
        (self.driver_holder.clone(), self.driver_epoch)
    }

    /// Claim the driver seat. `stale_holder` lets the caller (the daemon, which
    /// owns lease liveness) take over a seat whose holder's lease has expired.
    pub fn claim_driver(
        &mut self,
        holder: &str,
        stale_holder: Option<&str>,
        event_id: EventId,
        causation_id: Option<EventId>,
        now_ms: i64,
    ) -> Result<DriverClaimOutcome, ToolLedgerError> {
        if let Some(current) = self.driver_holder.clone() {
            if current == holder {
                return Ok(DriverClaimOutcome::AlreadyHeld {
                    driver_epoch: self.driver_epoch,
                });
            }
            if stale_holder != Some(current.as_str()) {
                return Ok(DriverClaimOutcome::Busy {
                    holder: current,
                    driver_epoch: self.driver_epoch,
                });
            }
        }
        let driver_epoch = self.driver_epoch.saturating_add(1);
        self.append_driver_changed(
            Some(holder.to_string()),
            driver_epoch,
            event_id,
            causation_id,
            now_ms,
        )?;
        Ok(DriverClaimOutcome::Claimed { driver_epoch })
    }

    /// Release the driver seat. Only the recorded holder may release, and an
    /// optional `expected_epoch` CAS makes a delayed reclaim a no-op when the
    /// seat has since been re-claimed.
    pub fn release_driver(
        &mut self,
        holder: &str,
        expected_epoch: Option<u64>,
        event_id: EventId,
        causation_id: Option<EventId>,
        now_ms: i64,
    ) -> Result<DriverReleaseOutcome, ToolLedgerError> {
        if self.driver_holder.as_deref() != Some(holder) {
            return Ok(DriverReleaseOutcome::NotDriver {
                holder: self.driver_holder.clone(),
                driver_epoch: self.driver_epoch,
            });
        }
        if expected_epoch.is_some_and(|expected| expected != self.driver_epoch) {
            return Ok(DriverReleaseOutcome::StaleEpoch {
                driver_epoch: self.driver_epoch,
            });
        }
        let driver_epoch = self.driver_epoch.saturating_add(1);
        self.append_driver_changed(None, driver_epoch, event_id, causation_id, now_ms)?;
        Ok(DriverReleaseOutcome::Released { driver_epoch })
    }

    fn append_driver_changed(
        &mut self,
        holder: Option<String>,
        driver_epoch: u64,
        event_id: EventId,
        causation_id: Option<EventId>,
        now_ms: i64,
    ) -> Result<SessionFact, ToolLedgerError> {
        let fact = SessionFact {
            schema: FactSchema::v2(),
            session_id: self.session_id.clone(),
            log_id: self.log_id.clone(),
            fact_seq: 0,
            event_id,
            ts_ms: now_ms,
            causation_id,
            turn_id: None,
            call_id: None,
            interaction_id: None,
            payload: FactPayload::DriverChanged(DriverChanged {
                holder: holder.clone(),
                driver_epoch,
                changed_at_ms: now_ms,
            }),
        };
        let outcome = self.append_and_publish(fact, now_ms)?;
        self.driver_holder = holder;
        self.driver_epoch = driver_epoch;
        Ok(outcome.fact)
    }

    fn append_and_publish(
        &mut self,
        fact: SessionFact,
        now_ms: i64,
    ) -> Result<AppendOutcome, ToolLedgerError> {
        let outcome = self.store.append(&self.lease, fact, now_ms)?;
        crate::projection::publish_projection(&self.session_dir, &outcome.fact, &outcome.events);
        Ok(outcome)
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

    /// Voluntarily give up this ledger's writer lease.
    ///
    /// Recovery owns the fence only for the duration of its batch. Holding it
    /// until the lease expires would block the owning session's own ledger (a
    /// different writer id) with `WriterBusy`, which the runtime maps to
    /// `LEDGER_BLOCKED` for every tool call in the window.
    pub fn release_writer_lease(&mut self, now_ms: i64) -> Result<(), ToolLedgerError> {
        self.store.release_writer(&self.lease, now_ms)?;
        Ok(())
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
            None,
            Some(turn_id),
            call_id,
            interaction_id.clone(),
            FactPayload::InteractionRequested(payload),
            now_ms,
        );
        let outcome = self.append_and_publish(fact, now_ms)?;
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
        self.append_interaction_resolved_with_causation(
            event_id, None, turn_id, call_id, payload, now_ms,
        )
    }

    /// Append a resolution whose command id is the canonical causation id.
    pub fn append_interaction_resolved_with_causation(
        &mut self,
        event_id: EventId,
        causation_id: Option<EventId>,
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
            causation_id,
            turn_id,
            call_id,
            interaction_id.clone(),
            FactPayload::InteractionResolved(payload),
            now_ms,
        );
        let outcome = self.append_and_publish(fact, now_ms)?;
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
            None,
            turn_id,
            call_id,
            interaction_id.clone(),
            FactPayload::InteractionExpired(payload),
            now_ms,
        );
        let outcome = self.append_and_publish(fact, now_ms)?;
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

    /// All terminal facts known to this ledger, ordered by canonical fact_seq.
    pub fn finished_facts(&self) -> Vec<&SessionFact> {
        let mut facts: Vec<_> = self
            .entries
            .values()
            .filter_map(ToolLedgerEntry::finished_fact)
            .collect();
        facts.sort_by_key(|fact| fact.fact_seq);
        facts
    }

    /// Append the recovery batch's final `SessionRecovered` fact.
    ///
    /// Recovery execution owns the same writer lease as the tool ledger so the
    /// terminal facts and the batch marker cannot be committed by different
    /// writers.
    pub fn append_session_recovered(
        &mut self,
        event_id: EventId,
        payload: SessionRecovered,
        now_ms: i64,
    ) -> Result<SessionFact, ToolLedgerError> {
        let fact = SessionFact {
            schema: FactSchema::v2(),
            session_id: self.session_id.clone(),
            log_id: self.log_id.clone(),
            fact_seq: 0,
            event_id,
            ts_ms: now_ms,
            causation_id: None,
            turn_id: None,
            call_id: None,
            interaction_id: None,
            payload: FactPayload::SessionRecovered(payload),
        };
        let outcome = self.append_and_publish(fact, now_ms)?;
        Ok(outcome.fact)
    }

    /// Close a `Reconcile` intent with probe evidence.
    ///
    /// The probe must match the `probe_ref` frozen in the durable `ToolIntent`.
    /// The resulting terminal is marked `reconciled=true` and carries either a
    /// content-addressed evidence reference or an exact evidence fact/event
    /// pair. Repeating the same reconciliation is idempotent.
    #[allow(clippy::too_many_arguments)]
    pub fn append_reconciled_finished(
        &mut self,
        event_id: EventId,
        call_id: ToolCallId,
        probe_ref: ContentRef,
        recovery_ref: RecoveryRef,
        terminal_status: ToolTerminalStatus,
        output_ref: Option<ContentRef>,
        error: Option<ToolError>,
        evidence: ToolReconciliationEvidence,
        now_ms: i64,
    ) -> Result<SessionFact, ToolLedgerError> {
        if terminal_status == ToolTerminalStatus::Backgrounded {
            return Err(ToolLedgerError::ReconciliationTerminalInvalid { call_id });
        }

        let entry = self
            .entries
            .get(&call_id)
            .ok_or_else(|| ToolLedgerError::IntentMissing {
                call_id: call_id.clone(),
            })?;
        let intent = entry
            .intent()
            .cloned()
            .ok_or_else(|| ToolLedgerError::IntentMissing {
                call_id: call_id.clone(),
            })?;
        let turn_id = entry.intent_fact().and_then(|fact| fact.turn_id.clone());
        let existing_finished = entry.finished_fact().cloned();

        let ToolReplayCapability::Reconcile {
            probe_ref: expected_probe_ref,
        } = &intent.replay_capability
        else {
            return Err(ToolLedgerError::ReconciliationNotAllowed { call_id });
        };
        if expected_probe_ref != &probe_ref {
            return Err(ToolLedgerError::ReconciliationProbeMismatch { call_id });
        }

        let (evidence_ref, evidence_fact_seq, evidence_event_id) = match evidence {
            ToolReconciliationEvidence::ContentRef(content_ref) => (Some(content_ref), None, None),
            ToolReconciliationEvidence::Fact { fact_seq, event_id } => {
                (None, Some(fact_seq), Some(event_id))
            }
        };
        let finished = ToolFinished {
            call_id: call_id.clone(),
            execution_id: Some(intent.execution_id.clone()),
            terminal_status,
            output_ref,
            error,
            metrics: ToolMetrics {
                started_at_ms: intent.intent_at_ms,
                finished_at_ms: now_ms,
                retry_count: 0,
                output_bytes: 0,
                progress_bytes_total: 0,
            },
            reconciled: true,
            evidence_ref,
            evidence_fact_seq,
            evidence_event_id,
            recovery_ref: Some(recovery_ref),
            finished_at_ms: now_ms,
        };

        if let Some(existing) = existing_finished {
            if matches!(
                &existing.payload,
                FactPayload::ToolFinished(existing) if existing == &finished
            ) {
                return Ok(existing);
            }
            return Err(ToolLedgerError::FinishedConflict { call_id });
        }

        self.append_finished(event_id, turn_id, finished, now_ms)
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
        let outcome = self.append_and_publish(fact, now_ms)?;
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
        let outcome = self.append_and_publish(fact, now_ms)?;
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

    #[allow(clippy::too_many_arguments)]
    fn build_interaction_fact(
        &self,
        event_id: EventId,
        causation_id: Option<EventId>,
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
            causation_id,
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
                // Legacy facts predate the structured decision; keep them
                // idempotent instead of raising a terminal conflict.
                && (existing.decision.is_none() || existing.decision == incoming.decision)
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
