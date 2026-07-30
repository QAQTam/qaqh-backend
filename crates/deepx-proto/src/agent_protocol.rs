//! UI ↔ Agent frame definitions (JSON-LP over stdin/stdout, child process).
//!
//! v5: Round-based protocol. Each API call is a Round with optional
//! streaming preview. No duplication between streaming and final content.
//! Frontend appends blocks in order — no state machine required.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Authoritative runtime state for one desktop session.
///
/// Emitted by SessionActivity via `Agent2Ui::SessionEvents` whenever the
/// agent session transitions between lifecycle states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum SessionActivityState {
    /// Agent is initializing (loading config, creating session, etc.).
    Starting,
    /// No turn in progress; waiting for user input.
    Idle,
    /// A turn is actively running (gate → tools loop).
    Working,
    /// Turn suspended — waiting for user response (permission, ask, plan review).
    WaitingUser,
    /// Agent subprocess has disconnected.
    Disconnected,
}

/// Snapshot/event payload emitted by the desktop bridge for every session state change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SessionActivity {
    /// Session identifier (8 hex chars).
    pub seed: String,
    /// Current lifecycle state.
    pub state: SessionActivityState,
    /// Active turn ID, if a turn is in progress or suspended.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub turn_id: Option<String>,
    /// Monotonic event sequence number for this session.
    #[ts(type = "number")]
    pub seq: u64,
    /// Unix timestamp of this state change.
    #[ts(type = "number")]
    pub updated_at: u64,
}

/// An item pending review for todo_activate. Carried in PlanSubmitted.review_type="todo_activation".
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TodoActivationItem {
    pub id: String,
    pub title: String,
    pub description: String,
    /// "small" | "medium" | "large"
    pub complexity: String,
}

/// An image attachment in a user message, for multimodal (vision) models.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ImageBlock {
    /// MIME type (e.g. "image/png", "image/jpeg").
    pub mime_type: String,
    /// Base64-encoded image data (without the data URI prefix).
    pub data: String,
}

// ═══════════════════════════════════════════════════════════════════════════
// UI → Agent
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "type")]
#[non_exhaustive]
#[ts(export)]
pub enum Ui2Agent {
    /// User typed a message. Triggers InputEngine → TurnEngine → gate → tools pipeline.
    #[serde(rename = "user_input")]
    UserInput {
        text: String,
        /// Optional image attachments (mime_type, base64 data).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageBlock>,
    },

    /// Frontend-requested tool execution (e.g. from a UI button or inline action).
    #[serde(rename = "tool_call")]
    ToolCall {
        id: String,
        name: String,
        action: String,
        #[ts(type = "any")]
        args: serde_json::Value,
    },

    /// Create a new session and switch to it.
    #[serde(rename = "create_session")]
    CreateSession,

    /// Cancel the current turn (stops gate streaming and tool execution).
    #[serde(rename = "cancel")]
    Cancel,

    /// Shut down the agent process gracefully.
    #[serde(rename = "shutdown")]
    Shutdown,

    /// Reload config from disk (provider, model, permission, etc.).
    #[serde(rename = "reload_config")]
    ReloadConfig,

    /// Remove the last turn from the session.
    #[serde(rename = "undo_turn")]
    UndoTurn { turn_id: String },

    /// Trigger context compaction (summarize old turns to free token budget).
    #[serde(rename = "compact")]
    Compact,

    /// Restore a previously saved session by seed.
    #[serde(rename = "resume_session")]
    ResumeSession { seed: String },

    /// Create a fresh session (alias for CreateSession, triggers end of previous session).
    #[serde(rename = "new_session")]
    NewSession,

    /// Load older (archived) turns for display in the history panel.
    #[serde(rename = "load_more_turns")]
    LoadMoreTurns {
        /// Load turns older than this turn_id.
        before_turn_id: String,
        /// How many turns to load.
        #[serde(default = "default_load_count")]
        count: u32,
    },

    /// Set the agent operating mode (Normal, Plan, Code).
    #[serde(rename = "set_mode")]
    SetMode { mode: String },

    /// Response to a permission request dialog.
    #[serde(rename = "permission_response")]
    PermissionResponse {
        tool_call_id: String,
        approved: bool,
        /// If true, trust the target folder permanently.
        #[serde(default)]
        trust_folder: bool,
    },

    /// User's answers to an ask_user prompt. Resumes a suspended turn.
    /// `answers` contains one entry for Single mode, N entries for Batch mode.
    #[serde(rename = "ask_response")]
    AskResponse {
        /// Matches the ask_id from Agent2Ui::AskUser.
        ask_id: String,
        answers: Vec<AskAnswer>,
    },

    /// User dismissed the ask_user dialog without answering.
    /// Agent should abort the suspended turn.
    #[serde(rename = "ask_dismiss")]
    AskDismiss { ask_id: String },

    /// User's decision on a submitted plan review.
    /// Resumes a turn suspended by plan_submit.
    #[serde(rename = "plan_review")]
    PlanReview {
        /// Matches the call_id of the plan_submit tool call.
        call_id: String,
        approved: bool,
        /// Optional rejection reason.
        #[serde(default)]
        message: String,
        /// Explicit UI opt-in for autonomous execution of this approved plan.
        #[serde(default)]
        autonomous: bool,
    },

    /// Unload an explicitly-activated skill ($name mention) from context.
    #[serde(rename = "unload_skill")]
    UnloadSkill {
        /// Skill name (must match the name field in SKILL.md frontmatter).
        name: String,
    },

    /// Explicitly activate a skill by name (equivalent to $skill-name mention).
    /// Triggers SkillsChanged emission on success.
    #[serde(rename = "activate_skill")]
    ActivateSkill {
        /// Skill name (must match the name field in SKILL.md frontmatter).
        name: String,
    },

    /// Revision-safe skills operation used by the V2 workbench.
    #[serde(rename = "skill_operation")]
    SkillOperation {
        operation_id: String,
        action: String,
        name: String,
        #[serde(default)]
        expected_revision: u64,
    },

    /// Reload the skill catalog from disk and refresh the catalog system message.
    #[serde(rename = "reload_skills")]
    ReloadSkills,
}

// ═══════════════════════════════════════════════════════════════════════════
// Shared types
// ═══════════════════════════════════════════════════════════════════════════

/// Information about an available skill for the frontend skills panel.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    /// "project" or "user"
    pub scope: String,
    /// Display path relative to workspace (e.g. "skills/deepx/deepx-debug")
    pub source: String,
}

/// Payload for Agent2Ui::SkillsChanged.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SkillsStatus {
    /// All discoverable skills in the workspace.
    pub available: Vec<SkillInfo>,
    /// Names of currently loaded (explicit, $mention-activated) skills.
    pub active: Vec<String>,
    #[serde(default)]
    pub catalog_revision: String,
    #[serde(default)]
    pub context_epoch: u64,
    #[serde(default)]
    pub operation_revision: u64,
    #[serde(default)]
    pub token_budget: usize,
    #[serde(default)]
    pub token_usage: usize,
    #[serde(default)]
    pub runtime: Vec<SkillRuntimeInfo>,
    #[serde(default)]
    pub diagnostics: Vec<String>,
}

/// Runtime state of a skill within a session.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SkillRuntimeInfo {
    pub name: String,
    pub description: String,
    /// Current lifecycle state: "catalog", "requested", "active", "review_due", or "unavailable".
    pub state: String,
    /// Display source path (e.g. "skills/deepx/deepx-debug" or "~/.deepx/skills/...").
    pub source: String,
    /// Number of turns before the lease expires and the skill must be re-validated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub lease_remaining: Option<u8>,
    /// Estimated token count of the skill body.
    pub token_count: usize,
    /// Error message if the skill failed to load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub error: Option<String>,
}

/// Tool call definition sent in RoundComplete.tool_calls.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ToolCallDef {
    pub id: String,
    pub name: String,
    /// Human-readable args summary (e.g. "foo.rs", "search pattern")
    pub args_display: String,
    /// Raw JSON arguments string
    pub args_json: String,
}

/// Tool execution result sent in ToolResults.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ToolResultDef {
    pub tool_call_id: String,
    pub output: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub file: Option<FileSnapshotInfo>,
}

/// File metadata snapshot for rich rendering.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct FileSnapshotInfo {
    pub path: String,
    pub lines: u32,
    pub size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub start_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub end_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub tag: Option<String>,
}

/// Document tracking entry.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct DocInfo {
    pub tag: String,
    pub path: String,
    pub turns_since_read: u32,
    pub is_stale: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TaskInfo {
    pub id: String,
    pub subject: String,
    pub description: String,
    pub status: String,
}

/// One round of a turn (one API call).
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct RoundData {
    pub round_num: u32,
    #[serde(default)]
    pub is_final: bool,
    pub thinking: Option<String>,
    pub answer: Option<String>,
    pub tool_calls: Vec<ToolCallDef>,
    pub tool_results: Vec<ToolResultDef>,
    /// Ordered blocks preserving the LLM's output sequence (reasoning ↔ text ↔ tool).
    #[serde(default)]
    pub blocks: Vec<RoundBlock>,
}

/// Backend-owned intrinsic impact of a permission-gated action.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum PermissionRisk {
    Low,
    Medium,
    High,
}

/// One full turn (user message + all rounds).
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct TurnData {
    pub turn_id: String,
    pub user_text: String,
    pub rounds: Vec<RoundData>,
}

/// One block in a round, preserving the LLM's output order.
///
/// Blocks are streamed to the frontend in order so it can reconstruct
/// the exact sequence of reasoning → text → tool calls from the model.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum RoundBlock {
    /// Model reasoning/thinking block (collapsible in UI).
    Reasoning { content: String },
    /// Plain text answer block.
    Text { content: String },
    /// A tool call the model wants to invoke.
    Tool { card: ToolCallDef },
}

// ═══════════════════════════════════════════════════════════════════════════
// Ask-user types (v6: multi-question support)
// ═══════════════════════════════════════════════════════════════════════════

/// Display mode for an ask_user prompt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum AskMode {
    /// One question — shown in a modal dialog. Answer is sent immediately.
    #[default]
    Single,
    /// Multiple questions — shown as a form. All answers submitted together.
    Batch,
}

/// How an ask_user prompt left the active queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum AskResolution {
    Answered,
    Dismissed,
}

/// One question in an ask_user prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AskQuestion {
    /// Unique ID within this ask (e.g. "q1", "q2").
    pub id: String,
    /// Question text (supports Markdown).
    pub question: String,
    /// Preset choices. Empty = free-text only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// Allow the user to type a custom answer instead of picking an option.
    #[serde(default = "default_true")]
    pub allow_custom: bool,
}

/// One answer in a user response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AskAnswer {
    /// Matches AskQuestion.id.
    pub question_id: String,
    /// Selected option text, or custom input.
    pub answer: String,
}

fn default_true() -> bool {
    true
}

// ═══════════════════════════════════════════════════════════════════════════
// Agent → UI (v5 — round-based)
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "type")]
#[non_exhaustive]
#[ts(export)]
pub enum Agent2Ui {
    // ── Turn lifecycle ──
    /// A new turn starts. Frontend creates a user message + turn container.
    #[serde(rename = "turn_start")]
    TurnStart { turn_id: String, user_text: String },

    /// Turn complete. All rounds and tool results have been sent.
    #[serde(rename = "turn_end")]
    TurnEnd {
        turn_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        stop_reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        usage: Option<deepx_types::UsageInfo>,
    },

    // ── Streaming preview (optional, additive) ──
    /// Live typing preview for the current round.
    /// Frontend shows this as a draft; RoundComplete replaces it.
    #[serde(rename = "round_delta")]
    RoundDelta {
        turn_id: String,
        round_num: u32,
        kind: RoundDeltaKind,
        delta: String,
    },

    // ── Round complete (authoritative) ──
    /// One API call finished. Contains everything the model produced.
    /// Frontend replaces any draft from RoundDelta with this content.
    #[serde(rename = "round_complete")]
    RoundComplete {
        turn_id: String,
        round_num: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        thinking: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        answer: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCallDef>,
        /// Ordered blocks matching LLM output sequence (preferred).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        blocks: Vec<RoundBlock>,
        /// true = this is the final round of the turn
        is_final: bool,
    },

    // ── Tool execution results ──
    /// Results from executing the tool calls in a RoundComplete.
    /// Sent after each tool finishes, before the next round or TurnEnd.
    #[serde(rename = "tool_results")]
    ToolResults {
        turn_id: String,
        round_num: u32,
        results: Vec<ToolResultDef>,
    },

    /// Real-time stdout/stderr chunk from a running exec tool.
    /// Frontend accumulates these until the corresponding ToolResult arrives.
    #[serde(rename = "tool_exec_delta")]
    ToolExecDelta { tool_call_id: String, delta: String },

    // ── Session restore ──
    /// Full session history sent on resume.
    #[serde(rename = "session_restored")]
    SessionRestored {
        seed: String,
        turns: Vec<TurnData>,
        tokens_used: u32,
        #[serde(default)]
        cache_hit_pct: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        usage: Option<deepx_types::UsageInfo>,
        #[serde(default)]
        usage_totals: deepx_types::UsageInfo,
        #[serde(default)]
        usage_requests: u32,
        #[serde(default)]
        cache_reported_requests: u32,
        /// Total number of turns in this session.
        #[serde(default)]
        total_turns: u32,
        /// True if there are more (older) turns beyond what's sent.
        #[serde(default)]
        has_more: bool,
    },

    /// Older turns loaded from history.
    #[serde(rename = "more_turns")]
    MoreTurns {
        turns: Vec<TurnData>,
        /// True if there are still more (older) turns available.
        has_more: bool,
    },

    /// A new session was created (response to CreateSession).
    #[serde(rename = "session_created")]
    SessionCreated { seed: String },

    // ── System events ──
    #[serde(rename = "error")]
    Error { message: String },

    /// A provider request failed transiently and will be retried. This is
    /// deliberately non-terminal: clients must keep the turn running.
    #[serde(rename = "provider_retrying")]
    ProviderRetrying {
        turn_id: String,
        round_num: u32,
        attempt: u32,
        max_retries: u32,
        delay_secs: u64,
        error: String,
    },

    #[serde(rename = "tool_notice")]
    ToolNotice {
        message: String,
        /// "warn" or "error"
        level: String,
    },

    /// A plan has been submitted for review (plan_submit intercepted)
    /// or a todo activation is pending approval.
    /// Frontend should show PlanReviewPanel with Approve/Reject buttons.
    /// Turn is suspended until PlanReview response arrives.
    #[serde(rename = "plan_submitted")]
    PlanSubmitted {
        /// The plan_submit or todo_activate tool-call ID.
        call_id: String,
        /// PLAN.md content to display (for review_type="plan").
        #[serde(default)]
        plan_content: String,
        /// Discriminator: "plan" (plan_submit) or "todo_activation" (todo_activate).
        #[serde(default)]
        review_type: String,
        /// Structured todo items for todo_activation review.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        todo_items: Option<Vec<TodoActivationItem>>,
    },

    /// Plan review resolved (approved or rejected). Frontend closes the panel.
    #[serde(rename = "plan_resolved")]
    PlanResolved {
        /// The plan_submit tool-call ID.
        call_id: String,
        approved: bool,
    },

    #[serde(rename = "dashboard")]
    Dashboard {
        hp_connected: bool,
        session_seed: String,
        tool_calls_total: u32,
        tool_failures: u32,
        current_phase: String,
        streaming: bool,
        #[serde(default)]
        dsml_compat_count: u32,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        documents: Vec<DocInfo>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        recent_edits: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tasks: Vec<TaskInfo>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        current_todo_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        session_title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        usage: Option<deepx_types::UsageInfo>,
        #[serde(default)]
        context_limit: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        model: Option<String>,
    },

    /// Provider-confirmed usage for one model request. This event is additive
    /// and may be emitted more than once for the same turn/round; consumers
    /// replace the previous sample instead of double-counting it.
    #[serde(rename = "usage_updated")]
    UsageUpdated {
        turn_id: String,
        round_num: u32,
        usage: deepx_types::UsageInfo,
        context_limit: u32,
        model: String,
    },

    /// Cache-key prefix changed since the last gate call.  Contains the
    /// names of the components that differed (e.g. "system_prompt",
    /// "catalog", "tool_defs").  An empty list means the prefix is stable.
    #[serde(rename = "cache_diagnostics")]
    CacheDiagnostics {
        prefix_hash: String,
        prefix_changed: bool,
        change_reasons: Vec<String>,
    },

    /// Agent finished the current turn. Frontend shows the Done indicator.
    #[serde(rename = "done")]
    Done,

    /// Agent completed compacting old turns.
    #[serde(rename = "compact_start")]
    CompactStart {
        turns_total: u32,
        turns_keeping: u32,
    },

    #[serde(rename = "compact_end")]
    CompactEnd {
        summary_chars: usize,
        turns_compacted: u32,
        /// Number of frontend turns to remove when applying compact locally.
        /// Equals total_turns_in_store - kept_user_count (includes any system
        /// turns from prior compactions, so it precisely tells the UI which turns
        /// to drop).
        turns_removed: u32,
    },

    /// Streaming delta from the compact LLM call — shown to the user
    /// so they can see the model's summary being built in real-time.
    #[serde(rename = "compact_delta")]
    CompactDelta { delta: String },

    /// Turn was cancelled by user. Frontend shows cancelled UI.
    #[serde(rename = "cancelled")]
    Cancelled,

    /// Agent acknowledged shutdown command.
    #[serde(rename = "shutdown_ack")]
    ShutdownAck,

    /// Agent is booted and ready to accept commands.
    #[serde(rename = "ready")]
    Ready,

    #[serde(rename = "audit_record")]
    AuditRecord {
        tool_name: String,
        result_summary: String,
        success: bool,
        /// ISO-8601 timestamp of the tool invocation.
        #[serde(default)]
        time: String,
        /// JSON-serialized tool arguments for formatting.
        #[serde(default)]
        args: String,
    },

    /// Structured streaming output from a running command.
    /// `seq` is monotonic per command and `stream` is either stdout or stderr.
    #[serde(rename = "exec_progress")]
    ExecProgress {
        tool_call_id: String,
        stream: String,
        seq: u64,
        chunk: String,
    },

    /// Tool call detected in streaming response — preview card before execution.
    #[serde(rename = "tool_call_preview")]
    ToolCallPreview {
        turn_id: String,
        round_num: u32,
        index: usize,
        id: String,
        name: String,
        args_so_far: String,
    },

    /// Realtime code stats delta from a file operation (write/edit/delete/move).
    #[serde(rename = "code_delta")]
    CodeDelta {
        lines_added: usize,
        lines_removed: usize,
        files_created: usize,
        files_deleted: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        file: Option<String>,
    },

    /// Heartbeat: daemon responds to frontend ping.
    #[serde(rename = "pong")]
    Pong,

    /// Skills catalog changed — frontend should refresh the skills panel.
    /// Emitted on explicit activation ($name), deactivation (UnloadSkill),
    /// and catalog reload (ReloadSkills).
    #[serde(rename = "skills_changed")]
    SkillsChanged {
        #[serde(flatten)]
        status: SkillsStatus,
    },

    #[serde(rename = "skill_operation_resolved")]
    SkillOperationResolved {
        operation_id: String,
        success: bool,
        revision: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        error: Option<String>,
    },

    /// Permission request: agent asks user to approve/deny a tool call.
    /// Frontend shows a dialog with tool details and target paths.
    #[serde(rename = "permission_request")]
    PermissionRequest {
        tool_call_id: String,
        tool_name: String,
        /// Human-readable reason for the request.
        reason: String,
        /// Target paths affected by the tool.
        paths: Vec<String>,
        /// Tool category: "read", "write", "exec", "net".
        category: String,
        /// Current permission level (1-4).
        level: u8,
        /// Intrinsic action impact, computed by the backend.
        risk: PermissionRisk,
        /// Plain-language effect of approving the action.
        consequence: String,
    },

    /// Ask-user prompt (v6). Agent suspends turn and waits for user response.
    /// Frontend shows AskDialog (Single) or AskForm (Batch).
    #[serde(rename = "ask_user")]
    AskUser {
        /// Turn containing the original ask_user tool call.
        turn_id: String,
        /// Assistant round containing the original ask_user tool call.
        round_num: u32,
        /// Original ask_user tool-call ID.
        ask_id: String,
        /// How to present the questions.
        #[serde(default)]
        mode: AskMode,
        /// One question per entry. Single mode typically has 1; Batch has N.
        questions: Vec<AskQuestion>,
    },

    /// The active ask was accepted or dismissed by the agent.
    #[serde(rename = "ask_resolved")]
    AskResolved {
        ask_id: String,
        resolution: AskResolution,
    },

    /// The ask response was rejected without consuming the active prompt.
    #[serde(rename = "ask_rejected")]
    AskRejected { ask_id: String, message: String },
}

/// Priority lane for daemon event routing.
///
/// Critical: session lifecycle, errors, permissions — never delayed or dropped.
/// Standard: messages, tool results, plan reviews — blocked on queue full.
/// Bulk:     exec_progress, audit_record, compact_delta — lossy, batchable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventLane {
    Critical,
    Standard,
    Bulk,
}

impl Agent2Ui {
    /// Classify this event into a routing lane for daemon multiplexing.
    pub fn lane(&self) -> EventLane {
        match self {
            // ── Critical: session lifecycle, errors, user interaction ──
            Agent2Ui::SessionCreated { .. }
            | Agent2Ui::SessionRestored { .. }
            | Agent2Ui::Error { .. }
            | Agent2Ui::Cancelled
            | Agent2Ui::ShutdownAck
            | Agent2Ui::Ready
            | Agent2Ui::Done
            | Agent2Ui::TurnStart { .. }
            | Agent2Ui::TurnEnd { .. }
            | Agent2Ui::PermissionRequest { .. }
            | Agent2Ui::AskUser { .. }
            | Agent2Ui::AskResolved { .. }
            | Agent2Ui::AskRejected { .. }
            | Agent2Ui::PlanSubmitted { .. }
            | Agent2Ui::PlanResolved { .. }
            | Agent2Ui::RoundComplete { .. }
            | Agent2Ui::Pong => EventLane::Critical,

            // ── Bulk: high-frequency, loss-tolerant streaming ──
            Agent2Ui::ExecProgress { .. }
            | Agent2Ui::AuditRecord { .. }
            | Agent2Ui::CompactDelta { .. }
            | Agent2Ui::UsageUpdated { .. }
            | Agent2Ui::CacheDiagnostics { .. } => EventLane::Bulk,

            // ── Standard: everything else ──
            Agent2Ui::ProviderRetrying { .. }
            | Agent2Ui::MoreTurns { .. }
            | Agent2Ui::RoundDelta { .. }
            | Agent2Ui::ToolResults { .. }
            | Agent2Ui::ToolExecDelta { .. }
            | Agent2Ui::ToolCallPreview { .. }
            | Agent2Ui::CodeDelta { .. }
            | Agent2Ui::ToolNotice { .. }
            | Agent2Ui::Dashboard { .. }
            | Agent2Ui::CompactStart { .. }
            | Agent2Ui::CompactEnd { .. }
            | Agent2Ui::SkillsChanged { .. }
            | Agent2Ui::SkillOperationResolved { .. } => EventLane::Standard,
        }
    }
}

fn default_load_count() -> u32 {
    20
}

/// Streaming block kind for RoundDelta.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum RoundDeltaKind {
    /// Model is reasoning (thinking phase).
    Thinking,
    /// Agent is executing tool calls — tool names follow.
    ToolCalling,
    /// Model is generating the visible answer.
    Answering,
}

/// A single code delta record for persistence.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CodeDeltaRecord {
    pub timestamp: u64,
    pub lines_added: usize,
    pub lines_removed: usize,
    pub files_created: usize,
    pub files_deleted: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub file: Option<String>,
}

/// Daily aggregated code stats.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CodeDaily {
    pub date: String,
    pub lines_added: usize,
    pub lines_removed: usize,
    pub files_created: usize,
    pub files_deleted: usize,
}

// ═══════════════════════════════════════════════════════════════════════════
// Daemon ↔ Frontend protocol (socket transport)
// ═══════════════════════════════════════════════════════════════════════════

/// Frontend → Daemon frame. Wraps Ui2Agent with the target session seed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrontendToDaemon {
    pub seed: String,
    #[serde(flatten)]
    pub frame: Ui2Agent,
}

/// Daemon → Frontend frame. Wraps Agent2Ui with the source session seed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonToFrontend {
    pub seed: String,
    #[serde(flatten)]
    pub event: Agent2Ui,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_response_single_round_trip() {
        let json = r#"{"type":"ask_response","ask_id":"a1","answers":[{"question_id":"q1","answer":"Option A"}]}"#;
        let frame: Ui2Agent = serde_json::from_str(json).expect("deserialize AskResponse");
        match &frame {
            Ui2Agent::AskResponse { ask_id, answers } => {
                assert_eq!(ask_id, "a1");
                assert_eq!(answers.len(), 1);
                assert_eq!(answers[0].question_id, "q1");
                assert_eq!(answers[0].answer, "Option A");
            }
            other => panic!(
                "expected AskResponse, got {:?}",
                std::any::type_name_of_val(other)
            ),
        }
        let back = serde_json::to_string(&frame).expect("serialize");
        assert!(back.contains("\"type\":\"ask_response\""));
        assert!(back.contains("\"ask_id\":\"a1\""));
    }

    #[test]
    fn ask_response_batch_round_trip() {
        let json = r#"{"type":"ask_response","ask_id":"a2","answers":[{"question_id":"q1","answer":"A"},{"question_id":"q2","answer":"Custom"}]}"#;
        let frame: Ui2Agent = serde_json::from_str(json).expect("deserialize batch");
        match &frame {
            Ui2Agent::AskResponse { ask_id, answers } => {
                assert_eq!(ask_id, "a2");
                assert_eq!(answers.len(), 2);
                assert_eq!(answers[0].question_id, "q1");
                assert_eq!(answers[1].answer, "Custom");
            }
            other => panic!(
                "expected AskResponse, got {:?}",
                std::any::type_name_of_val(other)
            ),
        }
    }

    #[test]
    fn ask_dismiss_round_trip() {
        let json = r#"{"type":"ask_dismiss","ask_id":"a1"}"#;
        let frame: Ui2Agent = serde_json::from_str(json).expect("deserialize AskDismiss");
        match &frame {
            Ui2Agent::AskDismiss { ask_id } => assert_eq!(ask_id, "a1"),
            other => panic!(
                "expected AskDismiss, got {:?}",
                std::any::type_name_of_val(other)
            ),
        }
    }

    #[test]
    fn ask_user_single_serialize() {
        let event = Agent2Ui::AskUser {
            turn_id: "t1".into(),
            round_num: 0,
            ask_id: "a1".into(),
            mode: AskMode::Single,
            questions: vec![AskQuestion {
                id: "q1".into(),
                question: "Choose one".into(),
                options: vec!["A".into(), "B".into()],
                allow_custom: true,
            }],
        };
        let json = serde_json::to_string(&event).expect("serialize");
        assert!(json.contains("\"type\":\"ask_user\""));
        assert!(json.contains("\"ask_id\":\"a1\""));
        assert!(json.contains("\"mode\":\"single\""));
    }

    #[test]
    fn ask_user_batch_serialize() {
        let event = Agent2Ui::AskUser {
            turn_id: "t2".into(),
            round_num: 1,
            ask_id: "b1".into(),
            mode: AskMode::Batch,
            questions: vec![
                AskQuestion {
                    id: "q1".into(),
                    question: "Arch?".into(),
                    options: vec!["A".into(), "B".into()],
                    allow_custom: false,
                },
                AskQuestion {
                    id: "q2".into(),
                    question: "Strategy?".into(),
                    options: vec![],
                    allow_custom: true,
                },
            ],
        };
        let json = serde_json::to_string(&event).expect("serialize");
        assert!(json.contains("\"type\":\"ask_user\""));
        assert!(json.contains("\"mode\":\"batch\""));
        let back: Agent2Ui = serde_json::from_str(&json).expect("deserialize");
        match &back {
            Agent2Ui::AskUser {
                turn_id,
                round_num,
                ask_id,
                mode,
                questions,
            } => {
                assert_eq!(turn_id, "t2");
                assert_eq!(*round_num, 1);
                assert_eq!(ask_id, "b1");
                assert!(matches!(mode, AskMode::Batch));
                assert_eq!(questions.len(), 2);
                assert!(!questions[0].allow_custom);
                assert!(questions[1].allow_custom);
            }
            other => panic!(
                "expected AskUser, got {:?}",
                std::any::type_name_of_val(other)
            ),
        }
    }

    #[test]
    fn ask_user_round_trip_preserves_turn_and_call_identity() {
        let event = Agent2Ui::AskUser {
            turn_id: "t7".into(),
            round_num: 3,
            ask_id: "call-ask-1".into(),
            mode: AskMode::Batch,
            questions: vec![AskQuestion {
                id: "q1".into(),
                question: "Choose".into(),
                options: vec!["A".into()],
                allow_custom: true,
            }],
        };
        let json = serde_json::to_string(&event).unwrap();
        let decoded: Agent2Ui = serde_json::from_str(&json).unwrap();

        assert!(matches!(
            decoded,
            Agent2Ui::AskUser {
                turn_id,
                round_num: 3,
                ask_id,
                ..
            } if turn_id == "t7" && ask_id == "call-ask-1"
        ));
    }

    #[test]
    fn ask_acknowledgements_round_trip() {
        let events = [
            Agent2Ui::AskResolved {
                ask_id: "a1".into(),
                resolution: AskResolution::Answered,
            },
            Agent2Ui::AskRejected {
                ask_id: "a1".into(),
                message: "stale ask_id".into(),
            },
        ];

        for event in events {
            let json = serde_json::to_string(&event).unwrap();
            serde_json::from_str::<Agent2Ui>(&json).unwrap();
        }
    }

    #[test]
    fn legacy_scalar_ask_response_is_rejected() {
        assert!(
            serde_json::from_str::<Ui2Agent>(r#"{"type":"ask_response","answer":"A"}"#,).is_err()
        );
    }

    #[test]
    fn session_activity_serializes_stable_runtime_state() {
        let activity = SessionActivity {
            seed: "session-a".into(),
            state: SessionActivityState::WaitingUser,
            turn_id: Some("t7".into()),
            seq: 4,
            updated_at: 123,
        };

        let json = serde_json::to_value(activity).unwrap();
        assert_eq!(json["state"], "waiting_user");
        assert_eq!(json["turn_id"], "t7");
        assert_eq!(json["seq"], 4);
    }

    // ── Lane classification tests ──

    #[test]
    fn exec_progress_is_bulk_lane() {
        let event = Agent2Ui::ExecProgress {
            tool_call_id: "t1".into(),
            stream: "stdout".into(),
            seq: 1,
            chunk: "hello".into(),
        };
        assert_eq!(event.lane(), EventLane::Bulk);
    }

    #[test]
    fn audit_record_is_bulk_lane() {
        let event = Agent2Ui::AuditRecord {
            tool_name: "exec".into(),
            result_summary: "ok".into(),
            success: true,
            time: "2024-01-01".into(),
            args: "{}".into(),
        };
        assert_eq!(event.lane(), EventLane::Bulk);
    }

    #[test]
    fn session_lifecycle_is_critical_lane() {
        for event in [
            Agent2Ui::SessionCreated {
                seed: "s".into(),
            },
            Agent2Ui::Error {
                message: "boom".into(),
            },
            Agent2Ui::Cancelled,
            Agent2Ui::Done,
            Agent2Ui::TurnStart {
                turn_id: "t".into(),
                user_text: "hello".into(),
            },
            Agent2Ui::TurnEnd {
                turn_id: "t".into(),
                stop_reason: None,
                usage: None,
            },
            Agent2Ui::PermissionRequest {
                tool_call_id: "p".into(),
                tool_name: "exec".into(),
                reason: "need".into(),
                paths: vec![],
                category: "shell".into(),
                level: 1,
                risk: PermissionRisk::Low,
                consequence: "none".into(),
            },
        ] {
            assert_eq!(event.lane(), EventLane::Critical, "{event:?}");
        }
    }

    #[test]
    fn round_delta_is_standard_lane() {
        let event = Agent2Ui::RoundDelta {
            turn_id: "t".into(),
            round_num: 1,
            kind: RoundDeltaKind::Answering,
            delta: "text".into(),
        };
        assert_eq!(event.lane(), EventLane::Standard);
    }

    #[test]
    fn provider_retry_is_standard_lane() {
        let event = Agent2Ui::ProviderRetrying {
            turn_id: "t".into(),
            round_num: 1,
            attempt: 1,
            max_retries: 3,
            delay_secs: 1,
            error: "timeout".into(),
        };
        assert_eq!(event.lane(), EventLane::Standard);
    }

    #[test]
    fn tool_results_is_standard_lane() {
        let event = Agent2Ui::ToolResults {
            turn_id: "t".into(),
            round_num: 0,
            results: vec![],
        };
        assert_eq!(event.lane(), EventLane::Standard);
    }

    #[test]
    fn compact_delta_is_bulk_lane() {
        let event = Agent2Ui::CompactDelta {
            delta: "summary...".into(),
        };
        assert_eq!(event.lane(), EventLane::Bulk);
    }

    #[test]
    fn usage_update_is_bulk_lane() {
        let event = Agent2Ui::UsageUpdated {
            turn_id: "t".into(),
            round_num: 0,
            usage: deepx_types::UsageInfo::default(),
            context_limit: 128_000,
            model: "deepseek-chat".into(),
        };
        assert_eq!(event.lane(), EventLane::Bulk);
    }

    #[test]
    fn all_variants_have_explicit_lane() {
        // Smoke test: call lane() on every major variant to ensure
        // exhaustive coverage without panicking.
        let events = [
            Agent2Ui::SessionCreated { seed: "s".into() },
            Agent2Ui::SessionRestored { seed: "s".into(), turns: vec![], tokens_used: 0, cache_hit_pct: 0.0, usage: None, usage_totals: deepx_types::UsageInfo::default(), usage_requests: 0, cache_reported_requests: 0, total_turns: 0, has_more: false },
            Agent2Ui::TurnStart { turn_id: "t".into(), user_text: "u".into() },
            Agent2Ui::TurnEnd { turn_id: "t".into(), stop_reason: None, usage: None },
            Agent2Ui::RoundDelta { turn_id: "t".into(), round_num: 0, kind: RoundDeltaKind::Thinking, delta: "d".into() },
            Agent2Ui::RoundComplete { turn_id: "t".into(), round_num: 0, thinking: None, answer: None, tool_calls: vec![], blocks: vec![], is_final: false },
            Agent2Ui::ToolResults { turn_id: "t".into(), round_num: 0, results: vec![] },
            Agent2Ui::ToolExecDelta { tool_call_id: "c".into(), delta: "d".into() },
            Agent2Ui::ExecProgress { tool_call_id: "c".into(), stream: "stdout".into(), seq: 0, chunk: "c".into() },
            Agent2Ui::ToolCallPreview { turn_id: "t".into(), round_num: 0, index: 0, id: "c".into(), name: "n".into(), args_so_far: "{}".into() },
            Agent2Ui::CodeDelta { lines_added: 1, lines_removed: 0, files_created: 0, files_deleted: 0, file: None },
            Agent2Ui::AuditRecord { tool_name: "n".into(), result_summary: "s".into(), success: true, time: "t".into(), args: "{}".into() },
            Agent2Ui::ToolNotice { level: "info".into(), message: "m".into() },
            Agent2Ui::Dashboard { hp_connected: false, session_seed: "s".into(), tool_calls_total: 0, tool_failures: 0, current_phase: "idle".into(), streaming: false, dsml_compat_count: 0, documents: vec![], recent_edits: vec![], tasks: vec![], session_title: None, usage: None, context_limit: 0, model: None },
            Agent2Ui::UsageUpdated { turn_id: "t".into(), round_num: 0, usage: deepx_types::UsageInfo::default(), context_limit: 0, model: "m".into() },
            Agent2Ui::ProviderRetrying { turn_id: "t".into(), round_num: 0, attempt: 1, max_retries: 3, delay_secs: 1, error: "timeout".into() },
            Agent2Ui::Done,
            Agent2Ui::CompactStart { turns_total: 1, turns_keeping: 1 },
            Agent2Ui::CompactEnd { summary_chars: 0, turns_compacted: 1, turns_removed: 0 },
            Agent2Ui::CompactDelta { delta: "d".into() },
            Agent2Ui::Cancelled,
            Agent2Ui::ShutdownAck,
            Agent2Ui::Ready,
            Agent2Ui::Error { message: "e".into() },
            Agent2Ui::Pong,
        ];
        for event in &events {
            let _lane = event.lane(); // must not panic
        }
    }
}
