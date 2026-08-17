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
//! │     compact, misc                                │
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

use super::engine_compact::{CompactEngine, CompactMeta};
use super::engine_goal::GoalEngine;
use super::engine_input::InputEngine;
use super::engine_misc::MiscEngine;
use super::engine_session::SessionEngine;
use super::engine_tool::PermissionDisposition;
use super::paced_emitter::PacedEmitter;
use super::types::*;
use crate::state::agent::AgentState;

pub fn ringing_command_is_interrupt(
    env: &qaqh_ringing::RingingWorkerCommandEnvelope,
) -> bool {
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
fn parse_subagent_status_tag(text: &str) -> Option<(String, String)> {
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

// ═══════════════════════════════════════════════════════
// Loop — the dispatcher
// ═══════════════════════════════════════════════════════

/// Pre-created in-process worker channel ends.
///
/// `Loop::new_ipc` uses these internally; an in-process host (daemon actor,
/// tests) can create them before the loop, keep the producer/consumer sides,
/// and construct the loop later with [`Loop::from_channels`].
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
    /// Miscellaneous: undo, dashboard, mode.
    misc: MiscEngine,
    /// Unified context-ingestion pipeline — the single door into the message
    /// store for every message source (user/model/tool/skills/subagent/goal).
    /// Registered with the built-in sources at construction; new sources
    /// (ACP/MCP loops) register here without touching the dispatcher.
    flow: qaqh_message::ContextFlow,
    /// Pending compact result (set when compact is running in background).
    pending_compact_rx: Option<mpsc::Receiver<CompactMeta>>,
    pending_compact_id: Option<String>,
    pending_compact_causation: Option<String>,

    /// Direct output emitter. The renderer performs frame-level coalescing.
    paced_emitter: PacedEmitter,
}

impl Loop {
    /// Create a Loop backed by real stdin/stdout via background I/O threads.
    ///
    /// Spawns:
    /// - **Reader thread**: reads JSON-LP from `input`, dispatches Ringing commands
    ///   to `cmd_rx`. Sets CancelToken on interrupt-type commands (Cancel,
    ///   ResumeSession, NewSession, Shutdown).
    /// - **Writer thread**: receives Ringing events from `event_tx`, writes
    ///   JSON-LP to `output`. Flushes every 2ms. Sets `writer_dead` on exit.
    ///
    /// Both threads use `catch_unwind` to log panics rather than silently dying.
    pub fn new_ipc(
        agent: AgentState,
        input: impl BufRead + Send + 'static,
        output: impl Write + Send + 'static,
    ) -> Self {
        let channels = LoopChannels::new();
        let cancel_for_reader = channels.cancel.clone();
        let cmd_tx = channels.cmd_tx.clone();
        let writer_dead_for_thread = channels.writer_dead.clone();

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
                        Ok(Some(super::wire::WorkerCommandFrame::Ringing(env))) => {
                            let causation = env.command_id.clone();
                            if ringing_command_is_interrupt(&env) {
                                cancel_for_reader.set();
                                qaqh_workspace::set_cancel(true);
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
                eprintln!("[qaqh AGENT] reader thread panicked: {}", msg);
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
                    match channels.event_rx.recv() {
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
                eprintln!("[qaqh AGENT] writer thread panicked: {}", msg);
            }
            writer_dead_for_thread.store(true, Ordering::SeqCst);
            log::info!("[AGENT] writer thread exiting");
        });

        Self::from_channels(
            agent,
            channels.cmd_rx,
            channels.event_tx,
            channels.cancel,
            channels.writer_dead,
        )
    }

    /// Construct a Loop from pre-created in-process channel ends.
    ///
    /// The caller owns the producer side (`cmd_tx`) and consumer side
    /// (`event_rx`) from [`LoopChannels`]. This is the same loop as
    /// `new_ipc`; only the process boundary is replaced by channels.
    pub fn from_channels(
        agent: AgentState,
        cmd_rx: mpsc::Receiver<WorkerCommand>,
        event_tx: mpsc::SyncSender<WriterEvent>,
        cancel: CancelToken,
        writer_dead: Arc<AtomicBool>,
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
        let paced_emitter =
            PacedEmitter::new(seed, event_tx.clone(), writer_dead.clone(), cancel.arc());

        let mut flow = qaqh_message::ContextFlow::new();
        qaqh_message::builtin::register_all(&mut flow);

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
            flow,
            pending_compact_rx: None,
            pending_compact_id: None,
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
            eprintln!("[qaqh AGENT] engine panic during dispatch: {msg}");

            self.reset_all_engines();
            self.phase = LoopPhase::Idle;
            self.cancel.clear();
            qaqh_workspace::set_cancel(false);

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
        self.session.tool.clear_pending();
        self.session.stats = StatsCollector::new();

        // Session-agnostic engines：无状态（M3 后无 Engine trait reset）
        self.goal = GoalEngine::new();
        self.misc.reset();
        self.finish_pending_compact(qaqh_domain::CompactStatus::Cancelled);

        self.pending.clear();
    }

    /// Close any suspended transaction before replacing the active session.
    /// An unanswered ask/tool round must never be persisted into, or resumed
    /// against, the next session.
    fn prepare_session_switch(&mut self) {
        self.session.agent.reset_compaction_coordination();
        if self.session.turn.is_suspended() {
            self.session.agent.msg.remove_last_step_if_incomplete();
        }
        self.session.flush();
        self.reset_all_engines();
        self.cancel.clear();
        qaqh_workspace::set_cancel(false);
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

    /// 见缝插针注入消费者（回合 lap 边界调用）：从 cmd_rx 吸收排队的
    /// `as_system` 注入（子代理报告等），经 ContextFlow 挂起 pending，由
    /// 调用方随后 drain_turn_boundary 在 lap 边界落盘 trailing。
    ///
    /// 时机保证（PLAN-FIX-INJECTION-CACHE ②）：只在工具回合完成后的
    /// lap 边界被调用——此时本轮 tool_call 与其 tool_result 均已提交
    /// （工具执行是同步阻塞的），注入取号必然排在本轮全部结果之后，绝不
    /// 夹在 assistant(toolcall) 与其 tool_result 之间。注入以 user + name=
    /// subagent 角色落盘（chat/responses 两协议的对话流主体，可见性保证）。
    pub fn drain_pending_injections(&mut self) {
        use qaqh_domain::ConversationCommand;
        use qaqh_ringing::RingingCommand;
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            let super::wire::WorkerCommandFrame::Ringing(ref env) = cmd.frame;
            match &env.command {
                // ── 见缝插针注入：as_system 消息（子代理报告等）──────────
                // 统一收敛到 ContextFlow：submit 挂起 pending，lap 边界由
                // drain_turn_boundary 落盘（trailing developer 消息）——
                // 注入按 role 注入（developer），与 skills envelope 同通道、
                // 同生命周期语义（Preserved）。不再有 turn/trailing 双路径。
                RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
                    text,
                    as_system: true,
                    ..
                }) => {
                    // 子代理注入改走 user 角色（见缝插针 trailing 语义不变）：
                    // chat 协议下 developer 会降级为中段 system 导致感知弱化，
                    // user 是 chat/responses 两种协议的对话流主体，可见性有保证；
                    // name 标记供回放/前端识别（不当作真实用户输入）。
                    let msg = qaqh_types::Message {
                        msg_id: None,
                        role: qaqh_types::Message::ROLE_USER.into(),
                        name: Some("subagent".into()),
                        content: vec![qaqh_types::ContentBlock::text(text)],
                    };
                    match self.flow.submit(
                        qaqh_message::builtin::SUBAGENT,
                        msg,
                        Some(env.command_id.clone()),
                    ) {
                        Ok(()) => {
                            log::info!(
                                "[INJECT] subagent report queued via ContextFlow (seed={}, text_len={}, pending={})",
                                self.session.agent.session.seed,
                                text.len(),
                                self.flow.pending_len()
                            );
                            // 吸收路径无独立注入回合 → 前端 tracker 收不到
                            // TurnStarted 标签，终态收敛缺失（幽灵 Lost）。
                            // 补发轻量控制面事件：不进回合状态机、不进模型
                            // 上下文，仅作为前端子代理状态区的收敛信号。
                            if let Some((name, state)) = parse_subagent_status_tag(text) {
                                log::info!(
                                    "[INJECT] subagent status emitted (name={name}, state={state}) for absorbed injection"
                                );
                                self.paced_emitter.emit_domain(
                                    qaqh_domain::DomainEvent::Control(
                                        qaqh_domain::ControlEvent::SubagentStatus {
                                            seed: self.session.agent.session.seed.clone(),
                                            name,
                                            state,
                                        },
                                    ),
                                );
                            }
                        }
                        Err(e) => {
                            log::error!("[INJECT] ContextFlow submit failed: {e}");
                        }
                    }
                }
                // ── 其它命令（用户消息/非注入）：绝不丢弃 ────────────────
                // daemon 侧已 ACK（accepted），静默丢弃会让调用方永久悬挂。
                // 放入 deferred_ringing，主循环 idle 时按 FIFO 派发（复用
                // 既有 session 切换保留队列，语义一致）。
                _ => {
                    self.deferred_ringing.push_back(cmd);
                }
            }
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
            if self.pending_compact_rx.is_none() && !self.ready_emitted {
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
                    self.finish_pending_compact(qaqh_domain::CompactStatus::Cancelled);
                    log::error!("[AGENT] cmd_rx closed — stdin pipe broken. Exiting.");
                    eprintln!("[qaqh AGENT] stdin pipe broken — exiting.");
                    break;
                }
            };

            // ── Dispatch with panic safety ──
            let causation = cmd.causation.clone();
            self.safe_dispatch(|this| {
                let _scope = this.paced_emitter.enter_causation(causation.as_deref());
                let super::wire::WorkerCommandFrame::Ringing(env) = cmd.frame;
                this.dispatch_ringing_one(env);
            });
        }

        // ── Cleanup ──
        qaqh_workspace::runtime::shutdown_tools();
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
            self.session_eng
                .create_with_seed(&mut self.session.agent, &self.cancel);
            self.sync_emitter_seed();
            let seed = self.session.agent.session.seed.clone();
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
            let super::wire::WorkerCommandFrame::Ringing(env) = cmd.frame;
            if self.pending.is_empty() {
                let causation = cmd.causation.clone();
                let _scope = self.paced_emitter.enter_causation(causation.as_deref());
                self.dispatch_ringing_one(env);
            } else {
                self.deferred_ringing
                    .push_back(super::types::WorkerCommand {
                        frame: super::wire::WorkerCommandFrame::Ringing(env),
                        causation: cmd.causation,
                    });
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
                // legacy SessionRestored 已退役：Ringing 恢复由 daemon bootstrap 快照承担。
            }
        }
        if self.pending.new_session {
            self.pending.new_session = false;
            self.prepare_session_switch();
            self.session_eng
                .create(&mut self.session.agent, &self.cancel);
            self.sync_emitter_seed();
            self.misc
                .emit_dashboard(&self.session.agent, &self.paced_emitter);
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
            let super::wire::WorkerCommandFrame::Ringing(env) = cmd.frame;
            let _scope = self.paced_emitter.enter_causation(cmd.causation.as_deref());
            self.dispatch_ringing_one(env);
        }
    }

    fn finish_pending_compact(&mut self, status: qaqh_domain::CompactStatus) {
        self.session.agent.finish_manual_compact();
        self.pending_compact_rx = None;
        let Some(compact_id) = self.pending_compact_id.take() else {
            self.pending_compact_causation = None;
            return;
        };
        let causation = self.pending_compact_causation.take();
        let _scope = self.paced_emitter.enter_causation(causation.as_deref());
        self.paced_emitter
            .emit_domain(qaqh_domain::DomainEvent::Conversation(
                qaqh_domain::ConversationEvent::CompactFinished {
                    compact_id,
                    status,
                    summary_chars: Some(0),
                    turns_compacted: Some(0),
                    turns_removed: Some(0),
                },
            ));
    }

    /// Check if a background compact has completed and apply the result.
    fn check_pending_compact(&mut self) {
        if let Some(ref rx) = self.pending_compact_rx {
            match rx.try_recv() {
                Ok(meta) => {
                    self.session.agent.finish_manual_compact();
                    self.pending_compact_rx = None;
                    let compact_id = self.pending_compact_id.take();
                    let causation = self.pending_compact_causation.take();
                    let _scope = self.paced_emitter.enter_causation(causation.as_deref());
                    if compact_id.as_deref() != Some(meta.compact_id.as_str()) {
                        log::warn!(
                            "[COMPACT] pending/result id mismatch: pending={compact_id:?}, result={}",
                            meta.compact_id
                        );
                    }
                    let mut ctx = RingContext {
                        agent: &mut self.session.agent,
                        emitter: &self.paced_emitter,
                        cancel: &self.cancel,
                        phase: &mut self.phase,
                        pending: &mut self.pending,
                        writer_dead: &self.writer_dead,
                        stats: &mut self.session.stats,
                        flow: &mut self.flow,

                    };
                    self.compact.apply_result(&mut ctx, &meta);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Worker thread died without sending result.
                    // Clear pending state and report error so frontend
                    // doesn't stay stuck at the "compacting" animation.
                    log::error!("[COMPACT] worker thread disconnected without result");
                    self.pending_compact_rx = None;
                    self.finish_pending_compact(qaqh_domain::CompactStatus::Failed);
                    // 失败同时由 OperationFailed 暴露具体原因。
                    self.emit_operation_failed(
                        "compact-worker-crashed",
                        qaqh_domain::ErrorScope::Conversation,
                        "compact_worker_crashed",
                        "Context compaction failed: worker thread crashed.",
                    );
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // Still running — check again next loop iteration.
                }
            }
        }
    }

    fn emit_ringing_skills_status(&mut self) {
        let workspace = qaqh_workspace::CURRENT_WORKSPACE
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let status = self.session.agent.build_skills_status(&workspace);
        self.paced_emitter
            .emit_domain(qaqh_domain::DomainEvent::Control(
                qaqh_domain::ControlEvent::SkillsUpdated {
                    available: status
                        .available
                        .iter()
                        .map(|s| qaqh_domain::SkillInfo {
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
                        .map(|item| qaqh_domain::SkillRuntimeInfo {
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
        if self.pending_compact_rx.is_some() || self.session.agent.manual_compact_running() {
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
                flow: &mut self.flow,

            };
            self.compact.build_prompt_and_meta(&mut ctx)
        };
        if let Some((prompt, kept, head, provider, compact_id)) = compact {
            self.session.agent.begin_manual_compact();
            let context_revision = self.session.agent.msg.context_revision();
            let pending_compact_id = compact_id.clone();
            let (tx, rx) = mpsc::channel();
            let event_tx = self.event_tx.clone();
            let compact_seed = self.session.agent.session.seed.clone();
            let worker_causation = causation.clone();
            match std::thread::Builder::new()
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
                            context_revision,
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
                            context_revision,
                            error: Some(format!(
                                "Compact worker panicked: {}",
                                Self::panic_msg_from_err(error)
                            )),
                        },
                    };
                    let _ = tx.send(meta);
                }) {
                Ok(_) => {
                    self.pending_compact_rx = Some(rx);
                    self.pending_compact_id = Some(pending_compact_id);
                    self.pending_compact_causation = causation;
                }
                Err(error) => {
                    log::error!("[COMPACT] failed to spawn worker: {error}");
                    self.session.agent.finish_manual_compact();
                    let _scope = self.paced_emitter.enter_causation(causation.as_deref());
                    self.paced_emitter
                        .emit_domain(qaqh_domain::DomainEvent::Control(
                            qaqh_domain::ControlEvent::OperationFailed {
                                occurrence_id: pending_compact_id.clone(),
                                scope: qaqh_domain::ErrorScope::Conversation,
                                error: qaqh_domain::DomainError {
                                    error_id: pending_compact_id.clone(),
                                    code: "compact_failed".into(),
                                    message: "Context compaction could not start.".into(),
                                    retryable: true,
                                    dedupe_key: Some("compact_failed".into()),
                                },
                                operation_id: Some(pending_compact_id.clone()),
                            },
                        ));
                    self.paced_emitter
                        .emit_domain(qaqh_domain::DomainEvent::Conversation(
                            qaqh_domain::ConversationEvent::CompactFinished {
                                compact_id: pending_compact_id,
                                status: qaqh_domain::CompactStatus::Failed,
                                summary_chars: Some(0),
                                turns_compacted: Some(0),
                                turns_removed: Some(0),
                            },
                        ));
                }
            }
        } else {
            self.paced_emitter
                .emit_domain(qaqh_domain::DomainEvent::Conversation(
                    qaqh_domain::ConversationEvent::CompactFinished {
                        compact_id: format!("compact-skipped-{}", self.session.agent.session.seed),
                        status: qaqh_domain::CompactStatus::Skipped,
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
    fn emit_operation_completed(&self, command_id: &str, scope: qaqh_domain::ErrorScope) {
        self.paced_emitter
            .emit_domain(qaqh_domain::DomainEvent::Control(
                qaqh_domain::ControlEvent::OperationCompleted {
                    occurrence_id: command_id.to_string(),
                    scope,
                    operation_id: Some(command_id.to_string()),
                },
            ));
    }

    fn emit_operation_failed(
        &self,
        command_id: &str,
        scope: qaqh_domain::ErrorScope,
        code: &str,
        message: &str,
    ) {
        self.paced_emitter
            .emit_domain(qaqh_domain::DomainEvent::Control(
                qaqh_domain::ControlEvent::OperationFailed {
                    occurrence_id: command_id.to_string(),
                    scope,
                    error: qaqh_domain::DomainError {
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

    fn dispatch_ringing_one(&mut self, env: qaqh_ringing::RingingWorkerCommandEnvelope) {
        use qaqh_domain::{ControlCommand, ConversationCommand, DomainEvent, ToolCommand};
        use qaqh_ringing::RingingCommand;

        self.ready_emitted = false;
        let expected_revision = env.expected_revision.unwrap_or_default();
        let command_id = env.command_id.clone();

        // compact 进行中收到 as_system 注入（子代理报告等）：不拒绝、不丢弃——
        // 延迟到 compact 完成后 dispatch（与其它 deferred 命令同队列）。否则子
        // 代理结果在紧凑窗口到达会被下方 ConversationSendMessage 分支以
        // OperationFailed(compact_in_progress) 拒绝，而 collector 只看 daemon
        // 的 Accepted ACK、无从感知该拒绝 → 结果真实丢失（不回传主代理）。
        if self.session.agent.manual_compact_running()
            && matches!(
                &env.command,
                RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
                    as_system: true,
                    ..
                })
            )
        {
            log::info!("[AGENT] deferring subagent injection while compact running");
            self.deferred_ringing.push_back(super::types::WorkerCommand {
                frame: super::wire::WorkerCommandFrame::Ringing(env),
                causation: Some(command_id),
            });
            return;
        }

        match env.command {
            RingingCommand::Control(command) => match command {
                ControlCommand::SessionCreate {
                    close_current,
                    cwd: _,
                    tool_mode,
                    custom_tools,
                } => {
                    // 可选工具模式预置在 create 前应用，保证新会话的
                    // system prompt 与工具集首轮就位（daemon 路径已先落盘）。
                    if let Some(tool_mode) = tool_mode {
                        self.session
                            .agent
                            .apply_tool_mode(&tool_mode, &custom_tools);
                    }
                    if close_current {
                        self.prepare_session_switch();
                    } else {
                        self.finish_pending_compact(qaqh_domain::CompactStatus::Cancelled);
                        self.session.agent.reset_compaction_coordination();
                    }
                    self.session_eng
                        .create(&mut self.session.agent, &self.cancel);
                    self.sync_emitter_seed();
                    self.paced_emitter.emit_domain(DomainEvent::Control(
                        qaqh_domain::ControlEvent::SessionStateChanged {
                            seed: self.session.agent.session.seed.clone(),
                            state: qaqh_domain::SessionState::Created,
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
                            qaqh_domain::ControlEvent::SessionStateChanged {
                                seed,
                                state: qaqh_domain::SessionState::Resumed,
                            },
                        ));
                    } else {
                        self.emit_operation_failed(
                            &command_id,
                            qaqh_domain::ErrorScope::Control,
                            "session_resume_failed",
                            "session could not be resumed",
                        );
                    }
                }
                ControlCommand::SessionShutdown => {
                    self.pending.shutdown = true;
                    self.emit_operation_completed(&command_id, qaqh_domain::ErrorScope::Control);
                }
                ControlCommand::AgentReloadConfig => {
                    self.session_eng
                        .reload_config(&mut self.session.agent, &self.cancel);
                    self.emit_operation_completed(&command_id, qaqh_domain::ErrorScope::Control);
                }
                ControlCommand::SetToolMode {
                    tool_mode,
                    custom_tools,
                } => {
                    self.session
                        .agent
                        .apply_tool_mode(&tool_mode, &custom_tools);
                    self.emit_operation_completed(&command_id, qaqh_domain::ErrorScope::Control);
                }
                ControlCommand::SkillsReload => self.emit_ringing_skills_status(),
                ControlCommand::SkillsActivate { name } => {
                    let _ = self.session.agent.skills.queue_request(&name, "user");
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
                            .emit_domain(qaqh_domain::DomainEvent::Control(
                                qaqh_domain::ControlEvent::OperationFailed {
                                    occurrence_id: operation_id.clone(),
                                    scope: qaqh_domain::ErrorScope::Control,
                                    error: qaqh_domain::DomainError {
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
                ControlCommand::SessionArchive { .. } => {
                    log::debug!("SessionArchive is handled by daemon registry");
                }
                ControlCommand::SessionUnarchive { .. } => {
                    log::debug!("SessionUnarchive is handled by daemon registry");
                }
                ControlCommand::SessionDelete { .. } => {
                    log::debug!("SessionDelete is handled by daemon registry");
                }
                ControlCommand::InteractionAskRespond {
                    interaction_id,
                    answers,
                } => {
                    // answers 已是 domain AskAnswer（Ringing 命令直接携带）。
                    let mut ctx = RingContext {
                        agent: &mut self.session.agent,
                        emitter: &self.paced_emitter,
                        cancel: &self.cancel,
                        phase: &mut self.phase,
                        pending: &mut self.pending,
                        writer_dead: &self.writer_dead,
                        stats: &mut self.session.stats,
                        flow: &mut self.flow,

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
                        flow: &mut self.flow,

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
                        flow: &mut self.flow,

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
                    as_system,
                } => {
                    if self.session.agent.manual_compact_running() {
                        self.emit_operation_failed(
                            &command_id,
                            qaqh_domain::ErrorScope::Conversation,
                            "compact_in_progress",
                            "Context compaction is running; wait for it to finish before sending a new message",
                        );
                        return;
                    }
                    // images 已是 domain ImageBlock（Ringing 命令直接携带）。
                    // as_system=true 时走系统注入通道（跳过用户输入专属处理）。
                    let mut ctx = RingContext {
                        agent: &mut self.session.agent,
                        emitter: &self.paced_emitter,
                        cancel: &self.cancel,
                        phase: &mut self.phase,
                        pending: &mut self.pending,
                        writer_dead: &self.writer_dead,
                        stats: &mut self.session.stats,
                        flow: &mut self.flow,

                    };
                    let outcome = if as_system {
                        self.input
                            .handle_system_input(&mut ctx, &text, Some(command_id.as_str()))
                    } else {
                        self.input
                            .handle_user_input(&mut ctx, qaqh_message::builtin::USER, &text, images)
                    };
                    drop(ctx);
                    self.apply_outcome(outcome);
                }
                ConversationCommand::ConversationCancel { turn_id } => {
                    self.cancel.set();
                    qaqh_workspace::set_cancel(true);
                    self.reset_all_engines();
                    self.paced_emitter.emit_domain(DomainEvent::Conversation(
                        qaqh_domain::ConversationEvent::ConversationCancelled { turn_id },
                    ));
                }
                ConversationCommand::ConversationUndoTurn { turn_id } => {
                    // 与 legacy UndoTurn 语义对齐：活动回合被挂起（ask/权限/plan
                    // 未决）时拒绝 undo，避免跨引擎状态不一致。
                    if self
                        .session
                        .turn
                        .suspended_turn_id()
                        .is_some_and(|active_turn_id| active_turn_id != turn_id)
                    {
                        self.emit_operation_failed(
                            &command_id,
                            qaqh_domain::ErrorScope::Conversation,
                            "undo_conflict",
                            &format!("Cannot undo {turn_id}: a different active turn is suspended"),
                        );
                        return;
                    }
                    self.session.turn.reset();
                    self.session.tool.clear_pending();
                    self.misc.handle_undo(&mut self.session.agent, &turn_id);
                    self.emit_operation_completed(
                        &command_id,
                        qaqh_domain::ErrorScope::Conversation,
                    );
                }
                ConversationCommand::ConversationSetMode { mode } => {
                    let mode = match mode {
                        qaqh_domain::ConversationMode::Plan => "plan",
                        qaqh_domain::ConversationMode::Code => "code",
                    };
                    self.misc.set_mode(&mut self.session.agent, mode);
                    self.emit_operation_completed(
                        &command_id,
                        qaqh_domain::ErrorScope::Conversation,
                    );
                }
                ConversationCommand::ConversationCompact { .. } => {
                    let outcome = self.start_compact(Some(command_id));
                    self.apply_outcome(outcome);
                }
                ConversationCommand::ConversationLoadMore { .. } => {
                    self.emit_operation_failed(
                        &command_id,
                        qaqh_domain::ErrorScope::Conversation,
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
                        flow: &mut self.flow,

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
                        flow: &mut self.flow,

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
                                qaqh_domain::ErrorScope::Tool,
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

    // ═══════════════════════════════════════════════════
    // Outcome handler — the Ring's decision point
    // ═══════════════════════════════════════════════════

    /// Apply the outcome returned by an engine.
    ///
    /// This is the central decision point of the Ringing V1 architecture.
    /// Each Outcome variant maps to a specific Loop action:
    ///
    /// - `TurnComplete` → flush, emit TurnEnd + Done, return to Idle
    /// - `ContinueTurn` → re-enter TurnEngine for another gate lap (recursive)
    /// - `YieldToUser` → do nothing, wait for PermissionResponse or UserInput
    /// - `Handled` / `Error` / `Shutdown` → straightforward
    fn apply_outcome(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::TurnComplete { turn_id, usage } => {
                self.session.agent.skills.complete_user_turn();
                // Persist session state
                self.session.flush();
                self.paced_emitter
                    .emit_domain(qaqh_domain::DomainEvent::Conversation(
                        qaqh_domain::ConversationEvent::TurnCompleted {
                            turn_id,
                            stop_reason: None,
                            usage,
                        },
                    ));



                // Goal mode auto-advance: if the LLM completed a step
                // (via todo(action=set, status=completed)), inject the next step.
                if let Ok(store) = qaqh_workspace::todo::load_todo() {
                    if store.mode == qaqh_workspace::todo::TodoMode::Goal {
                        if let Some(ref current_id) = store.current_id {
                            if let Some(item) = store.items.iter().find(|i| &i.id == current_id) {
                                if item.status == qaqh_workspace::todo::TodoStatus::InProgress {
                                    let prompt = format!(
                                        "[自动执行计划 / 目标模式]\n\n\
                                         T{}: {}\n{}\n\n\
                                         完成此步骤后，调用 todo(action=\"set\", id=\"{}\", status=\"completed\", evidence=\"...\").",
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
                                        flow: &mut self.flow,
                                    };
                                    let next_outcome = self.input.handle_user_input(
                                        &mut ctx,
                                        qaqh_message::builtin::GOAL,
                                        &prompt,
                                        vec![],
                                    );
                                    drop(ctx);
                                    self.apply_outcome(next_outcome);
                                    return;
                                }
                            }
                        }
                    }
                }

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
                self.paced_emitter
                    .emit_domain(qaqh_domain::DomainEvent::Conversation(
                        qaqh_domain::ConversationEvent::TurnCompleted {
                            turn_id,
                            stop_reason: Some("cancelled".into()),
                            usage,
                        },
                    ));
                self.phase = LoopPhase::Idle;
            }
            Outcome::TurnFailed {
                turn_id,
                usage: _,
                message,
            } => {
                self.session.agent.skills.abort_user_turn();
                self.session.flush();
                self.paced_emitter
                    .emit_domain(qaqh_domain::DomainEvent::Conversation(
                        qaqh_domain::ConversationEvent::TurnFailed {
                            turn_id,
                            error: qaqh_domain::DomainError {
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
                self.phase = LoopPhase::Idle;
            }
            Outcome::ContinueTurn {
                turn_id,
                round_num,
                usage,
            } => {
                // 见缝插针注入：工具调用回合结束 → 下一轮 gate 前，消费
                // cmd_rx 中排队的 as_system 注入（子代理报告等）——submit 进
                // ContextFlow pending，lap 边界统一落盘（trailing developer），
                // 使下一轮 LLM 请求立即可见。非注入命令移入 deferred 队列兜底。
                self.drain_pending_injections();
                {
                    let model = self.session.agent.config.model.clone();
                    let effort = self.session.agent.config.reasoning_effort.clone();
                    let drained = self
                        .flow
                        .drain_turn_boundary(&mut self.session.agent.msg, &model, &effort)
                        .0;
                    if drained > 0 {
                        log::info!(
                            "[INJECT] lap boundary drained {drained} injection(s) via ContextFlow"
                        );
                        // 注入落盘到 messages.jsonl 即成 history；注入日志已退役
                        // （PLAN B1），无需 committed 标记。
                    }
                }
                // Another lap: re-enter TurnEngine.
                let mut ctx = RingContext {
                    agent: &mut self.session.agent,
                    emitter: &self.paced_emitter,
                    cancel: &self.cancel,
                    phase: &mut self.phase,
                    pending: &mut self.pending,
                    writer_dead: &self.writer_dead,
                    stats: &mut self.session.stats,
                    flow: &mut self.flow,
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
                self.paced_emitter
                    .emit_domain(qaqh_domain::DomainEvent::Control(
                        qaqh_domain::ControlEvent::OperationFailed {
                            occurrence_id: format!(
                                "occ-outcome-{}",
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_millis())
                                    .unwrap_or(0),
                            ),
                            scope: qaqh_domain::ErrorScope::System,
                            error: qaqh_domain::DomainError {
                                error_id: format!(
                                    "outcome-error-{}",
                                    std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_millis())
                                        .unwrap_or(0),
                                ),
                                code: "outcome_error".into(),
                                message: msg,
                                retryable: false,
                                dedupe_key: None,
                            },
                            operation_id: None,
                        },
                    ));
                self.phase = LoopPhase::Idle;
            }
            Outcome::Shutdown => {
                self.pending.shutdown = true;
            }
        }
    }
}
