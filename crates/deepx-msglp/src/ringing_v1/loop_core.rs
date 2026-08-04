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

use std::collections::VecDeque;
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

fn ringing_command_is_interrupt(env: &deepx_ringing::RingingWorkerCommandEnvelope) -> bool {
    matches!(
        &env.command,
        deepx_ringing::RingingCommand::Control(
            deepx_domain::ControlCommand::SessionResume { .. }
                | deepx_domain::ControlCommand::SessionShutdown
                | deepx_domain::ControlCommand::SessionCreate { .. }
        ) | deepx_ringing::RingingCommand::Conversation(
            deepx_domain::ConversationCommand::ConversationCancel { .. }
        )
    )
}

// ═══════════════════════════════════════════════════════
// Loop — the dispatcher
// ═══════════════════════════════════════════════════════

pub struct Loop {
    // ── Process-level I/O ──
    /// Incoming command channel (fed by reader thread).
    cmd_rx: mpsc::Receiver<super::types::WorkerCommand>,
    /// Outgoing event channel (consumed by writer thread).
    event_tx: mpsc::SyncSender<super::types::WriterEvent>,

    // ── Process-level signals ──
    /// Cancellation token shared across engines.
    cancel: CancelToken,
    /// Current phase (Idle / GateRunning / ToolsRunning).
    phase: LoopPhase,
    /// Deferred interrupt commands received while busy.
    pending: PendingState,
    /// Ringing commands already acknowledged by the daemon while a legacy
    /// session switch is pending. An accepted command must execute exactly
    /// once after the switch; it must never be silently discarded.
    deferred_ringing: VecDeque<super::types::WorkerCommand>,
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
    pending_compact_causation: Option<String>,

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
        // resume 模式下 `--resume-seed` 只写入 resume_seed 字段，seed 此时
        // 仍为空；用 resume_seed 兜底，避免 PacedEmitter 以空 seed 构造
        // （Ringing 事件信封会被 daemon 按 seed 过滤丢弃）。init_session
        // 完成后还会经 sync_emitter_seed 再次同步权威值。
        let seed = if !agent.session.seed.is_empty() {
            agent.session.seed.clone()
        } else {
            agent.session.resume_seed.clone().unwrap_or_default()
        };
        let cancel = CancelToken::new();
        let cancel_for_reader = cancel.clone();

        let (cmd_tx, cmd_rx) = mpsc::sync_channel::<super::types::WorkerCommand>(4096);
        // std 的 sync_channel 在构造时预分配 (capacity + 1) 个 slot 的环形缓冲，
        // 每个 slot 是 size_of::<WriterEvent>() = 512 字节（枚举按最大变体对齐）。
        // 旧值 655360 × 512B ≈ 320MB —— 每个 worker 进程启动即常驻，这正是
        // 单 session 内存 300MB+ 的根因。writer 线程逐事件即时写 stdout，突发
        // 事件由 PacedEmitter 以 ≤50ms 节流合并，16384 个 slot（8MB）在保留
        // 背压语义的同时把固定开销降到合理范围。
        let (event_tx, event_rx) = mpsc::sync_channel::<super::types::WriterEvent>(16384);
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
                    match super::wire::read_worker_command_frame(&mut reader) {
                        Ok(Some(super::wire::WorkerCommandFrame::Legacy(frame))) => {
                            let is_interrupt = matches!(
                                frame,
                                Ui2Agent::Cancel
                                    | Ui2Agent::ResumeSession { .. }
                                    | Ui2Agent::NewSession
                                    | Ui2Agent::Shutdown
                            );
                            if is_interrupt {
                                cancel_for_reader.set();
                                deepx_workspace::CANCEL.store(true, Ordering::SeqCst);
                            }
                            if cmd_tx
                                .send(super::types::WorkerCommand {
                                    frame: super::wire::WorkerCommandFrame::Legacy(frame),
                                    causation: None,
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok(Some(super::wire::WorkerCommandFrame::Ringing(env))) => {
                            let causation = env.command_id.clone();
                            if ringing_command_is_interrupt(&env) {
                                cancel_for_reader.set();
                                deepx_workspace::CANCEL.store(true, Ordering::SeqCst);
                            }
                            if cmd_tx
                                .send(super::types::WorkerCommand {
                                    frame: super::wire::WorkerCommandFrame::Ringing(env),
                                    causation: Some(causation),
                                })
                                .is_err()
                            {
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
        // main loop can exit gracefully. 支持 legacy 与 Ringing 双格式，
        // 但同一帧只承载一种协议（不嵌套）。
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
                        Ok(super::types::WriterEvent::Legacy(event)) => {
                            if super::wire::write_legacy_event_frame(&mut writer, &event).is_err() {
                                break;
                            }
                        }
                        Ok(super::types::WriterEvent::Ringing(env)) => {
                            if super::wire::write_ringing_event_frame(&mut writer, &env).is_err() {
                                break;
                            }
                        }
                        Ok(super::types::WriterEvent::Timeline(env)) => {
                            if super::wire::write_timeline_intent_frame(&mut writer, &env).is_err()
                            {
                                break;
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

        let paced_emitter =
            PacedEmitter::new(seed, event_tx.clone(), writer_dead.clone(), cancel.arc());

        Loop {
            cmd_rx,
            event_tx,
            cancel,
            phase: LoopPhase::Idle,
            pending: PendingState::default(),
            deferred_ringing: VecDeque::new(),
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
            pending_compact_causation: None,
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
            deepx_workspace::CANCEL.store(false, Ordering::SeqCst);

            let _ = self
                .event_tx
                .send(super::types::WriterEvent::Legacy(Agent2Ui::Error {
                    message: format!("Internal error (recovered): {msg}"),
                }));
            let _ = self
                .event_tx
                .send(super::types::WriterEvent::Legacy(Agent2Ui::Done));
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
        self.pending_compact_causation = None;

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
        deepx_workspace::CANCEL.store(false, Ordering::SeqCst);
    }

    /// 将会话 seed 同步到 PacedEmitter（Ringing 事件信封路由键）。
    /// 必须在任何会话创建/恢复（含 auto-create）之后、后续 emit_domain
    /// 之前调用；否则事件携带旧/空 seed，被 daemon SSE 的 owns_seed
    /// 过滤丢弃，前端收不到流式输出。
    fn sync_emitter_seed(&mut self) {
        let seed = self.session.agent.session.seed.clone();
        self.paced_emitter.set_seed(&seed);
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
    /// cannot replace context inside an in-flight lap, and Ringing
    /// ConversationSendMessage is explicitly rejected because the daemon has
    /// already ACKed it (a silent drop would strand the frontend forever).
    pub fn poll_interrupts(&mut self) -> bool {
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            let frame = match cmd.frame {
                super::wire::WorkerCommandFrame::Legacy(frame) => frame,
                super::wire::WorkerCommandFrame::Ringing(env) => {
                    if ringing_command_is_interrupt(&env) {
                        self.cancel.set();
                        deepx_workspace::CANCEL.store(true, Ordering::SeqCst);
                        self.phase = LoopPhase::Idle;
                        return true;
                    }
                    // 忙碌期非中断 Ringing 命令：显式拒绝而非静默丢弃。
                    // 命令在 daemon 侧已被 ACK（accepted），若 worker 无声
                    // 丢弃，前端将永远等待业务终态（消息无限排队/乐观 turn
                    // 永不结束）。以被拒命令的 command_id 作为 causation，
                    // 使 daemon 能把 OperationFailed 折叠进对应 receipt。
                    if let deepx_ringing::RingingCommand::Conversation(
                        deepx_domain::ConversationCommand::ConversationSendMessage { .. },
                    ) = &env.command
                    {
                        let command_id = env.command_id.clone();
                        let _scope =
                            self.paced_emitter.enter_causation(Some(&command_id));
                        self.emit_operation_failed(
                            &command_id,
                            deepx_domain::ErrorScope::Conversation,
                            "busy",
                            "A turn is already running; cancel it before sending a new message",
                        );
                    }
                    continue;
                }
            };
            match frame {
                Ui2Agent::Cancel => {
                    self.cancel.set();
                    deepx_workspace::CANCEL.store(true, Ordering::SeqCst);
                    self.phase = LoopPhase::Idle;
                    let _ = self
                        .event_tx
                        .send(super::types::WriterEvent::Legacy(Agent2Ui::Cancelled));
                    return true;
                }
                Ui2Agent::ResumeSession { seed } => {
                    self.cancel.set();
                    deepx_workspace::CANCEL.store(true, Ordering::SeqCst);
                    self.pending.session = Some(seed);
                    let _ = self
                        .event_tx
                        .send(super::types::WriterEvent::Legacy(Agent2Ui::Cancelled));
                    return true;
                }
                Ui2Agent::NewSession => {
                    self.cancel.set();
                    deepx_workspace::CANCEL.store(true, Ordering::SeqCst);
                    self.pending.new_session = true;
                    let _ = self
                        .event_tx
                        .send(super::types::WriterEvent::Legacy(Agent2Ui::Cancelled));
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
                    let _ =
                        self.event_tx
                            .send(super::types::WriterEvent::Legacy(Agent2Ui::Error {
                                message: "Context compaction requires an idle session.".into(),
                            }));
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
                let _ = self
                    .event_tx
                    .send(super::types::WriterEvent::Legacy(Agent2Ui::Ready));
                self.ready_emitted = true;
            }

            // ── Block for next command (with timeout to poll compact) ──
            let cmd = match self.cmd_rx.recv_timeout(std::time::Duration::from_secs(1)) {
                Ok(f) => {
                    log::info!(
                        "[AGENT] received worker command frame: {:?}",
                        std::mem::discriminant(&f.frame)
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
            let causation = cmd.causation.clone();
            self.safe_dispatch(|this| {
                let _scope = this.paced_emitter.enter_causation(causation.as_deref());
                match cmd.frame {
                    super::wire::WorkerCommandFrame::Legacy(frame) => this.dispatch_one(frame),
                    super::wire::WorkerCommandFrame::Ringing(env) => this.dispatch_ringing_one(env),
                }
            });
        }

        // ── Cleanup ──
        deepx_workspace::runtime::shutdown_tools();
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
                // init_session 已把 agent.session.seed 设为权威值（恢复成功
                // 为原 seed，fallback 为新 seed）；此后 Ringing 事件必须携带它。
                self.sync_emitter_seed();
                let total = self.session.agent.msg.turn_count() as u32;
                let start = total.saturating_sub(INITIAL_LOAD_COUNT as u32) as usize;
                let recent = crate::util::build_turns_from_context(
                    &self.session.agent,
                    Some(start),
                    Some(INITIAL_LOAD_COUNT),
                );
                let _ = self.event_tx.send(super::types::WriterEvent::Legacy(
                    Agent2Ui::SessionRestored {
                        seed: self.session.agent.session.seed.clone(),
                        turns: recent,
                        tokens_used: self.session.agent.session.usage_totals.total_tokens,
                        cache_hit_pct: crate::util::cache_hit_pct(
                            &self.session.agent.session.usage_totals,
                        ),
                        usage: self.session.agent.session.last_usage.clone(),
                        usage_totals: self.session.agent.session.usage_totals.clone(),
                        usage_requests: self.session.agent.session.usage_requests,
                        cache_reported_requests: self
                            .session
                            .agent
                            .session
                            .effective_cache_reported_requests(),
                        total_turns: total,
                        has_more: start > 0,
                    },
                ));
            }
            self.misc
                .emit_dashboard(&self.session.agent, &self.paced_emitter);
            let _ = self
                .event_tx
                .send(super::types::WriterEvent::Legacy(Agent2Ui::Ready));
            self.paced_emitter
                .emit_domain(deepx_domain::DomainEvent::Control(
                    deepx_domain::ControlEvent::AgentLifecycleChanged {
                        state: deepx_domain::AgentLifecycleState::Ready,
                    },
                ));
        } else if has_seed && !self.session.agent.session.from_resume {
            self.session_eng
                .create_with_seed(&mut self.session.agent, &self.cancel);
            self.sync_emitter_seed();
            let _ = self.event_tx.send(super::types::WriterEvent::Legacy(
                Agent2Ui::SessionCreated {
                    seed: self.session.agent.session.seed.clone(),
                },
            ));
            let seed = self.session.agent.session.seed.clone();
            self.paced_emitter
                .emit_domain(deepx_domain::DomainEvent::Control(
                    deepx_domain::ControlEvent::SessionStateChanged {
                        seed: seed.clone(),
                        state: deepx_domain::SessionState::Created,
                    },
                ));
            self.paced_emitter
                .emit_domain(deepx_domain::DomainEvent::Control(
                    deepx_domain::ControlEvent::AgentLifecycleChanged {
                        state: deepx_domain::AgentLifecycleState::Ready,
                    },
                ));
            self.misc
                .emit_dashboard(&self.session.agent, &self.paced_emitter);
            let _ = self
                .event_tx
                .send(super::types::WriterEvent::Legacy(Agent2Ui::Ready));
        } else {
            self.misc
                .emit_dashboard(&self.session.agent, &self.paced_emitter);
            let _ = self
                .event_tx
                .send(super::types::WriterEvent::Legacy(Agent2Ui::Ready));
            self.paced_emitter
                .emit_domain(deepx_domain::DomainEvent::Control(
                    deepx_domain::ControlEvent::AgentLifecycleChanged {
                        state: deepx_domain::AgentLifecycleState::Ready,
                    },
                ));
        }
    }

    // ═══════════════════════════════════════════════════
    // Pending queue drain
    // ═══════════════════════════════════════════════════

    /// Process all queued commands from the channel.
    ///
    /// Interrupt-type commands (Cancel, ResumeSession, NewSession, Shutdown)
    /// set the cancel token and queue a pending action. Ringing commands have
    /// already been acknowledged by the daemon, so commands received during a
    /// session switch are retained and dispatched once the switch completes.
    fn drain_pending(&mut self) {
        self.dispatch_deferred_ringing();
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            let frame = match cmd.frame {
                super::wire::WorkerCommandFrame::Legacy(frame) => frame,
                super::wire::WorkerCommandFrame::Ringing(env) => {
                    if self.pending.is_empty() {
                        let causation = cmd.causation.clone();
                        let _scope = self.paced_emitter.enter_causation(causation.as_deref());
                        self.dispatch_ringing_one(env);
                    } else {
                        self.deferred_ringing.push_back(super::types::WorkerCommand {
                            frame: super::wire::WorkerCommandFrame::Ringing(env),
                            causation: cmd.causation,
                        });
                    }
                    continue;
                }
            };
            match frame {
                Ui2Agent::Cancel => {
                    if std::mem::take(&mut self.terminal_for_queued_interrupt) {
                        self.cancel.clear();
                        deepx_workspace::CANCEL.store(false, Ordering::SeqCst);
                        continue;
                    }
                    self.cancel.set();
                    deepx_workspace::CANCEL.store(true, Ordering::SeqCst);
                    self.phase = LoopPhase::Idle;
                    let _ = self
                        .event_tx
                        .send(super::types::WriterEvent::Legacy(Agent2Ui::Cancelled));
                }
                Ui2Agent::ResumeSession { seed } => {
                    let terminal_emitted = std::mem::take(&mut self.terminal_for_queued_interrupt);
                    self.cancel.set();
                    deepx_workspace::CANCEL.store(true, Ordering::SeqCst);
                    self.pending.session = Some(seed);
                    if !terminal_emitted {
                        let _ = self
                            .event_tx
                            .send(super::types::WriterEvent::Legacy(Agent2Ui::Cancelled));
                    }
                }
                Ui2Agent::NewSession => {
                    let terminal_emitted = std::mem::take(&mut self.terminal_for_queued_interrupt);
                    self.cancel.set();
                    deepx_workspace::CANCEL.store(true, Ordering::SeqCst);
                    self.pending.new_session = true;
                    if !terminal_emitted {
                        let _ = self
                            .event_tx
                            .send(super::types::WriterEvent::Legacy(Agent2Ui::Cancelled));
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
                    let causation = cmd.causation.clone();
                    let _scope = self.paced_emitter.enter_causation(causation.as_deref());
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
                self.sync_emitter_seed();
                let total = self.session.agent.msg.turn_count() as u32;
                let start = total.saturating_sub(INITIAL_LOAD_COUNT as u32) as usize;
                let recent = crate::util::build_turns_from_context(
                    &self.session.agent,
                    Some(start),
                    Some(INITIAL_LOAD_COUNT),
                );
                let _ = self.event_tx.send(super::types::WriterEvent::Legacy(
                    Agent2Ui::SessionRestored {
                        seed: self.session.agent.session.seed.clone(),
                        turns: recent,
                        tokens_used: self.session.agent.session.usage_totals.total_tokens,
                        cache_hit_pct: crate::util::cache_hit_pct(
                            &self.session.agent.session.usage_totals,
                        ),
                        usage: self.session.agent.session.last_usage.clone(),
                        usage_totals: self.session.agent.session.usage_totals.clone(),
                        usage_requests: self.session.agent.session.usage_requests,
                        cache_reported_requests: self
                            .session
                            .agent
                            .session
                            .effective_cache_reported_requests(),
                        total_turns: total,
                        has_more: start > 0,
                    },
                ));
            }
            let _ = self
                .event_tx
                .send(super::types::WriterEvent::Legacy(Agent2Ui::Ready));
        }
        if self.pending.new_session {
            self.pending.new_session = false;
            self.prepare_session_switch();
            self.session_eng
                .create(&mut self.session.agent, &self.cancel);
            self.sync_emitter_seed();
            let _ = self.event_tx.send(super::types::WriterEvent::Legacy(
                Agent2Ui::SessionCreated {
                    seed: self.session.agent.session.seed.clone(),
                },
            ));
            self.misc
                .emit_dashboard(&self.session.agent, &self.paced_emitter);
            let _ = self
                .event_tx
                .send(super::types::WriterEvent::Legacy(Agent2Ui::Ready));
        }
        if self.pending.reload_config {
            self.pending.reload_config = false;
            self.session_eng
                .reload_config(&mut self.session.agent, &self.cancel);
        }
        self.dispatch_deferred_ringing();
    }

    /// Dispatch accepted Ringing commands in FIFO order once no session switch
    /// is pending. Stop as soon as a deferred command schedules another switch;
    /// later commands remain queued for the next drain.
    fn dispatch_deferred_ringing(&mut self) {
        while self.pending.is_empty() {
            let Some(cmd) = self.deferred_ringing.pop_front() else {
                break;
            };
            let super::wire::WorkerCommandFrame::Ringing(env) = cmd.frame else {
                unreachable!("only Ringing commands are deferred");
            };
            let _scope = self.paced_emitter.enter_causation(cmd.causation.as_deref());
            self.dispatch_ringing_one(env);
        }
    }

    /// Check if a background compact has completed and apply the result.
    fn check_pending_compact(&mut self) {
        if let Some(ref rx) = self.pending_compact_rx {
            match rx.try_recv() {
                Ok(meta) => {
                    self.pending_compact_rx = None;
                    let causation = self.pending_compact_causation.take();
                    let _scope = self.paced_emitter.enter_causation(causation.as_deref());
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
                    self.pending_compact_causation = None;
                    let _ =
                        self.event_tx
                            .send(super::types::WriterEvent::Legacy(Agent2Ui::Error {
                                message: "Context compaction failed: worker thread crashed.".into(),
                            }));
                    let _ = self.event_tx.send(super::types::WriterEvent::Legacy(
                        Agent2Ui::CompactEnd {
                            summary_chars: 0,
                            turns_compacted: 0,
                            turns_removed: 0,
                        },
                    ));
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // Still running — check again next loop iteration.
                }
            }
        }
    }

    /// Emit Agent2Ui::SkillsChanged with current available/active skills.
    fn emit_skills_status(&mut self) {
        let workspace = deepx_workspace::CURRENT_WORKSPACE
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let status = self.session.agent.build_skills_status(&workspace);
        let _ = self
            .event_tx
            .send(super::types::WriterEvent::Legacy(Agent2Ui::SkillsChanged {
                status,
            }));
    }

    fn emit_ringing_skills_status(&mut self) {
        let workspace = deepx_workspace::CURRENT_WORKSPACE
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let status = self.session.agent.build_skills_status(&workspace);
        self.paced_emitter
            .emit_domain(deepx_domain::DomainEvent::Control(
                deepx_domain::ControlEvent::SkillsUpdated {
                    available: status
                        .available
                        .iter()
                        .map(|s| deepx_domain::SkillInfo {
                            name: s.name.clone(),
                            description: s.description.clone(),
                            scope: s.scope.clone(),
                            source: s.source.clone(),
                        })
                        .collect(),
                    active: status.active.clone(),
                    catalog_revision: Some(status.catalog_revision.clone()),
                    operation_revision: Some(status.operation_revision),
                    context_epoch: status.context_epoch as usize,
                    token_budget: status.token_budget,
                    token_usage: status.token_usage,
                    runtime: status
                        .runtime
                        .iter()
                        .map(|item| deepx_domain::SkillRuntimeInfo {
                            name: item.name.clone(),
                            description: item.description.clone(),
                            state: item.state.clone(),
                            source: item.source.clone(),
                            token_count: item.token_count,
                            error: item.error.clone(),
                        })
                        .collect(),
                    diagnostics: status.diagnostics.clone(),
                },
            ));
    }

    // ═══════════════════════════════════════════════════
    // Single-command dispatch
    // ═══════════════════════════════════════════════════

    fn start_compact(&mut self, causation: Option<String>) -> Outcome {
        if self.pending_compact_rx.is_some() {
            return Outcome::Error("Context compaction is already running.".into());
        }
        let compact = {
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
            self.compact.build_prompt_and_meta(&mut ctx)
        };
        if let Some((prompt, kept, head, provider, compact_id)) = compact {
            let (tx, rx) = mpsc::channel();
            let event_tx = self.event_tx.clone();
            let compact_seed = self.session.agent.session.seed.clone();
            let worker_causation = causation.clone();
            std::thread::Builder::new()
                .name("compact-worker".into())
                .spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        super::engine_compact::run_compact_worker(
                            compact_seed,
                            compact_id.clone(),
                            prompt,
                            provider,
                            kept,
                            head,
                            event_tx,
                            worker_causation,
                        )
                    }));
                    let meta = match result {
                        Ok(meta) => meta,
                        Err(error) => CompactMeta {
                            compact_id,
                            summary: String::new(),
                            kept_user_count: kept,
                            head_user_count: head,
                            error: Some(format!(
                                "Compact worker panicked: {}",
                                Self::panic_msg_from_err(error)
                            )),
                        },
                    };
                    let _ = tx.send(meta);
                })
                .ok();
            self.pending_compact_rx = Some(rx);
            self.pending_compact_causation = causation;
        } else {
            self.paced_emitter.emit(Agent2Ui::CompactEnd {
                summary_chars: 0,
                turns_compacted: 0,
                turns_removed: 0,
            });
            self.paced_emitter
                .emit_domain(deepx_domain::DomainEvent::Conversation(
                    deepx_domain::ConversationEvent::CompactFinished {
                        compact_id: format!("compact-skipped-{}", self.session.agent.session.seed),
                        status: deepx_domain::CompactStatus::Skipped,
                        summary_chars: Some(0),
                        turns_compacted: Some(0),
                        turns_removed: Some(0),
                    },
                ));
        }
        Outcome::Handled
    }

    /// Dispatch an already typed Ringing command without constructing a
    /// `Ui2Agent` frame. Legacy and Ringing ingress therefore remain separate
    /// at the worker boundary; both may share the domain engines underneath.
    fn emit_operation_completed(&self, command_id: &str, scope: deepx_domain::ErrorScope) {
        self.paced_emitter
            .emit_domain(deepx_domain::DomainEvent::Control(
                deepx_domain::ControlEvent::OperationCompleted {
                    occurrence_id: command_id.to_string(),
                    scope,
                    operation_id: Some(command_id.to_string()),
                },
            ));
    }

    fn emit_operation_failed(
        &self,
        command_id: &str,
        scope: deepx_domain::ErrorScope,
        code: &str,
        message: &str,
    ) {
        self.paced_emitter
            .emit_domain(deepx_domain::DomainEvent::Control(
                deepx_domain::ControlEvent::OperationFailed {
                    occurrence_id: command_id.to_string(),
                    scope,
                    error: deepx_domain::DomainError {
                        error_id: command_id.to_string(),
                        code: code.to_string(),
                        message: message.to_string(),
                        retryable: false,
                        dedupe_key: Some(command_id.to_string()),
                    },
                    operation_id: Some(command_id.to_string()),
                },
            ));
    }

    fn dispatch_ringing_one(&mut self, env: deepx_ringing::RingingWorkerCommandEnvelope) {
        use deepx_domain::{ControlCommand, ConversationCommand, DomainEvent, ToolCommand};
        use deepx_ringing::RingingCommand;

        self.ready_emitted = false;
        let expected_revision = env.expected_revision.unwrap_or_default();
        let command_id = env.command_id.clone();
        match env.command {
            RingingCommand::Control(command) => match command {
                ControlCommand::SessionCreate { close_current } => {
                    if close_current {
                        self.prepare_session_switch();
                    }
                    self.session_eng
                        .create(&mut self.session.agent, &self.cancel);
                    self.sync_emitter_seed();
                    self.paced_emitter.emit_domain(DomainEvent::Control(
                        deepx_domain::ControlEvent::SessionStateChanged {
                            seed: self.session.agent.session.seed.clone(),
                            state: deepx_domain::SessionState::Created,
                        },
                    ));
                    self.misc
                        .emit_dashboard(&self.session.agent, &self.paced_emitter);
                }
                ControlCommand::SessionResume { seed } => {
                    self.prepare_session_switch();
                    if self
                        .session_eng
                        .resume(&mut self.session.agent, &seed, &self.cancel)
                    {
                        self.sync_emitter_seed();
                        self.paced_emitter.emit_domain(DomainEvent::Control(
                            deepx_domain::ControlEvent::SessionStateChanged {
                                seed,
                                state: deepx_domain::SessionState::Resumed,
                            },
                        ));
                    } else {
                        self.emit_operation_failed(
                            &command_id,
                            deepx_domain::ErrorScope::Control,
                            "session_resume_failed",
                            "session could not be resumed",
                        );
                    }
                }
                ControlCommand::SessionShutdown => {
                    self.pending.shutdown = true;
                    self.emit_operation_completed(&command_id, deepx_domain::ErrorScope::Control);
                }
                ControlCommand::AgentReloadConfig => {
                    self.session_eng
                        .reload_config(&mut self.session.agent, &self.cancel);
                    self.emit_operation_completed(&command_id, deepx_domain::ErrorScope::Control);
                }
                ControlCommand::SkillsReload => self.emit_ringing_skills_status(),
                ControlCommand::SkillsActivate { name } => {
                    let _ = self.session.agent.skills.queue_request(&name, "user");
                    self.emit_ringing_skills_status();
                }
                ControlCommand::SkillsRelease { name } => {
                    self.session.agent.deactivate_explicit_skill(&name);
                    self.emit_ringing_skills_status();
                }
                ControlCommand::SkillsOperation {
                    operation_id,
                    action,
                    name,
                } => {
                    let (success, _revision, error) = self.session.agent.skills.apply_ui_operation(
                        &operation_id,
                        expected_revision,
                        &action,
                        &name,
                    );
                    self.emit_ringing_skills_status();
                    if !success {
                        self.paced_emitter
                            .emit_domain(deepx_domain::DomainEvent::Control(
                                deepx_domain::ControlEvent::OperationFailed {
                                    occurrence_id: operation_id.clone(),
                                    scope: deepx_domain::ErrorScope::Control,
                                    error: deepx_domain::DomainError {
                                        error_id: operation_id.clone(),
                                        code: "skill_operation_failed".into(),
                                        message: error
                                            .unwrap_or_else(|| "skill operation failed".into()),
                                        retryable: false,
                                        dedupe_key: Some(operation_id.clone()),
                                    },
                                    operation_id: Some(operation_id),
                                },
                            ));
                    }
                }
                ControlCommand::SessionClose { .. } => {
                    log::debug!("SessionClose is handled by daemon registry");
                }
                ControlCommand::InteractionAskRespond {
                    interaction_id,
                    answers,
                } => {
                    let answers = answers
                        .into_iter()
                        .map(|answer| deepx_proto::AskAnswer {
                            question_id: answer.question_id,
                            answer: answer.answer,
                        })
                        .collect::<Vec<_>>();
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
                    let outcome = self.session.turn.handle_ask_response(
                        &mut ctx,
                        &mut self.session.tool,
                        &interaction_id,
                        &answers,
                    );
                    drop(ctx);
                    self.apply_outcome(outcome);
                }
                ControlCommand::InteractionAskDismiss { interaction_id } => {
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
                    let outcome = self.session.turn.handle_ask_dismiss(
                        &mut ctx,
                        &mut self.session.tool,
                        &interaction_id,
                    );
                    drop(ctx);
                    self.apply_outcome(outcome);
                }
                ControlCommand::PlanReviewRespond {
                    interaction_id,
                    approved,
                    message,
                    autonomous,
                } => {
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
                    let outcome = self.session.turn.handle_plan_response(
                        &mut ctx,
                        &mut self.session.tool,
                        &interaction_id,
                        approved,
                        &message.unwrap_or_default(),
                        autonomous,
                    );
                    drop(ctx);
                    self.apply_outcome(outcome);
                }
            },
            RingingCommand::Conversation(command) => match command {
                ConversationCommand::ConversationSendMessage {
                    text,
                    images,
                    attachments: _,
                } => {
                    let images = images
                        .into_iter()
                        .map(|image| deepx_proto::ImageBlock {
                            mime_type: image.mime_type,
                            data: image.data,
                        })
                        .collect();
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
                    let outcome = self.input.handle_user_input(&mut ctx, &text, images);
                    drop(ctx);
                    self.apply_outcome(outcome);
                }
                ConversationCommand::ConversationCancel { turn_id } => {
                    self.cancel.set();
                    deepx_workspace::CANCEL.store(true, Ordering::SeqCst);
                    self.reset_all_engines();
                    self.paced_emitter.emit_domain(DomainEvent::Conversation(
                        deepx_domain::ConversationEvent::ConversationCancelled { turn_id },
                    ));
                }
                ConversationCommand::ConversationUndoTurn { turn_id } => {
                    self.session.turn.reset();
                    self.session.tool.reset();
                    self.misc
                        .handle_undo(&mut self.session.agent, &turn_id, &self.event_tx);
                    self.emit_operation_completed(
                        &command_id,
                        deepx_domain::ErrorScope::Conversation,
                    );
                }
                ConversationCommand::ConversationSetMode { mode } => {
                    let mode = match mode {
                        deepx_domain::ConversationMode::Normal => "normal",
                        deepx_domain::ConversationMode::Plan => "plan",
                        deepx_domain::ConversationMode::Code => "code",
                    };
                    self.misc.set_mode(&mut self.session.agent, mode);
                    self.emit_operation_completed(
                        &command_id,
                        deepx_domain::ErrorScope::Conversation,
                    );
                }
                ConversationCommand::ConversationCompact { .. } => {
                    let outcome = self.start_compact(Some(command_id));
                    self.apply_outcome(outcome);
                }
                ConversationCommand::ConversationLoadMore { .. } => {
                    self.emit_operation_failed(
                        &command_id,
                        deepx_domain::ErrorScope::Conversation,
                        "unsupported_command",
                        "Ringing v1 bootstrap already contains complete persisted history",
                    );
                }
            },
            RingingCommand::Tool(command) => match command {
                ToolCommand::ToolInvoke {
                    tool_call_id,
                    name,
                    action,
                    args,
                } => {
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
                    self.session.tool.handle_ui_tool_call(
                        &mut ctx,
                        &tool_call_id,
                        &name,
                        &action,
                        &args,
                    );
                }
                ToolCommand::ToolPermissionRespond {
                    tool_call_id,
                    approved,
                    trust_folder,
                    ..
                } => {
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
                    match self.session.tool.handle_permission_response(
                        &mut ctx,
                        &tool_call_id,
                        approved,
                        trust_folder,
                    ) {
                        PermissionDisposition::Ignored => {
                            drop(ctx);
                            self.emit_operation_failed(
                                &command_id,
                                deepx_domain::ErrorScope::Tool,
                                "interaction_not_found",
                                "tool permission request is no longer pending",
                            );
                        }
                        PermissionDisposition::UiHandled => {}
                        PermissionDisposition::LlmResolved { call_id, admitted } => {
                            let outcome = self.session.turn.handle_permission_resolved(
                                &mut ctx,
                                &mut self.session.tool,
                                &call_id,
                                admitted,
                            );
                            drop(ctx);
                            self.apply_outcome(outcome);
                        }
                    }
                }
            },
        }
    }

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
            let _ = self
                .event_tx
                .send(super::types::WriterEvent::Legacy(Agent2Ui::Error {
                    message: "Context compaction is running; wait for CompactEnd.".into(),
                }));
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
                    let _ =
                        self.event_tx
                            .send(super::types::WriterEvent::Legacy(Agent2Ui::Error {
                            message:
                                "Turn is suspended — resolve pending permissions or ask_user first."
                                    .into(),
                        }));
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
                deepx_workspace::CANCEL.store(true, Ordering::SeqCst);
                let suspended = self.session.turn.take_suspended_for_abort();
                if suspended.is_some() {
                    self.session.agent.msg.remove_last_step_if_incomplete();
                }
                // Cancel is a cross-engine reset: clear ALL mutable state
                self.reset_all_engines();
                self.phase = LoopPhase::Idle;
                let _ = self
                    .event_tx
                    .send(super::types::WriterEvent::Legacy(Agent2Ui::Cancelled));
                if let Some((turn_id, usage)) = suspended {
                    self.session.flush();
                    let _ =
                        self.event_tx
                            .send(super::types::WriterEvent::Legacy(Agent2Ui::TurnEnd {
                                turn_id: turn_id.clone(),
                                stop_reason: Some("cancelled".into()),
                                usage: usage.clone(),
                            }));
                    let _ = self
                        .event_tx
                        .send(super::types::WriterEvent::Legacy(Agent2Ui::Done));
                    self.paced_emitter
                        .emit_domain(deepx_domain::DomainEvent::Conversation(
                            deepx_domain::ConversationEvent::TurnCompleted {
                                turn_id,
                                stop_reason: Some("cancelled".into()),
                                usage,
                            },
                        ));
                }
            }
            Ui2Agent::Shutdown => {
                let _ = self
                    .event_tx
                    .send(super::types::WriterEvent::Legacy(Agent2Ui::ShutdownAck));
                self.pending.shutdown = true;
            }
            Ui2Agent::UndoTurn { turn_id } => {
                if self
                    .session
                    .turn
                    .suspended_turn_id()
                    .is_some_and(|active_turn_id| active_turn_id != turn_id)
                {
                    let _ =
                        self.event_tx
                            .send(super::types::WriterEvent::Legacy(Agent2Ui::Error {
                                message: format!(
                                    "Cannot undo {turn_id}: a different active turn is suspended"
                                ),
                            }));
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
                let _ =
                    self.event_tx
                        .send(super::types::WriterEvent::Legacy(Agent2Ui::MoreTurns {
                            turns: batch,
                            has_more: start > 0,
                        }));
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
                self.sync_emitter_seed();
                let _ = self.event_tx.send(super::types::WriterEvent::Legacy(
                    Agent2Ui::SessionCreated {
                        seed: self.session.agent.session.seed.clone(),
                    },
                ));
                self.misc
                    .emit_dashboard(&self.session.agent, &self.paced_emitter);
                return Some(Outcome::Handled);
            }
            Ui2Agent::ResumeSession { seed } => {
                self.prepare_session_switch();
                if self
                    .session_eng
                    .resume(&mut self.session.agent, seed, &self.cancel)
                {
                    self.sync_emitter_seed();
                    let total = self.session.agent.msg.turn_count() as u32;
                    let start = total.saturating_sub(INITIAL_LOAD_COUNT as u32) as usize;
                    let recent = crate::util::build_turns_from_context(
                        &self.session.agent,
                        Some(start),
                        Some(INITIAL_LOAD_COUNT),
                    );
                    let _ = self.event_tx.send(super::types::WriterEvent::Legacy(
                        Agent2Ui::SessionRestored {
                            seed: self.session.agent.session.seed.clone(),
                            turns: recent,
                            tokens_used: self.session.agent.session.usage_totals.total_tokens,
                            cache_hit_pct: crate::util::cache_hit_pct(
                                &self.session.agent.session.usage_totals,
                            ),
                            usage: self.session.agent.session.last_usage.clone(),
                            usage_totals: self.session.agent.session.usage_totals.clone(),
                            usage_requests: self.session.agent.session.usage_requests,
                            cache_reported_requests: self
                                .session
                                .agent
                                .session
                                .effective_cache_reported_requests(),
                            total_turns: total,
                            has_more: start > 0,
                        },
                    ));
                } else {
                    let _ =
                        self.event_tx
                            .send(super::types::WriterEvent::Legacy(Agent2Ui::Error {
                                message: format!("Failed to resume session: {seed}"),
                            }));
                }
                self.misc
                    .emit_dashboard(&self.session.agent, &self.paced_emitter);
                return Some(Outcome::Handled);
            }
            Ui2Agent::NewSession => {
                self.prepare_session_switch();
                self.session_eng
                    .create(&mut self.session.agent, &self.cancel);
                self.sync_emitter_seed();
                let _ = self.event_tx.send(super::types::WriterEvent::Legacy(
                    Agent2Ui::SessionCreated {
                        seed: self.session.agent.session.seed.clone(),
                    },
                ));
                self.misc
                    .emit_dashboard(&self.session.agent, &self.paced_emitter);
                return Some(Outcome::Handled);
            }
            Ui2Agent::ReloadConfig => {
                self.session_eng
                    .reload_config(&mut self.session.agent, &self.cancel);
                return Some(Outcome::Handled);
            }
            Ui2Agent::ReloadSkills => {
                let workspace = deepx_workspace::CURRENT_WORKSPACE
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                self.session.agent.inject_catalog(&workspace);
                self.emit_skills_status();
                self.emit_ringing_skills_status();
                return Some(Outcome::Handled);
            }
            Ui2Agent::UnloadSkill { name } => {
                self.session.agent.deactivate_explicit_skill(name);
                self.emit_skills_status();
                self.emit_ringing_skills_status();
                return Some(Outcome::Handled);
            }
            Ui2Agent::ActivateSkill { name } => {
                let _ = self.session.agent.skills.queue_request(name, "user");
                self.emit_skills_status();
                self.emit_ringing_skills_status();
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
                let _ = self.event_tx.send(super::types::WriterEvent::Legacy(
                    Agent2Ui::SkillOperationResolved {
                        operation_id: operation_id.clone(),
                        success,
                        revision,
                        error,
                    },
                ));
                self.emit_skills_status();
                self.emit_ringing_skills_status();
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
            Ui2Agent::UserInput { text, images } => Some(self.input.handle_user_input(
                &mut ctx,
                text,
                images.to_vec(),
            )),
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
                drop(ctx);
                Some(self.start_compact(None))
            }
            _ => None,
        }
    }

    // ═══════════════════════════════════════════════════
    // Outcome handler — the Ring's decision point
    // ═══════════════════════════════════════════════════

    /// Apply the outcome returned by an engine.
    ///
    /// This is the central decision point of the Ringing V1 architecture.
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
                let _ = self
                    .event_tx
                    .send(super::types::WriterEvent::Legacy(Agent2Ui::TurnEnd {
                        turn_id: turn_id.clone(),
                        stop_reason: None,
                        usage: usage.clone(),
                    }));
                self.paced_emitter
                    .emit_domain(deepx_domain::DomainEvent::Conversation(
                        deepx_domain::ConversationEvent::TurnCompleted {
                            turn_id,
                            stop_reason: None,
                            usage,
                        },
                    ));

                // Desktop notification: preview of assistant response
                self.misc.maybe_notify(&self.session.agent, &self.notify.tx);

                // Goal mode auto-advance: if the LLM completed a step
                // (via todo(action=update, status=completed)), inject the next step.
                if let Ok(store) = deepx_workspace::todo::load_todo() {
                    if store.mode == deepx_workspace::todo::TodoMode::Goal {
                        if let Some(ref current_id) = store.current_id {
                            if let Some(item) = store.items.iter().find(|i| &i.id == current_id) {
                                if item.status == deepx_workspace::todo::TodoStatus::InProgress {
                                    let prompt = format!(
                                        "[自动执行计划 / 目标模式]\n\n\
                                         T{}: {}\n{}\n\n\
                                         完成此步骤后，调用 todo(action=\"update\", id=\"{}\", status=\"completed\", evidence=\"...\").",
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
                                        self.input.handle_user_input(&mut ctx, &prompt, vec![]);
                                    drop(ctx);
                                    self.apply_outcome(next_outcome);
                                    return;
                                }
                            }
                        }
                    }
                }

                let _ = self
                    .event_tx
                    .send(super::types::WriterEvent::Legacy(Agent2Ui::Done));
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
                let _ = self
                    .event_tx
                    .send(super::types::WriterEvent::Legacy(Agent2Ui::Cancelled));
                let _ = self
                    .event_tx
                    .send(super::types::WriterEvent::Legacy(Agent2Ui::TurnEnd {
                        turn_id: turn_id.clone(),
                        stop_reason: Some("cancelled".into()),
                        usage: usage.clone(),
                    }));
                self.paced_emitter
                    .emit_domain(deepx_domain::DomainEvent::Conversation(
                        deepx_domain::ConversationEvent::TurnCompleted {
                            turn_id,
                            stop_reason: Some("cancelled".into()),
                            usage,
                        },
                    ));
                let _ = self
                    .event_tx
                    .send(super::types::WriterEvent::Legacy(Agent2Ui::Done));
                self.phase = LoopPhase::Idle;
            }
            Outcome::TurnFailed {
                turn_id,
                usage,
                message,
            } => {
                self.session.agent.skills.abort_user_turn();
                self.session.flush();
                let _ = self
                    .event_tx
                    .send(super::types::WriterEvent::Legacy(Agent2Ui::Error {
                        message: message.clone(),
                    }));
                let _ = self
                    .event_tx
                    .send(super::types::WriterEvent::Legacy(Agent2Ui::TurnEnd {
                        turn_id: turn_id.clone(),
                        stop_reason: Some("error".into()),
                        usage: usage.clone(),
                    }));
                self.paced_emitter
                    .emit_domain(deepx_domain::DomainEvent::Conversation(
                        deepx_domain::ConversationEvent::TurnFailed {
                            turn_id,
                            error: deepx_domain::DomainError {
                                error_id: format!(
                                    "turn-failed-{}",
                                    std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_millis())
                                        .unwrap_or(0),
                                ),
                                code: "turn_failed".into(),
                                message,
                                retryable: false,
                                dedupe_key: None,
                            },
                        },
                    ));
                let _ = self
                    .event_tx
                    .send(super::types::WriterEvent::Legacy(Agent2Ui::Done));
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
                let _ = self
                    .event_tx
                    .send(super::types::WriterEvent::Legacy(Agent2Ui::Error {
                        message: msg,
                    }));
                self.phase = LoopPhase::Idle;
            }
            Outcome::Shutdown => {
                self.pending.shutdown = true;
            }
        }
    }
}
