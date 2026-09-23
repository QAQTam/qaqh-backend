//! Canonical fact to projection-event publishing and reliable replay.

use std::collections::HashSet;

use qaqh_domain::RingingChannel;
use serde::{Deserialize, Serialize};

use crate::session_fact_v2::{
    EventId, FactPayload, LogId, MAX_SAFE_FACT_SEQ, ProjectionEvent, ProjectionSlot,
    ReliableCursor, ResetReason, ResetRequired, SessionFact, StreamKey, ValidationError,
    projection_slots,
};

use super::ProjectionSetDelta;

/// Reliable replay window boundary for one canonical log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayWindow {
    pub log_id: LogId,
    pub earliest_available_fact_seq: u64,
    pub snapshot_cursor: Option<ReliableCursor>,
}

/// Result of replaying a reliable event set from a caller-owned buffer/store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplayOutcome {
    Delivered { events: Vec<ProjectionEvent> },
    ResetRequired(ResetRequired),
}

/// Build the reliable projection events for one fact.
pub fn projection_events_for_fact(
    fact: &SessionFact,
    deltas: &[ProjectionSetDelta],
) -> Result<Vec<ProjectionEvent>, ValidationError> {
    deltas
        .iter()
        .map(|delta| {
            if !projection_slots(&fact.payload).contains(&delta.slot) {
                return Err(ValidationError::InvalidField {
                    field: "projection_slot",
                    message: "projection slot is not registered for this fact".into(),
                });
            }
            let payload_slot =
                delta
                    .payload_slot()
                    .ok_or_else(|| ValidationError::InvalidField {
                        field: "payload",
                        message: "payload does not participate in the reliable cursor".into(),
                    })?;
            if payload_slot != delta.slot {
                return Err(ValidationError::InvalidField {
                    field: "projection_slot",
                    message: "projection slot must match the typed payload family".into(),
                });
            }
            let stream_key = projection_stream_key(fact, delta.slot);
            ProjectionEvent::reliable(
                projection_event_id(fact, delta.slot),
                fact,
                stream_key,
                delta.slot,
                delta.payload.clone(),
            )
        })
        .collect()
}

/// Stable auxiliary event id for a source fact and projection slot.
pub fn projection_event_id(fact: &SessionFact, slot: ProjectionSlot) -> EventId {
    EventId::new(format!("{}:{:02}", fact.event_id.as_str(), slot.as_u16()))
}

/// Return the frozen wire stream for a fact/slot pair.
pub fn projection_stream_key(fact: &SessionFact, slot: ProjectionSlot) -> StreamKey {
    let channel = match slot {
        ProjectionSlot::Conversation => RingingChannel::Conversation,
        ProjectionSlot::Timeline => match &fact.payload {
            FactPayload::ToolCallDeclared(_) | FactPayload::ToolFinished(_) => RingingChannel::Tool,
            _ => RingingChannel::Conversation,
        },
        ProjectionSlot::Control => match &fact.payload {
            FactPayload::ToolIntent(_) => RingingChannel::Tool,
            _ => RingingChannel::Control,
        },
        ProjectionSlot::Resources => match &fact.payload {
            FactPayload::WorkspaceResourceChanged(_) => RingingChannel::Tool,
            _ => RingingChannel::Control,
        },
        ProjectionSlot::Meta => RingingChannel::Control,
    };
    StreamKey::Channel(channel)
}

/// Replay reliable events from a caller-owned event set.
pub fn replay_reliable(
    events: impl IntoIterator<Item = ProjectionEvent>,
    since: Option<&ReliableCursor>,
    window: &ReplayWindow,
) -> Result<ReplayOutcome, ValidationError> {
    validate_window(window)?;
    if let Some(since) = since {
        since.validate()?;
        if since.log_id != window.log_id {
            return Ok(reset_required(
                window,
                ResetReason::LogIdMismatch,
                window.snapshot_cursor.clone(),
            ));
        }
        if since.fact_seq < window.earliest_available_fact_seq {
            return Ok(
                if let Some(snapshot_cursor) = window.snapshot_cursor.clone() {
                    reset_required(window, ResetReason::CursorExpired, Some(snapshot_cursor))
                } else {
                    reset_required(window, ResetReason::SnapshotMissing, None)
                },
            );
        }
    }

    let mut reliable = Vec::new();
    for event in events {
        event.validate()?;
        let Some(cursor) = event.delivery.reliable_cursor().cloned() else {
            continue;
        };
        if cursor.log_id != window.log_id {
            return Err(ValidationError::InvalidField {
                field: "event.delivery.cursor.log_id",
                message: "reliable event log_id must match the replay window".into(),
            });
        }
        reliable.push((cursor, event));
    }
    reliable.sort_by_key(|(cursor, _)| (cursor.fact_seq, cursor.projection_index));

    let mut seen = HashSet::new();
    let mut delivered = Vec::new();
    for (cursor, event) in reliable {
        if !seen.insert((cursor.fact_seq, cursor.projection_index)) {
            continue;
        }
        if let Some(since) = since
            && !cursor.is_after(since)?
        {
            continue;
        }
        delivered.push(event);
    }

    Ok(ReplayOutcome::Delivered { events: delivered })
}

fn validate_window(window: &ReplayWindow) -> Result<(), ValidationError> {
    if window.earliest_available_fact_seq == 0
        || window.earliest_available_fact_seq > MAX_SAFE_FACT_SEQ
    {
        return Err(ValidationError::InvalidFactSeq {
            fact_seq: window.earliest_available_fact_seq,
        });
    }
    if let Some(snapshot_cursor) = &window.snapshot_cursor {
        snapshot_cursor.validate_snapshot_cursor()?;
        if snapshot_cursor.log_id != window.log_id {
            return Err(ValidationError::InvalidField {
                field: "snapshot_cursor.log_id",
                message: "snapshot cursor must belong to the replay window log".into(),
            });
        }
    }
    Ok(())
}

fn reset_required(
    window: &ReplayWindow,
    reason: ResetReason,
    snapshot_cursor: Option<ReliableCursor>,
) -> ReplayOutcome {
    ReplayOutcome::ResetRequired(ResetRequired {
        log_id: window.log_id.clone(),
        snapshot_cursor,
        reason,
    })
}
