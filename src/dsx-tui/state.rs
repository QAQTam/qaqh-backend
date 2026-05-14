//! AppState — pure UI state, no agent/orchestrator/tool domains.
//!
//! Three structs:
//!   - AppState: UI rendering state (~25 pure fields, no ctx/health/memory)
//!   - StreamBuffer: accumulates streaming deltas from Agent
//!   - AgentConnection: spawns Agent subprocess, manages JSON-LP IPC

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::types::{Message, UsageInfo, SessionMeta};
use crate::config::Config;
use super::input::state::InputState;
use super::mode::Mode;
/// Re-use gate/error types from legacy App (they live in app.rs during migration).
use super::app::{GateBlocked, ApiError, RetryProgress, RateLimitInfo};

// ─── AgentConnection ────────────────────────────────────────────────────

/// Frames sent from TUI → Agent via stdin (JSON-LP).
#[derive(Debug, Serialize)]
#[serde(tag = "t")]
pub(crate) enum TuiFrame {
    #[serde(rename = "user_input")]
    UserInput {
        text: String,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        tools_enabled: bool,
    },
    #[serde(rename = "slash")]
    SlashCommand {
        cmd: String,
        #[serde(default)]
        args: Vec<String>,
    },
    #[serde(rename = "confirm")]
    ConfirmTools {
        #[serde(default)]
        ids: Vec<String>,
    },
    #[serde(rename = "cancel")]
    Cancel,
    #[serde(rename = "query_balance")]
    QueryBalance,
    #[serde(rename = "mode_change")]
    ModeChange {
        mode: String,
    },
    #[serde(rename = "shutdown")]
    Shutdown,
}

/// Frames received from Agent → TUI via stdout (JSON-LP).
#[derive(Debug, Deserialize)]
#[serde(tag = "t")]
pub(crate) enum AgentFrame {
    #[serde(rename = "payload")]
    StreamPayload {
        delta: String,
        #[serde(default)]
        reasoning: Option<String>,
        #[serde(default)]
        tool_progress: Vec<(String, String)>,
    },
    #[serde(rename = "done")]
    StreamDone {
        #[serde(default)]
        usage: Option<UsageInfo>,
    },
    #[serde(rename = "error")]
    StreamError {
        message: String,
    },
    #[serde(rename = "mode_req")]
    TuiModeRequest {
        mode: String,
        #[serde(default)]
        context: Option<String>,
    },
    #[serde(rename = "balance")]
    BalanceResult {
        balance: String,
    },
}

// ─── AgentConnection ────────────────────────────────────────────────────

/// Manages the lifecycle of the Agent subprocess and JSON-LP IPC.
///
/// Spawns the agent binary on construction, writes JSON-LP frames to its
/// stdin, and reads JSON-LP frames from its stdout via a background reader
/// thread feeding a std::sync::mpsc channel.
///
/// Also tracks lightweight execution state needed by the render layer
/// (exec_pending, exec_started_at). Heavier agent concerns (health, memory,
/// turn scoring) live in dsx-agent, not here.
pub(crate) struct AgentConnection {
    child: Option<Child>,
    stdin: Option<Box<dyn Write + Send>>,
    frame_rx: std_mpsc::Receiver<AgentFrame>,
    config: Config,
    reader_handle: Option<thread::JoinHandle<()>>,

    // ── Execution state (render-visible) ──
    pub exec_pending: usize,
    pub exec_started_at: Option<Instant>,

    // ── Agent health state ──
    pub health: AgentHealth,
}

/// Lightweight health summary of the Agent subprocess.
#[derive(Debug, Default)]
pub(crate) struct AgentHealth {
    pub has_orphan_tool_uses: bool,
}

impl AgentConnection {
    /// Spawn the Agent subprocess and start the stdout reader thread.
    ///
    /// The `agent_bin` path points to the dsx-agent binary. If `None`,
    /// the connection is stubbed (for development/testing).
    pub fn spawn(
        agent_bin: Option<&str>,
        config: Config,
    ) -> anyhow::Result<Self> {
        let (frame_tx, frame_rx) = std_mpsc::channel();

        let (child, stdin, reader_handle) = if let Some(bin) = agent_bin {
            let mut child = Command::new(bin)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()?;

            let stdin = child.stdin.take()
                .map(|s| Box::new(s) as Box<dyn Write + Send>);
            let stdout = child.stdout.take()
                .expect("failed to capture agent stdout");

            // Background thread: reads JSON-LP lines from agent stdout
            let handle = thread::Builder::new()
                .name("agent-reader".into())
                .spawn(move || {
                    let reader = BufReader::new(stdout);
                    for line in reader.lines() {
                        match line {
                            Ok(line) => {
                                if line.trim().is_empty() {
                                    continue;
                                }
                                match serde_json::from_str::<AgentFrame>(&line) {
                                    Ok(frame) => {
                                        if frame_tx.send(frame).is_err() {
                                            break; // receiver dropped
                                        }
                                    }
                                    Err(e) => {
                                        log::warn!(
                                            "agent: invalid frame: {}  line={}",
                                            e, line
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                log::error!("agent: stdout read error: {}", e);
                                break;
                            }
                        }
                    }
                })
                .ok();

            (Some(child), stdin, handle)
        } else {
            // Stub mode: no agent binary, connection returns no frames
            (None, None, None)
        };

        Ok(Self {
            child,
            stdin,
            frame_rx,
            config,
            reader_handle,
            exec_pending: 0,
            exec_started_at: None,
            health: AgentHealth::default(),
        })
    }

    /// Send a frame to the Agent subprocess.
    pub fn send_frame(&mut self, frame: TuiFrame) {
        if let Some(ref mut stdin) = self.stdin {
            let mut line = serde_json::to_string(&frame)
                .unwrap_or_else(|_| r#"{"t":"error","message":"serialize failed"}"#.into());
            line.push('\n');
            if let Err(e) = stdin.write_all(line.as_bytes()) {
                log::warn!("agent: stdin write error: {}", e);
            }
            if let Err(e) = stdin.flush() {
                log::warn!("agent: stdin flush error: {}", e);
            }
        }
    }

    /// Non-blocking: returns the next available Agent frame, if any.
    pub fn try_recv(&self) -> Option<AgentFrame> {
        self.frame_rx.try_recv().ok()
    }

    /// Drain all available frames (non-blocking).
    pub fn drain_frames(&self) -> Vec<AgentFrame> {
        let mut frames = Vec::new();
        while let Ok(frame) = self.frame_rx.try_recv() {
            frames.push(frame);
        }
        frames
    }

    /// Read-only access to config.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Mutable config access (for slash commands that modify settings).
    pub fn config_mut(&mut self) -> &mut Config {
        &mut self.config
    }

    /// Forward config updates to the Agent subprocess.
    pub fn send_config_update(&mut self) {
        self.send_frame(TuiFrame::ModeChange {
            mode: format!("config/{}", self.config.model),
        });
    }

    /// Shutdown the Agent subprocess gracefully.
    pub fn shutdown(&mut self) {
        self.send_frame(TuiFrame::Shutdown);
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
    }
}

impl Drop for AgentConnection {
    fn drop(&mut self) {
        self.send_frame(TuiFrame::Shutdown);
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
    }
}

// ─── StreamBuffer ───────────────────────────────────────────────────────

/// Accumulates streaming content from the Agent.
///
/// Owned by the event loop; provides the source of truth for all
/// in-progress streaming output displayed by the render layer.
pub(crate) struct StreamBuffer {
    pub content: String,
    pub reasoning: String,
    pub tool_progress: Vec<(String, String)>,
    pub cancelled: bool,
    pub last_activity: Instant,
    /// Accumulated reasoning insights extracted during streaming
    pub reasoning_insights: Vec<String>,
}

impl StreamBuffer {
    pub fn new() -> Self {
        Self {
            content: String::new(),
            reasoning: String::new(),
            tool_progress: Vec::new(),
            cancelled: false,
            last_activity: Instant::now(),
            reasoning_insights: Vec::new(),
        }
    }

    /// Reset all streaming state for a new response.
    pub fn clear(&mut self) {
        self.content.clear();
        self.reasoning.clear();
        self.tool_progress.clear();
        self.cancelled = false;
        self.last_activity = Instant::now();
    }

    /// Apply an Agent StreamPayload frame to the buffer.
    pub fn apply_payload(
        &mut self,
        delta: &str,
        reasoning: Option<&str>,
        progress: &[(String, String)],
    ) {
        self.content.push_str(delta);
        if let Some(r) = reasoning {
            self.reasoning.push_str(r);
        }
        self.tool_progress = progress.to_vec();
        self.last_activity = Instant::now();
    }

    /// True if there is any content to display.
    pub fn is_active(&self) -> bool {
        !self.content.is_empty() || !self.reasoning.is_empty()
    }

    /// Build a snapshot Message from accumulated stream content.
    pub fn to_message(&self) -> Message {
        let mut msg = Message::assistant_empty();
        if !self.content.is_empty() {
            msg.content = Some(self.content.clone());
        }
        if !self.reasoning.is_empty() {
            msg.reasoning_content = Some(self.reasoning.clone());
        }
        msg
    }
}

// ─── AppState ───────────────────────────────────────────────────────────

/// Pure UI state for the TUI — no agent/orchestrator/tool/health/memory domains.
///
/// All ~25 fields are directly related to rendering and user interaction.
/// Orchestration state lives in AgentConnection, streaming in StreamBuffer.
pub(crate) struct AppState {
    // ── Embedded input state (E02) ──
    pub input_state: InputState,

    // ── Full TUI mode (includes Streaming, ApiConfig) ──
    pub mode: Mode,

    // ── Message display ──
    /// Flat message snapshot synced from Agent via StreamDone
    pub messages: Vec<Message>,
    pub scroll_offset: usize,
    pub auto_scroll: bool,
    pub last_max_scroll: usize,

    // ── Status bar ──
    pub status_msg: String,
    pub balance: Option<String>,
    pub token_estimate: u32,
    pub api_usage: Option<UsageInfo>,

    // ── Tool code panel (bottom panel) ──
    pub tool_code_path: String,
    pub tool_code_content: String,
    pub tool_code_action: String,
    pub tool_code_status: Option<&'static str>,
    pub exec_scroll_offset: usize,

    // ── Session ──
    pub session_seed: String,
    pub session_pick_idx: usize,

    // ── Toggle flags ──
    pub tools_enabled: bool,
    pub auto_mode: bool,
    pub dev_mode: bool,
    pub skip_all: bool,

    // ── Crash recovery ──
    pub dirty: bool,
    pub last_snapshot: Instant,

    // ── Gate/stream error state (from AgentFrame handlers) ──
    pub last_gate_block: Option<GateBlocked>,
    pub last_api_error: Option<ApiError>,
    pub retry_progress: Option<RetryProgress>,
    pub rate_limit_info: Option<RateLimitInfo>,
}

impl AppState {
    pub fn new(initial_mode: Mode) -> Self {
        Self {
            input_state: InputState::default(),
            mode: initial_mode,
            messages: Vec::new(),
            scroll_offset: 0,
            auto_scroll: true,
            last_max_scroll: 0,
            status_msg: String::new(),
            balance: None,
            token_estimate: 0,
            api_usage: None,
            tool_code_path: String::new(),
            tool_code_content: String::new(),
            tool_code_action: String::new(),
            tool_code_status: None,
            exec_scroll_offset: 0,
            session_seed: String::new(),
            session_pick_idx: 0,
            tools_enabled: true,
            auto_mode: false,
            dev_mode: false,
            skip_all: false,
            dirty: false,
            last_snapshot: Instant::now(),
            last_gate_block: None,
            last_api_error: None,
            retry_progress: None,
            rate_limit_info: None,
        }
    }

    /// Sync the flat message cache (called after StreamDone processing).
    pub fn set_messages(&mut self, msgs: Vec<Message>) {
        self.messages = msgs;
        self.dirty = true;
    }

    /// Append a user or assistant message to the display cache.
    pub fn push_message(&mut self, msg: Message) {
        self.messages.push(msg);
        self.dirty = true;
    }
}

// ─── RenderContext ──────────────────────────────────────────────────────

/// Lightweight snapshot assembled once per render cycle.
///
/// Aggregates data from AppState + StreamBuffer + AgentConnection so that
/// render functions access a single coherent view. AgentConnection is
/// exposed as `ctx.agent` for agent-domain display data (health, exec, etc.).
pub(crate) struct RenderContext<'a> {
    // From AppState
    pub messages: &'a [Message],
    pub mode: &'a Mode,
    pub input_buffer: &'a str,
    pub input_cursor: usize,
    pub scroll_offset: usize,
    pub auto_scroll: bool,
    pub status_msg: &'a str,
    pub balance: Option<&'a str>,
    pub token_estimate: u32,
    pub api_usage: Option<&'a UsageInfo>,
    pub tool_code_path: &'a str,
    pub tool_code_content: &'a str,
    pub tool_code_action: &'a str,
    pub tool_code_status: Option<&'a str>,
    pub exec_scroll_offset: usize,
    pub session_seed: &'a str,
    pub tools_enabled: bool,
    pub auto_mode: bool,
    pub dev_mode: bool,
    pub skip_all: bool,
    pub exit_prompt: bool,

    // From InputState (sub-structs for render access)
    pub slash_open: bool,
    pub slash_items: &'a [(String, String)],
    pub slash_filter: &'a str,
    pub slash_idx: usize,
    pub slash_submenu: Option<&'a super::input::state::SlashSubMenu>,
    pub session_list: &'a [crate::types::SessionMeta],
    pub intent_question: &'a str,
    pub intent_options: &'a [String],

    // From StreamBuffer
    pub stream_content: &'a str,
    pub stream_reasoning: &'a str,
    pub stream_tool_progress: &'a [(String, String)],
    pub stream_cancelled: bool,

    // Direct access to AgentConnection for agent-domain render data
    pub agent: &'a AgentConnection,

    // Config display fields (convenience copies from agent.config())
    pub config_model: &'a str,
    pub config_lang: &'a str,
    pub config_effort: Option<&'a str>,
    pub config_context_limit: u32,

    // From AppState (gate/stream error)
    pub last_gate_block: Option<&'a GateBlocked>,
    pub last_api_error: Option<&'a ApiError>,
    pub retry_progress: Option<&'a RetryProgress>,
    pub rate_limit_info: Option<&'a RateLimitInfo>,
}

impl<'a> RenderContext<'a> {
    pub fn from_parts(
        app: &'a AppState,
        buf: &'a StreamBuffer,
        agent: &'a AgentConnection,
    ) -> Self {
        let config = &agent.config;
        Self {
            messages: &app.messages,
            mode: &app.mode,
            input_buffer: &app.input_state.chat.buffer,
            input_cursor: app.input_state.chat.cursor,
            scroll_offset: app.scroll_offset,
            auto_scroll: app.auto_scroll,
            status_msg: &app.status_msg,
            balance: app.balance.as_deref(),
            token_estimate: app.token_estimate,
            api_usage: app.api_usage.as_ref(),
            tool_code_path: &app.tool_code_path,
            tool_code_content: &app.tool_code_content,
            tool_code_action: &app.tool_code_action,
            tool_code_status: app.tool_code_status,
            exec_scroll_offset: app.exec_scroll_offset,
            session_seed: &app.session_seed,
            tools_enabled: app.tools_enabled,
            auto_mode: app.auto_mode,
            dev_mode: app.dev_mode,
            skip_all: app.skip_all,
            exit_prompt: app.input_state.exit_prompt,

            slash_open: app.input_state.slash.open,
            slash_items: &app.input_state.slash.items,
            slash_filter: &app.input_state.slash.filter,
            slash_idx: app.input_state.slash.idx,
            slash_submenu: app.input_state.slash.submenu.as_ref(),
            session_list: &app.input_state.session.list,
            intent_question: &app.input_state.intent.question,
            intent_options: &app.input_state.intent.options,

            stream_content: &buf.content,
            stream_reasoning: &buf.reasoning,
            stream_tool_progress: &buf.tool_progress,
            stream_cancelled: buf.cancelled,

            agent,

            config_model: &config.model,
            config_lang: &config.prompt_lang,
            config_effort: config.effort.as_deref(),
            config_context_limit: config.context_limit,

            last_gate_block: app.last_gate_block.as_ref(),
            last_api_error: app.last_api_error.as_ref(),
            retry_progress: app.retry_progress.as_ref(),
            rate_limit_info: app.rate_limit_info.as_ref(),
        }
    }

    /// TUI i18n text bundle for the current config language.
    pub fn tt(&self) -> &'static super::tui_text::TuiText {
        super::tui_text::tui_text(self.config_lang)
    }

    /// Display-ready token count.
    pub fn tokens_used(&self) -> u32 {
        self.api_usage.map(|u| u.total_tokens).unwrap_or(self.token_estimate)
    }

    /// Context usage ratio (0.0–1.0).
    pub fn context_pct(&self) -> f64 {
        crate::tokenizer::context_usage_ratio(self.token_estimate, self.config_context_limit)
    }
}
