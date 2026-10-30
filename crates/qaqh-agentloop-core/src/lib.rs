//! Transport-independent primitives for a session's single decision owner.
//!
//! This crate must not assemble model context, execute tools, write storage, or
//! depend on the Ringing wire protocol. Those responsibilities belong to
//! separately constructed services. Task completions are correlated against
//! identities captured by the supervising host, never against IDs supplied by
//! a provider or tool.
//!
//! Dispatch authority is minted here, not exported as an open trait: a
//! supervising owner records a committed intent with
//! [`supervisor::Supervisor::authorize`] and receives an opaque, single-use
//! [`supervisor::DispatchTicket`]. Forging a dispatch therefore requires owning
//! the supervisor, and an identity from an older actor incarnation is refused.
//! A completion proves a result, never that work stopped.

pub mod driver;
pub mod commit_gate;
pub mod mailbox;
pub mod supervisor;

/// Identity of the session actor that dispatched an effect.
///
/// A newly loaded actor receives a different incarnation even when it serves
/// the same session. An earlier completion is evidence for the old task, not
/// permission to mutate the newly loaded actor.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActorIdentity {
    pub session_id: String,
    pub incarnation: u64,
}

/// Identity captured by the supervisor when an effect is dispatched.
///
/// `generation` distinguishes successive attempts at the same logical
/// effect. The digest binds the completion to the exact committed intent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EffectIdentity {
    pub actor: ActorIdentity,
    pub input_id: String,
    pub effect_id: String,
    pub generation: u64,
    pub intent_digest: [u8; 32],
}

/// The owner must check the complete identity before consuming a completion.
/// A mismatch does not prove a task stopped and does not release its resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionMatch {
    Current,
    DifferentSession,
    StaleIncarnation,
    DifferentInput,
    DifferentEffect,
    DifferentGeneration,
    DifferentIntent,
}

/// How far a dispatched effect actually got before its result was lost.
///
/// Only [`DispatchEvidence::NotStarted`] may be dispatched again without
/// reconciliation. Dropping a future, cancelling a join, or losing a delivery
/// is never evidence that an external side effect did not happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchEvidence {
    /// The capability was fenced before any work was submitted. Nothing
    /// observable was launched under this identity.
    NotStarted,
    /// Work was submitted and its result is missing or inconclusive.
    StartedUnknown,
    /// A definitive terminal result was observed for this exact identity.
    StartedTerminal,
}

impl EffectIdentity {
    /// Compare a supervisor-owned task identity to the currently expected one.
    pub fn match_expected(&self, expected: &Self) -> CompletionMatch {
        if self.actor.session_id != expected.actor.session_id {
            CompletionMatch::DifferentSession
        } else if self.actor.incarnation != expected.actor.incarnation {
            CompletionMatch::StaleIncarnation
        } else if self.input_id != expected.input_id {
            CompletionMatch::DifferentInput
        } else if self.effect_id != expected.effect_id {
            CompletionMatch::DifferentEffect
        } else if self.generation != expected.generation {
            CompletionMatch::DifferentGeneration
        } else if self.intent_digest != expected.intent_digest {
            CompletionMatch::DifferentIntent
        } else {
            CompletionMatch::Current
        }
    }
}
