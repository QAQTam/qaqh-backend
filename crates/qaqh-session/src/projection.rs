//! Rebuildable projection contract and canonical reader harness.

mod agent_graph;
mod control;
mod conversation;
mod mailbox;
mod meta;
mod replay;
mod resource;
mod set;
mod sink;
mod timeline;

pub use agent_graph::{
    AgentGraphEdge, AgentGraphEdgeStatus, AgentGraphError, AgentGraphNode, AgentGraphSnapshot,
    AgentGraphStore,
};
pub use control::{
    ControlDriverState, ControlInteractionResolution, ControlInteractionState, ControlProjection,
    ControlRoundState, ControlSnapshot, ControlSubagentState, ControlToolState,
};
pub use conversation::{
    ConversationAssistantBlockState, ConversationCompactionState, ConversationContextEntry,
    ConversationContextKind, ConversationInputState, ConversationProjection, ConversationSnapshot,
    ConversationToolCallState, ConversationToolResultState, ConversationTurnOutcome,
    ConversationTurnState,
};
pub use mailbox::{MailboxProjection, MailboxSnapshot};
pub use meta::{SessionMetaProjection, SessionMetaSnapshot};
pub use replay::{
    ReplayOutcome, ReplayWindow, projection_event_id, projection_events_for_fact,
    projection_replaceable_events_for_fact, projection_stream_key, replaceable_identity,
    replay_reliable,
};
pub use resource::{GraphEdgeState, ResourceProjection, ResourceSnapshot, WorkspaceResourceState};
pub use set::{ProjectionSet, ProjectionSetDelta, ProjectionSetSnapshot};
pub(crate) use sink::publish_projection;
pub use sink::{ProjectionSink, install_projection_sink};
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
