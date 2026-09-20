//! Rebuildable projection contract and canonical reader harness.

mod control;
mod conversation;
mod meta;
mod resource;
mod timeline;

pub use control::{
    ControlInteractionResolution, ControlInteractionState, ControlProjection, ControlRoundState,
    ControlSnapshot, ControlSubagentState, ControlToolState,
};
pub use conversation::{
    ConversationAssistantBlockState, ConversationCompactionState, ConversationContextEntry,
    ConversationContextKind, ConversationInputState, ConversationProjection, ConversationSnapshot,
    ConversationToolCallState, ConversationToolResultState, ConversationTurnOutcome,
    ConversationTurnState,
};
pub use meta::{SessionMetaProjection, SessionMetaSnapshot};
pub use resource::{GraphEdgeState, ResourceProjection, ResourceSnapshot, WorkspaceResourceState};
pub use timeline::{
    TimelineAssistantBlockState, TimelineCompactionState, TimelineEntry, TimelineEntryKind,
    TimelineInputState, TimelineProjection, TimelineSnapshot, TimelineToolCallState,
    TimelineToolResultState, TimelineTurnInterruptedState, TimelineTurnTerminalState,
};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::canonical::{CanonicalError, CommittedFactReader};
use crate::session_fact_v2::SessionFact;

/// Pure, rebuildable state derived from canonical facts.
///
/// The default `rebuild` implementation is intentionally incremental and
/// delegates to `apply`; implementations may override it only if they prove
/// the same result.
pub trait Projection: Default + Send {
    type Snapshot: Serialize + DeserializeOwned;
    type Delta: Serialize + Send;

    fn apply(&mut self, fact: &SessionFact) -> Option<Self::Delta>;
    fn snapshot(&self) -> Self::Snapshot;
    fn last_fact_seq(&self) -> u64;

    fn rebuild(facts: impl Iterator<Item = SessionFact>) -> Self {
        let mut projection = Self::default();
        for fact in facts {
            let _ = projection.apply(&fact);
        }
        projection
    }
}

/// Rebuild a projection exclusively from the reader's committed prefix.
pub fn rebuild_from_reader<P: Projection>(
    reader: &CommittedFactReader,
) -> Result<P, CanonicalError> {
    Ok(P::rebuild(reader.read_all()?.into_iter()))
}
