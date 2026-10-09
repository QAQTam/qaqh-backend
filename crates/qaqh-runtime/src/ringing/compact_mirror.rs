//! Worker compaction events → v2 Conversation stream mirror.
//!
//! The compaction engines emit `ConversationEvent::CompactStarted/Progress/
//! Finished`. The v1 Ringing broadcast face is gone (hub-fact-bus 阶段 3d), so
//! without this mirror those events reach no client at all.
//!
//! Mirroring happens here rather than at the emission points for two reasons:
//! the engines stay free of hub plumbing (`RingContext` cannot reach the v2 hub
//! by design), and every producer — auto compact, manual compact, the
//! cancel/skip fallbacks in `loop_outcome` — gets the same v2 shape for free.

use qaqh_domain::{CompactStatus, ConversationEvent, DomainEvent};
use qaqh_session::SessionManager;
use qaqh_session::session_fact_v2::CompactStatus as WireCompactStatus;

use super::v2::{V2HubError, V2ProjectionHub};

/// Minimum new characters since the last published snapshot.
///
/// The v2 delta carries the **cumulative** summary, so one client that misses a
/// frame still renders the full text. Coalescing matters because the hub's live
/// broadcast holds 1024 frames per session and overflowing it pushes every
/// subscriber into a full re-bootstrap reset — one event per provider token
/// would do exactly that.
const PROGRESS_MIN_CHARS: usize = 256;

/// One in-flight compaction's streamed summary.
struct PendingCompact {
    compact_id: String,
    text: String,
    /// Characters of `text` already published as a cumulative snapshot.
    published_chars: usize,
    /// Characters received so far, kept so the threshold check stays O(1).
    received_chars: usize,
}

impl PendingCompact {
    fn new(compact_id: &str) -> Self {
        Self {
            compact_id: compact_id.to_string(),
            text: String::new(),
            published_chars: 0,
            received_chars: 0,
        }
    }

    /// Append a provider chunk; return the cumulative text once enough new
    /// characters have arrived to be worth an event.
    fn record(&mut self, delta: &str) -> Option<String> {
        self.text.push_str(delta);
        self.received_chars += delta.chars().count();
        if self.received_chars - self.published_chars < PROGRESS_MIN_CHARS {
            return None;
        }
        self.published_chars = self.received_chars;
        Some(self.text.clone())
    }

    /// Publishable tail left over when the compaction ends.
    fn take_tail(self) -> Option<(String, String)> {
        if self.received_chars == self.published_chars {
            return None;
        }
        Some((self.compact_id, self.text))
    }
}

pub(crate) struct CompactMirror<'a> {
    hub: Option<&'a V2ProjectionHub>,
    sessions: &'a SessionManager,
    pending: Option<PendingCompact>,
}

impl<'a> CompactMirror<'a> {
    pub(crate) fn new(hub: Option<&'a V2ProjectionHub>, sessions: &'a SessionManager) -> Self {
        Self {
            hub,
            sessions,
            pending: None,
        }
    }

    /// Fold one worker domain event into the v2 conversation stream.
    /// `session_id` is the envelope's seed, which is also the session directory
    /// name — the same routing key the Ringing worker face used.
    pub(crate) fn observe(&mut self, session_id: &str, event: &DomainEvent) {
        let DomainEvent::Conversation(event) = event else {
            return;
        };
        match event {
            ConversationEvent::CompactStarted {
                compact_id,
                turns_total,
                turns_keeping,
            } => {
                // A stranded previous compaction (worker died before its
                // terminal event) gets its tail published before the new opens.
                self.flush(session_id);
                self.pending = Some(PendingCompact::new(compact_id));
                let turns_total = *turns_total;
                let turns_keeping = *turns_keeping;
                self.publish(session_id, move |hub, dir| {
                    hub.publish_compact_started(
                        dir,
                        session_id,
                        compact_id,
                        turns_total,
                        turns_keeping,
                    )
                });
            }
            ConversationEvent::CompactProgress { compact_id, delta } => {
                // `CompactStarted` is missed when the hub session loads late, so
                // open the buffer lazily and the summary still streams.
                if self
                    .pending
                    .as_ref()
                    .is_none_or(|pending| pending.compact_id != *compact_id)
                {
                    self.flush(session_id);
                    self.pending = Some(PendingCompact::new(compact_id));
                }
                let Some(pending) = self.pending.as_mut() else {
                    return;
                };
                let Some(text) = pending.record(delta) else {
                    return;
                };
                let compact_id = compact_id.clone();
                self.publish(session_id, move |hub, dir| {
                    hub.publish_compact_progress(dir, session_id, &compact_id, text)
                });
            }
            ConversationEvent::CompactFinished {
                compact_id,
                status,
                summary_chars,
                turns_compacted,
                turns_removed,
            } => {
                // Tail first, so the card shows the full summary before it
                // collapses into `已压缩`.
                self.flush(session_id);
                let status = wire_status(*status);
                let compact_id = compact_id.clone();
                let summary_chars = *summary_chars;
                let turns_compacted = *turns_compacted;
                let turns_removed = *turns_removed;
                self.publish(session_id, move |hub, dir| {
                    hub.publish_compact_finished(
                        dir,
                        session_id,
                        &compact_id,
                        status,
                        summary_chars,
                        turns_compacted,
                        turns_removed,
                    )
                });
            }
            _ => {}
        }
    }

    fn flush(&mut self, session_id: &str) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let Some((compact_id, text)) = pending.take_tail() else {
            return;
        };
        self.publish(session_id, move |hub, dir| {
            hub.publish_compact_progress(dir, session_id, &compact_id, text)
        });
    }

    /// Publish against `session_id`, skipping silently when no hub is attached
    /// (Ringing-disabled hosts and unit tests).
    fn publish<F>(&self, session_id: &str, action: F)
    where
        F: FnOnce(&V2ProjectionHub, &std::path::Path) -> Result<(), V2HubError>,
    {
        let Some(hub) = self.hub else { return };
        let session_dir = self.sessions.session_path_dir(session_id);
        if let Err(error) = action(hub, &session_dir) {
            log::warn!("[compact-v2] publish failed for {session_id}: {error}");
        }
    }
}

fn wire_status(status: CompactStatus) -> WireCompactStatus {
    match status {
        CompactStatus::Completed => WireCompactStatus::Completed,
        CompactStatus::Skipped => WireCompactStatus::Skipped,
        CompactStatus::Failed => WireCompactStatus::Failed,
        CompactStatus::Cancelled => WireCompactStatus::Cancelled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(len: usize) -> String {
        "补".repeat(len)
    }

    #[test]
    fn progress_waits_for_the_coalescing_threshold() {
        let mut pending = PendingCompact::new("compact-1");
        assert_eq!(pending.record(&chunk(PROGRESS_MIN_CHARS - 1)), None);
        let snapshot = pending.record("补").expect("threshold crossed");
        assert_eq!(snapshot.chars().count(), PROGRESS_MIN_CHARS);
        // The snapshot is cumulative, not the chunk that crossed the line.
        assert!(snapshot.starts_with("补补补"));
        assert_eq!(pending.published_chars, PROGRESS_MIN_CHARS);
    }

    #[test]
    fn snapshots_stay_cumulative_across_chunks() {
        let mut pending = PendingCompact::new("compact-1");
        pending.record(&chunk(PROGRESS_MIN_CHARS));
        let second = pending
            .record(&chunk(PROGRESS_MIN_CHARS))
            .expect("second frame");
        assert_eq!(second.chars().count(), PROGRESS_MIN_CHARS * 2);
        assert_eq!(pending.take_tail(), None, "nothing unpublished");
    }

    #[test]
    fn tail_is_handed_back_once() {
        let mut pending = PendingCompact::new("compact-9");
        pending.record("短尾");
        let (compact_id, text) = pending.take_tail().expect("tail pending");
        assert_eq!(compact_id, "compact-9");
        assert_eq!(text, "短尾");
    }

    #[test]
    fn status_vocabulary_matches_the_domain_event() {
        assert_eq!(
            wire_status(CompactStatus::Completed),
            WireCompactStatus::Completed
        );
        assert_eq!(
            wire_status(CompactStatus::Skipped),
            WireCompactStatus::Skipped
        );
        assert_eq!(
            wire_status(CompactStatus::Failed),
            WireCompactStatus::Failed
        );
        assert_eq!(
            wire_status(CompactStatus::Cancelled),
            WireCompactStatus::Cancelled
        );
    }
}
