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
const CONVERSATION_CONTROL: &[ProjectionSlot] =
    &[ProjectionSlot::Conversation, ProjectionSlot::Control];
const CONVERSATION_META: &[ProjectionSlot] = &[ProjectionSlot::Conversation, ProjectionSlot::Meta];
const CONTROL_META: &[ProjectionSlot] = &[ProjectionSlot::Control, ProjectionSlot::Meta];
const CONTROL_RESOURCES: &[ProjectionSlot] = &[ProjectionSlot::Control, ProjectionSlot::Resources];
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
        FactPayload::SessionCreated(_) => CONTROL_META,
        FactPayload::InputAccepted(_) => CONVERSATION_TIMELINE,
        FactPayload::TurnStarted(_) => CONVERSATION_CONTROL,
        FactPayload::ModelRoundStarted(_) => CONTROL_ONLY,
        FactPayload::AssistantBlockSealed(_) => CONVERSATION_TIMELINE,
        FactPayload::ToolCallDeclared(_) => CONVERSATION_TIMELINE,
        FactPayload::ToolIntent(_) => CONTROL_ONLY,
        FactPayload::ToolFinished(_) => CONVERSATION_TIMELINE,
        FactPayload::InteractionRequested(_) => CONTROL_ONLY,
        FactPayload::InteractionResolved(_) => CONTROL_ONLY,
        FactPayload::InteractionExpired(_) => CONTROL_ONLY,
        FactPayload::DriverChanged(_) => CONTROL_ONLY,
        FactPayload::TurnFinished(_) => CONVERSATION_CONTROL,
        FactPayload::TurnInterrupted(_) => CONVERSATION_CONTROL,
        FactPayload::SessionRecovered(_) => CONTROL_META,
        FactPayload::CompactionApplied(_) => CONVERSATION_META,
        FactPayload::SessionMetadataChanged(_) => META_ONLY,
        FactPayload::SessionTitleChanged(_) => META_ONLY,
        FactPayload::SessionDeleted(_) => META_ONLY,
        FactPayload::WorkspaceResourceChanged(_) => RESOURCES_ONLY,
        FactPayload::SubagentSpawned(_) => CONTROL_RESOURCES,
        FactPayload::SubagentFinished(_) => CONTROL_RESOURCES,
    }
}
