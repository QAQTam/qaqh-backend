//! Shared types for the new Ringing V1 architecture.
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use qaqh_domain::{AskMode, AskQuestion};
use qaqh_types::UsageInfo;

use crate::agent::state::agent::AgentState;

// ═══════════════════════════════════════════════════════
// CancelToken — shared abort flag
// ═══════════════════════════════════════════════════════

/// Cancellation token shared between Loop and all Engines.
///
/// Each Engine receives a `&CancelToken` via `RingContext`.
/// Long-running operations (Gate SSE, tool threads) clone the
/// inner `Arc<AtomicBool>` via `.arc()` and poll it periodically.
///
/// Tokens form a parent/child tree: cancelling a parent is immediately
/// visible to every descendant, while cancelling a child does not affect its
/// parent or siblings. Setting the root token is the responsibility of the
/// Loop dispatcher; Engines only read it.
#[derive(Clone)]
pub struct CancelToken {
    node: Arc<CancelNode>,
    /// 可选的取消判定钩子（测试用）：置位后 `is_set()` 完全由钩子裁决。
    ///
    /// 生产路径恒为 `None`，`is_set()` 退化为读 `inner`——零行为变化。
    /// 测试用它把取消点钉在**批执行中途**（先生成批、后在收割窗口置位）。
    #[allow(clippy::type_complexity)]
    query: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

struct CancelNode {
    /// Cancellation requested directly on this node.
    own: AtomicBool,
    /// Effective cancellation including the live ancestor chain.
    effective: Arc<AtomicBool>,
    parent: Option<Weak<CancelNode>>,
    children: Mutex<Vec<Weak<CancelNode>>>,
}

impl CancelNode {
    fn root() -> Arc<Self> {
        Arc::new(Self {
            own: AtomicBool::new(false),
            effective: Arc::new(AtomicBool::new(false)),
            parent: None,
            children: Mutex::new(Vec::new()),
        })
    }

    fn child(parent: &Arc<Self>) -> Arc<Self> {
        let inherited = parent.effective.load(Ordering::SeqCst);
        let child = Arc::new(Self {
            own: AtomicBool::new(inherited),
            effective: Arc::new(AtomicBool::new(inherited)),
            parent: Some(Arc::downgrade(parent)),
            children: Mutex::new(Vec::new()),
        });
        parent
            .children
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(Arc::downgrade(&child));
        // Catch a parent cancellation racing with child registration.
        if parent.effective.load(Ordering::SeqCst) {
            child.own.store(true, Ordering::SeqCst);
        }
        child.recompute();
        child
    }

    fn set_own(&self, cancelled: bool) {
        self.own.store(cancelled, Ordering::SeqCst);
        self.recompute();
        if cancelled {
            self.latch_descendants();
        }
    }

    fn recompute(&self) {
        // Re-read after publishing so a concurrent parent update cannot be
        // overwritten by an older child snapshot.
        loop {
            let own = self.own.load(Ordering::SeqCst);
            let parent = self
                .parent
                .as_ref()
                .and_then(Weak::upgrade)
                .is_some_and(|parent| parent.effective.load(Ordering::SeqCst));
            self.effective.store(own || parent, Ordering::SeqCst);
            let own_after = self.own.load(Ordering::SeqCst);
            let parent_after = self
                .parent
                .as_ref()
                .and_then(Weak::upgrade)
                .is_some_and(|parent| parent.effective.load(Ordering::SeqCst));
            if own == own_after && parent == parent_after {
                break;
            }
        }

        for child in self.live_children() {
            child.recompute();
        }
    }

    fn latch_descendants(&self) {
        for child in self.live_children() {
            child.own.store(true, Ordering::SeqCst);
            child.recompute();
            child.latch_descendants();
        }
    }

    fn live_children(&self) -> Vec<Arc<Self>> {
        let mut guard = self
            .children
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut live = Vec::new();
        let mut weak = Vec::new();
        for child in guard.drain(..) {
            if let Some(child) = child.upgrade() {
                weak.push(Arc::downgrade(&child));
                live.push(child);
            }
        }
        *guard = weak;
        live
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelToken {
    pub fn new() -> Self {
        Self {
            node: CancelNode::root(),
            query: None,
        }
    }

    /// 测试专用：用自定义判定钩子替换取消判定（生产路径不用）。
    ///
    /// 钩子返回 true 即视为「取消已到达」，用于精确控制取消时点。
    pub fn with_query_hook(hook: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        Self {
            node: CancelNode::root(),
            query: Some(hook),
        }
    }

    /// Derive a child token. Cancelling this token also cancels the child;
    /// cancelling the child never affects this token or its other children.
    pub fn child(&self) -> Self {
        Self {
            node: CancelNode::child(&self.node),
            query: None,
        }
    }

    /// Signal cancellation. Non-blocking.
    pub fn set(&self) {
        self.node.set_own(true);
    }

    /// Clear the cancel flag (called when starting a new turn).
    pub fn clear(&self) {
        self.node.set_own(false);
    }

    /// Check if cancellation has been requested.
    pub fn is_set(&self) -> bool {
        if let Some(hook) = &self.query {
            return hook();
        }
        self.node.effective.load(Ordering::SeqCst)
    }

    /// Clone the inner Arc for passing to threads / Gate layer.
    pub fn arc(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.node.effective)
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
    /// Exit the main loop.
    pub shutdown: bool,
}

impl PendingState {
    pub fn is_empty(&self) -> bool {
        !self.shutdown
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
/// Carries the plan review items that will be activated if approved.
#[derive(Debug, Clone)]
pub struct PendingTodoActivation {
    pub call_id: String,
    pub items: Vec<qaqh_domain::PlanReviewItem>,
}

/// Serialized snapshot of a turn mid-execution.
/// Stored in `TurnEngine.suspended` when a turn is paused for permissions
/// or awaiting user input. Restored via `TurnEngine.resume()`.
pub struct AdmittedTool {
    pub call_id: String,
    pub auth: Box<qaqh_workspace::AuthorizedToolCall>,
    /// Explicit context captured at admission and carried across the worker
    /// thread boundary.
    pub scope: qaqh_workspace::runtime::ToolExecutionScope,
}

pub struct TurnState {
    pub turn_id: String,
    pub round_num: u32,
    pub usage: Option<UsageInfo>,
    /// Tool call IDs still awaiting permission approval.
    pub pending_permission_ids: Vec<String>,
    /// Authorized calls held until every permission decision is recorded.
    pub deferred_authorized: Vec<AdmittedTool>,
    /// Original assistant tool-call order for deterministic write serialization.
    pub tool_call_order: Vec<String>,
    /// Calls that must run after earlier same-file writers.
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
/// Engines call `emit_domain()` / `emit_timeline()` without knowing whether
/// they're writing to a real mpsc channel (production) or a mock
/// (unit tests). This trait is the single point where all Ringing
/// events enter the output pipeline. (legacy `emit`/`emit_delta` 已随 M3 拆除)
pub trait Emitter {
    /// Emit a Ringing 领域事件（生产点直接构造，禁止 Agent2Ui→Ringing 转换）。
    /// 默认空实现：未启用 Ringing 出口时零行为变化。
    fn emit_domain(&self, _event: qaqh_domain::DomainEvent) {}

    /// Emit a native Ringing V1 timeline producer intent. Timeline is its own wire;
    /// it must never be reconstructed from `Agent2Ui` output.
    fn emit_timeline(&self, _intent: qaqh_domain::TimelineIntent) {}

    /// 同步当前会话 seed（Ringing 事件信封的路由键）。
    /// 会话创建/恢复后必须调用，否则事件被 daemon 按 seed 过滤丢弃。
    /// 默认空实现：未持有 seed 的 emitter（测试 mock）无需处理。
    fn set_seed(&self, _seed: &str) {}

    /// 后台线程（标题生成等）直发 writer 通道的句柄；未持有（测试 mock）
    /// 返回 None——调用方降级为仅主线程事件。
    fn event_tx(&self) -> Option<std::sync::mpsc::SyncSender<WriterEvent>> {
        None
    }
}

/// writer 线程通道载荷：Ringing worker envelope 或 timeline intent（互不嵌套）。
#[derive(Debug, Clone)]
pub enum WriterEvent {
    /// Ringing worker envelope（`wire: "Ringing_domain_v1"`）。
    Ringing(qaqh_ringing::RingingWorkerEventEnvelope),
    /// Native ordered transcript intent (`wire: "Ringing_timeline_intent_v1"`).
    Timeline(qaqh_ringing::RingingTimelineIntentEnvelope),
}

/// 命令通道载荷：legacy 帧或原生 Ringing DomainCommand。
///
/// 两种协议在 worker 边界保持可判别且互不转换；Ringing command_id 作为
/// causation 进入原生领域执行路径。
#[derive(Debug, Clone)]
pub struct WorkerCommand {
    pub frame: qaqh_ringing::worker::RingingWorkerCommandEnvelope,
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
    /// Unified context-ingestion pipeline. Engines route every message
    /// entering the model context (user input, assistant output, tool
    /// results, skills envelopes, subagent reports, goal prompts) through
    /// [`ContextFlow::ingest`](qaqh_message::ContextFlow::ingest) — the
    /// single door to the message store.
    pub flow: &'a mut qaqh_message::ContextFlow,
}

// ═══════════════════════════════════════════════════════
// StatsCollector
// ═══════════════════════════════════════════════════════

/// Accumulates code delta records during a session.
/// Flushed to `code_stats.jsonl` on TurnComplete and session save.
pub struct StatsCollector {
    pub code_stats: Vec<qaqh_domain::CodeDeltaRecord>,
}

impl Default for StatsCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl StatsCollector {
    pub fn new() -> Self {
        Self {
            code_stats: Vec::new(),
        }
    }
    pub fn push_delta(&mut self, delta: qaqh_domain::CodeDeltaRecord) {
        self.code_stats.push(delta);
    }
    /// Persist accumulated deltas to disk. Clears the in-memory buffer.
    pub fn flush(&mut self, seed: &str) {
        if self.code_stats.is_empty() || seed.is_empty() {
            return;
        }
        let dir = qaqh_types::platform::sessions_dir().join(seed);
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
            let skills = self.agent.session.skills.clone();
            let seed = self.agent.session.seed.clone();
            self.agent
                .enqueue_meta_op(crate::agent::state::agent::MetaOp::PersistSkills {
                    seed,
                    skills,
                });
        }
        self.agent.msg.flush_meta(
            &self.agent.config.model,
            &self.agent.config.reasoning_effort,
        );
        self.stats.flush(&self.agent.session.seed);
    }
}

#[cfg(test)]
mod tests {
    use super::CancelToken;

    #[test]
    fn parent_cancel_propagates_to_descendants_and_arc() {
        let parent = CancelToken::new();
        let child = parent.child();
        let grandchild = child.child();
        let child_arc = child.arc();
        let grandchild_arc = grandchild.arc();

        parent.set();

        assert!(parent.is_set());
        assert!(child.is_set());
        assert!(grandchild.is_set());
        assert!(child_arc.load(std::sync::atomic::Ordering::SeqCst));
        assert!(grandchild_arc.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn child_cancel_does_not_affect_parent_or_sibling() {
        let parent = CancelToken::new();
        let child = parent.child();
        let sibling = parent.child();

        child.set();

        assert!(child.is_set());
        assert!(!parent.is_set());
        assert!(!sibling.is_set());
    }

    #[test]
    fn parent_cancel_latches_descendants_across_parent_clear() {
        let parent = CancelToken::new();
        let child = parent.child();
        let grandchild = child.child();

        parent.set();
        parent.clear();

        assert!(!parent.is_set());
        assert!(
            child.is_set(),
            "parent cancellation must remain latched on child work"
        );
        assert!(
            grandchild.is_set(),
            "parent cancellation must remain latched on descendant work"
        );
    }

    #[test]
    fn clear_recomputes_against_live_parent_without_erasing_local_cancel() {
        let parent = CancelToken::new();
        let child = parent.child();

        parent.set();
        child.clear();
        assert!(
            child.is_set(),
            "a child must not clear cancellation inherited from its parent"
        );

        child.set();
        parent.clear();
        assert!(
            child.is_set(),
            "a child's own cancellation must survive parent clear"
        );
        assert!(!parent.is_set());
    }
}
