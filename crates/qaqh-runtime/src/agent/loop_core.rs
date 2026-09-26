//! Loop core — worker 进程内的单会话事件驱动循环（Ringing V1 架构）。
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────────────────┐
//! │  Loop（worker 进程，单会话）                          │
//! │  ├─ I/O: cmd_rx（reader 线程 stdin → JSON-LP）       │
//! │  │        event_tx（writer 线程 → stdout，2ms 批量）  │
//! │  ├─ Signal: cancel, phase, pending, writer_dead      │
//! │  ├─ Session: session (SessionBundle)                 │
//! │  │   ├─ agent: AgentState                            │
//! │  │   ├─ stats: StatsCollector                        │
//! │  │   ├─ turn: TurnEngine                             │
//! │  │   └─ tool: ToolEngine                             │
//! │  ├─ Engines: input, misc（compact 已去壳为自由函数）       │
//! │  ├─ flow: ContextFlow（消息落盘/注入融合）            │
//! │  ├─ injection_bus: 注入总线（idle 直派 / busy 入队）  │
//! │  └─ paced_emitter: 事件节拍 + causation 作用域        │
//! └──────────────────────────────────────────────────────┘
//! ```
//!
//! Loop 是**单会话**的：一次进程只承载一个 `SessionBundle`（会话隔离的
//! 单位）。会话切换时整包落盘并替换。进程级状态（I/O 通道、cancel token）
//! 不受影响。命令经 `dispatch_ringing_one` 直接路由到各引擎方法（无独立
//! `Engine` trait）；中断类命令由 reader 线程直接置 cancel 以便立即生效。
//!
//! # Panic recovery
//!
//! 每次派发都包在 `safe_dispatch()` 里。若引擎 panic：
//! 1. 所有引擎重置到干净 idle 状态
//! 2. cancel token 清空
//! 3. 向 daemon 发射 `ControlEvent::OperationFailed`（legacy Agent2Ui 已拆除）
//! 4. Loop 继续处理后续命令
//!
//! # 新增命令
//!
//! 1. 若命令跨 wire：在 `qaqh-domain` / `qaqh-ringing` 增加对应变体
//! 2. 在 `dispatch_ringing_one` 路由到对应引擎方法
//! 3. 需要复位语义的在 `reset_all_engines()` 中登记
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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use super::compaction_port::CompactionPort;
use super::engine_input::InputEngine;
use super::engine_misc::MiscEngine;
use super::injection::InjectionBus;
use super::lifecycle_port::{LifecyclePort, RuntimeLifecyclePort};
use super::paced_emitter::PacedEmitter;
use super::types::*;
use crate::RingingHub;
use crate::agent::state::agent::AgentState;

pub fn ringing_command_is_interrupt(env: &qaqh_ringing::RingingWorkerCommandEnvelope) -> bool {
    matches!(
        &env.command,
        qaqh_ringing::RingingCommand::Control(
            qaqh_domain::ControlCommand::SessionResume { .. }
                | qaqh_domain::ControlCommand::SessionShutdown
                | qaqh_domain::ControlCommand::SessionCreate { .. }
        ) | qaqh_ringing::RingingCommand::Conversation(
            qaqh_domain::ConversationCommand::ConversationCancel { .. }
        )
    )
}

/// 解析注入文本首行的 `[SUBAGENT 'name' STATE]` 标签 → (name, state)。
/// 标签规范见 `crates/qaqh-subagent/src/lib.rs` collect 收尾（COMPLETED /
/// ERROR / TIMEOUT / CANCELLED 变体）。与前端 `parse_subagent_injection`
/// 保持同一格式约定；解析失败返回 None（静默，不阻断注入本身）。
pub(super) fn parse_subagent_status_tag(text: &str) -> Option<(String, String)> {
    let first = text.lines().next()?.trim_start();
    let rest = first.strip_prefix("[SUBAGENT '")?;
    let (name, rest) = rest.split_once("' ")?;
    let state = if rest.starts_with("COMPLETED]") {
        "COMPLETED"
    } else if rest.starts_with("ERROR") {
        "ERROR"
    } else if rest.starts_with("TIMEOUT") {
        "TIMEOUT"
    } else if rest.starts_with("CANCELLED]") {
        "CANCELLED"
    } else {
        return None;
    };
    Some((name.to_string(), state.to_string()))
}

// ═══════════════════════════════════════════════════════
// Loop — the dispatcher
// ═══════════════════════════════════════════════════════

/// Pre-created in-process worker channel ends.
///
/// An in-process host (daemon actor, tests) can create them before the loop,
/// keep the producer/consumer sides, and construct the loop later with
/// [`Loop::from_channels`].
pub struct LoopChannels {
    pub cmd_tx: mpsc::SyncSender<WorkerCommand>,
    pub cmd_rx: mpsc::Receiver<WorkerCommand>,
    pub event_tx: mpsc::SyncSender<WriterEvent>,
    pub event_rx: mpsc::Receiver<WriterEvent>,
    pub cancel: CancelToken,
    pub writer_dead: Arc<AtomicBool>,
}

impl Default for LoopChannels {
    fn default() -> Self {
        Self::new()
    }
}

impl LoopChannels {
    /// Create the bounded channels used by a Ringing V1 loop.
    pub fn new() -> Self {
        // std 的 sync_channel 在构造时预分配 (capacity + 1) 个 slot 的环形缓冲，
        // 每个 slot 是 size_of::<WriterEvent>() = 512 字节（枚举按最大变体对齐）。
        // 旧值 655360 × 512B ≈ 320MB —— 每个 worker 进程启动即常驻，这正是
        // 单 session 内存 300MB+ 的根因。writer 线程逐事件即时写 stdout，突发
        // 事件由 PacedEmitter 以 ≤50ms 节流合并，16384 个 slot（8MB）在保留
        // 背压语义的同时把固定开销降到合理范围。
        let (cmd_tx, cmd_rx) = mpsc::sync_channel::<WorkerCommand>(4096);
        let (event_tx, event_rx) = mpsc::sync_channel::<WriterEvent>(16384);
        Self {
            cmd_tx,
            cmd_rx,
            event_tx,
            event_rx,
            cancel: CancelToken::new(),
            writer_dead: Arc::new(AtomicBool::new(false)),
        }
    }
}

pub struct Loop {
    // ── Process-level I/O ──
    /// Incoming command channel (fed by reader thread).
    pub(super) cmd_rx: mpsc::Receiver<super::types::WorkerCommand>,
    /// Outgoing event channel (consumed by writer thread).
    pub(super) event_tx: mpsc::SyncSender<super::types::WriterEvent>,

    // ── Process-level signals ──
    /// Cancellation token shared across engines.
    pub(super) cancel: CancelToken,
    /// Current phase (Idle / GateRunning / ToolsRunning).
    pub(super) phase: LoopPhase,
    /// Deferred interrupt commands received while busy.
    pub(super) pending: PendingState,
    /// Ringing commands already acknowledged by the daemon while a legacy
    /// session switch is pending. An accepted command must execute exactly
    /// once after the switch; it must never be silently discarded.
    pub(super) deferred_ringing: VecDeque<super::types::WorkerCommand>,
    /// Set to true when the writer thread exits (stdout pipe broken).
    pub(super) writer_dead: Arc<AtomicBool>,
    /// Whether a `Ready` event has already been emitted for the current
    /// idle period. Prevents the 1 Hz `Ready` storm that flooded the
    /// daemon's Critical lane (each Ready is EventLane::Critical and was
    /// sent every loop iteration, saturating priority queues and tripping
    /// the connection-death cascade).
    pub(super) ready_emitted: bool,

    // ── Session-scoped state (flushed/swapped on session change) ──
    /// The active session's data and engines. Loop 单会话：进程只承载一个
    /// bundle，切换时整包落盘并替换。
    pub(super) session: SessionBundle,

    // ── Session-agnostic engines (process lifetime, no session state) ──
    /// User input handler: compliance guard, auto-create session.
    pub(super) input: InputEngine,
    /// Miscellaneous: undo, dashboard, mode.
    pub(super) misc: MiscEngine,
    /// Unified context-ingestion pipeline — the single door into the message
    /// store for every message source (user/model/tool/skills/subagent/goal).
    /// Registered with the built-in sources at construction; new sources
    /// (ACP/MCP loops) register here without touching the dispatcher.
    pub(super) flow: qaqh_message::ContextFlow,
    /// Busy-turn injections waiting for the next lap boundary.
    pub(super) injection_bus: InjectionBus,
    /// Background compaction task state.
    pub(super) compaction: CompactionPort,

    /// Direct output emitter. The renderer performs frame-level coalescing.
    pub(super) paced_emitter: PacedEmitter,

    /// Lifecycle boundary: liveness, session lifecycle and title task port.
    pub(super) lifecycle: Box<dyn LifecyclePort>,

    /// Test-only panic injection seam: when set, dispatching a command whose
    /// `command_id` matches panics **on the dispatching thread** (same thread
    /// and stack as a real engine panic). `cfg(test)`-gated, so it never
    /// enters a production binary and is unreachable from the wire.
    #[cfg(test)]
    pub(super) panic_on_command_id: Option<String>,

    /// Daemon-side Ringing hub for durable content externalization. `None`
    /// in isolated unit tests; oversized canonical payloads then fail closed
    /// instead of being silently skipped.
    pub(super) hub: Option<Arc<RingingHub>>,
}

impl Loop {
    pub fn from_channels(
        agent: AgentState,
        cmd_rx: mpsc::Receiver<WorkerCommand>,
        event_tx: mpsc::SyncSender<WriterEvent>,
        cancel: CancelToken,
        writer_dead: Arc<AtomicBool>,
        liveness: std::sync::Arc<super::liveness::WorkerLiveness>,
        hub: Option<Arc<RingingHub>>,
    ) -> Self {
        // resume 模式下 `--resume-seed` 只写入 resume_seed 字段，seed 此时
        // 仍为空；用 resume_seed 兜底，避免 PacedEmitter 以空 seed 构造
        // （Ringing 事件信封会被 daemon 按 seed 过滤丢弃）。init_session
        // 完成后还会经 sync_emitter_seed 再次同步权威值。
        let seed = if !agent.session.session_id.is_empty() {
            agent.session.session_id.clone()
        } else {
            agent.session.resume_seed.clone().unwrap_or_default()
        };
        let paced_emitter = PacedEmitter::new(seed, event_tx.clone(), writer_dead.clone());

        let mut flow = qaqh_message::ContextFlow::new();
        qaqh_message::builtin::register_all(&mut flow);
        let lifecycle: Box<dyn LifecyclePort> = Box::new(RuntimeLifecyclePort::new(liveness));

        Loop {
            cmd_rx,
            event_tx,
            cancel,
            phase: LoopPhase::Idle,
            pending: PendingState::default(),
            deferred_ringing: VecDeque::new(),
            writer_dead,
            ready_emitted: false,
            session: SessionBundle::new(agent),
            input: InputEngine::new(),
            misc: MiscEngine::new(),
            flow,
            injection_bus: InjectionBus::new(),
            compaction: CompactionPort::new(),
            paced_emitter,
            lifecycle,
            #[cfg(test)]
            panic_on_command_id: None,
            hub,
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
    /// 4. A `ControlEvent::OperationFailed` is emitted to the daemon
    ///
    /// The Loop continues processing commands after recovery.
    fn safe_dispatch<F>(&mut self, f: F)
    where
        F: FnOnce(&mut Self) + std::panic::UnwindSafe,
    {
        // Idle-unload liveness: this dispatch counts as activity; the registry
        // must never unload while it is running.
        self.lifecycle.dispatch_started();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            f(self);
        }));

        if let Err(e) = result {
            let msg = Self::panic_msg_from_err(e);
            log::error!("[AGENT] engine panic during dispatch: {msg}");
            eprintln!("[qaqh AGENT] engine panic during dispatch: {msg}");

            self.reset_all_engines();
            self.phase = LoopPhase::Idle;
            self.cancel.clear();
            qaqh_workspace::clear_cancel();
            // L1: a panic must not widen the persistence loss window. Ops
            // enqueued before the panic are complete PersistOps — applying
            // them here keeps the archive at the last coherent round boundary.
            self.session.agent.drain_persist_ops();

            // panic 恢复：Ringing 侧以 OperationFailed 暴露（legacy Error/Done 已拆除）。
            self.paced_emitter
                .emit_domain(qaqh_domain::DomainEvent::Control(
                    qaqh_domain::ControlEvent::OperationFailed {
                        occurrence_id: format!(
                            "occ-panic-{}",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis())
                                .unwrap_or(0)
                        ),
                        scope: qaqh_domain::ErrorScope::System,
                        error: qaqh_domain::DomainError {
                            error_id: format!(
                                "panic-{}",
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_millis())
                                    .unwrap_or(0)
                            ),
                            code: "engine_panic_recovered".into(),
                            message: format!("Internal error (recovered): {msg}"),
                            retryable: false,
                            dedupe_key: None,
                        },
                        operation_id: None,
                    },
                ));
        }

        // Liveness bookkeeping runs on both the success and panic-recovery
        // paths: a completed dispatch is activity, and a suspended turn
        // (unresolved ask / permission / plan) blocks idle unload.
        self.lifecycle
            .dispatch_finished(self.session.turn.is_suspended());
    }

    /// Reset all engines to clean idle state.
    ///
    /// Called after a panic or on Cancel.
    /// Session-level engines are reset (turn, tool) to clear any
    /// suspended state or pending approvals. Stateless engines are
    /// no-ops. Stats accumulator is replaced with a fresh one.
    pub(super) fn reset_all_engines(&mut self) {
        self.reset_all_engines_inner(true);
    }

    /// Reset every non-turn engine and clear turn runtime state, but keep the
    /// actor terminal result available for duplicate cancel/terminal fencing.
    pub(super) fn reset_all_engines_preserving_turn_terminal(&mut self) {
        self.reset_all_engines_inner(false);
    }

    fn reset_all_engines_inner(&mut self, reset_turn_actor: bool) {
        // Session-level engines (hold mutable state)
        if reset_turn_actor {
            self.session.turn.reset();
        } else {
            self.session.turn.reset_runtime_state();
        }
        self.session.tool.clear_pending();
        self.session.stats = StatsCollector::new();

        // Session-agnostic engines：无状态（M3 后无 Engine trait reset）
        self.misc.reset();
        self.finish_pending_compact(qaqh_domain::CompactStatus::Cancelled);

        self.pending.clear();
    }

    /// Close any suspended transaction before replacing the active session.
    /// An unanswered ask/tool round must never be persisted into, or resumed
    /// against, the next session.
    pub(super) fn prepare_session_switch(&mut self) {
        self.clear_injections();
        self.session.agent.reset_compaction_coordination();
        if self.session.turn.is_suspended() {
            self.session.agent.msg.remove_last_step_if_incomplete();
        }
        self.session.flush();
        self.reset_all_engines();
        self.cancel.clear();
        qaqh_workspace::clear_cancel();
    }

    /// 将会话 seed 同步到 PacedEmitter（Ringing 事件信封路由键）。
    /// 必须在任何会话创建/恢复（含 auto-create）之后、后续 emit_domain
    /// 之前调用；否则事件携带旧/空 seed，被 daemon SSE 的 owns_seed
    /// 过滤丢弃，前端收不到流式输出。
    pub(super) fn sync_emitter_seed(&mut self) {
        let seed = self.session.agent.session.session_id.clone();
        self.paced_emitter.set_seed(&seed);
        self.injection_bus.switch_session(&seed);
    }

    /// Extract a human-readable message from a panic payload.
    pub(super) fn panic_msg_from_err(e: Box<dyn std::any::Any + Send>) -> String {
        if let Some(s) = e.downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = e.downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic".into()
        }
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
                self.finish_pending_compact(qaqh_domain::CompactStatus::Cancelled);
                log::error!("[AGENT] writer thread died — exiting");
                eprintln!("[qaqh AGENT] writer thread died — stdout pipe broken. Exiting.");
                break;
            }

            // ── Check background compact completion ──
            self.check_pending_compact();

            // Signal readiness at most once per truly idle period. A manual
            // compact runs in a background worker, but it still owns the
            // active context transaction until CompactEnd is applied.
            if !self.compaction.is_running() && !self.ready_emitted {
                self.ready_emitted = true;
            }

            // ── Block for next command (with timeout to poll compact) ──
            let cmd = match self.cmd_rx.recv_timeout(std::time::Duration::from_secs(1)) {
                Ok(f) => {
                    log::info!(
                        "[AGENT] received worker command frame: seed={} cmd={}",
                        f.frame.session_id,
                        f.frame.command_id
                    );
                    f
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // Compact polling path can also enqueue persist ops
                    // (finish_manual_compact → flush_meta); drain before idling.
                    self.session.agent.drain_persist_ops();
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.finish_pending_compact(qaqh_domain::CompactStatus::Cancelled);
                    log::error!("[AGENT] cmd_rx closed — stdin pipe broken. Exiting.");
                    eprintln!("[qaqh AGENT] stdin pipe broken — exiting.");
                    break;
                }
            };

            // ── Dispatch with panic safety ──
            self.dispatch_frame(cmd);
        }

        // ── Cleanup ──
        qaqh_workspace::runtime::shutdown_tools();
        self.session.flush();
        // Final drain: SessionBundle::flush enqueues a flush_meta op; the old
        // synchronous path wrote it before exiting (PR-1-6).
        self.session.agent.drain_persist_ops();
    }

    /// Initialize session state from pre-set seed (CLI args --seed / --resume-seed).
    fn init_session(&mut self) {
        let resume_seed = self.session.agent.session.resume_seed.take();
        let has_seed = !self.session.agent.session.session_id.is_empty();

        if let Some(seed) = resume_seed {
            if self
                .lifecycle
                .resume_session(&mut self.session.agent, &self.cancel, &seed)
            {
                // init_session 已把 agent.session.session_id 设为权威值（恢复成功
                // 为原 seed，fallback 为新 seed）；此后 Ringing 事件必须携带它。
                self.sync_emitter_seed();
                // legacy SessionRestored 已退役：Ringing 恢复由 daemon bootstrap 快照承担。
            }
            self.misc
                .emit_dashboard(&self.session.agent, &self.paced_emitter);
            self.paced_emitter
                .emit_domain(qaqh_domain::DomainEvent::Control(
                    qaqh_domain::ControlEvent::AgentLifecycleChanged {
                        state: qaqh_domain::AgentLifecycleState::Ready,
                    },
                ));
        } else if has_seed && !self.session.agent.session.from_resume {
            self.lifecycle
                .create_session_with_seed(&mut self.session.agent, &self.cancel);
            self.sync_emitter_seed();
            let seed = self.session.agent.session.session_id.clone();
            self.paced_emitter
                .emit_domain(qaqh_domain::DomainEvent::Control(
                    qaqh_domain::ControlEvent::SessionStateChanged {
                        seed: seed.clone(),
                        state: qaqh_domain::SessionState::Created,
                    },
                ));
            self.paced_emitter
                .emit_domain(qaqh_domain::DomainEvent::Control(
                    qaqh_domain::ControlEvent::AgentLifecycleChanged {
                        state: qaqh_domain::AgentLifecycleState::Ready,
                    },
                ));
            self.misc
                .emit_dashboard(&self.session.agent, &self.paced_emitter);
        } else {
            self.misc
                .emit_dashboard(&self.session.agent, &self.paced_emitter);
            self.paced_emitter
                .emit_domain(qaqh_domain::DomainEvent::Control(
                    qaqh_domain::ControlEvent::AgentLifecycleChanged {
                        state: qaqh_domain::AgentLifecycleState::Ready,
                    },
                ));
        }

        // 崩溃恢复不再重放注入日志（PLAN B1）：注入一旦落盘到 messages.jsonl
        // 即成 history，由 from_messages 按原写入位置恢复；未落盘的崩溃窗口
        // 注入静默丢弃（从未进入任何请求，无事实损失）。
    }

    // ═══════════════════════════════════════════════════
    // The single guarded dispatch entry point
    // ═══════════════════════════════════════════════════

    /// The **only** dispatch entry point: causation scope + `safe_dispatch`
    /// guard (catch_unwind + liveness bookkeeping) + per-command
    /// `drain_persist_ops`.
    ///
    /// BUG-2026-09-13-07: `drain_pending` (the first frame of every main-loop
    /// iteration, i.e. the unprotected path for the head-of-queue command
    /// during idle) and the `dispatch_deferred_ringing` loop body used to call
    /// `dispatch_ringing_one` bare. An engine panic there escaped `run()`
    /// (process death: the dequeued command plus every un-flushed PersistOp
    /// was lost) and skipped the liveness `busy` flag, so
    /// `unload_idle_sessions` could reap a working worker. Per the
    /// `liveness.rs` contract the flag must span the whole dispatch.
    ///
    /// Every dispatcher must route through here; never call
    /// `dispatch_ringing_one` directly.
    fn dispatch_frame(&mut self, cmd: super::types::WorkerCommand) {
        let causation = cmd.causation;
        self.safe_dispatch(|this| {
            let _scope = this.paced_emitter.enter_causation(causation.as_deref());
            this.dispatch_ringing_one(cmd.frame);
            // PR-1-6: flush queued persistence ops after the command's
            // dispatch completes (write order = enqueue order, Z5).
            this.session.agent.drain_persist_ops();
        });
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
            if self.pending.is_empty() {
                // 空闲期队首命令：与 run() 阻塞分支同一守卫路径。
                self.dispatch_frame(cmd);
            } else {
                self.deferred_ringing.push_back(cmd);
            }
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
            self.dispatch_frame(cmd);
        }
    }

    // ═══════════════════════════════════════════════════
    // Command router — arms live in loop_dispatch_* (Phase 2-5)
    // ═══════════════════════════════════════════════════

    fn dispatch_ringing_one(&mut self, env: qaqh_ringing::RingingWorkerCommandEnvelope) {
        use qaqh_ringing::RingingCommand;

        self.ready_emitted = false;
        let expected_revision = env.expected_revision.unwrap_or_default();
        let command_id = env.command_id.clone();

        #[cfg(test)]
        if self.panic_on_command_id.as_deref() == Some(command_id.as_str()) {
            panic!("injected engine panic for command {command_id}");
        }

        let command_session_id = env.session_id.clone();

        match env.command {
            RingingCommand::Control(command) => {
                self.on_control(command, &command_id, expected_revision);
            }
            RingingCommand::Conversation(command) => {
                self.on_conversation(command, &command_id, &command_session_id);
            }
            RingingCommand::Tool(command) => {
                self.on_tool(command, &command_id);
            }
        }
    }
}

#[cfg(test)]
mod parse_subagent_status_tag_tests {
    use super::parse_subagent_status_tag;

    #[test]
    fn parses_all_terminal_tags() {
        assert_eq!(
            parse_subagent_status_tag("[SUBAGENT 'explore' COMPLETED]\n\nfinal answer"),
            Some(("explore".to_string(), "COMPLETED".to_string()))
        );
        assert_eq!(
            parse_subagent_status_tag("[SUBAGENT 'x' ERROR exit=1]"),
            Some(("x".to_string(), "ERROR".to_string()))
        );
        assert_eq!(
            parse_subagent_status_tag("[SUBAGENT 'x' TIMEOUT after 120s]"),
            Some(("x".to_string(), "TIMEOUT".to_string()))
        );
        assert_eq!(
            parse_subagent_status_tag("[SUBAGENT 'x' CANCELLED]"),
            Some(("x".to_string(), "CANCELLED".to_string()))
        );
    }

    #[test]
    fn rejects_non_injection_text() {
        assert_eq!(parse_subagent_status_tag("normal user message"), None);
        assert_eq!(parse_subagent_status_tag("[SUBAGENT 'x' RUNNING]"), None);
        assert_eq!(parse_subagent_status_tag(""), None);
        // 标签不在首行（注入文本规范要求首行即标签）→ 不匹配。
        assert_eq!(
            parse_subagent_status_tag("some text\n[SUBAGENT 'x' COMPLETED]"),
            None
        );
    }
}

#[cfg(test)]
mod drain_dispatch_safety_tests {
    //! BUG-2026-09-13-07 回归：主循环空闲期的 `drain_pending` 直派与
    //! `dispatch_deferred_ringing` 循环体必须走 `dispatch_frame`
    //! （safe_dispatch 守卫 + liveness 记账 + drain_persist_ops）。
    //!
    //! 未修复代码上三例全红（见 PR 红→绿证据）。

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{Loop, LoopChannels};
    use crate::agent::liveness::WorkerLiveness;
    use crate::agent::state::agent::{AgentState, MetaOp};
    use crate::agent::types::{CancelToken, WorkerCommand};

    /// 在测试线程上构造 Loop：事件端由调用方持有 `event_rx`，加一个把事件
    /// 丢弃的 writer 线程，避免 SyncSender 背压阻塞被测派发路径。
    fn test_loop(
        liveness: Arc<WorkerLiveness>,
    ) -> (Loop, std::sync::mpsc::SyncSender<WorkerCommand>) {
        let channels = LoopChannels::new();
        let cmd_tx = channels.cmd_tx.clone();

        let event_rx = channels.event_rx;
        std::thread::spawn(move || {
            // 排空 event channel，防止满队列让 paced_emitter 阻塞。
            for _ in event_rx {}
        });

        let mut agent = AgentState::new(qaqh_config::Config::default());
        agent.ephemeral = true;
        // 无 session manager：persist ops 留在内存队列，但
        // `drain_persist_ops` 仍会 take 走队列 —— 正是本测试的观察点。
        agent.session_manager = None;

        let lp = Loop::from_channels(
            agent,
            channels.cmd_rx,
            channels.event_tx,
            CancelToken::new(),
            channels.writer_dead,
            liveness,
            None,
        );
        (lp, cmd_tx)
    }

    fn cmd(command_id: &str, command: qaqh_domain::ControlCommand) -> WorkerCommand {
        WorkerCommand {
            frame: qaqh_ringing::RingingWorkerCommandEnvelope::new(
                "test-seed",
                command_id,
                qaqh_ringing::RingingCommand::Control(command),
            ),
            causation: Some(command_id.to_string()),
        }
    }

    /// ① panic 不逃逸 ② liveness 不失效：`drain_pending` 直派路径注入 panic，
    /// run() 必须正常返回（进程存活），且派发期间 liveness.busy 置位、
    /// 结束后回落（期间 registry 不可 unload）。
    #[test]
    fn drain_pending_panic_does_not_escape_and_liveness_is_accounted() {
        let liveness = Arc::new(WorkerLiveness::new());
        let (mut lp, cmd_tx) = test_loop(liveness.clone());

        // 注入 seam：命中 command_id 即 panic（与真实引擎 panic 同线程同栈）。
        lp.panic_on_command_id = Some("boom".into());

        // 峰值观察：safe_dispatch 期间 busy 应被置位。
        // 由于 panic 在同一线程同步发生，用 run() 前后 + 事后断言覆盖。
        cmd_tx
            .send(cmd("boom", qaqh_domain::ControlCommand::SessionShutdown))
            .expect("test channel must not fail");
        // 触发 shutdown 的真正命令（panic 那条不产生 shutdown 副作用）。
        cmd_tx
            .send(cmd("bye", qaqh_domain::ControlCommand::SessionShutdown))
            .expect("test channel must not fail");
        drop(cmd_tx);

        // 未修复：panic 从 drain_pending 逃逸 → run() 直接 unwind，本测试 panic。
        // 修复后：panic 被 safe_dispatch 收容，run() 正常返回。
        lp.run();

        assert!(
            liveness.unloadable(),
            "派发结束后 liveness 必须回落（busy=false / suspend=false）"
        );
    }

    /// ② 细化：deferred 派发期间 busy 必须置位 —— 由测试命令内联观察。
    /// 未修复代码上此例失败（busy 从未置位 → 派发中仍可被 unload）。
    #[test]
    fn liveness_busy_is_set_while_dispatching_deferred_command() {
        let liveness = Arc::new(WorkerLiveness::new());
        let (mut lp, _cmd_tx) = test_loop(liveness.clone());

        // `safe_dispatch` 是本路径的 busy 窗口持有者；在 deferred 队列上跑一条
        // 命令，并在派发**内部**读取 unloadable 快照。
        let snapshot = Arc::new(AtomicUsize::new(usize::MAX));
        let snap = snapshot.clone();
        let live = liveness.clone();
        lp.session.agent.enqueue_meta_op(MetaOp::PersistMode {
            seed: "test-seed".into(),
            mode: 1,
        });
        lp.safe_dispatch(move |this| {
            snap.store(live.unloadable() as usize, Ordering::SeqCst);
            this.session.agent.drain_persist_ops();
        });

        assert_eq!(
            snapshot.load(Ordering::SeqCst),
            0,
            "派发进行中 worker 不可 unload（busy 已置位）"
        );
        assert!(liveness.unloadable(), "派发结束后必须可 unload");
    }

    /// ③ panic 路径也必须 flush 在飞的 persist ops（旧代码 drain_pending
    /// 首分支完全缺 drain_persist_ops）。
    #[test]
    fn drain_path_flushes_pending_persist_ops_on_panic() {
        let liveness = Arc::new(WorkerLiveness::new());
        let (mut lp, cmd_tx) = test_loop(liveness.clone());

        // 先入队一条 MetaOp，模拟 panic 前已产生的持久化工作。
        lp.session.agent.enqueue_meta_op(MetaOp::PersistMode {
            seed: "test-seed".into(),
            mode: 1,
        });
        assert_eq!(lp.session.agent.pending_meta_ops.len(), 1);

        lp.panic_on_command_id = Some("boom".into());
        cmd_tx
            .send(cmd("boom", qaqh_domain::ControlCommand::SessionShutdown))
            .expect("test channel must not fail");
        cmd_tx
            .send(cmd("bye", qaqh_domain::ControlCommand::SessionShutdown))
            .expect("test channel must not fail");
        drop(cmd_tx);
        lp.run();

        assert!(
            lp.session.agent.pending_meta_ops.is_empty(),
            "panic 路径上在飞的 persist ops 必须被 flush 取空"
        );
    }

    /// deferred 队列（会话切换期间积压）的 panic 同样不得逃逸，且队列排空。
    #[test]
    fn deferred_ringing_panic_is_contained_and_queue_drains() {
        let liveness = Arc::new(WorkerLiveness::new());
        let (mut lp, _cmd_tx) = test_loop(liveness.clone());

        // 构造积压：直接往 deferred 队列塞命令，模拟已 ack 的 Ringing 命令
        // 在会话切换期间堆积。用非中断命令（SessionShutdown 会置 pending，
        // 令 drain 循环提前停下，不是本用例的观察对象）。
        lp.deferred_ringing
            .push_back(cmd("ok", qaqh_domain::ControlCommand::SkillsReload));
        lp.panic_on_command_id = Some("boom".into());
        lp.deferred_ringing
            .push_back(cmd("boom", qaqh_domain::ControlCommand::SkillsReload));

        // 直接驱动 drain 路径（run 会阻塞在 recv_timeout）。
        lp.drain_pending();

        assert!(
            lp.deferred_ringing.is_empty(),
            "deferred 队列必须被排空（panic 不得中断 drain）"
        );
        assert!(liveness.unloadable(), "派发结束后 liveness 必须回落");
    }

    /// 兜底：`safe_dispatch` 自身的 busy 窗口断言（防止 future 回归把它挪走）。
    #[test]
    fn safe_dispatch_marks_busy_during_and_clears_after() {
        let liveness = Arc::new(WorkerLiveness::new());
        let (mut lp, _cmd_tx) = test_loop(liveness.clone());

        let snapshot = Arc::new(AtomicUsize::new(usize::MAX));
        let snap = snapshot.clone();
        let live = liveness.clone();
        lp.safe_dispatch(move |_this| {
            snap.store(live.unloadable() as usize, Ordering::SeqCst);
        });

        assert_eq!(
            snapshot.load(Ordering::SeqCst),
            0,
            "safe_dispatch 内部不可 unload（busy 置位）"
        );
        assert!(liveness.unloadable(), "safe_dispatch 结束后恢复可 unload");
    }

    /// panic payload 必须被 safe_dispatch 消化（不二次 panic / 不留残余 busy）。
    #[test]
    fn safe_dispatch_contains_panic_and_restores_liveness() {
        let liveness = Arc::new(WorkerLiveness::new());
        let (mut lp, _cmd_tx) = test_loop(liveness.clone());

        lp.safe_dispatch(|_this| panic!("simulated engine panic"));

        assert!(liveness.unloadable(), "panic 恢复后必须可 unload");
        assert_eq!(lp.phase, super::LoopPhase::Idle, "panic 后相位复位 Idle");
    }
}
