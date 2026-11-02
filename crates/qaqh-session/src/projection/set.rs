//! Aggregate of all canonical-fact projections.

use serde::{Deserialize, Serialize};

use crate::session_fact_v2::{ProjectionPayload, ProjectionSlot, SessionFact, projection_slots};

use super::{
    ControlProjection, ControlSnapshot, ConversationProjection, ConversationSnapshot,
    MailboxProjection, MailboxSnapshot, Projection, ResourceProjection, ResourceSnapshot,
    SessionMetaProjection, SessionMetaSnapshot, TeamProjection, TeamSnapshot,
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
    pub control: ControlProjection,
    pub resources: ResourceProjection,
    pub meta: SessionMetaProjection,
    pub mailbox: MailboxProjection,
    pub team: TeamProjection,
}

impl ProjectionSet {
    /// Approximate owned heap and text payload for the in-memory projection.
    /// Calculated from borrowed state without materializing a snapshot.
    pub fn memory_usage_estimate(&self) -> (u64, u64, u64) {
        let conversation = self.conversation.snapshot_for_memory();
        let (control_entries, control_payload, control_heap) = self.control.memory_usage_estimate();
        let (resource_entries, resource_payload, resource_heap) =
            self.resources.memory_usage_estimate();
        let (meta_entries, meta_payload, meta_heap) = self.meta.memory_usage_estimate();
        let (mailbox_entries, mailbox_payload, mailbox_heap) = self.mailbox.memory_usage_estimate();
        let (team_entries, team_payload, team_heap) = self.team.memory_usage_estimate();
        let entries = control_entries
            .saturating_add(resource_entries)
            .saturating_add(meta_entries)
            .saturating_add(mailbox_entries)
            .saturating_add(team_entries)
            .saturating_add(conversation.turn_count)
            .saturating_add(conversation.context_count);
        let payload = control_payload
            .saturating_add(resource_payload)
            .saturating_add(meta_payload)
            .saturating_add(mailbox_payload)
            .saturating_add(team_payload)
            .saturating_add(conversation.payload_bytes);
        let heap = (std::mem::size_of::<ProjectionSet>() as u64)
            .saturating_add(conversation.turn_capacity_bytes)
            .saturating_add(conversation.context_capacity_bytes)
            .saturating_add(control_heap)
            .saturating_add(resource_heap)
            .saturating_add(meta_heap)
            .saturating_add(mailbox_heap)
            .saturating_add(team_heap);
        (entries, payload, heap)
    }

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
            ProjectionPayload::ControlDelta(_) => Some(ProjectionSlot::Control),
            ProjectionPayload::ResourceDelta(_) => Some(ProjectionSlot::Resources),
            ProjectionPayload::MetaDelta(_) => Some(ProjectionSlot::Meta),
            ProjectionPayload::MailboxDelta(_) => Some(ProjectionSlot::Mailbox),
            ProjectionPayload::TeamDelta(_) => Some(ProjectionSlot::Team),
            // `OperationFailed` 是运行期通知而非 fact 投影，不归属任何 slot。
            ProjectionPayload::AuditRef(_)
            | ProjectionPayload::Unknown(_)
            | ProjectionPayload::OperationFailed(_) => None,
        }
    }
}
