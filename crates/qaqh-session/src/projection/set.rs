//! Aggregate of all canonical-fact projections.

use serde::{Deserialize, Serialize};

use crate::session_fact_v2::{ProjectionPayload, ProjectionSlot, SessionFact, projection_slots};

use super::{
    ControlProjection, ControlSnapshot, ConversationProjection, ConversationSnapshot,
    MailboxProjection, MailboxSnapshot, Projection, ResourceProjection, ResourceSnapshot,
    SessionMetaProjection, SessionMetaSnapshot, TeamProjection, TeamSnapshot, TimelineProjection,
    TimelineSnapshot,
};

/// A reliable projection delta paired with its frozen static slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionSetDelta {
    pub slot: ProjectionSlot,
    pub payload: ProjectionPayload,
}

/// Rebuildable snapshots owned by one [`ProjectionSet`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionSetSnapshot {
    pub conversation: ConversationSnapshot,
    pub timeline: TimelineSnapshot,
    pub control: ControlSnapshot,
    pub resources: ResourceSnapshot,
    pub meta: SessionMetaSnapshot,
    #[serde(default)]
    pub mailbox: MailboxSnapshot,
    #[serde(default)]
    pub team: TeamSnapshot,
}

/// All projections for one session, applied in frozen slot order.
#[derive(Debug, Default)]
pub struct ProjectionSet {
    pub conversation: ConversationProjection,
    pub timeline: TimelineProjection,
    pub control: ControlProjection,
    pub resources: ResourceProjection,
    pub meta: SessionMetaProjection,
    pub mailbox: MailboxProjection,
    pub team: TeamProjection,
}

impl ProjectionSet {
    /// Apply one canonical fact to every projection.
    ///
    /// Every reducer sees the fact so it can maintain cross-slot state.
    /// Only deltas whose slot appears in [`projection_slots`] are returned as
    /// reliable projection output.
    pub fn apply(&mut self, fact: &SessionFact) -> Vec<ProjectionSetDelta> {
        let allowed_slots = projection_slots(&fact.payload);
        let mut deltas = Vec::with_capacity(allowed_slots.len());

        if let Some(delta) = self.conversation.apply(fact) {
            push_allowed(
                &mut deltas,
                allowed_slots,
                ProjectionSlot::Conversation,
                ProjectionPayload::ConversationDelta(delta),
            );
        }
        if let Some(delta) = self.timeline.apply(fact) {
            push_allowed(
                &mut deltas,
                allowed_slots,
                ProjectionSlot::Timeline,
                ProjectionPayload::TimelineDelta(delta),
            );
        }
        if let Some(delta) = self.control.apply(fact) {
            push_allowed(
                &mut deltas,
                allowed_slots,
                ProjectionSlot::Control,
                ProjectionPayload::ControlDelta(delta),
            );
        }
        if let Some(delta) = self.resources.apply(fact) {
            push_allowed(
                &mut deltas,
                allowed_slots,
                ProjectionSlot::Resources,
                ProjectionPayload::ResourceDelta(delta),
            );
        }
        if let Some(delta) = self.meta.apply(fact) {
            push_allowed(
                &mut deltas,
                allowed_slots,
                ProjectionSlot::Meta,
                ProjectionPayload::MetaDelta(delta),
            );
        }
        if let Some(delta) = self.mailbox.apply(fact) {
            push_allowed(
                &mut deltas,
                allowed_slots,
                ProjectionSlot::Mailbox,
                ProjectionPayload::MailboxDelta(delta),
            );
        }
        if let Some(delta) = self.team.apply(fact) {
            push_allowed(
                &mut deltas,
                allowed_slots,
                ProjectionSlot::Team,
                ProjectionPayload::TeamDelta(delta),
            );
        }

        deltas
    }

    pub fn snapshot(&self) -> ProjectionSetSnapshot {
        ProjectionSetSnapshot {
            conversation: self.conversation.snapshot(),
            timeline: self.timeline.snapshot(),
            control: self.control.snapshot(),
            resources: self.resources.snapshot(),
            meta: self.meta.snapshot(),
            mailbox: self.mailbox.snapshot(),
            team: self.team.snapshot(),
        }
    }

    pub fn last_fact_seq(&self) -> u64 {
        [
            self.conversation.last_fact_seq(),
            self.timeline.last_fact_seq(),
            self.control.last_fact_seq(),
            self.resources.last_fact_seq(),
            self.meta.last_fact_seq(),
            self.mailbox.last_fact_seq(),
            self.team.last_fact_seq(),
        ]
        .into_iter()
        .max()
        .unwrap_or_default()
    }

    pub fn rebuild(facts: impl Iterator<Item = SessionFact>) -> Self {
        let mut set = Self::default();
        for fact in facts {
            let _ = set.apply(&fact);
        }
        set
    }
}

fn push_allowed(
    deltas: &mut Vec<ProjectionSetDelta>,
    allowed_slots: &[ProjectionSlot],
    slot: ProjectionSlot,
    payload: ProjectionPayload,
) {
    if allowed_slots.contains(&slot) {
        deltas.push(ProjectionSetDelta { slot, payload });
    }
}

impl ProjectionSetDelta {
    pub fn payload_slot(&self) -> Option<ProjectionSlot> {
        match &self.payload {
            ProjectionPayload::ConversationDelta(_) => Some(ProjectionSlot::Conversation),
            ProjectionPayload::TimelineDelta(_) => Some(ProjectionSlot::Timeline),
            ProjectionPayload::ControlDelta(_) => Some(ProjectionSlot::Control),
            ProjectionPayload::ResourceDelta(_) => Some(ProjectionSlot::Resources),
            ProjectionPayload::MetaDelta(_) => Some(ProjectionSlot::Meta),
            ProjectionPayload::MailboxDelta(_) => Some(ProjectionSlot::Mailbox),
            ProjectionPayload::TeamDelta(_) => Some(ProjectionSlot::Team),
            ProjectionPayload::AuditRef(_) | ProjectionPayload::Unknown(_) => None,
        }
    }
}
