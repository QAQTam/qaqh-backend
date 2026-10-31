//! Isolated commit owner joining batch durability and context projection.
//!
//! Not wired into production until business transition validation, durable
//! effect capabilities and supervised execution are complete. Call this owner
//! on a bounded blocking executor, never on an async actor worker.

use qaqh_agentloop_core::{ActorIdentity, EffectIdentity};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use crate::{
    canonical::{
        BatchCommitError, BatchFaultPoint, BatchJournal, BatchReadError, BatchReceipt,
        CanonicalError, SessionBlobStore,
    },
    commit_state::{CommitState, CommitStateError, RecoveredObligations},
    context_projection::{ContextBudget, ContextProjectionError, ContextProjector},
    projection::{
        ProjectionSet, ProjectionSetSnapshot, projection_events_for_fact,
        projection_replaceable_events_for_fact, publish_projection,
    },
    session_fact_v2::{FactPayload, LogId, SessionFact, SessionId},
};

pub use crate::canonical::BatchFaultPoint as CommitFaultPoint;
pub use crate::commit_state::{
    RecoveredObligations as SessionObligations, UnsettledInteraction as OpenInteraction,
    UnsettledTool as OpenTool,
};

/// Bounded in-process double-issue guard. Durable idempotency belongs to the
/// batch journal; this set only stops one process from minting twice.
const ISSUED_HISTORY: usize = 4096;

/// Upper bound on tool arguments materialized into a dispatch capability.
const MAX_EFFECT_ARGS: usize = 256 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum SessionCommitError {
    #[error("storage: {0}")]
    Storage(#[from] CanonicalError),
    #[error("context projection: {0}")]
    Context(#[from] ContextProjectionError),
    #[error("business transition: {0}")]
    Transition(#[from] CommitStateError),
    #[error("batch outcome unknown: {0}")]
    Unknown(CanonicalError),
    #[error("batch was not committed: {0}")]
    NotCommitted(CanonicalError),
    #[error("commit owner is fenced after an uncertain or unreconciled result")]
    Fenced,
}

pub struct SessionCommit {
    session_dir: PathBuf,
    session_id: SessionId,
    log_id: LogId,
    journal: BatchJournal,
    blobs: SessionBlobStore,
    context: ContextProjector,
    projections: ProjectionSet,
    state: CommitState,
    fenced: bool,
    incarnation: u64,
    issued: HashSet<(String, u64)>,
    /// FIFO of `issued` keys so the in-process double-issue guard stays bounded;
    /// durable idempotency is the batch journal's job, not this set's.
    issued_order: std::collections::VecDeque<(String, u64)>,
    capability_live: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// A non-cloneable, in-process capability produced only after a successful
/// fresh commit. Recovery never reissues capabilities for older intentions.
///
/// The capability is not dispatch authority by itself: it must be handed to the
/// supervising owner, which mints its own single-use ticket. What this type
/// proves is that one commit port produced the intent, and it stops being
/// usable once that port is dropped or fenced.
pub struct CommittedExecution {
    identity: EffectIdentity,
    fact: SessionFact,
    request_messages: Option<Vec<qaqh_types::Message>>,
    tool_args: Option<Vec<u8>>,
    live: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Why a capability could not be consumed.
///
/// Every variant means nothing was started under this identity, so the intent
/// may be authorized again later without reconciliation. Revocation fences new
/// work; it never proves that previously started work stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceReason {
    /// The commit port was dropped, or fenced after an uncertain batch result.
    OwnerGone,
    /// The stored fact no longer hashes to the digest this identity claims.
    DigestMismatch,
    /// The committed intent is not something this chain can dispatch.
    UnsupportedIntent,
}

impl std::fmt::Display for FenceReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OwnerGone => f.write_str("commit owner is gone or fenced"),
            Self::DigestMismatch => {
                f.write_str("committed fact does not match the identity digest")
            }
            Self::UnsupportedIntent => f.write_str("committed intent is not dispatchable"),
        }
    }
}

/// Exactly the committed material one authorized task may observe.
pub struct CommittedParts {
    pub identity: EffectIdentity,
    pub fact: SessionFact,
    pub request_messages: Option<Vec<qaqh_types::Message>>,
    /// Verified effective tool arguments, resolved from the intent's blob at
    /// mint time so the runner cannot read a later revision of them.
    pub tool_args: Option<Vec<u8>>,
}

impl CommittedExecution {
    pub fn identity(&self) -> &EffectIdentity {
        &self.identity
    }
    pub fn fact(&self) -> &SessionFact {
        &self.fact
    }
    pub fn request_messages(&self) -> Option<&[qaqh_types::Message]> {
        self.request_messages.as_deref()
    }
    pub fn tool_args(&self) -> Option<&[u8]> {
        self.tool_args.as_deref()
    }

    /// Consume the authority and its exact frozen input together.
    ///
    /// The digest is re-derived from the fact rather than trusting the copy
    /// carried by the identity, so a capability cannot be paired with
    /// different committed material.
    pub fn into_parts(self) -> Result<CommittedParts, FenceReason> {
        if !self.live.load(std::sync::atomic::Ordering::Acquire) {
            return Err(FenceReason::OwnerGone);
        }
        if !matches!(
            self.fact.payload,
            FactPayload::ModelRoundStarted(_) | FactPayload::ToolIntent(_)
        ) {
            return Err(FenceReason::UnsupportedIntent);
        }
        let digest: [u8; 32] = Sha256::digest(
            serde_json::to_vec(&self.fact).map_err(|_| FenceReason::DigestMismatch)?,
        )
        .into();
        if digest != self.identity.intent_digest {
            return Err(FenceReason::DigestMismatch);
        }
        Ok(CommittedParts {
            identity: self.identity,
            fact: self.fact,
            request_messages: self.request_messages,
            tool_args: self.tool_args,
        })
    }
}

impl Drop for SessionCommit {
    fn drop(&mut self) {
        self.capability_live
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

impl SessionCommit {
    /// Open the session's writer, creating the journal only for an empty directory.
    ///
    /// Call sites must not probe `storage-format` themselves: three copies of that
    /// probe is how "create vs open" drifted apart between the bootstrap path and
    /// the spawn path.
    pub fn open_or_create(
        session_dir: impl AsRef<Path>,
        session_id: SessionId,
        log_id: LogId,
        budget: ContextBudget,
    ) -> Result<Self, SessionCommitError> {
        let path = session_dir.as_ref().to_path_buf();
        if crate::canonical::is_batch_root(&path) {
            Self::open(path, session_id, log_id, budget)
        } else {
            Self::create(path, session_id, log_id, budget)
        }
    }

    /// Create an isolated store only for a newly allocated session directory.
    pub fn create(
        session_dir: impl AsRef<Path>,
        session_id: SessionId,
        log_id: LogId,
        budget: ContextBudget,
    ) -> Result<Self, SessionCommitError> {
        let path = session_dir.as_ref().to_path_buf();
        fs::create_dir_all(&path).map_err(CanonicalError::from)?;
        let blobs = SessionBlobStore::open(&path).map_err(CanonicalError::from)?;
        let journal = BatchJournal::create(&path, session_id.clone(), log_id.clone())?;
        let context = ContextProjector::empty(session_id.clone(), log_id.clone(), budget);
        let projections = ProjectionSet::default();
        Ok(Self {
            session_dir: path,
            session_id,
            log_id,
            journal,
            blobs,
            context,
            projections,
            state: CommitState::default(),
            fenced: false,
            incarnation: next_incarnation()?,
            issued: HashSet::new(),
            issued_order: std::collections::VecDeque::new(),
            capability_live: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        })
    }

    /// Recover only from the verified batch prefix, never the legacy store.
    pub fn open(
        session_dir: impl AsRef<Path>,
        session_id: SessionId,
        log_id: LogId,
        budget: ContextBudget,
    ) -> Result<Self, SessionCommitError> {
        let path = session_dir.as_ref().to_path_buf();
        let journal = BatchJournal::open(&path, session_id.clone(), log_id.clone())?;
        let blobs = SessionBlobStore::open(&path).map_err(CanonicalError::from)?;
        let mut context = ContextProjector::empty(session_id.clone(), log_id.clone(), budget);
        let mut projections = ProjectionSet::default();
        let mut state = CommitState::default();
        journal
            .visit_committed(|facts| {
                for fact in facts {
                    state.apply(fact)?;
                    context
                        .apply(fact, &blobs)
                        .map_err(SessionCommitError::from)?;
                    let _ = projections.apply(fact);
                }
                Ok::<(), SessionCommitError>(())
            })
            .map_err(|error| match error {
                BatchReadError::Storage(error) => SessionCommitError::Storage(error),
                BatchReadError::Visitor(error) => error,
            })?;
        Ok(Self {
            session_dir: path,
            session_id,
            log_id,
            journal,
            blobs,
            context,
            projections,
            state,
            fenced: false,
            incarnation: next_incarnation()?,
            issued: HashSet::new(),
            issued_order: std::collections::VecDeque::new(),
            capability_live: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        })
    }

    pub fn head(&self) -> Result<u64, SessionCommitError> {
        self.ensure_healthy()?;
        Ok(self.journal.committed_head())
    }

    pub fn projections(&self) -> Result<&ProjectionSet, SessionCommitError> {
        self.ensure_healthy()?;
        Ok(&self.projections)
    }

    pub fn snapshot(&self) -> Result<ProjectionSetSnapshot, SessionCommitError> {
        self.ensure_healthy()?;
        Ok(self.projections.snapshot())
    }

    /// After an unknown outcome, drop the fenced owner and reopen it before
    /// querying the original batch id. A receipt is only a durable position,
    /// not authorization to dispatch an effect.
    pub fn lookup_batch(&self, batch_id: &str) -> Result<Option<BatchReceipt>, SessionCommitError> {
        self.ensure_healthy()?;
        Ok(self.journal.lookup(batch_id)?.cloned())
    }

    pub fn request_messages(&self) -> Result<&[qaqh_types::Message], SessionCommitError> {
        self.ensure_healthy()?;
        Ok(self.context.request_messages()?)
    }

    /// Inspect open turns, unsettled tool calls, unsettled interactions, and
    /// queued inputs folded from the committed prefix.
    pub fn obligations(&self) -> Result<RecoveredObligations, SessionCommitError> {
        self.ensure_healthy()?;
        Ok(self.state.obligations())
    }

    /// Materialize the committed prefix. Used by recovery to rebuild inputs that
    /// were accepted but not yet turned into a turn, and by read-model paths
    /// that must not take the writer lock.
    pub fn committed_facts(&self) -> Result<Vec<SessionFact>, SessionCommitError> {
        self.ensure_healthy()?;
        let mut facts = Vec::new();
        self.journal
            .visit_committed(|batch| {
                facts.extend_from_slice(batch);
                Ok::<(), SessionCommitError>(())
            })
            .map_err(|error| match error {
                BatchReadError::Storage(error) => SessionCommitError::Storage(error),
                BatchReadError::Visitor(error) => error,
            })?;
        Ok(facts)
    }

    /// Arm a one-shot durability fault for the next batch commit on this owner.
    pub fn inject_fault_once(&mut self, point: BatchFaultPoint) {
        self.journal.inject_fault_once(point);
    }

    /// Read and hash-verify a session-scoped content blob.
    pub fn get_blob(
        &self,
        reference: &crate::session_fact_v2::ContentRef,
        max_bytes: usize,
    ) -> Result<Vec<u8>, SessionCommitError> {
        self.ensure_healthy()?;
        let bytes = self
            .blobs
            .get_bounded(reference, max_bytes)
            .map_err(|e| SessionCommitError::Storage(e.into()))?;
        if crate::canonical::sha256_content_hash(&bytes) != *reference.hash() {
            return Err(SessionCommitError::Storage(
                CanonicalError::CommitRecoveryRequired("blob hash mismatch".into()),
            ));
        }
        Ok(bytes)
    }

    /// Prepare content before a proposal. Content is uncommitted until a fact
    /// referencing it is in a successful batch; unused content may be collected.
    pub fn put_blob(
        &self,
        bytes: &[u8],
    ) -> Result<crate::session_fact_v2::ContentRef, SessionCommitError> {
        self.ensure_healthy()?;
        self.blobs
            .put_durable(bytes)
            .map_err(|e| SessionCommitError::Storage(e.into()))
    }

    /// Validate a candidate context before writing, then install it only after
    /// a confirmed commit. The writer is fenced on any uncertain result.
    pub fn commit(
        &mut self,
        batch_id: &str,
        expected_head: u64,
        facts: Vec<SessionFact>,
    ) -> Result<BatchReceipt, SessionCommitError> {
        self.ensure_healthy()?;
        // A previously committed retry must not fold its messages twice.
        let previous = self.journal.lookup(batch_id)?.is_some();
        let candidate = if !previous {
            let mut candidate = self.context.clone();
            let mut state = self.state.clone();
            let mut staged_facts = Vec::with_capacity(facts.len());
            if expected_head != self.context.head() {
                return Err(SessionCommitError::NotCommitted(
                    CanonicalError::NotWritable("expected head mismatch".into()),
                ));
            }
            if facts.iter().any(|fact| fact.fact_seq != 0) {
                return Err(SessionCommitError::NotCommitted(
                    CanonicalError::NotWritable("proposal fact sequence must be zero".into()),
                ));
            }
            for (index, fact) in facts.iter().enumerate() {
                let mut staged = fact.clone();
                staged.fact_seq = expected_head.checked_add(index as u64 + 1).ok_or(
                    SessionCommitError::NotCommitted(CanonicalError::FactSeqExhausted),
                )?;
                state.apply(&staged)?;
                candidate.apply(&staged, &self.blobs)?;
                staged_facts.push(staged);
            }
            Some((candidate, state, staged_facts))
        } else {
            None
        };
        let receipt = match self.journal.commit(batch_id, expected_head, facts) {
            Ok(receipt) => receipt,
            Err(BatchCommitError::NotCommitted(error)) => {
                return Err(SessionCommitError::NotCommitted(error));
            }
            Err(BatchCommitError::Unknown(error)) => {
                self.fenced = true;
                self.capability_live
                    .store(false, std::sync::atomic::Ordering::Release);
                return Err(SessionCommitError::Unknown(error));
            }
        };
        if previous {
            return Ok(receipt);
        }
        if let Some((candidate, state, staged_facts)) = candidate {
            self.context = candidate;
            self.state = state;
            for fact in &staged_facts {
                let deltas = self.projections.apply(fact);
                match projection_events_for_fact(fact, &deltas) {
                    Ok(mut events) => {
                        match projection_replaceable_events_for_fact(fact, &deltas) {
                            Ok(replaceables) => events.extend(replaceables),
                            Err(error) => log::warn!(
                                "committed fact {} produced invalid replaceable events: {error}",
                                fact.fact_seq
                            ),
                        }
                        publish_projection(&self.session_dir, fact, &events);
                    }
                    // Durability already succeeded; the display gap must be
                    // visible instead of silently dropped.
                    Err(error) => log::error!(
                        "committed fact {} could not be projected: {error}",
                        fact.fact_seq
                    ),
                }
            }
        }
        Ok(receipt)
    }

    /// Commit one batch and issue one-use in-process dispatch authority only
    /// for intents newly committed by this call. Replays and post-crash
    /// reconciliation deliberately issue none: dispatch may already have run.
    pub fn commit_with_effect(
        &mut self,
        batch_id: &str,
        expected_head: u64,
        facts: Vec<SessionFact>,
        effect_event_id: &str,
    ) -> Result<(BatchReceipt, Option<CommittedExecution>), SessionCommitError> {
        self.ensure_healthy()?;
        let previous = self.journal.lookup(batch_id)?.is_some();
        let mut staged_state = self.state.clone();
        let mut selected = None;
        for (index, fact) in facts.iter().enumerate() {
            let mut staged = fact.clone();
            staged.fact_seq = expected_head.checked_add(index as u64 + 1).ok_or(
                SessionCommitError::NotCommitted(CanonicalError::FactSeqExhausted),
            )?;
            if !previous {
                staged_state.apply(&staged)?;
            }
            if fact.event_id.as_str() == effect_event_id {
                let (effect_id, generation) = match &fact.payload {
                    // Generation is the committed attempt, not a constant: a
                    // retry inside the same round produces a different
                    // authority even when the round repeats.
                    FactPayload::ModelRoundStarted(round) => {
                        let attempt = round
                            .identity
                            .as_ref()
                            .map_or(round.round, |identity| identity.attempt);
                        (
                            format!("model:{}", fact.event_id),
                            u64::from(attempt).max(1),
                        )
                    }
                    // A tool execution id is already unique per declaration; a
                    // retry of the same execution is a reconciliation, not a
                    // new generation.
                    FactPayload::ToolIntent(intent) => {
                        (format!("tool:{}", intent.execution_id.as_str()), 1_u64)
                    }
                    _ => {
                        return Err(SessionCommitError::NotCommitted(
                            CanonicalError::NotWritable(
                                "only isolated model effects are currently supported".into(),
                            ),
                        ));
                    }
                };
                if selected.is_some() {
                    return Err(SessionCommitError::NotCommitted(
                        CanonicalError::NotWritable("ambiguous effect event".into()),
                    ));
                }
                let input = if previous {
                    None
                } else {
                    staged_state.active_input().cloned()
                };
                if !previous
                    && matches!(fact.payload, FactPayload::ModelRoundStarted(_))
                    && (index != 0 || expected_head != self.context.head())
                {
                    return Err(SessionCommitError::NotCommitted(
                        CanonicalError::NotWritable(
                            "model intent must use current committed context".into(),
                        ),
                    ));
                }
                selected = Some((staged, effect_id, generation, input));
            }
        }
        let (fact, effect_id, generation, input) = selected.ok_or_else(|| {
            SessionCommitError::NotCommitted(CanonicalError::NotWritable(
                "effect event not in batch".into(),
            ))
        })?;
        if !previous && self.issued.contains(&(effect_id.clone(), generation)) {
            return Err(SessionCommitError::NotCommitted(
                CanonicalError::NotWritable("effect already issued".into()),
            ));
        }
        let prepared = if !previous {
            let input = input.ok_or_else(|| {
                SessionCommitError::NotCommitted(CanonicalError::NotWritable(
                    "effect has no active input".into(),
                ))
            })?;
            let digest: [u8; 32] =
                Sha256::digest(serde_json::to_vec(&fact).map_err(CanonicalError::from)?).into();
            let request = if let FactPayload::ModelRoundStarted(round) = &fact.payload {
                let messages = self.context.request_messages()?.to_vec();
                let hash = crate::canonical::sha256_content_hash(
                    &serde_json::to_vec(&messages).map_err(CanonicalError::from)?,
                );
                if round.context_revision != expected_head || round.request_hash != hash {
                    return Err(SessionCommitError::NotCommitted(
                        CanonicalError::NotWritable(
                            "model intent context head or digest mismatch".into(),
                        ),
                    ));
                }
                Some(messages)
            } else {
                None
            };
            // A tool intent carries its authorized arguments by reference. They
            // are resolved and hash-checked here so the runner cannot observe a
            // different revision than the one that was committed.
            let tool_args = if let FactPayload::ToolIntent(intent) = &fact.payload {
                let reference = intent.effective_args_ref.as_ref().ok_or_else(|| {
                    SessionCommitError::NotCommitted(CanonicalError::NotWritable(
                        "tool intent has no effective arguments".into(),
                    ))
                })?;
                let bytes = self
                    .blobs
                    .get_bounded(reference, MAX_EFFECT_ARGS)
                    .map_err(|error| SessionCommitError::Storage(error.into()))?;
                if crate::canonical::sha256_content_hash(&bytes) != *reference.hash() {
                    return Err(SessionCommitError::NotCommitted(
                        CanonicalError::NotWritable(
                            "tool arguments do not match their committed reference".into(),
                        ),
                    ));
                }
                Some(bytes)
            } else {
                None
            };
            Some((input, digest, request, tool_args))
        } else {
            None
        };
        let receipt = self.commit(batch_id, expected_head, facts)?;
        let Some((input, digest, request_messages, tool_args)) = prepared else {
            return Ok((receipt, None));
        };
        self.issued.insert((effect_id.clone(), generation));
        self.issued_order.push_back((effect_id.clone(), generation));
        while self.issued_order.len() > ISSUED_HISTORY {
            if let Some(expired) = self.issued_order.pop_front() {
                self.issued.remove(&expired);
            }
        }
        Ok((
            receipt,
            Some(CommittedExecution {
                identity: EffectIdentity {
                    actor: ActorIdentity {
                        session_id: self.session_id.as_str().to_owned(),
                        incarnation: self.incarnation,
                    },
                    input_id: input.as_str().to_owned(),
                    effect_id,
                    generation,
                    intent_digest: digest,
                },
                fact,
                request_messages,
                tool_args,
                live: self.capability_live.clone(),
            }),
        ))
    }

    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }
    pub fn log_id(&self) -> &LogId {
        &self.log_id
    }

    /// The incarnation a host must adopt on its supervisor before authorizing
    /// capabilities minted by this owner.
    pub fn incarnation(&self) -> u64 {
        self.incarnation
    }

    fn ensure_healthy(&self) -> Result<(), SessionCommitError> {
        if self.fenced {
            Err(SessionCommitError::Fenced)
        } else {
            Ok(())
        }
    }
}

fn next_incarnation() -> Result<u64, SessionCommitError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        current.checked_add(1)
    })
    .map_err(|_| {
        SessionCommitError::NotCommitted(CanonicalError::NotWritable(
            "actor incarnation exhausted".into(),
        ))
    })
}
