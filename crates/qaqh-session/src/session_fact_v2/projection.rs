//! Static `FactPayload` to `ProjectionSlot` registry.
//!
//! The slot list is the canonical index source for reliable projection events.
//! It is intentionally independent from reducer execution order.

use super::types::{FactPayload, ProjectionSlot};

pub type ProjectionIndex = u16;

/// Reliable projection indices reserve `u16::MAX` for snapshot END_OF_FACT.
pub const MAX_RELIABLE_PROJECTION_INDEX: ProjectionIndex = u16::MAX - 1;
pub const END_OF_FACT: ProjectionIndex = u16::MAX;

const CONVERSATION_TIMELINE: &[ProjectionSlot] =
    &[ProjectionSlot::Conversation, ProjectionSlot::Timeline];
const CONVERSATION_META: &[ProjectionSlot] = &[ProjectionSlot::Conversation, ProjectionSlot::Meta];
const CONVERSATION_TIMELINE_MAILBOX_TEAM: &[ProjectionSlot] = &[
    ProjectionSlot::Conversation,
    ProjectionSlot::Timeline,
    ProjectionSlot::Mailbox,
    ProjectionSlot::Team,
];
const MAILBOX_TEAM: &[ProjectionSlot] = &[ProjectionSlot::Mailbox, ProjectionSlot::Team];
const CONVERSATION_CONTROL_TEAM: &[ProjectionSlot] = &[
    ProjectionSlot::Conversation,
    ProjectionSlot::Control,
    ProjectionSlot::Team,
];
const CONTROL_META_TEAM: &[ProjectionSlot] = &[
    ProjectionSlot::Control,
    ProjectionSlot::Meta,
    ProjectionSlot::Team,
];
const CONTROL_RESOURCES_TEAM: &[ProjectionSlot] = &[
    ProjectionSlot::Control,
    ProjectionSlot::Resources,
    ProjectionSlot::Team,
];
const CONTROL_ONLY: &[ProjectionSlot] = &[ProjectionSlot::Control];
const META_ONLY: &[ProjectionSlot] = &[ProjectionSlot::Meta];
const RESOURCES_ONLY: &[ProjectionSlot] = &[ProjectionSlot::Resources];

impl ProjectionSlot {
    pub const fn as_u16(self) -> ProjectionIndex {
        self as ProjectionIndex
    }
}

/// Return the fixed, slot-sorted projection registry for one canonical fact.
pub fn projection_slots(payload: &FactPayload) -> &'static [ProjectionSlot] {
    match payload {
        FactPayload::SessionCreated(_) => CONTROL_META_TEAM,
        FactPayload::InputAccepted(_) => CONVERSATION_TIMELINE_MAILBOX_TEAM,
        FactPayload::TurnStarted(_) => CONVERSATION_CONTROL_TEAM,
        FactPayload::ModelRoundStarted(_) => CONTROL_ONLY,
        FactPayload::AssistantBlockSealed(_) => CONVERSATION_TIMELINE,
        FactPayload::ToolCallDeclared(_) => CONVERSATION_TIMELINE,
        FactPayload::ToolIntent(_) => CONTROL_ONLY,
        FactPayload::ToolFinished(_) => CONVERSATION_TIMELINE,
        FactPayload::InteractionRequested(_) => CONTROL_ONLY,
        FactPayload::InteractionResolved(_) => CONTROL_ONLY,
        FactPayload::InteractionExpired(_) => CONTROL_ONLY,
        FactPayload::DriverChanged(_) => CONTROL_ONLY,
        FactPayload::TurnFinished(_) => CONVERSATION_CONTROL_TEAM,
        FactPayload::TurnInterrupted(_) => CONVERSATION_CONTROL_TEAM,
        FactPayload::SessionRecovered(_) => CONTROL_META_TEAM,
        FactPayload::CompactionApplied(_) => CONVERSATION_META,
        FactPayload::SessionMetadataChanged(_) => META_ONLY,
        FactPayload::SessionTitleChanged(_) => META_ONLY,
        FactPayload::SessionDeleted(_) => META_ONLY,
        FactPayload::WorkspaceResourceChanged(_) => RESOURCES_ONLY,
        FactPayload::SubagentSpawned(_) => CONTROL_RESOURCES_TEAM,
        FactPayload::SubagentFinished(_) => CONTROL_RESOURCES_TEAM,
        FactPayload::InterAgentCommunication(_) => MAILBOX_TEAM,
    }
}
