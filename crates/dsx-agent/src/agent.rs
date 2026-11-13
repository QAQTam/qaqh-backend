//! AgentState: the orchestrator-owned state, independent of TUI.
//!
//! This is the extracted core of the old `App` struct — all fields and methods
//! that belong to the orchestration/memory/health/session domains, not to UI.
//!
//! In the new architecture:
//!   - dsx-agent owns AgentState
//!   - Orchestrator modules take &mut AgentState, never &mut App

use std::time::Instant;

use crate::config;
use crate::assembly::ContextAssembler;
use crate::health::monitor::MonitorState;
use crate::health::DsAgentsHealthPlatform;
use crate::memory::{LongTermMemory, PitfallGuide, SemanticMemory, ShortTermMemory};
use crate::session;
use crate::skills::SkillIndex;
use crate::tokenizer;
use dsx_types::{Message, SafetyLevel, TaskPhase, ToolCall, UsageInfo};

// ── AgentState ──

pub struct AgentState {
    /// Canonical conversation context with strict alternation guarantees.
    pub ctx: ContextAssembler,

    // ── Configuration ──
    pub config: crate::config::Config,
    pub tools_enabled: bool,

    // ── Token tracking ──
    pub token_estimate: u32,
    pub token_breakdown: Option<crate::tokenizer::TokenBreakdown>,
    pub api_usage: Option<UsageInfo>,
    pub session_tokens: u64,
    pub cache_hit_pct: f64,
    pub reasoning_tokens: u32,

    // ── Explore-before-read state machine ──
    pub has_explored: bool,
    pub turns_since_last_read: u32,

    // ── Pending tool confirmation ──
    pub pending_tools: Vec<(ToolCall, SafetyLevel, String)>,
    pub tool_results: Vec<(String, String)>,

    // ── Session persistence ──
    pub session_seed: String,
    pub session_start: u64,
    pub resume_seed: Option<String>,

    // ── Crash recovery ──
    pub dirty: bool,
    pub last_snapshot: Instant,

    // ── Tool chain safety ──
    pub tool_failures: u32,
    pub tool_calls_this_turn: u32,
    pub turn_scores: Vec<f32>,
    pub auto_verify: Vec<String>,
    pub consecutive_tool_turns: u32,

    // ── Registered tool definitions (from dsx-tools) ──
    pub tool_defs: Vec<dsx_types::ToolDef>,

    // ── ask_user flow ──
    pub intent_question: String,
    pub intent_options: Vec<String>,

    /// tool_call_id when waiting for user response to ask_user
    pub pending_ask_user: Option<String>,

    // ── Skill matching ──
    pub skill_index: SkillIndex,
    pub active_skill_bodies: Vec<(String, String)>,

    // ── Exec orchestration ──
    pub exec_pending: usize,
    pub exec_started_at: Option<Instant>,
    pub exec_child_pids: Vec<u32>,

    // ── Sudo ──
    pub sudo_pending: Vec<(ToolCall, String)>,
    pub sudo_password: String,

    // ── Project knowledge ──
    pub project_map: String,
    pub semantic_memory: SemanticMemory,
    pub long_mem: LongTermMemory,
    pub short_mem: ShortTermMemory,
    pub pitfall_guide: PitfallGuide,

    // ── Mode flags ──
    pub auto_mode: bool,
    pub current_task_phase: TaskPhase,
    pub dev_mode: bool,

    // ── Tool code view state ──
    pub tool_code_path: String,
    pub tool_code_content: String,
    pub tool_code_action: String,
    pub tool_code_status: Option<&'static str>,

    // ── Health / monitoring ──
    pub monitor: MonitorState,
    pub health: DsAgentsHealthPlatform,
    pub files_written_this_turn: Vec<String>,
    pub skip_all: bool,
    pub gate_message: Option<String>,
    pub health_status_line: String,
    pub pending_notes: Vec<String>,

    // ── Tool round limits ──
    pub max_tool_rounds: u32,

    // ── Streaming state (agent-owned — pending IPC serialisation) ──
    pub stream_content: String,
    pub stream_reasoning: String,
    pub stream_tool_progress: Vec<(String, String)>,
    pub stream_cancelled: bool,
    pub last_activity: Instant,
}

impl AgentState {
    pub fn new(config: crate::config::Config) -> Self {
        let prompt = config::system_prompt(&config.prompt_lang);
        let auto_mode = config.auto_mode;
        let mut ctx = ContextAssembler::new();
        ctx.push_system(Message::system(&prompt));

        // Scan skills once at startup
        let skill_index = SkillIndex::scan();

        let max_tool_rounds = config.max_tool_rounds.unwrap_or(10);

        let mut state = Self {
            ctx,
            config,
            tools_enabled: true,
            token_estimate: 0,
            token_breakdown: None,
            api_usage: None,
            session_tokens: 0,
            cache_hit_pct: 0.0,
            reasoning_tokens: 0,
            has_explored: false,
            turns_since_last_read: 0,
            pending_tools: Vec::new(),
            tool_results: Vec::new(),
            session_seed: String::new(),
            session_start: 0,
            resume_seed: None,
            dirty: false,
            last_snapshot: Instant::now(),
            tool_failures: 0,
            tool_calls_this_turn: 0,
            turn_scores: Vec::new(),
            auto_verify: Vec::new(),
            consecutive_tool_turns: 0,
            tool_defs: Vec::new(),
            intent_question: String::new(),
            intent_options: Vec::new(),
            pending_ask_user: None,
            skill_index,
            active_skill_bodies: Vec::new(),
            exec_pending: 0,
            exec_started_at: None,
            exec_child_pids: Vec::new(),
            sudo_pending: Vec::new(),
            sudo_password: String::new(),
            project_map: String::new(),
            semantic_memory: SemanticMemory::new(),
            long_mem: LongTermMemory::default(),
            short_mem: ShortTermMemory::new("", ""),
            pitfall_guide: session::load_pitfalls(),
            auto_mode,
            current_task_phase: TaskPhase::Coding,
            dev_mode: false,
            tool_code_path: String::new(),
            tool_code_content: String::new(),
            tool_code_action: String::new(),
            tool_code_status: None,
            monitor: MonitorState::new(),
            health: DsAgentsHealthPlatform::new(),
            files_written_this_turn: Vec::new(),
            skip_all: false,
            gate_message: None,
            health_status_line: String::new(),
            pending_notes: Vec::new(),
            stream_content: String::new(),
            stream_reasoning: String::new(),
            stream_tool_progress: Vec::new(),
            max_tool_rounds,
            stream_cancelled: false,
            last_activity: Instant::now(),
        };

        // Seed invariants if fresh
        if state.long_mem.architecture.is_empty() {
            state.long_mem.add_invariant("tool_use/tool_result MUST be adjacent in messages — API 400 if interleaved");
            state.long_mem.add_invariant("Sudo password via stdin pipe, NEVER in command string");
            state.long_mem.add_invariant("exec_pending gates agent loop to prevent double-request race");
        }

        crate::tools::AUTO_MODE.store(auto_mode, std::sync::atomic::Ordering::Relaxed);
        state
    }

    // ── Token helpers ──

    pub fn tokens_used(&self) -> u32 {
        self.api_usage.as_ref().map(|u| u.total_tokens).unwrap_or(self.token_estimate)
    }

    pub fn context_pct(&self) -> f64 {
        tokenizer::context_usage_ratio(self.token_estimate, self.config.context_limit)
    }

    // ── Context helpers ──

    /// Unified system note entry. Delegates to ContextAssembler.
    pub fn system_note(&mut self, tag: &str, msg: String) {
        self.ctx.annotate(tag, &msg);
    }

    /// Flush pending annotations.
    pub fn flush_notes(&mut self) {
        self.ctx.flush_annotations();
    }

    // ── Exec tracking ──

    /// Decrement exec_pending counter. Resets exec_started_at when reaches 0.
    pub fn decrement_exec_pending(&mut self) {
        self.exec_pending = self.exec_pending.saturating_sub(1);
        if self.exec_pending == 0 {
            self.exec_started_at = None;
        }
    }

    // ── Persist ──

    /// Save session to disk if seeded and non-empty, AND no pending tool calls.
    pub fn maybe_save_session(&mut self) {
        if self.ctx.has_pending_tools() { return; }
        let msgs = self.ctx.to_vec();
        if msgs.len() > 1 && !self.session_seed.is_empty() {
            session::finalize_session(
                &self.session_seed,
                &msgs,
                &self.config.model,
                self.config.effort.as_deref(),
                None,
            );
        }
    }

    // ── Learning / memory extraction ──

    /// Scan all assistant messages for architecture decisions and error patterns.
    pub fn extract_reasoning_insights(&mut self) {
        for msg in self.ctx.to_vec() {
            if msg.role != "assistant" { continue; }
            let Some(ref reasoning) = msg.reasoning_content else { continue };
            if reasoning.len() < 20 { continue; }

            let arch_keywords = [
                "I'll use", "I will use", "Let's use", "改用", "采用",
                "approach", "design", "pattern", "方案", "架构",
                "decided to", "better to", "recommend",
            ];
            for kw in &arch_keywords {
                if let Some(pos) = reasoning.find(kw) {
                    let start = pos.saturating_sub(30);
                    let end = reasoning.len().min(pos + kw.len() + 80);
                    let snippet = &reasoning[start..end];
                    self.semantic_memory.add_arch_decision(kw, snippet);
                    break;
                }
            }

            let error_keywords = ["Error:", "error[", "panic:", "bug:", "fixed by", "原因是", "修复"];
            for kw in &error_keywords {
                if let Some(pos) = reasoning.find(kw) {
                    let start = pos.saturating_sub(20);
                    let end = reasoning.len().min(pos + kw.len() + 100);
                    let snippet = &reasoning[start..end];
                    self.semantic_memory.add_error_pattern(kw, snippet, None);
                    break;
                }
            }
        }
    }

}

// ── ToolResultAppender ──
//
// Unified entry point for writing tool results to the context.
// Eliminates the 3+ scattered `push_tool` call sites in the orchestrator.

use crate::orchestrator::{arg_parser, tracker};
use crate::tools::wrap_tool_result;

pub struct ToolResultAppender<'a> {
    pub state: &'a mut AgentState,
}

impl<'a> ToolResultAppender<'a> {
    pub fn new(state: &'a mut AgentState) -> Self {
        Self { state }
    }

    /// Append a tool result to the context and record all side effects.
    ///
    /// This is the ONLY entry point for pushing tool results. It handles:
    /// 1. ContextAssembler push (authoritative)
    /// 2. Side-effect tracking (tool_results vec, file tracking, health)
    /// 3. Tool-code preview state
    ///
    /// Returns: whether the tool succeeded (result does NOT start with [ERROR]/[FAIL]).
    pub fn append(&mut self, tool_name: &str, tc_id: &str, args: &str, raw: &str) -> bool {
        let failed = raw.starts_with("[ERROR]") || raw.starts_with("[FAIL]");
        let result = wrap_tool_result(tool_name, raw);

        // 1. Push to ContextAssembler (authoritative message store)
        if let Err(e) = self.state.ctx.push_tool_result(tc_id, &result) {
            log::warn!("ToolResultAppender: push_tool_result failed for {}: {:?}", tc_id, e);
            let _ = self.state.ctx.push_tool_result_for(tc_id, &result);
        }

        // 2. Side-effect tracking
        self.state.tool_results.push((tool_name.to_string(), result.clone()));
        if failed {
            self.state.health.record_error(tool_name, raw);
        }

        // 3. Tool-code preview
        tracker::track_tool_code(self.state, tool_name, args, raw);

        // 4. File tracking (for sandbox enforcement)
        if !failed && tool_name == "file" {
            let action = arg_parser::tool_action(args);
            if action == "write" || action == "edit" {
                tracker::track_file_written(self.state, args);
            }
        }

        // 5. Auto-verify on Rust file edit
        if raw.starts_with("[OK]") && tool_name == "file" {
            let action = arg_parser::tool_action(args);
            if (action == "write" || action == "edit") && !self.state.auto_verify.contains(&"cargo check".to_string()) {
                if let Some(path) = arg_parser::parse_file_arg(args) {
                    if path.ends_with(".rs") {
                        self.state.auto_verify.push("cargo check".to_string());
                    }
                }
            }
        }

        !failed
    }

    /// Consume self and return the underlying state.
    pub fn into_inner(self) -> &'a mut AgentState {
        self.state
    }
}
