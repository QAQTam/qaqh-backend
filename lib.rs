//! deepx-msglp: message-loop driver for the agent child process.
//!
//! The [`Loop`] reads [`Ui2Agent`] frames via a channel fed by a background
//! I/O thread, and writes [`Agent2Ui`] frames to a channel consumed by a
//! background writer thread. It drives the full user-input → gate → tools →
//! response pipeline.
//!
//! Responsibilities:
//!   1. Ingest [`Ui2Agent`] frames via channel (background I/O thread)
//!   2. Drive `UserInput` through gate → message → tools
//!   3. Propagate `Cancel` via [`CancelToken`] / `Arc<AtomicBool>`
//!   4. Emit all [`Agent2Ui`] responses via channel
//!   5. Handle session lifecycle (CreateSession, ResumeSession, Shutdown)
//!   6. Check for interrupt commands between rounds (Cancel, session switch)

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

pub mod agent;
use agent::AgentState;
mod lifecycle;
mod dashboard;
pub mod logger;
use dashboard::{build_documents, build_recent_edits, build_tasks};
use deepx_message::Effect;
use deepx_proto::{Agent2Ui, Ui2Agent, RoundDeltaKind};
use deepx_types::platform;

/// Number of recent turns sent on session restore for incremental loading.
const INITIAL_LOAD_COUNT: usize = 20;

// ═══════════════════════════════════════════════════════
// CancelToken — shared abort flag
// ═══════════════════════════════════════════════════════

#[derive(Clone)]
pub struct CancelToken {
    inner: Arc<AtomicBool>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self { inner: Arc::new(AtomicBool::new(false)) }
    }

    pub fn set(&self) {
        self.inner.store(true, Ordering::SeqCst);
    }

    pub fn clear(&self) {
        self.inner.store(false, Ordering::SeqCst);
    }

    pub fn is_set(&self) -> bool {
        self.inner.load(Ordering::SeqCst)
    }

    pub fn arc(&self) -> Arc<AtomicBool> {
        self.inner.clone()
    }
}

// ═══════════════════════════════════════════════════════
// LoopPhase — what's currently running
// ═══════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq)]
enum LoopPhase {
    Idle,
    GateRunning,
    ToolsRunning,
}

// ═══════════════════════════════════════════════════════
// Loop — channel-based event loop.
//
// Background I/O threads handle stdin/stdout; the main loop
// uses mpsc channels. This allows Cancel and session-switch
// commands to arrive while the loop is busy processing.
// ═══════════════════════════════════════════════════════

pub struct Loop {
    agent: AgentState,
    cmd_rx: mpsc::Receiver<Ui2Agent>,
    event_tx: mpsc::SyncSender<Agent2Ui>,
    cancel: CancelToken,
    phase: LoopPhase,
    /// Pending session switch requested while busy (seed to resume).
    pending_session: Option<String>,
    /// Pending new-session request while busy.
    pending_new_session: bool,
    /// Pending shutdown.
    pending_shutdown: bool,
    /// Pending ReloadConfig requested while busy (workspace/config change).
    pending_reload_config: bool,
    /// Accumulated code deltas (flushed on save_full/save_append).
    code_stats: Vec<deepx_proto::CodeDeltaRecord>,
    /// Set to true when the writer thread dies (stdout pipe broken).
    /// The main loop checks this and exits gracefully.
    writer_dead: Arc<AtomicBool>,
}

/// Extract file paths that a tool writes to (mutates).
/// Returns empty vec for read-only and non-file tools.
fn file_write_paths(tool_name: &str, args: &serde_json::Value) -> Vec<String> {
    if tool_name != "file" { return Vec::new(); }
    let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
    let mut paths = Vec::new();
    // All actions that modify files
    match action {
        "write" | "edit" | "edit_diff" | "delete" => {
            if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                paths.push(p.to_string());
            }
            if let Some(arr) = args.get("paths").and_then(|v| v.as_array()) {
                for v in arr { if let Some(s) = v.as_str() { paths.push(s.to_string()); } }
            }
        }
        "move" | "copy" => {
            // Both source and dest are affected; dest is the write target
            if let Some(p) = args.get("dest").and_then(|v| v.as_str()) {
                paths.push(p.to_string());
            }
            if let Some(p) = args.get("source").and_then(|v| v.as_str()) {
                paths.push(p.to_string());
            }
        }
        _ => {}
    }
    paths
}

impl Loop {
    /// Create a Loop backed by real stdin/stdout via background I/O threads.
    ///
    /// Spawns:
    /// - a reader thread that reads JSON-LP from `input` and sends to `cmd_rx`
    /// - a writer thread that receives from `event_tx` and writes JSON-LP to `output`
    ///
    /// For Cancel frames, the reader thread also sets the CancelToken directly
    /// so that an in-progress handle_user_input round exits immediately.
    pub fn new_ipc(
        agent: AgentState,
        input: impl BufRead + Send + 'static,
        output: impl Write + Send + 'static,
    ) -> Self {
        let cancel = CancelToken::new();
        let cancel_for_reader = cancel.clone();

        let (cmd_tx, cmd_rx) = mpsc::sync_channel::<Ui2Agent>(4096);
        let (event_tx, event_rx) = mpsc::sync_channel::<Agent2Ui>(4096);
        let writer_dead = Arc::new(AtomicBool::new(false));
        let writer_dead_for_thread = writer_dead.clone();

        // Reader thread: stdin → cmd_tx
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut reader = std::io::BufReader::new(input);
                loop {
                    match deepx_proto::read_frame(&mut reader) {
                        Ok(Some(frame)) => {
                            let is_interrupt = matches!(frame,
                                Ui2Agent::Cancel | Ui2Agent::ResumeSession { .. }
                                | Ui2Agent::NewSession | Ui2Agent::Shutdown
                            );
                            if is_interrupt {
                                // Set cancel token directly so busy loops see it immediately
                                cancel_for_reader.set();
                                deepx_tools::CANCEL.store(true, Ordering::SeqCst);
                            }
                            // Send through channel for the main loop to handle
                            if cmd_tx.send(frame).is_err() {
                                break; // Loop dropped
                            }
                        }
                        Ok(None) | Err(_) => {
                    log::warn!("[AGENT] reader thread: stdin EOF or read error — exiting");
                    break;
                }
                    }
                }
            }));
            if let Err(e) = result {
                let msg = if let Some(s) = e.downcast_ref::<&str>() { s.to_string() }
                    else if let Some(s) = e.downcast_ref::<String>() { s.clone() }
                    else { "unknown panic".into() };
                log::error!("[AGENT] reader thread panicked: {}", msg);
                eprintln!("[DEEPX AGENT] reader thread panicked: {}", msg);
            }
            log::info!("[AGENT] reader thread exiting");
        });

        // Writer thread: event_rx → stdout
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut writer = std::io::BufWriter::new(output);
                while let Ok(event) = event_rx.recv() {
                    match deepx_proto::write_frame(&mut writer, &event) {
                        Ok(()) => {}
                        Err(e) => {
                            log::error!("[AGENT] writer thread: write_frame error: {e}");
                            break;
                        }
                    }
                }
            }));
            if let Err(e) = result {
                let msg = if let Some(s) = e.downcast_ref::<&str>() { s.to_string() }
                    else if let Some(s) = e.downcast_ref::<String>() { s.clone() }
                    else { "unknown panic".into() };
                log::error!("[AGENT] writer thread panicked: {}", msg);
                eprintln!("[DEEPX AGENT] writer thread panicked: {}", msg);
            }
            writer_dead_for_thread.store(true, Ordering::SeqCst);
            log::info!("[AGENT] writer thread exiting");
        });

        Loop {
            agent,
            cmd_rx,
            event_tx,
            cancel,
            phase: LoopPhase::Idle,
            pending_session: None,
            pending_new_session: false,
            pending_shutdown: false,
            pending_reload_config: false,
            code_stats: Vec::new(),
            writer_dead,
        }
    }

    /// Send a critical event (blocking — must be delivered).
    fn emit(&self, event: Agent2Ui) {
        if self.writer_dead.load(Ordering::SeqCst) {
            // Writer thread already dead — this event and all future events
            // will be silently dropped. The main loop will detect this on its
            // next idle check and exit.
            return;
        }
        if self.event_tx.send(event).is_err() {
            // Receiver dropped — writer thread died.
            log::error!("[AGENT] emit failed: writer thread dead (event_tx disconnected)");
            // Don't set writer_dead here — the writer thread sets it when it exits.
            // The event_tx.send() failure already means the receiver is gone.
        }
    }

    /// Send a delta event (non-blocking — dropped if channel full).
    /// Use for streaming content that has overlapping successors (RoundDelta, ExecProgress).
    fn emit_delta(&self, event: Agent2Ui) {
        let _ = self.event_tx.try_send(event);
    }

    /// Drain all pending commands from the channel (non-blocking).
    /// Interrupt-type commands (Cancel, ResumeSession, NewSession, Shutdown)
    /// are handled immediately. Other commands are dispatched immediately
    /// UNLESS a session switch is pending — in that case, non-interrupt
    /// commands are dropped (the frontend re-sends them after Ready).
    fn drain_pending(&mut self) {
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            match cmd {
                Ui2Agent::Cancel => {
                    self.cancel.set();
                    deepx_tools::CANCEL.store(true, Ordering::SeqCst);
                    if self.phase == LoopPhase::ToolsRunning {
                        deepx_tools::bridge::cancel_current_tool();
                    }
                    self.phase = LoopPhase::Idle;
                    self.emit(Agent2Ui::Cancelled);
                }
                Ui2Agent::ResumeSession { seed } => {
                    self.cancel.set();
                    deepx_tools::CANCEL.store(true, Ordering::SeqCst);
                    self.pending_session = Some(seed);
                    self.emit(Agent2Ui::Cancelled);
                }
                Ui2Agent::NewSession => {
                    self.cancel.set();
                    deepx_tools::CANCEL.store(true, Ordering::SeqCst);
                    self.pending_new_session = true;
                    self.emit(Agent2Ui::Cancelled);
                }
                Ui2Agent::Shutdown => {
                    self.pending_shutdown = true;
                }
                // If a session switch is pending, drop non-interrupt commands
                // to prevent dispatching them to the wrong (old) session.
                // The frontend re-sends UserInput after receiving Ready.
                _other if self.pending_session.is_some() || self.pending_new_session => {
                    log::info!("[AGENT] dropping non-interrupt command during pending session switch");
                }
                // For commands that arrive while idle, dispatch immediately
                other => self.dispatch(other),
            }
        }
    }

    /// Check for interrupt commands during long-running operations.
    /// Returns true if the current operation should abort.
    fn check_interrupts(&mut self) -> bool {
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            match cmd {
                Ui2Agent::Cancel => {
                    self.cancel.set();
                    deepx_tools::CANCEL.store(true, Ordering::SeqCst);
                    if self.phase == LoopPhase::ToolsRunning {
                        deepx_tools::bridge::cancel_current_tool();
                    }
                    self.phase = LoopPhase::Idle;
                    self.emit(Agent2Ui::Cancelled);
                    return true;
                }
                Ui2Agent::ResumeSession { seed } => {
                    self.cancel.set();
                    deepx_tools::CANCEL.store(true, Ordering::SeqCst);
                    self.pending_session = Some(seed);
                    self.emit(Agent2Ui::Cancelled);
                    return true;
                }
                Ui2Agent::NewSession => {
                    self.cancel.set();
                    deepx_tools::CANCEL.store(true, Ordering::SeqCst);
                    self.pending_new_session = true;
                    self.emit(Agent2Ui::Cancelled);
                    return true;
                }
                Ui2Agent::Shutdown => {
                    self.pending_shutdown = true;
                    return true;
                }
                Ui2Agent::ReloadConfig => {
                    // Queue for processing when back to idle — do NOT interrupt
                    // the current operation (workspace/config reload is non-destructive).
                    self.pending_reload_config = true;
                }
                // Queue non-interrupt commands for later
                _ => {
                    // Silently drop non-interrupt commands during busy processing;
                    // they will be re-sent by the frontend after Ready.
                    log::info!("[AGENT] dropping non-interrupt command during busy phase");
                }
            }
        }
        false
    }

    /// Dispatch a single command (called when idle).
    fn dispatch(&mut self, frame: Ui2Agent) {
        match frame {
            Ui2Agent::UserInput { text } => { self.handle_user_input(&text); }
            Ui2Agent::Cancel => { self.handle_cancel(); }
            Ui2Agent::CreateSession => { self.handle_create_session(); }
            Ui2Agent::ResumeSession { ref seed } => { self.handle_resume_session(seed); }
            Ui2Agent::LoadMoreTurns { ref before_turn_id, count } => {
                let all_turns = build_turns_from_context(&self.agent);
                let idx = all_turns.iter().position(|t| t.turn_id == *before_turn_id);
                let end = idx.unwrap_or(all_turns.len());
                let start = end.saturating_sub(count as usize);
                let batch: Vec<_> = all_turns[start..end].to_vec();
                self.emit(Agent2Ui::MoreTurns {
                    turns: batch,
                    has_more: start > 0,
                });
            }
            Ui2Agent::NewSession => { self.handle_create_session(); }
            Ui2Agent::ReloadConfig => { self.handle_reload_config(); }
            Ui2Agent::Shutdown => {
                self.flush_meta_and_stats();
                self.emit(Agent2Ui::ShutdownAck);
                self.pending_shutdown = true;
            }
            Ui2Agent::ToolCall { id, name, action, args } => { self.handle_tool_call(&id, &name, &action, &args); }
            Ui2Agent::UndoTurn { ref turn_id } => { self.handle_undo_turn(turn_id); }
            Ui2Agent::Compact => { self.handle_compact(); }
            _ => {}
        }
    }

    pub fn run(&mut self) {
        self.agent.rebind_store();

        // Auto-init: if seed is pre-set (from --seed or --resume-seed CLI args),
        // create or resume the session immediately instead of waiting for IPC commands.
        let resume_seed = self.agent.session.resume_seed.take();
        let has_seed = !self.agent.session.seed.is_empty();

        if let Some(seed) = resume_seed {
            self.handle_resume_session(&seed);
            self.emit(Agent2Ui::Ready);
        } else if has_seed && !self.agent.session.from_resume {
            // New session with pre-set seed (from --seed)
            lifecycle::create_session_with_seed(&mut self.agent);
            self.agent.rebind_store();
            self.emit(Agent2Ui::SessionCreated {
                seed: self.agent.session.seed.clone(),
            });
            self.emit_dashboard();
            self.emit(Agent2Ui::Ready);
        } else {
            self.emit_dashboard();
            self.emit(Agent2Ui::Ready);
        }

        log::info!("[AGENT] entering main event loop, waiting for Ui2Agent...");
        loop {
            // Process any queued commands first
            self.drain_pending();

            // Handle pending session switch (set during busy period)
            if let Some(seed) = self.pending_session.take() {
                self.handle_resume_session(&seed);
                self.emit(Agent2Ui::Ready);
            }
            if self.pending_new_session {
                self.pending_new_session = false;
                self.handle_create_session();
                self.emit(Agent2Ui::Ready);
            }
            if self.pending_shutdown {
                break;
            }
            if self.pending_reload_config {
                self.pending_reload_config = false;
                self.handle_reload_config();
            }

            // Signal readiness before blocking (for Tauri refresh recovery).
            // Use emit_delta: if channel is full, drop it — Done already implies
            // readiness. Only startup/reconnect need guaranteed delivery.
            self.emit_delta(Agent2Ui::Ready);

            // Check if the writer thread has died (stdout pipe broken).
            // This catches cases where the agent is still processing commands
            // but can no longer communicate with the frontend.
            if self.writer_dead.load(Ordering::SeqCst) {
                log::error!("[AGENT] writer thread died — stdout pipe broken. Exiting main loop.");
                eprintln!("[DEEPX AGENT] writer thread died — stdout pipe broken. Exiting.");
                break;
            }

            // Block waiting for next command
            let frame: Ui2Agent = match self.cmd_rx.recv() {
                Ok(f) => {
                    log::info!("[AGENT] received Ui2Agent frame");
                    f
                }
                Err(_) => {
                    // cmd_rx closed — the reader thread exited, meaning stdin pipe broke.
                    // Log detailed exit reason for debugging agent kill issues.
                    log::error!("[AGENT] cmd_rx closed — reader thread stopped, stdin pipe broken. Exiting main loop. pending_shutdown={}", self.pending_shutdown);
                    eprintln!("[DEEPX AGENT] stdin pipe broken — exiting. pending_shutdown={}", self.pending_shutdown);
                    break;
                }
            };

            // Wrap dispatch in catch_unwind so a panic in any command handler
            // (UserInput, Cancel, etc.) doesn't silently kill the agent process.
            // The panic is logged and the main loop exits cleanly.
             let dispatch_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                 self.dispatch(frame);
             }));
             if let Err(e) = dispatch_result {
                 let msg = if let Some(s) = e.downcast_ref::<&str>() { s.to_string() }
                     else if let Some(s) = e.downcast_ref::<String>() { s.clone() }
                     else { "unknown panic".into() };
                 log::error!("[AGENT] main loop panic during dispatch: {}", msg);
                 eprintln!("[DEEPX AGENT] main loop panic during dispatch: {}", msg);
                 let _ = self.event_tx.try_send(Agent2Ui::Error {
                     message: format!("Agent main loop panicked: {}", msg),
                 });
                 break;
             }
        }

        deepx_tools::bridge::shutdown_tools();
        self.flush_meta_and_stats();
    }

    fn flush_code_stats(&mut self) {
        if self.code_stats.is_empty() { return; }
        let seed = &self.agent.session.seed;
        if seed.is_empty() { return; }
        let dir = deepx_types::platform::sessions_dir().join(seed);
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("code_stats.jsonl");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            use std::io::Write;
            for delta in self.code_stats.drain(..) {
                let line = serde_json::to_string(&delta).unwrap_or_default();
                let _ = writeln!(f, "{line}");
            }
            let _ = f.flush();
            let _ = f.sync_all();
        }
    }

    fn flush_meta_and_stats(&mut self) {
        self.agent.msg.flush_meta(&self.agent.config.model, &self.agent.config.reasoning_effort);
        self.flush_code_stats();
    }

    fn handle_cancel(&mut self) {
        self.cancel.set();
        deepx_tools::CANCEL.store(true, Ordering::SeqCst);
        match self.phase {
            LoopPhase::ToolsRunning => { deepx_tools::bridge::cancel_current_tool(); }
            _ => {}
        }
        self.phase = LoopPhase::Idle;
        self.emit(Agent2Ui::Cancelled);
    }

    fn handle_create_session(&mut self) {
        lifecycle::create_session(&mut self.agent);
        self.agent.rebind_store();
        self.emit(Agent2Ui::SessionCreated {
            seed: self.agent.session.seed.clone(),
        });
        self.emit_dashboard();
    }

    // Slice to the latest INITIAL_LOAD_COUNT turns for incremental loading.
    fn handle_resume_session(&mut self, seed: &str) {
        log::info!("[AGENT] handle_resume_session seed={seed}");
        if lifecycle::init_session(&mut self.agent, Some(seed)) {
            log::info!("[AGENT] init_session succeeded, current_seed={}", self.agent.session.seed);
            self.agent.rebind_store();
            let current_seed = self.agent.session.seed.clone();
            if current_seed == seed {
                let all_turns = build_turns_from_context(&self.agent);
                let total = all_turns.len() as u32;
                let start = total.saturating_sub(INITIAL_LOAD_COUNT as u32) as usize;
                let recent: Vec<_> = all_turns[start..].to_vec();
                let has_more = start > 0;
                log::info!("[AGENT] sending SessionRestored, turns.len={} (total={}, has_more={})", recent.len(), total, has_more);
                self.emit(Agent2Ui::SessionRestored {
                    seed: current_seed,
                    turns: recent,
                    tokens_used: 0,
                    cache_hit_pct: 0.0,
                    total_turns: total,
                    has_more,
                });
            } else {
                log::info!("[AGENT] seed changed {} -> {}, sending SessionCreated", seed, current_seed);
                self.emit(Agent2Ui::SessionCreated {
                    seed: current_seed,
                });
            }
            self.emit_dashboard();
        } else {
            log::info!("[AGENT] init_session returned false");
            self.emit(Agent2Ui::Error {
                message: format!("Failed to resume session: {seed}"),
            });
        }
    }

    fn handle_reload_config(&mut self) {
        if let Ok(cfg) = deepx_config::Config::load() {
            self.agent.config.api_key = cfg.api_key;
            self.agent.config.model = cfg.model;
            self.agent.config.base_url = cfg.base_url;
            self.agent.config.endpoint = cfg.endpoint;
            self.agent.config.provider_id = cfg.provider_id;
            self.agent.config.reasoning_effort = cfg.reasoning_effort;
            self.agent.config.max_tokens = cfg.max_tokens;
            self.agent.config.context_limit = cfg.context_limit;
            if let Some(ref key) = cfg.context7_api_key {
                if !key.is_empty() {
                    deepx_tools::bridge::set_context7_key(key);
                }
            }
            deepx_tools::bridge::load_workspace(&self.agent.session.seed);
        }
    }

    fn handle_tool_call(&mut self, id: &str, name: &str, _action: &str, args: &serde_json::Value) {
        log::info!("[AGENT] handle_tool_call: name={name} id={id}");
        let turn_id = format!("tc_{id}");
        let round_num = 0u32;

        // Pre-emit turn and round so the frontend has a target for ExecProgress
        let turn_id_for_emit = turn_id.clone();
        self.emit(Agent2Ui::TurnStart {
            turn_id: turn_id_for_emit,
            user_text: format!("tool: {name}"),
        });
        let args_display: String = args.get("command")
            .and_then(|v| v.as_str())
            .unwrap_or(name)
            .chars()
            .take(80)
            .collect();
        self.emit(Agent2Ui::RoundComplete {
            turn_id: turn_id.clone(),
            round_num,
            thinking: None,
            answer: None,
            tool_calls: vec![deepx_proto::ToolCallDef {
                id: id.to_string(),
                name: name.to_string(),
                args_display: args_display.clone(),
                args_json: args.to_string(),
            }],
            blocks: vec![deepx_proto::RoundBlock::Tool {
                card: deepx_proto::ToolCallDef {
                    id: id.to_string(),
                    name: name.to_string(),
                    args_display,
                    args_json: args.to_string(),
                },
            }],
            is_final: false,
        });

        // Use execute_tool_with_id_full with a progress channel for streaming
        let (progress_tx, progress_rx) = std::sync::mpsc::channel::<(String, String)>();
        let tool_name = name.to_string();
        let tool_id = id.to_string();
        let tool_id_for_result = tool_id.clone();
        let args_s = args.to_string();
        let handle = std::thread::Builder::new()
            .stack_size(4 * 1024 * 1024)
            .spawn(move || {
                let result = deepx_tools::bridge::execute_tool_with_id_full(&tool_name, "", &args_s, &tool_id, Some(progress_tx));
                (tool_id, result.content, result.success, result.code_delta)
            })
            .expect("failed to spawn tool thread");
        // Drain progress while tool runs
        loop {
            match progress_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok((tc_id, chunk)) => {
                    self.emit(Agent2Ui::ExecProgress {
                        tool_call_id: tc_id,
                        chunk,
                    });
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let (tid, output, success, code_delta) = handle.join().unwrap_or_else(|_| (tool_id_for_result, "[ERROR] tool thread panicked".into(), false, None));
        if let Some(ref delta) = code_delta {
            self.code_stats.push(delta.clone());
            self.emit_delta(Agent2Ui::CodeDelta {
                lines_added: delta.lines_added,
                lines_removed: delta.lines_removed,
                files_created: delta.files_created,
                files_deleted: delta.files_deleted,
                file: delta.file.clone(),
            });
        }
        self.emit(Agent2Ui::ToolResults {
            turn_id: turn_id.clone(),
            round_num,
            results: vec![deepx_proto::ToolResultDef {
                tool_call_id: tid,
                output,
                success,
                file: None,
            }],
        });
        self.emit(Agent2Ui::TurnEnd {
            turn_id: turn_id.clone(),
            stop_reason: None,
            usage: None,
        });
    }

    fn handle_undo_turn(&mut self, turn_id: &str) {
        log::info!("[AGENT] UndoTurn {turn_id} — turns before: {}", self.agent.msg.turn_count());
        if self.agent.msg.truncate_before_turn(turn_id) {
            log::info!("[AGENT] UndoTurn — truncated, turns after: {}", self.agent.msg.turn_count());
            // Full rewrite needed — the JSONL on disk still has the truncated messages.
            self.agent.msg.snapshot_full(&self.agent.config.model, &self.agent.config.reasoning_effort);
            let all_turns = build_turns_from_context(&self.agent);
            let total = all_turns.len() as u32;
            let start = total.saturating_sub(INITIAL_LOAD_COUNT as u32) as usize;
            let recent: Vec<_> = all_turns[start..].to_vec();
            let has_more = start > 0;
            self.emit(Agent2Ui::SessionRestored {
                seed: self.agent.session.seed.clone(),
                turns: recent,
                tokens_used: 0,
                cache_hit_pct: 0.0,
                total_turns: total,
                has_more,
            });
        } else {
            log::info!("[AGENT] UndoTurn — truncate_before_turn returned false");
        }
    }

    fn handle_compact(&mut self) {
        const KEEP: usize = 5;
        log::info!("[AGENT] handle_compact: {} turns", self.agent.msg.turn_count());
        if self.agent.msg.turn_count() <= KEEP {
            self.emit_delta(Agent2Ui::ToolNotice {
                message: format!("Compact skipped: need >{} turns (have {})", KEEP, self.agent.msg.turn_count()),
                level: "info".into(),
            });
            return;
        }

        let compact_count = self.agent.msg.turn_count() - KEEP;
        self.emit(Agent2Ui::CompactStart {
            turns_total: self.agent.msg.turn_count() as u32,
            turns_keeping: KEEP as u32,
        });

        let contexts: Vec<String> = {
            let all = self.agent.msg.build_context_for_gate("", &[]);
            all.iter()
                .filter(|m| m.role != "system")
                .take(compact_count * 3) // rough: ~3 msgs per turn
                .map(|m| {
                    let text: String = m.content.iter().filter_map(|b| match b {
                        deepx_types::ContentBlock::Text { text } => Some(text.clone()),
                        deepx_types::ContentBlock::ToolUse { name, input, .. } =>
                            Some(format!("[ToolCall {} args={}]", name, input)),
                        deepx_types::ContentBlock::ToolResult { content, .. } =>
                            Some(format!("[ToolResult {}]", &content[..content.len().min(300)])),
                        _ => None,
                    }).collect::<Vec<_>>().join("\n");
                    format!("[{}]: {}", m.role, &text[..text.len().min(1000)])
                })
                .collect()
        };
        if contexts.is_empty() { return; }

        let prompt = build_compact_prompt(&contexts);
        let provider = deepx_gate::ProviderConfig::openai(
            &self.agent.config.base_url, &self.agent.config.api_key,
            &self.agent.config.model, None, None, None,
            Default::default(), Default::default(), false, false,
        );
        let msgs = vec![deepx_types::Message::user(&prompt)];
        let summary = match deepx_gate::chat_sync(&provider, msgs, 2048) {
            Ok(s) if !s.trim().is_empty() => s,
            Ok(_) => {
                self.emit(Agent2Ui::Error {
                    message: "Compact failed: model returned empty response. Try again.".into(),
                });
                self.emit(Agent2Ui::CompactEnd { summary_chars: 0, turns_compacted: 0 });
                return;
            }
            Err(e) => {
                self.emit(Agent2Ui::Error { message: e });
                self.emit(Agent2Ui::CompactEnd { summary_chars: 0, turns_compacted: 0 });
                return;
            }
        };

        let chars = summary.chars().count();
        self.agent.msg.apply_compact(&summary, KEEP);
        // Full rewrite needed — compact changes system_messages, not just new messages.
        self.agent.msg.snapshot_full(&self.agent.config.model, &self.agent.config.reasoning_effort);
        self.emit(Agent2Ui::CompactEnd {
            summary_chars: chars, turns_compacted: compact_count as u32,
        });
        self.emit_delta(Agent2Ui::ToolNotice {
            message: format!("Compacted {} turns → {} chars summary", compact_count, chars),
            level: "info".into(),
        });
        self.emit_dashboard();
    }

    // ── User input handler ──

    fn handle_user_input(&mut self, text: &str) {
        if self.agent.session.seed.is_empty() {
            // Auto-create a session on first user input.
            // The frontend is responsible for ensuring this only happens
            // when the user explicitly starts a new conversation.
            log::info!("[AGENT] seed is empty — auto-creating session on first user input");
            lifecycle::create_session(&mut self.agent);
            self.agent.rebind_store();
            self.emit(Agent2Ui::SessionCreated {
                seed: self.agent.session.seed.clone(),
            });
            self.emit_dashboard();
        }

        self.cancel.clear();
        deepx_tools::CANCEL.store(false, Ordering::SeqCst);

        self.agent.msg.push_user(text);

        let turn_id = format!("t{}", self.agent.msg.turn_count());
        self.emit(Agent2Ui::TurnStart {
            turn_id: turn_id.clone(),
            user_text: text.to_string(),
        });

        let ep = deepx_config::registry::find_endpoint(&self.agent.config.provider_id, &self.agent.config.endpoint);
        let provider = deepx_gate::ProviderConfig::openai(
            &self.agent.config.base_url,
            &self.agent.config.api_key,
            &self.agent.config.model,
            ep.as_ref().and_then(|e| e.user_id_mode.clone()),
            ep.as_ref().and_then(|e| e.chat_path.clone()),
            ep.as_ref().and_then(|e| e.balance_path.clone()),
            ep.as_ref().map(|e| e.thinking_mode.clone()).unwrap_or_default(),
            ep.as_ref().map(|e| e.cache_field.clone()).unwrap_or_default(),
            ep.as_ref().map(|e| e.has_balance).unwrap_or(true),
            ep.as_ref().map(|e| e.supports_thinking).unwrap_or(true),
        );

        let mut round_num = 0u32;
        let mut last_usage: Option<deepx_types::UsageInfo> = None;

        // Delta batching: accumulate deltas and flush every ~30ms
        let mut answer_buf = String::new();
        let mut think_buf = String::new();
        let mut last_flush = std::time::Instant::now();
        const FLUSH_INTERVAL_MS: u64 = 30;
        const FLUSH_CHAR_THRESHOLD: usize = 20;

        loop {
            // ── Check for interrupt commands between rounds ──
            if self.check_interrupts() {
                self.agent.msg.remove_last_step_if_incomplete();
                self.flush_meta_and_stats();
                break;
            }

            if self.cancel.is_set() || deepx_tools::CANCEL.load(Ordering::SeqCst) {
                self.agent.msg.remove_last_step_if_incomplete();
                self.flush_meta_and_stats();
                break;
            }

            // Check for pending session switch (set by check_interrupts)
            if self.pending_session.is_some() || self.pending_new_session {
                self.agent.msg.remove_last_step_if_incomplete();
                self.flush_meta_and_stats();
                break;
            }

            let messages = self.agent.build_context();

            let tools = Some(self.agent.tool_defs.clone());
            let mut content = String::new();
            let mut reasoning = String::new();
            let mut tool_calls_raw = serde_json::Value::Null;
            let mut had_error = false;

            self.phase = LoopPhase::GateRunning;
            // Clone the Arc<AtomicBool> so the gate can check cancel in its
            // SSE read loop without borrowing self.
            let cancel_arc = self.cancel.arc();
            let result = deepx_gate::chat_stream(
                &provider,
                messages,
                tools,
                self.agent.config.max_tokens,
                Some(self.agent.config.reasoning_effort.clone()),
                Some(self.agent.session.seed.clone()),
                Some(&cancel_arc),
                &mut |event| {
                    match event {
                        deepx_gate::StreamEvent::ContentDelta(d) => {
                            if self.cancel.is_set() { return; }
                            content.push_str(&d);
                            answer_buf.push_str(&d);
                            if last_flush.elapsed().as_millis() as u64 >= FLUSH_INTERVAL_MS
                                || answer_buf.len() >= FLUSH_CHAR_THRESHOLD
                            {
                                if !think_buf.is_empty() {
                                    self.emit_delta(Agent2Ui::RoundDelta {
                                        turn_id: turn_id.clone(), round_num,
                                        kind: RoundDeltaKind::Thinking,
                                        delta: std::mem::take(&mut think_buf),
                                    });
                                }
                                if !answer_buf.is_empty() {
                                    self.emit_delta(Agent2Ui::RoundDelta {
                                        turn_id: turn_id.clone(), round_num,
                                        kind: RoundDeltaKind::Answering,
                                        delta: std::mem::take(&mut answer_buf),
                                    });
                                }
                                last_flush = std::time::Instant::now();
                            }
                        }
                        deepx_gate::StreamEvent::ReasoningDelta(r) => {
                            if self.cancel.is_set() { return; }
                            reasoning.push_str(&r);
                            think_buf.push_str(&r);
                            if last_flush.elapsed().as_millis() as u64 >= FLUSH_INTERVAL_MS
                                || think_buf.len() >= FLUSH_CHAR_THRESHOLD
                            {
                                if !think_buf.is_empty() {
                                    self.emit_delta(Agent2Ui::RoundDelta {
                                        turn_id: turn_id.clone(), round_num,
                                        kind: RoundDeltaKind::Thinking,
                                        delta: std::mem::take(&mut think_buf),
                                    });
                                }
                                if !answer_buf.is_empty() {
                                    self.emit_delta(Agent2Ui::RoundDelta {
                                        turn_id: turn_id.clone(), round_num,
                                        kind: RoundDeltaKind::Answering,
                                        delta: std::mem::take(&mut answer_buf),
                                    });
                                }
                                last_flush = std::time::Instant::now();
                            }
                        }
                        deepx_gate::StreamEvent::Done { raw_message, usage, .. } => {
                            // Flush buffered deltas before processing completion
                            if !think_buf.is_empty() {
                                self.emit_delta(Agent2Ui::RoundDelta {
                                    turn_id: turn_id.clone(), round_num,
                                    kind: RoundDeltaKind::Thinking,
                                    delta: std::mem::take(&mut think_buf),
                                });
                            }
                            if !answer_buf.is_empty() {
                                self.emit_delta(Agent2Ui::RoundDelta {
                                    turn_id: turn_id.clone(), round_num,
                                    kind: RoundDeltaKind::Answering,
                                    delta: std::mem::take(&mut answer_buf),
                                });
                            }
                            if let Some(ref u) = usage {
                                self.agent.session.tokens += u.total_tokens as u64;
                                last_usage = usage.clone();
                            }
                            content.clear();
                            reasoning.clear();
                            let mut blocks: Vec<serde_json::Value> = Vec::new();
                            for block in &raw_message.content {
                                match block {
                                    deepx_types::ContentBlock::Text { text } => content.push_str(text),
                                    deepx_types::ContentBlock::Reasoning { reasoning: r } => reasoning.push_str(r),
                                    deepx_types::ContentBlock::ToolUse { id, name, input } => {
                                        blocks.push(serde_json::json!({
                                            "id": id,
                                            "name": name,
                                            "arguments": input.to_string(),
                                        }));
                                    }
                                    _ => {}
                                }
                            }
                            if !blocks.is_empty() {
                                tool_calls_raw = serde_json::Value::Array(blocks);
                            }
                        }
                        deepx_gate::StreamEvent::ToolCallProgress { index, id, name, args_so_far } => {
                        self.emit_delta(Agent2Ui::ToolCallPreview {
                                turn_id: turn_id.clone(),
                                round_num,
                                index,
                                id,
                                name,
                                args_so_far,
                            });
                        }
                        deepx_gate::StreamEvent::Retrying { attempt, max_retries, delay_secs, error } => {
                            let msg = format!("API error, retrying ({attempt}/{max_retries}) in {delay_secs}s: {error}");
                            self.emit(Agent2Ui::Error { message: msg });
                        }
                        deepx_gate::StreamEvent::Error(msg) => {
                            self.emit(Agent2Ui::Error { message: msg });
                            had_error = true;
                        }
                        _ => {}
                    }
                },
            );

            if had_error || result.is_err() {
                self.flush_meta_and_stats();
                break;
            }

            // Cancel may have been requested during the gate phase. The gate
            // now aborts promptly (via SSE_READ_TIMEOUT), but we still need to
            // prevent processing partial content / executing tools.
            if self.cancel.is_set() || deepx_tools::CANCEL.load(Ordering::SeqCst) {
                self.agent.msg.remove_last_step_if_incomplete();
                self.flush_meta_and_stats();
                break;
            }

            let parsed = parse_tool_calls_from_response(&content, &reasoning, &tool_calls_raw, &self.agent);
            let assistant_msg = build_assistant_message(&content, &reasoning, &parsed);
            let effect = self.agent.msg.push_assistant(assistant_msg.clone());

            emit_round_complete(&self.event_tx, &turn_id, round_num, &assistant_msg, &content, &reasoning, &parsed);

            match effect {
                Effect::None => {
                    self.phase = LoopPhase::ToolsRunning;

                    // Threaded tool execution with real-time progress streaming
                    let pending = self.agent.msg.get_last_step_pending();
                    if !pending.is_empty() {
                        // ── Conflict detection: same-file writes must be serialized ──
                        // Build a map: file_path → tool indices that write to it
                        let mut file_writers: HashMap<String, Vec<usize>> = HashMap::new();
                        for (i, tool) in pending.iter().enumerate() {
                            for path in file_write_paths(&tool.name, &tool.args) {
                                file_writers.entry(path).or_default().push(i);
                            }
                        }
                        // Merge overlapping groups via union-find over tool indices
                        let mut serial_groups: Vec<Vec<usize>> = Vec::new();
                        {
                            let mut visited = vec![false; pending.len()];
                            for indices in file_writers.values() {
                                if indices.is_empty() { continue; }
                                let rep = indices[0];
                                if visited[rep] { continue; }
                                let mut group_set: HashSet<usize> = HashSet::new();
                                let mut stack: Vec<usize> = indices.clone();
                                while let Some(idx) = stack.pop() {
                                    if !group_set.insert(idx) { continue; }
                                    visited[idx] = true;
                                    for other in file_writers.values() {
                                        if other.contains(&idx) {
                                            for &oi in other {
                                                if !group_set.contains(&oi) { stack.push(oi); }
                                            }
                                        }
                                    }
                                }
                                let mut group: Vec<usize> = group_set.into_iter().collect();
                                group.sort();
                                if group.len() > 1 { serial_groups.push(group); }
                            }
                        }
                        // Tools that must run after their group's first tool
                        let mut serial_after: HashSet<usize> = HashSet::new();
                        for group in &serial_groups {
                            for &idx in &group[1..] { serial_after.insert(idx); }
                        }

                        let (progress_tx, progress_rx) = std::sync::mpsc::channel::<(String, String)>();
                        // Track (tc_id, JoinHandle) so we can identify panicked threads.
                        let mut handles: Vec<(String, std::thread::JoinHandle<(String, String, bool, Option<deepx_proto::CodeDeltaRecord>)>)> = Vec::new();
                        let mut tool_infos = Vec::new();

                        for (i, tool) in pending.iter().enumerate() {
                            if serial_after.contains(&i) { continue; } // run sequentially later
                            let tx = progress_tx.clone();
                            let name = tool.name.clone();
                            let id = tool.id.clone();
                            let args = tool.args.to_string();
                            tool_infos.push((id.clone(), name.clone()));
                            let id_for_handle = id.clone();
                            let handle = std::thread::Builder::new()
                                .stack_size(4 * 1024 * 1024)
                                .spawn(move || {
                                    let result = deepx_tools::bridge::execute_tool_with_id_full(&name, "", &args, &id, Some(tx));
                                    (id, result.content, result.success, result.code_delta)
                                })
                                .expect("failed to spawn tool thread");
                            handles.push((id_for_handle, handle));
                        }
                        drop(progress_tx); // close sender when all threads drop their clones

                        // Drain progress while tools run (with cancel check)
                        // Batch chunks per tool_call_id, emit at most every 50ms
                        // to avoid flooding the frontend with per-line re-renders.
                        log::info!("[AGENT] drain loop start");
                        let mut batches: HashMap<String, String> = HashMap::new();
                        let batch_interval = std::time::Duration::from_millis(50);
                        let cancelled = loop {
                            if self.cancel.is_set() || deepx_tools::CANCEL.load(Ordering::SeqCst) {
                                log::info!("[AGENT] drain loop cancel");
                                break true;
                            }
                            match progress_rx.recv_timeout(batch_interval) {
                                Ok((tc_id, chunk)) => {
                                    batches.entry(tc_id).or_default().push_str(&chunk);
                                    // Keep draining any additional ready chunks without blocking
                                    while let Ok((tid, c)) = progress_rx.try_recv() {
                                        batches.entry(tid).or_default().push_str(&c);
                                    }
                                    // Flush all accumulated batches
                                    for (tid, merged) in batches.drain() {
                                        log::info!("[AGENT] ExecProgress batch: {} {} chars", tid, merged.len());
                                self.emit_delta(Agent2Ui::ExecProgress {
                                            tool_call_id: tid,
                                            chunk: merged,
                                        });
                                    }
                                }
                                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                                    // No new data in 50ms — flush any pending batches
                                    if !batches.is_empty() {
                                        for (tid, merged) in batches.drain() {
                                    self.emit_delta(Agent2Ui::ExecProgress {
                                                tool_call_id: tid,
                                                chunk: merged,
                                            });
                                        }
                                    }
                                    continue;
                                }
                                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                                    log::info!("[AGENT] drain loop disconnected");
                                    // Flush final batches
                                    for (tid, merged) in batches.drain() {
                                self.emit_delta(Agent2Ui::ExecProgress {
                                            tool_call_id: tid,
                                            chunk: merged,
                                        });
                                    }
                                    break false;
                                }
                            }
                        };

                        if cancelled {
                            log::info!("[AGENT] cancelled, pushing placeholder results + background reaper");
                            let ts = chrono_local_datetime();
                            // Push placeholder results so the store doesn't get stuck
                            for (tc_id, _tool_name) in &tool_infos {
                                self.agent.msg.push_tool_result_direct(tc_id, &format!("[timeis: {ts}]\n[CANCELLED]"));
                            }
                            // Spawn a background reaper thread to join the tool
                            // threads. This avoids leaking threads (M1) while
                            // keeping the main loop responsive — tools that
                            // check CANCEL will return quickly; others run to
                            // completion in the background.
                            std::thread::spawn(move || {
                                for (_id, h) in handles {
                                    let _ = h.join();
                                }
                            });
                        } else {
                            let ts = chrono_local_datetime();
                            for (tc_id, h) in handles {
                                match h.join() {
                                    Ok((_id, content, _success, code_delta)) => {
                                        self.agent.msg.push_tool_result_direct(&tc_id, &format!("[timeis: {ts}]\n{content}"));
                                        if let Some(ref delta) = code_delta {
                                            self.code_stats.push(delta.clone());
                                            self.emit_delta(Agent2Ui::CodeDelta {
                                                lines_added: delta.lines_added,
                                                lines_removed: delta.lines_removed,
                                                files_created: delta.files_created,
                                                files_deleted: delta.files_deleted,
                                                file: delta.file.clone(),
                                            });
                                        }
                                    }
                                    Err(_) => {
                                        // Thread panicked — inject an error result
                                        // so the step's all_tools_satisfied() can
                                        // eventually return true (fixes M2).
                                        log::error!("[AGENT] tool thread panicked for {tc_id}");
                                        self.agent.msg.push_tool_result_direct(&tc_id, &format!("[timeis: {ts}]\n[ERROR] tool thread panicked"));
                                    }
                                }
                            }
                        }

                        // ── Execute serialized follow-up tools (same-file write conflicts) ──
                        if !serial_groups.is_empty() {
                            let ts = chrono_local_datetime();
                            for group in &serial_groups {
                                for &idx in &group[1..] {
                                    let tool = &pending[idx];
                                    let result = deepx_tools::bridge::execute_tool_with_id_full(
                                        &tool.name, "", &tool.args.to_string(), &tool.id, None,
                                    );
                                    self.agent.msg.push_tool_result_direct(
                                        &tool.id,
                                        &format!("[timeis: {ts}]\n{}", result.content),
                                    );
                                    if let Some(ref delta) = result.code_delta {
                                        self.code_stats.push(delta.clone());
                                        self.emit_delta(Agent2Ui::CodeDelta {
                                            lines_added: delta.lines_added,
                                            lines_removed: delta.lines_removed,
                                            files_created: delta.files_created,
                                            files_deleted: delta.files_deleted,
                                            file: delta.file.clone(),
                                        });
                                    }
                                }
                            }
                        }
                    }

                    let results = self.agent.msg.last_step_tool_results();
                    let mut tool_defs = Vec::new();
                    for (tc_id, tool_name, result_content, success) in &results {
                        tool_defs.push(deepx_proto::ToolResultDef {
                            tool_call_id: tc_id.clone(),
                            output: result_content.clone(),
                            success: *success,
                            file: None,
                        });
                        self.emit_delta(Agent2Ui::AuditRecord {
                            tool_name: tool_name.clone(),
                            result_summary: result_content.lines().next().unwrap_or("").chars().take(120).collect(),
                            success: *success,
                        });
                    }
                    if !tool_defs.is_empty() {
                        self.emit(Agent2Ui::ToolResults {
                            turn_id: turn_id.clone(),
                            round_num,
                            results: tool_defs,
                        });
                    }

                    // Refresh status panel after tool execution
                    self.emit_dashboard();

                    // Flush pending messages to disk each round so that
                    // pending_save doesn't accumulate across rounds.  Large
                    // pending_save vectors cause heavy heap pressure during
                    // serde_json::to_string in append_messages, which has been
                    // linked to intermittent 0xc0000005 crashes after 3-4
                    // tool-intensive rounds.
                    self.flush_meta_and_stats();

                    round_num += 1;
                    continue;
                }
                Effect::TurnComplete => {}
                _ => {}
            }

            self.flush_meta_and_stats();

            // Persist per-turn token usage for dashboard statistics
            if let Some(ref usage) = last_usage {
                record_token_usage(usage, &self.agent.config.model);
            }

            self.emit(Agent2Ui::TurnEnd {
                turn_id: turn_id.clone(),
                stop_reason: None,
                usage: last_usage.clone(),
            });

            break;
        }

        self.emit(Agent2Ui::Done);

        // ── Desktop notification: response preview ──
        let preview = self.agent.msg.turns().last()
            .and_then(|t| t.steps.last())
            .map(|s| {
                s.assistant.content.iter()
                    .filter_map(|b| match b {
                        deepx_types::ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        if !preview.is_empty() {
            let first_20: String = preview.split_whitespace().take(20).collect::<Vec<_>>().join(" ");
            let body = if preview.split_whitespace().count() > 20 {
                format!("{}...", first_20)
            } else {
                first_20
            };
            // Spawn thread to avoid blocking — notification API may be slow
            std::thread::spawn(move || {
                let _ = notify_rust::Notification::new()
                    .summary("DeepX")
                    .body(&body)
                    .appname("DeepX")
                    .timeout(notify_rust::Timeout::Milliseconds(8000))
                    .show();
            });
        }
    }

    // ── Dashboard ──

    fn emit_dashboard(&self) {
        self.emit_delta(Agent2Ui::Dashboard {
            hp_connected: true,
            session_seed: self.agent.session.seed.clone(),
            context_limit: self.agent.config.context_limit,
            tool_calls_total: 0,
            tool_failures: 0,
            current_phase: "single".into(),
            streaming: false,
            dsml_compat_count: self.agent.dsml_compat_count,
            documents: build_documents(),
            recent_edits: build_recent_edits(),
            tasks: build_tasks(),
            session_title: self.agent.session.title.clone(),
            usage: None,
            model: Some(self.agent.config.model.clone()),
        });
    }
}

// ═══════════════════════════════════════════════════════
// Helper functions
// ═══════════════════════════════════════════════════════

fn parse_tool_calls_from_response(
    content: &str, _reasoning: &str, tool_calls_raw: &serde_json::Value,
    agent: &AgentState,
) -> Vec<deepx_types::ToolCall> {
    let mut parsed = deepx_gate::tool_parser::parse_tool_calls(tool_calls_raw);
    if parsed.is_empty() {
        let stripped = deepx_gate::tool_parser::strip_fenced_code(content);
        if deepx_gate::tool_parser::has_dsml(&stripped) {
            let (_, dsml) = deepx_gate::tool_parser::parse_dsml_tool_calls(&stripped, &agent.tool_defs);
            if !dsml.is_empty() { parsed = dsml; }
        }
        if parsed.is_empty() && has_xml(content) {
            let names: Vec<String> = agent.tool_defs.iter().map(|t| t.function.name.clone()).collect();
            let stripped2 = deepx_gate::tool_parser::strip_fenced_code(content);
            let (_, xml) = deepx_gate::tool_parser::parse_xml_tool_calls(&stripped2, &names);
            if !xml.is_empty() { parsed = xml; }
        }
    }
    parsed
}

fn has_xml(s: &str) -> bool {
    s.contains("<tool_use>") || s.contains("<invoke ") || s.contains("<tool_calls>")
}

fn build_assistant_message(
    content: &str, reasoning: &str, parsed: &[deepx_types::ToolCall],
) -> deepx_types::Message {
    use deepx_types::{ContentBlock, Message};
    let mut blocks = Vec::new();
    if !reasoning.is_empty() {
        blocks.push(ContentBlock::Reasoning { reasoning: reasoning.to_string() });
    }
    if !content.is_empty() {
        blocks.push(ContentBlock::Text { text: content.to_string() });
    }
    for tc in parsed {
        let input: serde_json::Value = serde_json::from_str(&tc.function.arguments).unwrap_or_default();
        blocks.push(ContentBlock::ToolUse { id: tc.id.clone(), name: tc.function.name.clone(), input });
    }
    Message { msg_id: None, role: "assistant".into(), name: None, content: blocks }
}

/// Extract a short human-readable display string from a tool call's arguments.
fn format_tool_args_display(name: &str, input: &serde_json::Value) -> String {
    // Try action field first (for namespace-style tools)
    let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("");
    let display_name = if action.is_empty() { name.to_string() } else { format!("{}/{}", name, action) };

    match name {
        "exec" => input.get("command")
            .and_then(|v| v.as_str())
            .map(|c| c.chars().take(80).collect())
            .unwrap_or(display_name),
        "file" => {
            // Show path/pattern based on action
            let primary = match action {
                "search" => input.get("pattern"),
                _ => input.get("path"),
            };
            primary.and_then(|v| v.as_str())
                .map(|p| format!("{} {}", action, p.chars().take(60).collect::<String>()))
                .unwrap_or(display_name)
        }
        "task" => input.get("subject")
            .or_else(|| input.get("status"))
            .and_then(|v| v.as_str())
            .map(|s| format!("{}/{}", action, s.chars().take(60).collect::<String>()))
            .unwrap_or(display_name),
        "web" => input.get("url")
            .or_else(|| input.get("query"))
            .or_else(|| input.get("name"))
            .and_then(|v| v.as_str())
            .map(|s| s.chars().take(80).collect())
            .unwrap_or(display_name),
        "process" => input.get("id")
            .and_then(|v| v.as_u64())
            .map(|id| format!("{}/{}", action, id))
            .unwrap_or(display_name),
        "explore" => input.get("path")
            .and_then(|v| v.as_str())
            .map(|p| p.to_string())
            .unwrap_or(display_name),
        "ask_user" => input.get("question")
            .and_then(|v| v.as_str())
            .map(|q| q.chars().take(60).collect())
            .unwrap_or(display_name),
        _ => display_name,
    }
}

fn emit_round_complete(
    event_tx: &mpsc::SyncSender<Agent2Ui>,
    turn_id: &str, round_num: u32, assistant_msg: &deepx_types::Message,
    _content: &str, _reasoning: &str, _parsed: &[deepx_types::ToolCall],
) {
    use deepx_types::ContentBlock;
    let mut blocks = Vec::new();
    let mut tool_calls = Vec::new();
    for cb in &assistant_msg.content {
        match cb {
            ContentBlock::Reasoning { reasoning } if !reasoning.is_empty() => {
                blocks.push(deepx_proto::RoundBlock::Reasoning { content: reasoning.clone() });
            }
            ContentBlock::Text { text } if !text.is_empty() => {
                blocks.push(deepx_proto::RoundBlock::Text { content: text.clone() });
            }
            ContentBlock::ToolUse { id, name, input } => {
                let display = format_tool_args_display(name, input);
                tool_calls.push(deepx_proto::ToolCallDef {
                    id: id.clone(), name: name.clone(),
                    args_display: display.clone(), args_json: input.to_string(),
                });
                blocks.push(deepx_proto::RoundBlock::Tool {
                    card: deepx_proto::ToolCallDef {
                        id: id.clone(), name: name.clone(),
                        args_display: display, args_json: input.to_string(),
                    },
                });
            }
            _ => {}
        }
    }
    let _ = event_tx.send(Agent2Ui::RoundComplete {
        turn_id: turn_id.into(),
        round_num,
        thinking: if _reasoning.is_empty() { None } else { Some(_reasoning.into()) },
        answer: if _content.is_empty() { None } else { Some(_content.into()) },
        tool_calls: tool_calls.clone(),
        blocks,
        is_final: tool_calls.is_empty(),
    });
}

fn build_turns_from_context(agent: &AgentState) -> Vec<deepx_proto::TurnData> {
    use deepx_types::ContentBlock;
    let mut turns = Vec::new();
    for (ti, turn) in agent.msg.turns().iter().enumerate() {
        let mut rounds = Vec::new();
        for (ri, step) in turn.steps.iter().enumerate() {
            let thinking = step.assistant.content.iter().find_map(|b| {
                if let ContentBlock::Reasoning { reasoning } = b { Some(reasoning.clone()) } else { None }
            });
            let answer = step.assistant.content.iter().find_map(|b| {
                if let ContentBlock::Text { text } = b { Some(text.clone()) } else { None }
            });
            let tcs: Vec<deepx_proto::ToolCallDef> = step.assistant.content.iter().filter_map(|b| {
                if let ContentBlock::ToolUse { id, name, input } = b {
                    Some(deepx_proto::ToolCallDef {
                        id: id.clone(), name: name.clone(),
                        args_display: name.clone(), args_json: input.to_string(),
                    })
                } else { None }
            }).collect();
            let trs: Vec<deepx_proto::ToolResultDef> = step.tool_results.iter().flat_map(|msg| {
                msg.content.iter().filter_map(|b| {
                    if let ContentBlock::ToolResult { tool_use_id, content } = b {
                        Some(deepx_proto::ToolResultDef {
                            tool_call_id: tool_use_id.clone(),
                            output: content.clone(), success: true, file: None,
                        })
                    } else { None }
                })
            }).collect();
            rounds.push(deepx_proto::RoundData {
                round_num: ri as u32, thinking, answer, tool_calls: tcs, tool_results: trs,
            });
        }
        let user_text = turn.user.content.iter().find_map(|b| {
            if let ContentBlock::Text { text } = b { Some(text.clone()) } else { None }
        }).unwrap_or_default();
        turns.push(deepx_proto::TurnData {
            turn_id: format!("t{}", ti + 1), user_text, rounds,
        });
    }
    turns
}

fn build_compact_prompt(contexts: &[String]) -> String {
    let conv = contexts.join("\n");
    format!(
        "Summarize this conversation history into a compact summary.\n\
        Keep: user intents, operations performed (tool calls + results), files changed, unfinished tasks.\n\
        Drop: verbatim code, full tool outputs, thinking details.\n\
        Use concise bullet points under 1500 characters.\n\n\
        {}\n\nSummary:", conv
    )
}

/// Append per-turn token usage to `token_stats.jsonl` for dashboard aggregation.
fn record_token_usage(usage: &deepx_types::UsageInfo, model: &str) {
    use std::io::Write;
    let dir = platform::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("token_stats.jsonl");
    let today = chrono_local_date();
    let line = serde_json::json!({
        "date": today,
        "prompt_tokens": usage.prompt_tokens,
        "completion_tokens": usage.completion_tokens,
        "cache_hit": usage.prompt_cache_hit_tokens,
        "cache_miss": usage.prompt_cache_miss_tokens,
        "model": model,
    });
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{}", serde_json::to_string(&line).unwrap_or_default());
    }
}

/// Return today's date as "YYYY-MM-DD" (UTC+8).
pub(crate) fn chrono_local_date() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    // UTC+8 offset
    let secs = dur.as_secs() + 8 * 3600;
    let days = secs / 86400;
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Return current time as "UTC+8 YYYY-MM-DD HH:MM".
pub(crate) fn chrono_local_datetime() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    // UTC+8 offset
    let secs = dur.as_secs() + 8 * 3600;
    let days = secs / 86400;
    let day_secs = secs % 86400;
    let hours = day_secs / 3600;
    let minutes = (day_secs % 3600) / 60;
    let (y, m, d) = civil_from_days(days as i64);
    format!("UTC+8 {y:04}-{m:02}-{d:02} {hours:02}:{minutes:02}")
}

/// Convert days since epoch 0000-01-01 to (year, month, day).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Algorithm from Howard Hinnant
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
