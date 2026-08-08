//! Native, ordered transcript model.
//!
//! This is deliberately independent of `Agent2Ui`: a timeline is the
//! authoritative representation of what a desktop transcript displays, not a
//! projection of a legacy message protocol.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// A display block in one model round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum TimelineBlockKind {
    Reasoning,
    Text,
    Tool,
    Notice,
}

/// Lifecycle of a display block. Markdown is rendered only after `Sealed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum TimelineBlockState {
    Open,
    Sealed,
}

/// State updates for a tool block; all updates retain the block's position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum TimelineToolState {
    Prepared,
    Running,
    Succeeded,
    Failed,
}

/// Terminal state of a transcript turn. This is distinct from block sealing:
/// a cancelled or failed turn may have valid, already-sealed Markdown blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum TimelineTurnState {
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// Sanitised failure information retained with a transcript terminal event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TimelineFailure {
    pub code: String,
    pub message: String,
}

/// Tool permission data belongs to the transcript tool block, while the
/// interaction request/response lifecycle stays on the native control plane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TimelineToolPermission {
    pub reason: String,
    pub paths: Vec<String>,
    pub category: String,
    pub level: u8,
    pub risk: String,
    pub consequence: String,
}

/// Immutable identity and mutable presentation state for one tool block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TimelineTool {
    pub tool_call_id: String,
    pub name: String,
    pub state: TimelineToolState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Original structured arguments as supplied by the tool producer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_json: Option<String>,
    /// The retained tool-output tail. Large output remains an explicit content
    /// reference in the eventual transport record rather than being silently
    /// truncated by the transcript protocol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub progress: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<TimelineFailure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<TimelineToolPermission>,
}

/// Fully materialized display block saved in timeline snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TimelineBlock {
    pub block_id: String,
    /// Stable order within one round. It never changes when the block updates.
    pub block_order: u32,
    pub kind: TimelineBlockKind,
    pub state: TimelineBlockState,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<TimelineTool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TimelineRound {
    pub round_num: u32,
    pub sealed: bool,
    pub is_final: bool,
    pub blocks: Vec<TimelineBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TimelineTurn {
    pub turn_id: String,
    /// seq of the TurnOpened entry that created this turn — the authoritative
    /// time order across snapshots. `0` means unknown (legacy persisted data);
    /// consumers fall back to the turn_id numeric suffix in that case.
    #[serde(default)]
    #[ts(type = "number")]
    pub created_seq: u64,
    pub user_text: String,
    pub sealed: bool,
    pub state: TimelineTurnState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<TimelineFailure>,
    pub rounds: Vec<TimelineRound>,
}

/// Authoritative recovery state, not an event array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TimelineSnapshot {
    /// The largest timeline sequence included in `turns`.
    #[ts(type = "number")]
    pub watermark: u64,
    pub turns: Vec<TimelineTurn>,
}

/// One mutation of the ordered transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum TimelineEvent {
    TurnOpened {
        user_text: String,
    },
    BlockOpened {
        block: TimelineBlock,
    },
    /// `fragment_seq` is monotonic within a text/reasoning block.
    TextDelta {
        block_id: String,
        #[ts(type = "number")]
        fragment_seq: u64,
        delta: String,
    },
    ToolUpdated {
        block_id: String,
        tool: TimelineTool,
    },
    /// A tool-output chunk, appended to the current progress buffer by the
    /// single transcript reducer.
    ToolProgress {
        block_id: String,
        chunk: String,
    },
    BlockSealed {
        block_id: String,
    },
    RoundSealed {
        is_final: bool,
    },
    TurnSealed {
        state: TimelineTurnState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure: Option<TimelineFailure>,
    },
}

/// A globally ordered record for one session seed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TimelineEntry {
    /// Strictly monotonic for one `(server epoch, seed)` across all display kinds.
    #[ts(type = "number")]
    pub timeline_seq: u64,
    pub turn_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round_num: Option<u32>,
    pub event: TimelineEvent,
}

/// Producer-to-writer command for the native transcript. Producers never
/// allocate `timeline_seq` or a text fragment sequence: those are assigned by
/// the single writer after intents from model and tool workers have been
/// serialized onto one queue.
///
/// This is deliberately not an `Agent2Ui` or Ringing-event wrapper. It has no
/// channel, delivery, SSE, or legacy message fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum TimelineIntent {
    TurnOpened {
        turn_id: String,
        user_text: String,
    },
    BlockOpened {
        turn_id: String,
        #[ts(type = "number")]
        round_num: u32,
        block_id: String,
        kind: TimelineBlockKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<TimelineTool>,
    },
    TextDelta {
        turn_id: String,
        #[ts(type = "number")]
        round_num: u32,
        block_id: String,
        delta: String,
    },
    ToolUpdated {
        turn_id: String,
        #[ts(type = "number")]
        round_num: u32,
        block_id: String,
        tool: TimelineTool,
    },
    /// Append execution output without replacing the tool's identity or
    /// arguments. The transcript writer applies this patch to the block.
    ToolProgress {
        turn_id: String,
        #[ts(type = "number")]
        round_num: u32,
        block_id: String,
        chunk: String,
    },
    BlockSealed {
        turn_id: String,
        #[ts(type = "number")]
        round_num: u32,
        block_id: String,
    },
    RoundSealed {
        turn_id: String,
        #[ts(type = "number")]
        round_num: u32,
        is_final: bool,
    },
    TurnSealed {
        turn_id: String,
        state: TimelineTurnState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure: Option<TimelineFailure>,
    },
}
