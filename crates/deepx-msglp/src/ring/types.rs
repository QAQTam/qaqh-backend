//! Shared types for the new Ring architecture.
//!
//! These types form the interface contract between the Loop dispatcher
//! and each Engine. An Engine receives a `&mut RingContext` and returns
//! an `Outcome` telling the Loop what to do next.
//!
//! # Architecture layers
//!
//! ```text
//! ┌─────────────────────────────────────────────┐
//! │  Loop (process-level: I/O, cancel, pending) │
//! │  ┌───────────────────────────────────────┐  │
//! │  │  SessionBundle (session-level)        │  │
//! │  │  agent, stats, turn, tool             │  │
//! │  └───────────────────────────────────────┘  │
//! │  session_engine, input, compact, misc       │
//! └─────────────────────────────────────────────┘
//! ```
//!
//! `SessionBundle` is the unit of session isolation. In a future
//! multi-session architecture, Loop would hold `HashMap<Seed, SessionBundle>`
//! and swap the active one on session switch. The rest of the Loop
//! (I/O channels, cancel token, session-agnostic engines) stays unchanged.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;

use deepx_proto::{Agent2Ui, AskMode, AskQuestion};
use deepx_types::UsageInfo;

use crate::state::agent::AgentState;

// ═══════════════════════════════════════════════════════
// CancelToken — shared abort flag
// ═══════════════════════════════════════════════════════

/// Cancellation token shared between Loop and all Engines.
///
/// Each Engine receives a `&CancelToken` via `RingContext`.
/// Long-running operations (Gate SSE, tool threads) clone the
/// inner `Arc<AtomicBool>` via `.arc()` and poll it periodically.
///
/// Setting the token is the responsibility of the Loop dispatcher
/// (on receiving `Ui2Agent::Cancel` or session-switch commands).
/// Engines only read it.
#[derive(Clone)]
pub struct CancelToken {
    pub(crate) inner: Arc<AtomicBool>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(AtomicBool::new(false)),
        }
    }
    /// Signal cancellation. Non-blocking.
    pub fn set(&self) {
        self.inner.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    /// Clear the cancel flag (called when starting a new turn).
    pub fn clear(&self) {
        self.inner.store(false, std::sync::atomic::Ordering::SeqCst);
    }
    /// Check if cancellation has been requested.
    pub fn is_set(&self) -> bool {
        self.inner.load(std::sync::atomic::Ordering::SeqCst)
    }
    /// Clone the inner Arc for passing to threads / Gate layer.
    pub fn arc(&self) -> Arc<AtomicBool> {
        self.inner.clone()
    }
}

// ═══════════════════════════════════════════════════════
// LoopPhase — what's currently running
// ═══════════════════════════════════════════════════════

/// Tracks what the Loop is currently doing.
///
/// Used by `handle_cancel()` to decide whether to also cancel
/// the current tool execution (if `ToolsRunning`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LoopPhase {
    /// Waiting for the next Ui2Agent command.
    Idle,
    /// Gate SSE stream is in progress.
    GateRunning,
    /// Tools are executing (parallel or serial).
    ToolsRunning,
    /// Goal mode active: autonomous plan execution in progress.
    /// The GoalEngine drives turn scheduling and compact refresh.
    GoalActive,
}

// ═══════════════════════════════════════════════════════
// PendingState — interrupt queue
// ═══════════════════════════════════════════════════════

/// Queue of deferred commands that arrived while the Loop was busy.
///
/// Interrupt-type commands (Cancel, ResumeSession, NewSession, Shutdown)
/// are stored here when they arrive mid-turn. They are processed once
/// the current operation yields (TurnComplete / YieldToUser / Error).
#[derive(Debug, Default)]
pub struct PendingState {
    /// Session seed to resume after current operation finishes.
    pub session: Option<String>,
    /// Create a new session as soon as possible.
    pub new_session: bool,
    /// Exit the main loop.
    pub shutdown: bool,
    /// Reload config from disk.
    pub reload_config: bool,
}

impl PendingState {
    pub fn is_empty(&self) -> bool {
        self.session.is_none() && !self.new_session && !self.shutdown && !self.reload_config
    }
    pub fn clear(&mut self) {
        *self = PendingState::default();
    }
}

// ═══════════════════════════════════════════════════════
// Outcome — the Engine→Loop protocol
// ═══════════════════════════════════════════════════════

/// Returned by every Engine after processing a command.
/// The Loop dispatcher matches on this to decide the next action.
///
/// # Flow diagram
///
/// ```text
/// UserInput → InputEngine → ContinueTurn → TurnEngine.run()
///   ├── Gate SSE → parse → tools
///   ├── Tools complete → ContinueTurn (recursive)
///   ├── Permission needed → YieldToUser → (wait)
///   ├── ask_user called → YieldToUser → (wait)
///   └── Turn complete → TurnComplete → Done
/// ```
pub enum Outcome {
    /// Turn finished successfully. Loop emits TurnEnd + Done, returns to Idle.
    TurnComplete {
        turn_id: String,
        usage: Option<UsageInfo>,
    },

    /// A suspended turn was deliberately aborted by the user.
    TurnAborted {
        turn_id: String,
        usage: Option<UsageInfo>,
        /// The reader already queued the interrupt command that caused this
        /// outcome; Loop must consume it without emitting a second terminal.
        consume_queued_interrupt: bool,
    },

    /// A running turn ended because the provider or gate failed. This is a
    /// terminal transaction: Loop emits Error + TurnEnd + Done and returns
    /// to Idle so clients never remain stuck in `running`.
    TurnFailed {
        turn_id: String,
        usage: Option<UsageInfo>,
        message: String,
    },

    /// Turn needs another lap around the ring (tools executed → back to gate).
    /// Loop calls TurnEngine.run() recursively.
    ContinueTurn {
        turn_id: String,
        round_num: u32,
        usage: Option<UsageInfo>,
    },

    /// Turn paused — waiting for user action.
    /// Loop returns to Idle and only processes PermissionResponse, Cancel,
    /// or session-switch commands until the turn resumes.
    YieldToUser {
        turn_id: String,
        reason: YieldReason,
    },

    /// Command was handled. Loop returns to Idle.
    Handled,

    /// Fatal error during command processing. Loop emits Error, returns to Idle.
    Error(String),

    /// Loop should exit cleanly.
    Shutdown,
}

/// Why the turn yielded to the user.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum YieldReason {
    /// One or more tool calls need permission approval.
    PermissionPending,
    /// The ask_user tool was called — waiting for user's response.
    AskUser,
    /// A plan was submitted for review — waiting for user approval/rejection.
    PlanReview,
}

// ═══════════════════════════════════════════════════════
// TurnState — saved turn snapshot for suspend/resume
// ═══════════════════════════════════════════════════════

/// One authorized ask_user tool call waiting for a typed user response.
#[derive(Debug, Clone)]
pub struct PendingAsk {
    pub call_id: String,
    pub mode: AskMode,
    pub questions: Vec<AskQuestion>,
}

/// A plan_submit tool call intercepted and waiting for user review.
#[derive(Debug, Clone)]
pub struct PendingPlan {
    pub call_id: String,
    pub content: String,
}

/// A todo_activate tool call intercepted and waiting for user review.
/// Carries the todo items that will be activated if approved.
#[derive(Debug, Clone)]
pub struct PendingTodoActivation {
    pub call_id: String,
    pub items: Vec<deepx_proto::TodoActivationItem>,
}

/// Serialized snapshot of a turn mid-execution.
/// Stored in `TurnEngine.suspended` when a turn is paused for permissions
/// or awaiting user input. Restored via `TurnEngine.resume()`.
pub struct AdmittedTool {
    pub call_id: String,
    pub auth: Box<deepx_workspace::authorization::AuthorizedToolCall>,
}

pub struct TurnState {
    pub turn_id: String,
    pub round_num: u32,
    pub usage: Option<UsageInfo>,
    /// Tool call IDs still awaiting permission approval.
    pub pending_permission_ids: Vec<String>,
    /// Authorized calls held until every permission decision is recorded.
    pub deferred_authorized: Vec<AdmittedTool>,
    /// Original assistant tool-call order for deterministic conflict handling.
    pub tool_call_order: Vec<String>,
    /// Calls that must run after earlier conflicting writers.
    pub serial_call_ids: HashSet<String>,
    /// Authorized ask_user calls in assistant tool-call order.
    pub pending_asks: VecDeque<PendingAsk>,
    /// Intercepted plan_submit calls waiting for user review.
    pub pending_plans: VecDeque<PendingPlan>,
    /// Intercepted todo_activate call waiting for user review.
    pub pending_todo_activation: Option<PendingTodoActivation>,
    /// Session ID at the time of suspension (validated on resume to prevent
    /// stale turn resumption after a session switch).
    pub session_id: String,
    /// Why this turn was suspended.
    pub reason: YieldReason,
}

// ═══════════════════════════════════════════════════════
// Emitter — type-safe event output
// ═══════════════════════════════════════════════════════

/// Abstraction over the output channel.
///
/// Engines call `emit()` / `emit_delta()` without knowing whether
/// they're writing to a real mpsc channel (production) or a mock
/// (unit tests). This trait is the single point where all Agent2Ui
/// events enter the output pipeline.
pub trait Emitter {
    /// Emit a critical event. Blocks if the channel is full.
    /// The event MUST be delivered (TurnStart, TurnEnd, ToolResults, etc.).
    fn emit(&self, event: Agent2Ui);

    /// Emit a streaming delta. May drop if the channel is full.
    /// Used for high-frequency events (RoundDelta, ExecProgress).
    fn emit_delta(&self, event: Agent2Ui);

    /// Emit a Ringing 领域事件（生产点直接构造，禁止 Agent2Ui→Ringing 转换）。
    /// 默认空实现：未启用 Ringing 出口时零行为变化。
    fn emit_domain(&self, _event: deepx_domain::DomainEvent) {}
}

/// writer 线程通道载荷：legacy 帧或 Ringing worker envelope（互不嵌套）。
#[derive(Debug, Clone)]
pub enum WriterEvent {
    /// legacy JSON-LP 帧（`Agent2Ui`）。
    Legacy(Agent2Ui),
    /// Ringing worker envelope（`wire: "Ringing_domain_v1"`）。
    Ringing(deepx_ringing::RingingWorkerEventEnvelope),
}

/// 命令通道载荷：legacy 帧 + 可选的 Ringing command_id（causation）。
///
/// Ringing 命令经 ingress 映射后仍走 legacy 引擎；`causation` 随帧传递，
/// 使执行期间产出的 Ringing 事件能携带 `causation_id = command_id`。
#[derive(Debug, Clone)]
pub struct WorkerCommand {
    pub frame: deepx_proto::Ui2Agent,
    pub causation: Option<String>,
}

// ═══════════════════════════════════════════════════════
// RingContext — what each Engine can access
// ═══════════════════════════════════════════════════════

/// The shared service layer passed to every Engine.
///
/// Engines receive `&mut RingContext` and can access:
/// - `agent` — the MessageStore, config, session metadata
/// - `emitter` — output channel (via the Emitter trait)
/// - `cancel` — cancellation signal (read-only for engines)
/// - `phase` — current LoopPhase (engines set this)
/// - `pending` — deferred interrupt queue
/// - `writer_dead` — set when stdout pipe breaks
/// - `stats` — code delta accumulator
/// - `notify` — desktop notification channel
///
/// Engines CANNOT access:
/// - Other engines' private state
/// - The raw I/O channels (cmd_rx, event_tx)
/// - Other sessions' state
pub struct RingContext<'a> {
    /// Core agent state: message store, config, session metadata.
    pub agent: &'a mut AgentState,
    /// Output event emitter (trait object for testability).
    pub emitter: &'a dyn Emitter,
    /// Cancellation signal. Engines read; only Loop writes.
    pub cancel: &'a CancelToken,
    /// Current loop phase. Engines set this to GateRunning / ToolsRunning.
    pub phase: &'a mut LoopPhase,
    /// Deferred interrupt queue. Engines can check if a session switch
    /// or shutdown is pending between rounds.
    pub pending: &'a mut PendingState,
    /// Set to true when the writer thread exits (stdout pipe broken).
    pub writer_dead: &'a Arc<AtomicBool>,
    /// Accumulated code deltas for the current session.
    pub stats: &'a mut StatsCollector,
    /// Desktop notification channel.
    pub notify: &'a NotifyHandle,
}

// ═══════════════════════════════════════════════════════
// StatsCollector
// ═══════════════════════════════════════════════════════

/// Accumulates code delta records during a session.
/// Flushed to `code_stats.jsonl` on TurnComplete and session save.
pub struct StatsCollector {
    pub code_stats: Vec<deepx_proto::CodeDeltaRecord>,
}

impl StatsCollector {
    pub fn new() -> Self {
        Self {
            code_stats: Vec::new(),
        }
    }
    pub fn push_delta(&mut self, delta: deepx_proto::CodeDeltaRecord) {
        self.code_stats.push(delta);
    }
    /// Persist accumulated deltas to disk. Clears the in-memory buffer.
    pub fn flush(&mut self, seed: &str) {
        if self.code_stats.is_empty() || seed.is_empty() {
            return;
        }
        let dir = deepx_types::platform::sessions_dir().join(seed);
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("code_stats.jsonl");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            use std::io::Write;
            for delta in self.code_stats.drain(..) {
                let line = serde_json::to_string(&delta).unwrap_or_default();
                let _ = writeln!(f, "{line}");
            }
            let _ = f.flush();
            let _ = f.sync_all();
        }
    }
}

// ═══════════════════════════════════════════════════════
// NotifyHandle
// ═══════════════════════════════════════════════════════

/// Wrapper around the notification thread's sender channel.
pub struct NotifyHandle {
    pub tx: mpsc::Sender<crate::services::notification::NotifyMessage>,
}

impl NotifyHandle {
    pub fn notify(&self, body: String) {
        let _ = self
            .tx
            .send(crate::services::notification::NotifyMessage::Toast(body));
    }
}

// ═══════════════════════════════════════════════════════
// SessionBundle — session-scoped state
// ═══════════════════════════════════════════════════════

/// Groups all state that is scoped to a single session.
///
/// # Single-session architecture (current)
///
/// Loop holds a single `SessionBundle`. On session switch, the current
/// bundle is flushed to disk and replaced with the target session's data.
///
/// # Multi-session architecture (future)
///
/// Loop would hold `HashMap<String, SessionBundle>` keyed by session seed.
/// The active session is looked up from the map. Switching sessions
/// preserves the old bundle in memory (with LRU eviction for memory
/// pressure). The IPC protocol would need a `session_id` field added
/// to `Ui2Agent` variants to route commands to the correct session.
///
/// ```text
/// // Future multi-session sketch:
/// pub struct Loop {
///     sessions: HashMap<String, SessionBundle>,  // seed → bundle
///     active_seed: String,
///     // ... process-level state unchanged
/// }
/// ```
///
/// # Session lifecycle
///
/// - **Created**: `CreateSession` or auto-created on first `UserInput`
/// - **Flushed**: `TurnComplete` → `agent.msg.flush_meta()` + `stats.flush()`
/// - **Swapped**: `ResumeSession` → current flushed, target loaded from disk
/// - **Destroyed**: `Shutdown` → final flush + process exit
pub struct SessionBundle {
    /// Core agent state: MessageStore, Config, SessionMeta, ToolDefs.
    pub agent: AgentState,
    /// Accumulated code deltas for this session.
    pub stats: StatsCollector,
    /// Turn engine: suspended turn state, gate→tools cycle.
    pub turn: super::engine_turn::TurnEngine,
    /// Tool engine: pending approvals, trusted folders, execution.
    pub tool: super::engine_tool::ToolEngine,
}

impl SessionBundle {
    pub fn new(agent: AgentState) -> Self {
        Self {
            agent,
            stats: StatsCollector::new(),
            turn: super::engine_turn::TurnEngine::new(),
            tool: super::engine_tool::ToolEngine::new(),
        }
    }

    /// Flush session metadata and code stats to disk.
    /// Called on TurnComplete and before session switch.
    pub fn flush(&mut self) {
        self.agent.session.skills = self.agent.skills.session_state();
        if !self.agent.ephemeral && !self.agent.session.seed.is_empty() {
            deepx_session::SessionManager::global()
                .persist_skills(&self.agent.session.seed, self.agent.session.skills.clone());
        }
        self.agent.msg.flush_meta(
            &self.agent.config.model,
            &self.agent.config.reasoning_effort,
        );
        self.stats.flush(&self.agent.session.seed);
    }
}
