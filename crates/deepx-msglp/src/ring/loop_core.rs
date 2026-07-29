//! Loop core — thin event dispatcher with panic recovery.
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────────────┐
//! │  Loop (process-level)                            │
//! │  ├─ I/O: cmd_rx, event_tx                        │
//! │  ├─ Signal: cancel, phase, pending, writer_dead  │
//! │  ├─ Session: session (SessionBundle)             │
//! │  │   ├─ agent: AgentState                        │
//! │  │   ├─ stats: StatsCollector                    │
//! │  │   ├─ turn: TurnEngine                         │
//! │  │   └─ tool: ToolEngine                         │
//! │  └─ Stateless engines: session_eng, input,       │
//! │     compact, misc, notify                        │
//! └──────────────────────────────────────────────────┘
//! ```
//!
//! `SessionBundle` is the unit of session isolation. Session-level engines
//! (TurnEngine, ToolEngine) and state (AgentState, StatsCollector) are
//! grouped together. On session switch, the entire bundle is flushed and
//! replaced. Process-level state (I/O channels, cancel token) is unaffected.
//!
//! # Panic recovery
//!
//! Every dispatch is wrapped in `safe_dispatch()`. If an engine panics:
//! 1. All engines are reset to clean state via `reset_all_engines()`
//! 2. Cancel token is cleared
//! 3. An `Agent2Ui::Error` is emitted to the frontend
//! 4. The Loop continues processing commands
//!
//! # Extensibility
//!
//! To add a new command handler:
//! 1. Implement `Engine` trait on your struct
//! 2. Add it to `try_handle_via_engines()` or the fallback match
//! 3. Add `reset()` support
//!
//! # Ring flow
//!
//! ```text
//! UserInput → InputEngine.handle() → Outcome::ContinueTurn
//!   → TurnEngine.run()
//!     → Gate SSE → parse → admit_batch → execute → ContinueTurn
//!     → (loop until YieldToUser or TurnComplete)
//!   → Outcome::TurnComplete → TurnEnd + Done → Idle
//! ```

use std::io::{BufRead, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use deepx_proto::{Agent2Ui, Ui2Agent};

use super::engine::Engine;
use super::engine_compact::{CompactEngine, CompactMeta};
use super::engine_goal::GoalEngine;
use super::engine_input::InputEngine;
use super::engine_misc::MiscEngine;
use super::engine_session::SessionEngine;
use super::engine_tool::PermissionDisposition;
use super::paced_emitter::PacedEmitter;
use super::types::*;
use crate::services::notification;
use crate::state::agent::AgentState;

/// Number of recent turns sent on session restore for incremental loading.
const INITIAL_LOAD_COUNT: usize = 20;

// ═══════════════════════════════════════════════════════
// Loop — the dispatcher
// ═══════════════════════════════════════════════════════

pub struct Loop {
    // ── Process-level I/O ──
    /// Incoming command channel (fed by reader thread).
    cmd_rx: mpsc::Receiver<Ui2Agent>,
    /// Outgoing event channel (consumed by writer thread).
    event_tx: mpsc::SyncSender<Agent2Ui>,

    // ── Process-level signals ──
    /// Cancellation token shared across engines.
    cancel: CancelToken,
    /// Current phase (Idle / GateRunning / ToolsRunning).
    phase: LoopPhase,
    /// Deferred interrupt commands received while busy.
    pending: PendingState,
    /// Set to true when the writer thread exits (stdout pipe broken).
    writer_dead: Arc<AtomicBool>,
    /// A running turn already emitted its terminal transaction, but the
    /// reader-thread interrupt frame still needs to be drained.
    terminal_for_queued_interrupt: bool,
    /// Whether a `Ready` event has already been emitted for the current
    /// idle period. Prevents the 1 Hz `Ready` storm that flooded the
    /// daemon's Critical lane (each Ready is EventLane::Critical and was
    /// sent every loop iteration, saturating priority queues and tripping
    /// the connection-death cascade).
    ready_emitted: bool,

    // ── Session-scoped state (flushed/swapped on session change) ──
    /// The active session's data and engines.
    /// In a multi-session future, this becomes `HashMap<seed, SessionBundle>`.
    session: SessionBundle,

    // ── Session-agnostic engines (process lifetime, no session state) ──
    /// Session lifecycle: create, resume, reload config.
    session_eng: SessionEngine,
    /// User input handler: compliance guard, auto-create session.
    input: InputEngine,
    /// Context compaction: summarize old conversation turns.
    compact: CompactEngine,
    /// Goal mode: autonomous plan execution with compact-aware scheduling.
    goal: GoalEngine,
    /// Miscellaneous: undo, dashboard, mode, notifications.
    misc: MiscEngine,
    /// Desktop notification channel.
    notify: NotifyHandle,

    /// Pending compact result (set when compact is running in background).
    pending_compact_rx: Option<mpsc::Receiver<CompactMeta>>,

    /// Direct output emitter. The renderer performs frame-level coalescing.
    paced_emitter: PacedEmitter,
}

impl Loop {
    /// Create a Loop backed by real stdin/stdout via background I/O threads.
    ///
    /// Spawns:
    /// - **Reader thread**: reads JSON-LP from `input`, sends `Ui2Agent` frames
    ///   to `cmd_rx`. Sets CancelToken on interrupt-type commands (Cancel,
    ///   ResumeSession, NewSession, Shutdown).
    /// - **Writer thread**: receives `Agent2Ui` from `event_tx`, writes
    ///   JSON-LP to `output`. Flushes every 2ms. Sets `writer_dead` on exit.
    ///
    /// Both threads use `catch_unwind` to log panics rather than silently dying.
    pub fn new_ipc(
        agent: AgentState,
        input: impl BufRead + Send + 'static,
        output: impl Write + Send + 'static,
    ) -> Self {
        let cancel = CancelToken::new();
        let cancel_for_reader = cancel.clone();

        let (cmd_tx, cmd_rx) = mpsc::sync_channel::<Ui2Agent>(4096);
        let (event_tx, event_rx) = mpsc::sync_channel::<Agent2Ui>(655360);
        let writer_dead = Arc::new(AtomicBool::new(false));
        let writer_dead_for_thread = writer_dead.clone();

        // ── Reader thread: stdin → cmd_tx ──
        // Processes JSON-LP frames in a loop. Interrupt-type commands
        // set the cancel token directly so that in-progress turns see
        // the cancellation immediately (before the main loop processes
        // the channel).
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut reader = std::io::BufReader::new(input);
                loop {
                    match deepx_proto::read_frame(&mut reader) {
                        Ok(Some(frame)) => {
                            let is_interrupt = matches!(
                                frame,
                                Ui2Agent::Cancel
                                    | Ui2Agent::ResumeSession { .. }
                                    | Ui2Agent::NewSession
                                    | Ui2Agent::Shutdown
                            );
                            if is_interrupt {
                                cancel_for_reader.set();
                                deepx_tools::CANCEL.store(true, Ordering::SeqCst);
                            }
                            if cmd_tx.send(frame).is_err() {
                                break;
                            }
                        }
                        Ok(None) | Err(_) => {
                            log::warn!("[AGENT] reader thread: stdin EOF — exiting");
                            break;
                        }
                    }
                }
            }));
            if let Err(e) = result {
                let msg = Self::panic_msg_from_err(e);
                log::error!("[AGENT] reader thread panicked: {}", msg);
                eprintln!("[DEEPX AGENT] reader thread panicked: {}", msg);
            }
            log::info!("[AGENT] reader thread exiting");
        });

        // ── Writer thread: event_rx → stdout ──
        // Batches events and flushes every 2ms. Uses BufWriter for
        // efficient I/O. Sets writer_dead on any write error so the
        // main loop can exit gracefully.
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // Zero-buffer writer: block on recv(), write + flush each
                // event immediately.  No timeout, no drain batches — every
                // Agent2Ui event reaches stdout as fast as the OS pipe can
                // deliver it. The downstream daemon worker reader picks
                // up each line without buffering delay.
                let mut writer = output;
                loop {
                    match event_rx.recv() {
                        Ok(event) => {
                            if let Ok(json) = serde_json::to_string(&event) {
                                if writeln!(writer, "{}", json).is_err() {
                                    break;
                                }
                                let _ = writer.flush();
                            }
                        }
                        Err(_) => break,
                    }
                }
            }));
            if let Err(e) = result {
                let msg = Self::panic_msg_from_err(e);
                log::error!("[AGENT] writer thread panicked: {}", msg);
                eprintln!("[DEEPX AGENT] writer thread panicked: {}", msg);
            }
            writer_dead_for_thread.store(true, Ordering::SeqCst);
            log::info!("[AGENT] writer thread exiting");
        });

        let paced_emitter = PacedEmitter::new(event_tx.clone(), writer_dead.clone(), cancel.arc());

        Loop {
            cmd_rx,
            event_tx,
            cancel,
            phase: LoopPhase::Idle,
            pending: PendingState::default(),
            writer_dead,
            terminal_for_queued_interrupt: false,
            ready_emitted: false,
            session: SessionBundle::new(agent),
            session_eng: SessionEngine::new(),
            input: InputEngine::new(),
            compact: CompactEngine::new(),
            goal: GoalEngine::new(),
            misc: MiscEngine::new(),
            notify: NotifyHandle {
                tx: notification::NotificationThread::spawn().into_sender(),
            },
            pending_compact_rx: None,
            paced_emitter,
        }
    }

    // ── Convenience accessors ──

    // ═══════════════════════════════════════════════════
    // Panic recovery
    // ═══════════════════════════════════════════════════

    /// Execute a closure with panic recovery.
    ///
    /// If `f` panics:
    /// 1. All engines are reset to clean idle state
    /// 2. Cancel token is cleared
    /// 3. Phase is reset to Idle
    /// 4. An `Agent2Ui::Error` is emitted to the frontend
    /// 5. An `Agent2Ui::Done` is emitted (so the frontend knows it can continue)
    ///
    /// The Loop continues processing commands after recovery.
    fn safe_dispatch<F>(&mut self, f: F)
    where
        F: FnOnce(&mut Self) + std::panic::UnwindSafe,
    {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            f(self);
        }));

        if let Err(e) = result {
            let msg = Self::panic_msg_from_err(e);
            log::error!("[AGENT] engine panic during dispatch: {msg}");
            eprintln!("[DEEPX AGENT] engine panic during dispatch: {msg}");

            self.reset_all_engines();
            self.phase = LoopPhase::Idle;
            self.cancel.clear();
            deepx_tools::CANCEL.store(false, Ordering::SeqCst);

            let _ = self.event_tx.send(Agent2Ui::Error {
                message: format!("Internal error (recovered): {msg}"),
            });
            let _ = self.event_tx.send(Agent2Ui::Done);
        }
    }

    /// Reset all engines to clean idle state.
    ///
    /// Called after a panic or on Cancel.
    /// Session-level engines are reset (turn, tool) to clear any
    /// suspended state or pending approvals. Stateless engines are
    /// no-ops. Stats accumulator is replaced with a fresh one.
    fn reset_all_engines(&mut self) {
        // Session-level engines (hold mutable state)
        self.session.turn.reset();
        self.session.tool.reset();
        self.session.stats = StatsCollector::new();

        // Session-agnostic engines (stateless, no-op)
        self.session_eng.reset();
        self.input.reset();
        self.compact.reset();
        self.goal = GoalEngine::new();
        self.misc.reset();
        self.pending_compact_rx = None;

        self.pending.clear();
    }

    /// Close any suspended transaction before replacing the active session.
    /// An unanswered ask/tool round must never be persisted into, or resumed
    /// against, the next session.
    fn prepare_session_switch(&mut self) {
        if self.session.turn.is_suspended() {
            self.session.agent.msg.remove_last_step_if_incomplete();
        }
        self.session.flush();
        self.reset_all_engines();
        self.cancel.clear();
        deepx_tools::CANCEL.store(false, Ordering::SeqCst);
    }

    /// Extract a human-readable message from a panic payload.
    fn panic_msg_from_err(e: Box<dyn std::any::Any + Send>) -> String {
        if let Some(s) = e.downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = e.downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic".into()
        }
    }

    // ═══════════════════════════════════════════════════
    // Interrupt polling (called by engines during long ops)
    // ═══════════════════════════════════════════════════

    /// Poll the command channel for interrupt-type commands.
    /// Returns true if the current operation should abort.
    ///
    /// Called by TurnEngine between gate rounds and by ToolEngine
    /// during progress draining. Most non-interrupt commands received during
    /// a busy phase are dropped; Compact is explicitly rejected because it
    /// cannot replace context inside an in-flight lap.
    pub fn poll_interrupts(&mut self) -> bool {
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            match cmd {
                Ui2Agent::Cancel => {
                    self.cancel.set();
                    deepx_tools::CANCEL.store(true, Ordering::SeqCst);
                    self.phase = LoopPhase::Idle;
                    let _ = self.event_tx.send(Agent2Ui::Cancelled);
                    return true;
                }
                Ui2Agent::ResumeSession { seed } => {
                    self.cancel.set();
                    deepx_tools::CANCEL.store(true, Ordering::SeqCst);
                    self.pending.session = Some(seed);
                    let _ = self.event_tx.send(Agent2Ui::Cancelled);
                    return true;
                }
                Ui2Agent::NewSession => {
                    self.cancel.set();
                    deepx_tools::CANCEL.store(true, Ordering::SeqCst);
                    self.pending.new_session = true;
                    let _ = self.event_tx.send(Agent2Ui::Cancelled);
                    return true;
                }
                Ui2Agent::Shutdown => {
                    self.pending.shutdown = true;
                    return true;
                }
                Ui2Agent::ReloadConfig => {
                    // Non-destructive — queue for processing when idle
                    self.pending.reload_config = true;
                }
                Ui2Agent::Compact => {
                    // Compaction may only replace context between model laps.
                    // Never silently consume a direct IPC request mid-SSE or
                    // during tool execution.
                    let _ = self.event_tx.send(Agent2Ui::Error {
                        message: "Context compaction requires an idle session.".into(),
                    });
                }
                _ => {} // Drop non-interrupt commands during busy phase
            }
        }
        false
    }

    // ═══════════════════════════════════════════════════
    // Main event loop
    // ═══════════════════════════════════════════════════

    /// Run the main event loop. Blocks until shutdown or pipe break.
    ///
    /// # Lifecycle
    ///
    /// 1. **Init**: auto-create or resume session from CLI seed
    /// 2. **Loop**: drain pending → block for command → dispatch → repeat
    /// 3. **Exit**: flush session, shutdown tools
    ///
    /// # Cancellation
    ///
    /// The reader thread sets `cancel` on interrupt-type commands BEFORE
    /// they reach the channel. This means long-running operations (Gate
    /// SSE, tool execution) see the cancellation immediately via
    /// `cancel.is_set()` polling.
    pub fn run(&mut self) {
        self.session.agent.rebind_store();

        // ── Init: handle pre-set seed from CLI ──
        self.init_session();

        log::info!("[AGENT] entering main event loop");
        loop {
            // ── Process queued interrupts ──
            self.drain_pending();

            if self.pending.shutdown {
                // ── 跨会话记忆归档 ──
                #[cfg(feature = "memory")]
                if let Some(ref vec) = self.session.agent.vector {
                    // 从引擎读取 memory_dir，与 try_init_vector 保持路径一致
                    let memory_dir = vec.lock().ok().map(|g| g.memory_dir().to_path_buf());
                    if let Some(memory_dir) = memory_dir {
                        let messages: Vec<deepx_types::Message> = self
                            .session
                            .agent
                            .msg
                            .turns()
                            .iter()
                            .flat_map(|t| {
                                let mut msgs = vec![t.user.clone()];
                                for step in &t.steps {
                                    msgs.push(step.assistant.clone());
                                    msgs.extend(step.tool_results.clone());
                                }
                                msgs
                            })
                            .collect();
                        match deepx_session::memory_hook::archive_session_memories(
                            &self.session.agent.session.seed,
                            &messages,
                            &memory_dir,
                        ) {
                            Ok(n) => log::info!(
                                "Memory archived: {n} entries from session '{}'",
                                self.session.agent.session.seed
                            ),
                            Err(e) => log::warn!("Memory archive failed: {e}"),
                        }
                    }
                }
                break;
            }

            if self.writer_dead.load(Ordering::SeqCst) {
                log::error!("[AGENT] writer thread died — exiting");
                eprintln!("[DEEPX AGENT] writer thread died — stdout pipe broken. Exiting.");
                break;
            }

            // ── Check background compact completion ──
            self.check_pending_compact();

            // Signal readiness at most once per truly idle period. A manual
            // compact runs in a background worker, but it still owns the
            // active context transaction until CompactEnd is applied.
            if self.pending_compact_rx.is_none() && !self.ready_emitted {
                let _ = self.event_tx.send(Agent2Ui::Ready);
                self.ready_emitted = true;
            }

            // ── Block for next command (with timeout to poll compact) ──
            let frame = match self.cmd_rx.recv_timeout(std::time::Duration::from_secs(1)) {
                Ok(f) => {
                    log::info!(
                        "[AGENT] received Ui2Agent frame: {:?}",
                        std::mem::discriminant(&f)
                    );
                    f
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    log::error!("[AGENT] cmd_rx closed — stdin pipe broken. Exiting.");
                    eprintln!("[DEEPX AGENT] stdin pipe broken — exiting.");
                    break;
                }
            };

            // ── Dispatch with panic safety ──
            self.safe_dispatch(|this| {
                this.dispatch_one(frame);
            });
        }

        // ── Cleanup ──
        deepx_tools::runtime::shutdown_tools();
        self.session.flush();
    }

    /// Initialize session state from pre-set seed (CLI args --seed / --resume-seed).
    fn init_session(&mut self) {
        let resume_seed = self.session.agent.session.resume_seed.take();
        let has_seed = !self.session.agent.session.seed.is_empty();

        if let Some(seed) = resume_seed {
            if self
                .session_eng
                .resume(&mut self.session.agent, &seed, &self.cancel)
            {
                let total = self.session.agent.msg.turn_count() as u32;
                let start = total.saturating_sub(INITIAL_LOAD_COUNT as u32) as usize;
                let recent = crate::util::build_turns_from_context(
                    &self.session.agent,
                    Some(start),
                    Some(INITIAL_LOAD_COUNT),
                );
                let _ = self.event_tx.send(Agent2Ui::SessionRestored {
                    seed: self.session.agent.session.seed.clone(),
                    turns: recent,
                    tokens_used: self.session.agent.session.usage_totals.total_tokens,
                    cache_hit_pct: crate::util::cache_hit_pct(
                        &self.session.agent.session.usage_totals,
                    ),
                    usage: self.session.agent.session.last_usage.clone(),
                    usage_totals: self.session.agent.session.usage_totals.clone(),
                    usage_requests: self.session.agent.session.usage_requests,
                    cache_reported_requests: self.session.agent.session.effective_cache_reported_requests(),
                    total_turns: total,
                    has_more: start > 0,
                });
            }
            self.misc
                .emit_dashboard(&self.session.agent, &self.event_tx);
            let _ = self.event_tx.send(Agent2Ui::Ready);
        } else if has_seed && !self.session.agent.session.from_resume {
            self.session_eng
                .create_with_seed(&mut self.session.agent, &self.cancel);
            let _ = self.event_tx.send(Agent2Ui::SessionCreated {
                seed: self.session.agent.session.seed.clone(),
            });
            self.misc
                .emit_dashboard(&self.session.agent, &self.event_tx);
            let _ = self.event_tx.send(Agent2Ui::Ready);
        } else {
            self.misc
                .emit_dashboard(&self.session.agent, &self.event_tx);
            let _ = self.event_tx.send(Agent2Ui::Ready);
        }
    }

    // ═══════════════════════════════════════════════════
    // Pending queue drain
    // ═══════════════════════════════════════════════════

    /// Process all queued commands from the channel.
    ///
    /// Interrupt-type commands (Cancel, ResumeSession, NewSession, Shutdown)
    /// set the cancel token and queue a pending action. Non-interrupt commands
    /// received while a session switch is pending are dropped — the frontend
    /// re-sends UserInput after receiving Ready.
    fn drain_pending(&mut self) {
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            match cmd {
                Ui2Agent::Cancel => {
                    if std::mem::take(&mut self.terminal_for_queued_interrupt) {
                        self.cancel.clear();
                        deepx_tools::CANCEL.store(false, Ordering::SeqCst);
                        continue;
                    }
                    self.cancel.set();
                    deepx_tools::CANCEL.store(true, Ordering::SeqCst);
                    self.phase = LoopPhase::Idle;
                    let _ = self.event_tx.send(Agent2Ui::Cancelled);
                }
                Ui2Agent::ResumeSession { seed } => {
                    let terminal_emitted = std::mem::take(&mut self.terminal_for_queued_interrupt);
                    self.cancel.set();
                    deepx_tools::CANCEL.store(true, Ordering::SeqCst);
                    self.pending.session = Some(seed);
                    if !terminal_emitted {
                        let _ = self.event_tx.send(Agent2Ui::Cancelled);
                    }
                }
                Ui2Agent::NewSession => {
                    let terminal_emitted = std::mem::take(&mut self.terminal_for_queued_interrupt);
                    self.cancel.set();
                    deepx_tools::CANCEL.store(true, Ordering::SeqCst);
                    self.pending.new_session = true;
                    if !terminal_emitted {
                        let _ = self.event_tx.send(Agent2Ui::Cancelled);
                    }
                }
                Ui2Agent::Shutdown => {
                    self.terminal_for_queued_interrupt = false;
                    self.pending.shutdown = true;
                }
                // A suspended turn may have several permission responses queued.
                // Route them through the reason-aware dispatch guard instead of
                // dropping every response after the first one.
                other if self.pending.is_empty() => {
                    self.dispatch_one(other);
                }
                _ => {
                    log::info!("[AGENT] dropping command during pending session switch");
                }
            }
        }

        // ── Process deferred session switch ──
        if let Some(seed) = self.pending.session.take() {
            self.prepare_session_switch();
            if self
                .session_eng
                .resume(&mut self.session.agent, &seed, &self.cancel)
            {
                let total = self.session.agent.msg.turn_count() as u32;
                let start = total.saturating_sub(INITIAL_LOAD_COUNT as u32) as usize;
                let recent = crate::util::build_turns_from_context(
                    &self.session.agent,
                    Some(start),
                    Some(INITIAL_LOAD_COUNT),
                );
                let _ = self.event_tx.send(Agent2Ui::SessionRestored {
                    seed: self.session.agent.session.seed.clone(),
                    turns: recent,
                    tokens_used: self.session.agent.session.usage_totals.total_tokens,
                    cache_hit_pct: crate::util::cache_hit_pct(
                        &self.session.agent.session.usage_totals,
                    ),
                    usage: self.session.agent.session.last_usage.clone(),
                    usage_totals: self.session.agent.session.usage_totals.clone(),
                    usage_requests: self.session.agent.session.usage_requests,
                    cache_reported_requests: self.session.agent.session.effective_cache_reported_requests(),
                    total_turns: total,
                    has_more: start > 0,
                });
            }
            let _ = self.event_tx.send(Agent2Ui::Ready);
        }
        if self.pending.new_session {
            self.pending.new_session = false;
            self.prepare_session_switch();
            self.session_eng
                .create(&mut self.session.agent, &self.cancel);
            let _ = self.event_tx.send(Agent2Ui::SessionCreated {
                seed: self.session.agent.session.seed.clone(),
            });
            self.misc
                .emit_dashboard(&self.session.agent, &self.event_tx);
            let _ = self.event_tx.send(Agent2Ui::Ready);
        }
        if self.pending.reload_config {
            self.pending.reload_config = false;
            self.session_eng
                .reload_config(&mut self.session.agent, &self.cancel);
        }
    }

    /// Check if a background compact has completed and apply the result.
    fn check_pending_compact(&mut self) {
        if let Some(ref rx) = self.pending_compact_rx {
            match rx.try_recv() {
                Ok(meta) => {
                    self.pending_compact_rx = None;
                    let mut ctx = RingContext {
                        agent: &mut self.session.agent,
                        emitter: &self.paced_emitter,
                        cancel: &self.cancel,
                        phase: &mut self.phase,
                        pending: &mut self.pending,
                        writer_dead: &self.writer_dead,
                        stats: &mut self.session.stats,
                        notify: &self.notify,
                    };
                    self.compact.apply_result(&mut ctx, &meta);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Worker thread died without sending result.
                    // Clear pending state and report error so frontend
                    // doesn't stay stuck at the "compacting" animation.
                    log::error!("[COMPACT] worker thread disconnected without result");
                    self.pending_compact_rx = None;
                    let _ = self.event_tx.send(Agent2Ui::Error {
                        message: "Context compaction failed: worker thread crashed.".into(),
                    });
                    let _ = self.event_tx.send(Agent2Ui::CompactEnd {
                        summary_chars: 0,
                        turns_compacted: 0,
                        turns_removed: 0,
                    });
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // Still running — check again next loop iteration.
                }
            }
        }
    }

    /// Emit Agent2Ui::SkillsChanged with current available/active skills.
    fn emit_skills_status(&mut self) {
        let workspace = deepx_tools::CURRENT_WORKSPACE
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let status = self.session.agent.build_skills_status(&workspace);
        let _ = self.event_tx.send(Agent2Ui::SkillsChanged { status });
    }

    // ═══════════════════════════════════════════════════
    // Single-command dispatch
    // ═══════════════════════════════════════════════════

    /// Route a single Ui2Agent frame to the appropriate engine.
    ///
    /// # Dispatch order
    ///
    /// 1. **Guard**: if turn is suspended, only accept commands matching the
    ///    suspension reason (PermissionResponse for PermissionPending,
    ///    AskResponse/AskDismiss for AskUser, plus Cancel/session-switch/Shutdown)
    /// 2. **Engine chain**: try each engine's handler via explicit match
    /// 3. **Fallback**: commands needing direct event_tx access (Undo, SetMode,
    ///    LoadMoreTurns, Cancel, Shutdown)
    fn dispatch_one(&mut self, frame: Ui2Agent) {
        log::info!(
            "[AGENT] dispatch_one: frame={:?}",
            std::mem::discriminant(&frame)
        );
        // Any inbound command ends the idle period; the next time the loop
        // returns to idle it will re-emit Ready exactly once.
        self.ready_emitted = false;
        // A manual compact summarizes a frozen snapshot and later replaces
        // the active MessageStore. Accepting any other command while its
        // worker is running could append or mutate messages that apply_result
        // would then discard. Shutdown is safe because the process exits and
        // the compact result is never applied.
        if self.pending_compact_rx.is_some() && !matches!(&frame, Ui2Agent::Shutdown) {
            let _ = self.event_tx.send(Agent2Ui::Error {
                message: "Context compaction is running; wait for CompactEnd.".into(),
            });
            return;
        }
        // ── Guard: suspended turn — reason-aware command filtering ──
        if let Some(reason) = self.session.turn.suspended_reason() {
            match (&frame, reason) {
                // Permission pending → only accept PermissionResponse
                (Ui2Agent::PermissionResponse { .. }, YieldReason::PermissionPending) => {}
                // AskUser pending → accept only typed ask lifecycle commands.
                (Ui2Agent::AskResponse { .. }, YieldReason::AskUser) => {}
                (Ui2Agent::AskDismiss { .. }, YieldReason::AskUser) => {}
                // PlanReview pending → accept only plan review decisions.
                (Ui2Agent::PlanReview { .. }, YieldReason::PlanReview) => {}
                // Always accepted regardless of suspension reason
                (Ui2Agent::Cancel, _)
                | (Ui2Agent::ResumeSession { .. }, _)
                | (Ui2Agent::NewSession, _)
                | (Ui2Agent::UndoTurn { .. }, _)
                | (Ui2Agent::Shutdown, _) => {}
                _ => {
                    log::warn!("[AGENT] dropping command during suspended turn");
                    let _ = self.event_tx.send(Agent2Ui::Error {
                        message:
                            "Turn is suspended — resolve pending permissions or ask_user first."
                                .into(),
                    });
                    return;
                }
            }
        }

        // ── Phase 1: Engine-managed commands ──
        if let Some(outcome) = self.try_handle_via_engines(&frame) {
            self.apply_outcome(outcome);
            return;
        }

        // ── Phase 2: Fallback — commands needing direct event_tx ──
        match frame {
            Ui2Agent::Cancel => {
                self.cancel.set();
                deepx_tools::CANCEL.store(true, Ordering::SeqCst);
                let suspended = self.session.turn.take_suspended_for_abort();
                if suspended.is_some() {
                    self.session.agent.msg.remove_last_step_if_incomplete();
                }
                // Cancel is a cross-engine reset: clear ALL mutable state
                self.reset_all_engines();
                self.phase = LoopPhase::Idle;
                let _ = self.event_tx.send(Agent2Ui::Cancelled);
                if let Some((turn_id, usage)) = suspended {
                    self.session.flush();
                    let _ = self.event_tx.send(Agent2Ui::TurnEnd {
                        turn_id,
                        stop_reason: Some("cancelled".into()),
                        usage,
                    });
                    let _ = self.event_tx.send(Agent2Ui::Done);
                }
            }
            Ui2Agent::Shutdown => {
                let _ = self.event_tx.send(Agent2Ui::ShutdownAck);
                self.pending.shutdown = true;
            }
            Ui2Agent::UndoTurn { turn_id } => {
                if self
                    .session
                    .turn
                    .suspended_turn_id()
                    .is_some_and(|active_turn_id| active_turn_id != turn_id)
                {
                    let _ = self.event_tx.send(Agent2Ui::Error {
                        message: format!(
                            "Cannot undo {turn_id}: a different active turn is suspended"
                        ),
                    });
                    return;
                }
                // ── Cross-engine undo transaction ──
                // Undo is NOT just a message-store operation. It must also
                // reset TurnEngine and ToolEngine because they may hold
                // references to the deleted turn (suspended state, pending
                // approvals keyed by tool_call_id that no longer exists).
                self.session.turn.reset();
                self.session.tool.reset();
                self.misc
                    .handle_undo(&mut self.session.agent, &turn_id, &self.event_tx);
            }
            Ui2Agent::SetMode { mode } => {
                self.misc.set_mode(&mut self.session.agent, &mode);
            }
            Ui2Agent::LoadMoreTurns {
                before_turn_id,
                count,
            } => {
                let total = self.session.agent.msg.turn_count();
                let idx: usize = before_turn_id
                    .strip_prefix('t')
                    .and_then(|n| n.parse::<usize>().ok())
                    .map(|n| n.saturating_sub(1))
                    .unwrap_or(total);
                let end = idx.min(total);
                let start = end.saturating_sub(count as usize);
                let batch = crate::util::build_turns_from_context(
                    &self.session.agent,
                    Some(start),
                    Some(count as usize),
                );
                let _ = self.event_tx.send(Agent2Ui::MoreTurns {
                    turns: batch,
                    has_more: start > 0,
                });
            }
            // Already handled by engine chain — unreachable here
            Ui2Agent::UserInput { .. }
            | Ui2Agent::AskResponse { .. }
            | Ui2Agent::AskDismiss { .. }
            | Ui2Agent::PlanReview { .. }
            | Ui2Agent::CreateSession
            | Ui2Agent::ResumeSession { .. }
            | Ui2Agent::NewSession
            | Ui2Agent::ReloadConfig
            | Ui2Agent::ReloadSkills
            | Ui2Agent::UnloadSkill { .. }
            | Ui2Agent::ActivateSkill { .. }
            | Ui2Agent::ToolCall { .. }
            | Ui2Agent::PermissionResponse { .. }
            | Ui2Agent::Compact => {}
            _ => {}
        }
    }

    /// Route a command through the engine chain.
    ///
    /// Each engine gets a chance to handle the command. The first engine
    /// that returns `Some(outcome)` wins. Uses explicit match arms rather
    /// than dynamic dispatch through `engines_iter_mut()` to avoid borrow
    /// conflicts between the iterator and `self.ctx()`.
    fn try_handle_via_engines(&mut self, frame: &Ui2Agent) -> Option<Outcome> {
        // ── SessionEngine (doesn't need RingContext) ──
        match frame {
            Ui2Agent::CreateSession => {
                self.session_eng
                    .create(&mut self.session.agent, &self.cancel);
                let _ = self.event_tx.send(Agent2Ui::SessionCreated {
                    seed: self.session.agent.session.seed.clone(),
                });
                self.misc
                    .emit_dashboard(&self.session.agent, &self.event_tx);
                return Some(Outcome::Handled);
            }
            Ui2Agent::ResumeSession { seed } => {
                self.prepare_session_switch();
                if self
                    .session_eng
                    .resume(&mut self.session.agent, seed, &self.cancel)
                {
                    let total = self.session.agent.msg.turn_count() as u32;
                    let start = total.saturating_sub(INITIAL_LOAD_COUNT as u32) as usize;
                    let recent = crate::util::build_turns_from_context(
                        &self.session.agent,
                        Some(start),
                        Some(INITIAL_LOAD_COUNT),
                    );
                    let _ = self.event_tx.send(Agent2Ui::SessionRestored {
                        seed: self.session.agent.session.seed.clone(),
                        turns: recent,
                        tokens_used: self.session.agent.session.usage_totals.total_tokens,
                        cache_hit_pct: crate::util::cache_hit_pct(
                            &self.session.agent.session.usage_totals,
                        ),
                        usage: self.session.agent.session.last_usage.clone(),
                        usage_totals: self.session.agent.session.usage_totals.clone(),
                        usage_requests: self.session.agent.session.usage_requests,
                        cache_reported_requests: self.session.agent.session.effective_cache_reported_requests(),
                        total_turns: total,
                        has_more: start > 0,
                    });
                } else {
                    let _ = self.event_tx.send(Agent2Ui::Error {
                        message: format!("Failed to resume session: {seed}"),
                    });
                }
                self.misc
                    .emit_dashboard(&self.session.agent, &self.event_tx);
                return Some(Outcome::Handled);
            }
            Ui2Agent::NewSession => {
                self.prepare_session_switch();
                self.session_eng
                    .create(&mut self.session.agent, &self.cancel);
                let _ = self.event_tx.send(Agent2Ui::SessionCreated {
                    seed: self.session.agent.session.seed.clone(),
                });
                self.misc
                    .emit_dashboard(&self.session.agent, &self.event_tx);
                return Some(Outcome::Handled);
            }
            Ui2Agent::ReloadConfig => {
                self.session_eng
                    .reload_config(&mut self.session.agent, &self.cancel);
                return Some(Outcome::Handled);
            }
            Ui2Agent::ReloadSkills => {
                let workspace = deepx_tools::CURRENT_WORKSPACE
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                self.session.agent.inject_catalog(&workspace);
                self.emit_skills_status();
                return Some(Outcome::Handled);
            }
            Ui2Agent::UnloadSkill { name } => {
                self.session.agent.deactivate_explicit_skill(name);
                self.emit_skills_status();
                return Some(Outcome::Handled);
            }
            Ui2Agent::ActivateSkill { name } => {
                let _ = self.session.agent.skills.queue_request(name, "user");
                self.emit_skills_status();
                return Some(Outcome::Handled);
            }
            Ui2Agent::SkillOperation {
                operation_id,
                action,
                name,
                expected_revision,
            } => {
                let (success, revision, error) = self.session.agent.skills.apply_ui_operation(
                    operation_id,
                    *expected_revision,
                    action,
                    name,
                );
                let _ = self.event_tx.send(Agent2Ui::SkillOperationResolved {
                    operation_id: operation_id.clone(),
                    success,
                    revision,
                    error,
                });
                self.emit_skills_status();
                return Some(Outcome::Handled);
            }
            _ => {}
        }

        // ── Engines that need RingContext ──
        let mut ctx = RingContext {
            agent: &mut self.session.agent,
            emitter: &self.paced_emitter,
            cancel: &self.cancel,
            phase: &mut self.phase,
            pending: &mut self.pending,
            writer_dead: &self.writer_dead,
            stats: &mut self.session.stats,
            notify: &self.notify,
        };

        match frame {
            Ui2Agent::UserInput { text } => Some(self.input.handle_user_input(&mut ctx, text)),
            Ui2Agent::AskResponse { ask_id, answers } => {
                Some(self.session.turn.handle_ask_response(
                    &mut ctx,
                    &mut self.session.tool,
                    ask_id,
                    answers,
                ))
            }
            Ui2Agent::AskDismiss { ask_id } => Some(self.session.turn.handle_ask_dismiss(
                &mut ctx,
                &mut self.session.tool,
                ask_id,
            )),
            Ui2Agent::PlanReview {
                call_id,
                approved,
                message,
                autonomous,
            } => Some(self.session.turn.handle_plan_response(
                &mut ctx,
                &mut self.session.tool,
                &call_id,
                *approved,
                &message,
                *autonomous,
            )),
            Ui2Agent::ToolCall {
                id,
                name,
                action,
                args,
            } => {
                self.session
                    .tool
                    .handle_ui_tool_call(&mut ctx, id, name, action, args);
                Some(Outcome::Handled)
            }
            Ui2Agent::PermissionResponse {
                tool_call_id,
                approved,
                trust_folder,
            } => {
                match self.session.tool.handle_permission_response(
                    &mut ctx,
                    tool_call_id,
                    *approved,
                    *trust_folder,
                ) {
                    PermissionDisposition::Ignored | PermissionDisposition::UiHandled => {
                        Some(Outcome::Handled)
                    }
                    PermissionDisposition::LlmResolved { call_id, admitted } => {
                        Some(self.session.turn.handle_permission_resolved(
                            &mut ctx,
                            &mut self.session.tool,
                            &call_id,
                            admitted,
                        ))
                    }
                }
            }
            Ui2Agent::Compact => {
                if self.pending_compact_rx.is_some() {
                    return Some(Outcome::Error(
                        "Context compaction is already running.".into(),
                    ));
                }
                if let Some((prompt, kept, head, provider)) =
                    self.compact.build_prompt_and_meta(&mut ctx)
                {
                    // Step 2: spawn LLM call in background (catch_unwind so
                    // a panic still sends a result via the channel — otherwise
                    // the receiver disconnects and the frontend gets stuck).
                    let (tx, rx) = mpsc::channel();
                    let event_tx = self.event_tx.clone();
                    std::thread::Builder::new()
                        .name("compact-worker".into())
                        .spawn(move || {
                            let result =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    super::engine_compact::run_compact_worker(
                                        prompt, provider, kept, head, event_tx,
                                    )
                                }));
                            let meta = match result {
                                Ok(meta) => meta,
                                Err(e) => {
                                    let msg = Self::panic_msg_from_err(e);
                                    CompactMeta {
                                        summary: String::new(),
                                        kept_user_count: kept,
                                        head_user_count: head,
                                        error: Some(format!("Compact worker panicked: {msg}")),
                                    }
                                }
                            };
                            let _ = tx.send(meta);
                        })
                        .ok();
                    self.pending_compact_rx = Some(rx);
                } else {
                    // Runtime reserves the session as Working before sending
                    // a manual Compact command. Complete the activity even
                    // when there is nothing eligible to compact.
                    ctx.emitter.emit(Agent2Ui::CompactEnd {
                        summary_chars: 0,
                        turns_compacted: 0,
                        turns_removed: 0,
                    });
                }
                Some(Outcome::Handled)
            }
            _ => None,
        }
    }

    // ═══════════════════════════════════════════════════
    // Outcome handler — the Ring's decision point
    // ═══════════════════════════════════════════════════

    /// Apply the outcome returned by an engine.
    ///
    /// This is the central decision point of the Ring architecture.
    /// Each Outcome variant maps to a specific Loop action:
    ///
    /// - `TurnComplete` → flush, emit TurnEnd + Done, notify, return to Idle
    /// - `ContinueTurn` → re-enter TurnEngine for another gate lap (recursive)
    /// - `YieldToUser` → do nothing, wait for PermissionResponse or UserInput
    /// - `Handled` / `Error` / `Shutdown` → straightforward
    fn apply_outcome(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::TurnComplete { turn_id, usage } => {
                self.session.agent.skills.complete_user_turn();
                // Persist session state
                self.session.flush();
                if let Some(ref u) = usage {
                    crate::util::record_token_usage(u, &self.session.agent.config.model);
                }
                let _ = self.event_tx.send(Agent2Ui::TurnEnd {
                    turn_id,
                    stop_reason: None,
                    usage,
                });

                // Desktop notification: preview of assistant response
                self.misc.maybe_notify(&self.session.agent, &self.notify.tx);

                // Goal mode auto-advance: if the LLM completed a step
                // (via todo_step_complete tool), inject the next step.
                if let Ok(store) = deepx_tools::todo::load_todo() {
                    if store.mode == deepx_tools::todo::TodoMode::Goal {
                        if let Some(ref current_id) = store.current_id {
                            if let Some(item) = store.items.iter().find(|i| &i.id == current_id) {
                                if item.status == deepx_tools::todo::TodoStatus::InProgress {
                                    let prompt = format!(
                                        "[自动执行计划 / 目标模式]\n\n\
                                         T{}: {}\n{}\n\n\
                                         完成此步骤后，调用 todo_step_complete(id=\"{}\", summary=\"...\").",
                                        item.id, item.title, item.description, item.id
                                    );
                                    let mut ctx = RingContext {
                                        agent: &mut self.session.agent,
                                        emitter: &self.paced_emitter,
                                        cancel: &self.cancel,
                                        phase: &mut self.phase,
                                        pending: &mut self.pending,
                                        writer_dead: &self.writer_dead,
                                        stats: &mut self.session.stats,
                                        notify: &self.notify,
                                    };
                                    let next_outcome =
                                        self.input.handle_user_input(&mut ctx, &prompt);
                                    drop(ctx);
                                    self.apply_outcome(next_outcome);
                                    return;
                                }
                            }
                        }
                    }
                }

                let _ = self.event_tx.send(Agent2Ui::Done);
                self.phase = LoopPhase::Idle;
            }
            Outcome::TurnAborted {
                turn_id,
                usage,
                consume_queued_interrupt,
            } => {
                self.session.agent.skills.abort_user_turn();
                self.session.flush();
                self.reset_all_engines();
                self.terminal_for_queued_interrupt = consume_queued_interrupt;
                let _ = self.event_tx.send(Agent2Ui::Cancelled);
                let _ = self.event_tx.send(Agent2Ui::TurnEnd {
                    turn_id,
                    stop_reason: Some("cancelled".into()),
                    usage,
                });
                let _ = self.event_tx.send(Agent2Ui::Done);
                self.phase = LoopPhase::Idle;
            }
            Outcome::TurnFailed {
                turn_id,
                usage,
                message,
            } => {
                self.session.agent.skills.abort_user_turn();
                self.session.flush();
                let _ = self.event_tx.send(Agent2Ui::Error { message });
                let _ = self.event_tx.send(Agent2Ui::TurnEnd {
                    turn_id,
                    stop_reason: Some("error".into()),
                    usage,
                });
                let _ = self.event_tx.send(Agent2Ui::Done);
                self.phase = LoopPhase::Idle;
            }
            Outcome::ContinueTurn {
                turn_id,
                round_num,
                usage,
            } => {
                // Another lap: re-enter TurnEngine.
                let mut ctx = RingContext {
                    agent: &mut self.session.agent,
                    emitter: &self.paced_emitter,
                    cancel: &self.cancel,
                    phase: &mut self.phase,
                    pending: &mut self.pending,
                    writer_dead: &self.writer_dead,
                    stats: &mut self.session.stats,
                    notify: &self.notify,
                };
                let next_outcome = self.session.turn.run(
                    &mut ctx,
                    &mut self.session.tool,
                    turn_id,
                    round_num,
                    usage,
                );
                drop(ctx);

                // Poll compact result after each turn lap — the background
                // compact thread may have completed while we were blocked
                // on SSE streaming. Without this, CompactEnd is delayed
                // until the entire turn finishes.
                self.check_pending_compact();

                self.apply_outcome(next_outcome);
            }
            Outcome::YieldToUser { .. } => {
                // Turn suspended. Loop returns to Idle. The next
                // PermissionResponse or a typed ask command will trigger resume.
            }
            Outcome::Handled => {}
            Outcome::Error(msg) => {
                let _ = self.event_tx.send(Agent2Ui::Error { message: msg });
                self.phase = LoopPhase::Idle;
            }
            Outcome::Shutdown => {
                self.pending.shutdown = true;
            }
        }
    }
}
