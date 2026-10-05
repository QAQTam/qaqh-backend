use qaqh_types::Message;

/// A tool invocation extracted from the assistant message.
///
/// Note: the former `Effect { None, TurnComplete, CallGate }` enum was
/// dissolved (PR structure-simplification Phase 1): the assistant push
/// (`push_assistant`) now returns `bool` (`true` = turn completed) and
/// `CallGate` was dead since introduction (zero constructors, zero consumers).
#[derive(Debug, Clone)]
pub struct PendingTool {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
}

/// Host-side persistence instruction (PR-1-6 / A1).
///
/// MessageStore never touches the session manager singleton: every disk write is enqueued
/// as a [`PersistOp`] (see `MessageStore::take_persist_ops`), and the host
/// (loop) drains the queue after each command dispatch and replays the ops
/// against the injected `SessionManager` singleton. Single-threaded replay keeps the
/// on-disk byte order identical to the old synchronous writes (Z5 red line).
///
/// The op→`SessionManager` method mapping lives on the consumer side (runtime/agent /
/// runtime): this crate must not re-introduce a `qaqh-session` dependency
/// just to execute persistence.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum PersistOp {
    /// Append new messages to messages.jsonl and refresh meta/index
    /// (was `SessionManager::save_append`).
    Append {
        session_id: String,
        messages: Vec<Message>,
        model: String,
        effort: Option<String>,
        compact_skip: usize,
        /// `Some(id)` advances the archive-derived compact watermark; `None`
        /// preserves the existing meta watermark. Compaction summaries are
        /// ordinary appended `Message`s, so this field travels with the same
        /// op that makes the summary durable.
        #[serde(default)]
        compact_covered_through_msg_id: Option<u64>,
        turn_count: usize,
    },
    /// Refresh meta/index without new messages (was `update_meta`).
    UpdateMeta {
        session_id: String,
        model: String,
        effort: Option<String>,
        compact_skip: usize,
        turn_count: usize,
    },
    /// Full rewrite of messages.jsonl — undo / image repair aftermath.
    ///
    /// A full rewrite replaces the canonical archive with the current active
    /// view. The optional watermark below lets that rewritten view keep its
    /// compaction marker; `None` means the rewrite removed the marker.
    SaveFull {
        session_id: String,
        messages: Vec<Message>,
        model: String,
        effort: Option<String>,
        compact_skip: usize,
        /// `Some(id)` preserves an archive-derived compact marker across an
        /// active-view rewrite; `None` means the rewritten archive has no
        /// compacted prefix to hide.
        #[serde(default)]
        compact_covered_through_msg_id: Option<u64>,
        turn_count: usize,
    },
}
