//! The first-stage injection bus.
//!
//! This slice intentionally supports only Subagent reports. The bus owns the
//! busy-turn queue and command-id idempotency; `ContextFlow` remains the
//! message persistence boundary.

use std::collections::{HashSet, VecDeque};

use qaqh_types::{ContentBlock, Message};

/// Stable source id used by the existing ContextFlow registration.
pub const SUBAGENT_SOURCE: &str = "subagent";

/// A Subagent report waiting to be handed to ContextFlow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentInjection {
    pub session_id: String,
    pub command_id: String,
    pub text: String,
}

impl SubagentInjection {
    pub fn new(
        session_id: impl Into<String>,
        command_id: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            command_id: command_id.into(),
            text: text.into(),
        }
    }

    /// Preserve the established storage shape: `user` + `name=subagent`.
    pub fn message(&self) -> Message {
        Message {
            msg_id: None,
            role: Message::ROLE_USER.into(),
            name: Some(SUBAGENT_SOURCE.into()),
            content: vec![ContentBlock::text(&self.text)],
        }
    }
}

/// Result of attempting to enqueue a Subagent report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueResult {
    Queued,
    DuplicateCommandId,
    StaleSession,
}

/// FIFO queue and session-local idempotency set for Subagent reports.
#[derive(Debug, Default)]
pub struct InjectionBus {
    active_session: Option<String>,
    pending: VecDeque<SubagentInjection>,
    seen_command_ids: HashSet<String>,
}

impl InjectionBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Switch the active session and discard all queued/seen injections from
    /// the previous session. Re-selecting the same session is not a switch.
    pub fn switch_session(&mut self, session_id: &str) {
        if session_id.is_empty() {
            self.clear();
            self.active_session = None;
            return;
        }
        if self.active_session.as_deref() != Some(session_id) {
            self.clear();
            self.active_session = Some(session_id.to_string());
        }
    }

    /// Clear queued records and their idempotency history without changing
    /// the currently selected session.
    pub fn clear(&mut self) {
        self.pending.clear();
        self.seen_command_ids.clear();
    }

    pub fn enqueue(&mut self, injection: SubagentInjection) -> EnqueueResult {
        if injection.session_id.is_empty() {
            return EnqueueResult::StaleSession;
        }
        if self.active_session.is_none() {
            self.active_session = Some(injection.session_id.clone());
        }
        if self.active_session.as_deref() != Some(injection.session_id.as_str()) {
            return EnqueueResult::StaleSession;
        }
        if !self.seen_command_ids.insert(injection.command_id.clone()) {
            return EnqueueResult::DuplicateCommandId;
        }
        self.pending.push_back(injection);
        EnqueueResult::Queued
    }

    /// Take pending records in submission order. Seen command ids stay marked
    /// so a replay after the boundary cannot submit a second message.
    pub fn drain(&mut self) -> Vec<SubagentInjection> {
        self.pending.drain(..).collect()
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn injection(session_id: &str, command_id: &str, text: &str) -> SubagentInjection {
        SubagentInjection::new(session_id, command_id, text)
    }

    #[test]
    fn preserves_fifo_and_existing_message_shape() {
        let mut bus = InjectionBus::new();
        bus.switch_session("session-a");

        assert_eq!(
            bus.enqueue(injection("session-a", "cmd-1", "first")),
            EnqueueResult::Queued
        );
        assert_eq!(
            bus.enqueue(injection("session-a", "cmd-2", "second")),
            EnqueueResult::Queued
        );

        let drained = bus.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].text, "first");
        assert_eq!(drained[1].text, "second");
        let message = drained[0].message();
        assert_eq!(message.role, "user");
        assert_eq!(message.name.as_deref(), Some("subagent"));
    }

    #[test]
    fn rejects_duplicate_command_id_before_and_after_drain() {
        let mut bus = InjectionBus::new();
        bus.switch_session("session-a");

        assert_eq!(
            bus.enqueue(injection("session-a", "cmd-1", "first")),
            EnqueueResult::Queued
        );
        assert_eq!(
            bus.enqueue(injection("session-a", "cmd-1", "duplicate")),
            EnqueueResult::DuplicateCommandId
        );
        assert_eq!(bus.drain().len(), 1);
        assert_eq!(
            bus.enqueue(injection("session-a", "cmd-1", "replay")),
            EnqueueResult::DuplicateCommandId
        );
    }

    #[test]
    fn session_switch_clears_pending_and_idempotency_state() {
        let mut bus = InjectionBus::new();
        bus.switch_session("session-a");
        assert_eq!(
            bus.enqueue(injection("session-a", "cmd-1", "old")),
            EnqueueResult::Queued
        );

        bus.switch_session("session-b");
        assert_eq!(bus.pending_len(), 0);
        assert_eq!(bus.drain(), Vec::new());
        assert_eq!(
            bus.enqueue(injection("session-b", "cmd-1", "new")),
            EnqueueResult::Queued
        );
        assert_eq!(bus.drain()[0].session_id, "session-b");
    }
}
