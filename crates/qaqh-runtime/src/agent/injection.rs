//! Unified injection bus (knife-7 stage 1).
//!
//! Every non-user message injection (subagent reports today; system/MCP in
//! the future) flows through `Loop::inject()` and is queued here. The bus
//! owns the busy-turn queue and command-id idempotency; `ContextFlow`
//! remains the message persistence boundary.

use std::collections::{HashSet, VecDeque};

use qaqh_types::{ContentBlock, Message};

/// Stable source id used by the existing ContextFlow registration.
pub const SUBAGENT_SOURCE: &str = "subagent";

/// Injection priority. `Deferred` marks injections queued while a compact is
/// running; they are dispatched as turns once the compact finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectionPriority {
    Normal,
    Steer,
    Interject,
    Deferred,
}

impl InjectionPriority {
    fn rank(self) -> u8 {
        match self {
            Self::Interject => 0,
            Self::Steer => 1,
            Self::Normal => 2,
            Self::Deferred => 3,
        }
    }
}

/// Maximum steer messages merged into one safe point.
pub const MAX_STEER_PER_SAFE_POINT: usize = 8;
/// Maximum interject messages merged into one safe point.
pub const MAX_INTERJECT_PER_SAFE_POINT: usize = 4;

/// Injection semantics. Only `NextTurn` exists this round; `Interrupt` /
/// `Inline` are reserved for future sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectionSemantics {
    NextTurn,
}

/// A non-user message injection waiting to be handed to ContextFlow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Injection {
    pub session_id: String,
    pub command_id: String,
    /// Stable inter-agent input identity used by the canonical input admission
    /// path and by replay deduplication.
    pub input_id: String,
    pub input_purpose: qaqh_domain::ConversationInputPurpose,
    /// Source id (`SUBAGENT_SOURCE` today; future "system"/"mcp").
    pub source: &'static str,
    /// Role used for the persisted message shape (`Message::ROLE_USER`).
    pub role: &'static str,
    pub text: String,
    pub priority: InjectionPriority,
    pub semantics: InjectionSemantics,
}

impl Injection {
    /// Convenience constructor matching the pre-unification subagent shape:
    /// source=SUBAGENT_SOURCE, role=user, priority=Normal, semantics=NextTurn.
    pub fn new(
        session_id: impl Into<String>,
        command_id: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        let command_id = command_id.into();
        Self {
            session_id: session_id.into(),
            input_id: command_id.clone(),
            command_id,
            input_purpose: qaqh_domain::ConversationInputPurpose::TriggerTurn,
            source: SUBAGENT_SOURCE,
            role: Message::ROLE_USER,
            text: text.into(),
            priority: InjectionPriority::Normal,
            semantics: InjectionSemantics::NextTurn,
        }
    }

    /// Preserve the established storage shape: `user` + `name=subagent`
    /// (the `name` follows `self.source`, so a new source only needs to
    /// declare itself — the message shape for subagent stays identical).
    pub fn message(&self) -> Message {
        Message {
            msg_id: None,
            role: self.role.into(),
            name: Some(self.source.into()),
            content: vec![ContentBlock::text(&self.text)],
        }
    }
}

/// Result of attempting to enqueue an injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueResult {
    Queued,
    DuplicateCommandId,
    DuplicateInputId,
    StaleSession,
}

/// FIFO queue and session-local idempotency set for injections.
#[derive(Debug, Default)]
pub struct InjectionBus {
    active_session: Option<String>,
    pending: VecDeque<Injection>,
    seen_command_ids: HashSet<String>,
    seen_input_ids: HashSet<String>,
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
        self.seen_input_ids.clear();
    }

    pub fn enqueue(&mut self, injection: Injection) -> EnqueueResult {
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
        if !injection.input_id.is_empty() && !self.seen_input_ids.insert(injection.input_id.clone())
        {
            return EnqueueResult::DuplicateInputId;
        }
        self.pending.push_back(injection);
        EnqueueResult::Queued
    }

    /// Take pending records in priority order. Seen command ids stay marked
    /// so a replay after the boundary cannot submit a second message.
    pub fn drain(&mut self) -> Vec<Injection> {
        let mut records = self.pending.drain(..).collect::<Vec<_>>();
        records.sort_by_key(|injection| injection.priority.rank());
        records
    }

    /// Take at most `max_steer` steer and `max_interject` interject records.
    ///
    /// Records over the safe-point limits remain queued for the next boundary.
    /// This keeps a single model request from absorbing an unbounded burst.
    pub fn drain_limited(&mut self, max_steer: usize, max_interject: usize) -> Vec<Injection> {
        let mut records = self.pending.drain(..).collect::<Vec<_>>();
        records.sort_by_key(|injection| injection.priority.rank());
        let mut selected = Vec::new();
        let mut deferred = VecDeque::new();
        let mut steer_count = 0usize;
        let mut interject_count = 0usize;
        for injection in records {
            match injection.priority {
                InjectionPriority::Interject if interject_count < max_interject => {
                    interject_count += 1;
                    selected.push(injection);
                }
                InjectionPriority::Steer if steer_count < max_steer => {
                    steer_count += 1;
                    selected.push(injection);
                }
                _ => deferred.push_back(injection),
            }
        }
        self.pending = deferred;
        selected
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn injection(
        session_id: &str,
        command_id: impl Into<String>,
        text: impl Into<String>,
    ) -> Injection {
        Injection::new(session_id, command_id, text)
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
    fn rejects_duplicate_input_id_across_different_command_ids() {
        let mut bus = InjectionBus::new();
        bus.switch_session("session-a");

        let first = Injection {
            input_id: "msg-stable-1".into(),
            ..injection("session-a", "cmd-1", "first")
        };
        let replay = Injection {
            input_id: "msg-stable-1".into(),
            ..injection("session-a", "cmd-2", "replay")
        };
        assert_eq!(bus.enqueue(first), EnqueueResult::Queued);
        assert_eq!(bus.enqueue(replay), EnqueueResult::DuplicateInputId);
        assert_eq!(bus.drain().len(), 1);
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

    #[test]
    fn safe_point_delivery_orders_interject_then_steer_then_normal() {
        let mut bus = InjectionBus::new();
        bus.switch_session("session-a");
        let normal = Injection {
            priority: InjectionPriority::Normal,
            ..injection("session-a", "cmd-normal", "normal")
        };
        let steer = Injection {
            priority: InjectionPriority::Steer,
            ..injection("session-a", "cmd-steer", "steer")
        };
        let interject = Injection {
            priority: InjectionPriority::Interject,
            ..injection("session-a", "cmd-interject", "interject")
        };

        assert_eq!(bus.enqueue(normal), EnqueueResult::Queued);
        assert_eq!(bus.enqueue(steer), EnqueueResult::Queued);
        assert_eq!(bus.enqueue(interject), EnqueueResult::Queued);

        let drained = bus.drain();
        assert_eq!(drained[0].priority, InjectionPriority::Interject);
        assert_eq!(drained[1].priority, InjectionPriority::Steer);
        assert_eq!(drained[2].priority, InjectionPriority::Normal);
    }

    #[test]
    fn safe_point_limits_leave_overflow_queued() {
        let mut bus = InjectionBus::new();
        bus.switch_session("session-a");
        for index in 0..3 {
            let injection = Injection {
                priority: InjectionPriority::Interject,
                ..injection(
                    "session-a",
                    format!("cmd-interject-{index}"),
                    format!("interject {index}"),
                )
            };
            assert_eq!(bus.enqueue(injection), EnqueueResult::Queued);
        }
        for index in 0..3 {
            let injection = Injection {
                priority: InjectionPriority::Steer,
                ..injection(
                    "session-a",
                    format!("cmd-steer-{index}"),
                    format!("steer {index}"),
                )
            };
            assert_eq!(bus.enqueue(injection), EnqueueResult::Queued);
        }

        let first = bus.drain_limited(1, 1);
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].priority, InjectionPriority::Interject);
        assert_eq!(first[1].priority, InjectionPriority::Steer);
        assert_eq!(bus.pending_len(), 4);

        let second = bus.drain_limited(1, 1);
        assert_eq!(second.len(), 2);
        assert_eq!(second[0].priority, InjectionPriority::Interject);
        assert_eq!(second[1].priority, InjectionPriority::Steer);
        assert_eq!(bus.pending_len(), 2);
    }

    #[test]
    fn mixes_sources_fifo_and_dedupes_command_ids_across_sources() {
        let mut bus = InjectionBus::new();
        bus.switch_session("session-a");

        let sys = Injection {
            source: "system",
            ..injection("session-a", "cmd-1", "system report")
        };
        let sub = injection("session-a", "cmd-1", "subagent replay");
        let mcp = Injection {
            source: "mcp",
            ..injection("session-a", "cmd-2", "mcp report")
        };

        assert_eq!(bus.enqueue(sys), EnqueueResult::Queued);
        assert_eq!(
            bus.enqueue(sub),
            EnqueueResult::DuplicateCommandId,
            "command_id dedupe must be source-independent"
        );
        assert_eq!(bus.enqueue(mcp), EnqueueResult::Queued);

        let drained = bus.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].text, "system report");
        assert_eq!(drained[1].text, "mcp report");
        // message() 的 name 跟随 source；subagent 的既有形状不变。
        assert_eq!(drained[0].message().name.as_deref(), Some("system"));
        assert_eq!(drained[1].message().name.as_deref(), Some("mcp"));
        assert_eq!(drained[0].priority, InjectionPriority::Normal);
        assert_eq!(drained[0].semantics, InjectionSemantics::NextTurn);
    }
}
