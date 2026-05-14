//! App state machine: state, message sending, streaming, tool execution.

use crate::api::StreamEvent;
use crate::config::{self, Config, tui_text};
use crate::conversation::ContextAssembler;
use crate::{context, tokenizer};
use crate::health::DsAgentsHealthPlatform;
use crate::health::monitor::{MonitorState, pre_tool_gate, post_tool_record};
use crate::session;
use crate::types::{AgentPhase, Message, SafetyLevel, ToolCall, UsageInfo};
use std::time::Instant;
use tokio::sync::mpsc;

use super::utils::{char_boundary, detect_phase};
use super::{Mode, SubMenuState};
// ── App State ──

pub(crate) struct App {
    /// Canonical message container with strict alternation guarantees.
    /// All message mutations go through this — NEVER push to messages directly.
    pub(crate) ctx: ContextAssembler,
    /// Legacy flat view, synced from ctx before reads. Do NOT mutate.
    pub(crate) messages: Vec<Message>,
    pub(crate) tools_enabled: bool,
    pub(crate) input: String,
    pub(crate) input_cursor: usize,
    pub(crate) scroll_offset: usize,
    pub(crate) auto_scroll: bool,
    pub(crate) last_max_scroll: usize,     // updated each render, used for manual scroll init
    pub(crate) mode: Mode,
    pub(crate) config: Config,
    pub(crate) status_msg: String,
    pub(crate) token_estimate: u32,
    pub(crate) token_breakdown: Option<crate::tokenizer::TokenBreakdown>,
    pub(crate) api_usage: Option<UsageInfo>,
    pub(crate) session_tokens: u64,
    // Explore-before-read state machine
    pub(crate) has_explored: bool,
    pub(crate) turns_since_last_read: u32,

    pub(crate) pending_tools: Vec<(ToolCall, SafetyLevel, String)>,
    pub tool_results: Vec<(String, String)>,

    // Streaming state (real-time display)
    pub(crate) stream_content: String,
    pub(crate) stream_reasoning: String,
    pub(crate) stream_tool_progress: Vec<(String, String)>, // (name, args_so_far)
    pub(crate) stream_cancelled: bool,
    pub(crate) last_activity: Instant,

    // Slash menu
    pub(crate) slash_open: bool,
    pub(crate) slash_filter: String,
    pub(crate) slash_idx: usize,
    pub(crate) slash_items: Vec<(String, String)>,
    pub(crate) slash_submenu: Option<SubMenuState>,
    pub(crate) esc_just_closed_menu: bool,
    pub(crate) exit_prompt: bool,          // "Really exit?" confirmation dialog

    // Balance
    pub(crate) balance: Option<String>,
    pub(crate) cache_hit_pct: f64,
    pub(crate) reasoning_tokens: u32,

    // Session persistence
    pub(crate) session_seed: String,
    pub(crate) session_start: u64,
    pub(crate) session_list: Vec<crate::types::SessionMeta>,
    pub(crate) session_pick_idx: usize,

    // Crash recovery
    pub(crate) dirty: bool,
    pub(crate) last_snapshot: Instant,

    // Tool chain safety
    pub(crate) tool_failures: u32,
    pub(crate) tool_calls_this_turn: u32,
    // Turn importance scoring for value-driven compaction
    pub(crate) turn_scores: Vec<f32>,
    pub(crate) auto_verify: Vec<String>,
    pub(crate) intent_question: String,
    pub(crate) intent_options: Vec<String>,
    // Skill matching
    pub(crate) skill_index: crate::skills::SkillIndex,
    pub(crate) active_skill_bodies: Vec<(String, String)>, // (name, body)
    pub(crate) exec_buffer: String,           // accumulated streaming output
    pub(crate) exec_pending: usize,           // count of running async execs
    pub(crate) exec_started_at: Option<Instant>,
    pub(crate) exec_child_pids: Vec<u32>,      // child PIDs for cancellation,

    // Sudo password prompt
    pub(crate) sudo_pending: Vec<(crate::types::ToolCall, String)>,  // (tool_call, command)
    pub(crate) sudo_password: String,

    // Phase detection
    pub(crate) phase: AgentPhase,
    pub(crate) phase_count: u32,

    // Project knowledge (persists across compaction)
    pub(crate) project_map: String,
    pub(crate) semantic_memory: crate::memory::SemanticMemory,

    // Long/short/working memory (three-tier)
    pub(crate) long_mem: crate::memory::LongTermMemory,
    pub(crate) short_mem: crate::memory::ShortTermMemory,
    pub(crate) pitfall_guide: crate::memory::PitfallGuide,
    // Setup wizard

    // Auto mode
    pub(crate) auto_mode: bool,
    pub(crate) current_task_phase: crate::types::TaskPhase,
    // Dev mode
    pub(crate) dev_mode: bool,

    // Tool code view (bottom-left panel)
    pub(crate) tool_code_path: String,
    pub(crate) tool_code_content: String,
    pub(crate) tool_code_action: String,  // "read" | "edit" | "write" | "exec"
    pub(crate) tool_code_status: Option<&'static str>, // None=running, Some("OK"/"FAIL")

    /// Tool monitoring (loops, failures, circuit breaker).
    pub(crate) monitor: MonitorState,
    // Model health monitoring (loop/repetition detection)
    pub(crate) health: DsAgentsHealthPlatform,
    /// Cross-turn tool-only loop counter: model calls tools but never outputs text
    pub(crate) consecutive_tool_turns: u32,
    /// Files written this turn — used to block exec on freshly-written scripts
    pub(crate) files_written_this_turn: Vec<String>,
    /// Skip-all mode: auto-confirm all tools, bypass sudo/confirm/danger prompts
    pub(crate) skip_all: bool,
    /// Safety gate interrupt that was delayed due to pending async execs
    pub(crate) gate_message: Option<String>,
    /// Per-turn health status line for TUI display (bottom-right info panel)
    pub(crate) health_status_line: String,
    /// Exec panel scroll offset (PageUp/PageDown)
    pub(crate) exec_scroll_offset: usize,
    /// System note queue — flushed into last message at start_agent_loop
    pub(crate) pending_notes: Vec<String>,
}
impl App {
    pub(crate) fn new(config: Config, resume: bool) -> Self {
        let auto_mode = config.auto_mode;
        let prompt = config::system_prompt(&config.prompt_lang);
        let mut session_list = if resume { session::load_index() } else { vec![] };
        // Merge live (interrupted) sessions
        if resume {
            let live = session::find_live_sessions();
            for ls in live {
                if !session_list.iter().any(|s| s.seed == ls.seed) {
                    session_list.push(ls);
                }
            }
            // Sort by updated_at descending (newest first)
            session_list.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        }
        let initial_mode = if resume {
            Mode::SessionPicker
        } else if config.is_ready() {
            Mode::Chat
        } else {
            Mode::ApiConfig
        };

        let mut ctx = ContextAssembler::new();
        ctx.push_system(Message::system(prompt));

        let mut app = Self {
            ctx,
            messages: vec![Message::system(prompt)],
            tools_enabled: true,
            input: String::new(),
            input_cursor: 0,
            scroll_offset: 0,
            auto_scroll: true,
            last_max_scroll: 0,
            mode: initial_mode,
            config,
            status_msg: String::new(),
            token_estimate: 0,
            token_breakdown: None,
            api_usage: None,
            session_tokens: 0,
            has_explored: false,
            turns_since_last_read: 0,
            pending_tools: Vec::new(),
            tool_results: Vec::new(),
            stream_content: String::new(),
            stream_reasoning: String::new(),
            stream_tool_progress: Vec::new(),
            stream_cancelled: false,
            last_activity: Instant::now(),
            slash_open: false,
            slash_filter: String::new(),
            slash_idx: 0,
            slash_items: Vec::new(),
            slash_submenu: None,
            esc_just_closed_menu: false,
            exit_prompt: false,
            balance: None,
            cache_hit_pct: 0.0,
            reasoning_tokens: 0,
            session_seed: String::new(),
            session_start: 0,
            session_list,
            session_pick_idx: 0,
            dirty: false,
            last_snapshot: Instant::now(),
            tool_failures: 0,
            intent_question: String::new(),
            intent_options: Vec::new(),
            tool_calls_this_turn: 0,
            auto_verify: Vec::new(),
            turn_scores: Vec::new(),
            exec_buffer: String::new(),
            exec_pending: 0,
            exec_started_at: None,
            exec_child_pids: Vec::new(),
            skill_index: crate::skills::SkillIndex::scan(),
            active_skill_bodies: Vec::new(),
            sudo_pending: Vec::new(),
            sudo_password: String::new(),
            phase: AgentPhase::Idle,
            phase_count: 0,
            project_map: String::new(),
            semantic_memory: crate::memory::SemanticMemory::new(),
            long_mem: crate::memory::LongTermMemory::default(),
            short_mem: crate::memory::ShortTermMemory::new("", ""),
            pitfall_guide: crate::session::load_pitfalls(),

            auto_mode,
            current_task_phase: crate::types::TaskPhase::Coding,
            dev_mode: false,
            tool_code_path: String::new(),
            tool_code_content: String::new(),
            tool_code_action: String::new(),
            tool_code_status: None,
            exec_scroll_offset: 0,
            monitor: MonitorState::new(),
            health: DsAgentsHealthPlatform::new(),
            consecutive_tool_turns: 0,
            files_written_this_turn: Vec::new(),
            skip_all: false,
            gate_message: None,
            health_status_line: String::new(),
            pending_notes: Vec::new(),
        };
        // Seed project backbone invariants if fresh
        if app.long_mem.architecture.is_empty() {
            app.long_mem.add_invariant("tool_use/tool_result MUST be adjacent in messages — API 400 if interleaved");
            app.long_mem.add_invariant("Sudo password via stdin pipe, NEVER in command string");
            app.long_mem.add_invariant("exec_pending gates agent loop to prevent double-request race");
        }

        crate::tools::AUTO_MODE.store(auto_mode, std::sync::atomic::Ordering::Relaxed);
        app
    }
    pub(crate) fn tokens_used(&self) -> u32 {
        self.api_usage.as_ref().map(|u| u.total_tokens).unwrap_or(self.token_estimate)
    }

    pub(crate) fn tt(&self) -> &'static config::TuiText {
        tui_text(&self.config.prompt_lang)
    }

    pub(crate) fn context_pct(&self) -> f64 {
        // Use token_estimate (fresh per prepare_and_compact), NOT tokens_used() which
        // can be stale from the previous API response after compaction.
        tokenizer::context_usage_ratio(self.token_estimate, self.config.context_limit)
    }

    /// Unified system note entry. Delegates to ContextAssembler.
    pub(crate) fn system_note(&mut self, tag: &str, msg: String) {
        self.ctx.annotate(tag, &msg);
    }

    /// Flush pending annotations via ContextAssembler.
    pub(crate) fn flush_notes(&mut self) {
        self.ctx.flush_annotations();
        self.sync_messages();
    }

    /// Sync the flat messages cache from the ContextAssembler.
    pub(crate) fn sync_messages(&mut self) {
        self.messages = self.ctx.to_vec();
    }

    /// Push a tool result through the Assembler and sync.
    /// Logs errors but never crashes — tool result is dropped if orphan.
    pub(crate) fn push_tool(&mut self, id: &str, result: &str) {
        if let Err(e) = self.ctx.push_tool_result(id, result) {
            log::warn!("push_tool failed for {}: {:?} — trying cross-turn search", id, e);
            // Fall back to cross-turn search (for async race conditions)
            let _ = self.ctx.push_tool_result_for(id, result);
        }
    }

    // ── Message sending (streaming) ──

    /// Scan reasoning chains for architecture decisions and error patterns
    /// before L1 strips them. Re-scanning the same reasoning is harmless:
    /// arch_decisions capped at 10 and error_patterns dedup by keyword.
    pub(crate) fn extract_reasoning_insights(&mut self) {
        for msg in &self.messages {
            if msg.role != "assistant" { continue; }
            let Some(ref reasoning) = msg.reasoning_content else { continue };
            if reasoning.len() < 20 { continue; }

            // Architecture decision patterns
            let arch_keywords = [
                "I'll use", "I will use", "Let's use", "改用", "采用",
                "approach", "design", "pattern", "方案", "架构",
                "decided to", "better to", "recommend",
            ];
            for kw in &arch_keywords {
                if let Some(pos) = reasoning.find(kw) {
                    let start = char_boundary(reasoning, pos.saturating_sub(30));
                    let end = char_boundary(reasoning, usize::min(pos + kw.len() + 80, reasoning.len()));
                    let snippet = &reasoning[start..end];
                    self.semantic_memory.add_arch_decision(kw, snippet);
                    break; // one per message is enough
                }
            }

            // Error pattern detection
            let error_keywords = ["Error:", "error[", "panic:", "bug:", "fixed by", "原因是", "修复"];
            for kw in &error_keywords {
                if let Some(pos) = reasoning.find(kw) {
                    let start = char_boundary(reasoning, pos.saturating_sub(20));
                    let end = char_boundary(reasoning, usize::min(pos + kw.len() + 100, reasoning.len()));
                    let snippet = &reasoning[start..end];
                    self.semantic_memory.add_error_pattern(kw, snippet, None);
                    break;
                }
            }
        }
    }

    pub(crate) fn prepare_and_compact(&mut self) -> Vec<Message> {
        // Repair orphan tool_use/tool_result pairs — prevents API 400
        self.repair_messages();

        // [MEMORY FROZEN] Self-learning memory disabled — takes context but adds no value.
        let known_files = std::collections::HashSet::new();

        let prompt = crate::config::system_prompt(&self.config.prompt_lang);
        let empty = String::new();
        let builder = context::ContextBuilder::new(
            &self.messages, prompt, &empty,
            &empty,  // [MEMORY FROZEN] long_mem block
            &self.turn_scores, &known_files, self.config.context_limit,
        );
        let (mut prepared, breakdown) = builder.build();
        // Use total (all 8 layers) for the context bar, not just episodic conversations.
        // The API reports total_tokens which includes system+semantic+backbone+... so
        // the bar stays consistent before/after the API response.
        let total = breakdown.total;
        self.token_estimate = total;
        self.token_breakdown = Some(breakdown);

        // Pitfall guide (user-curated invariants, kept active)
        let pitfall_block = self.pitfall_guide.render(3);
        if !pitfall_block.is_empty() {
            let pitfall_pos = 1.min(prepared.len());
            prepared.insert(pitfall_pos, Message::system(&pitfall_block));
        }
        self.health.context_tokens = self.tokens_used();
        self.health.context_tier = crate::health::ContextTier::from_tokens(self.health.context_tokens, self.config.context_limit);

        // Show compact level based on episodic token ratio
        let pct = breakdown.episodic as f64 / self.config.context_limit as f64;
        if pct > 0.80 {
            self.status_msg = format!("CRIT: {} tok ({}%)", tokenizer::format_tokens(breakdown.episodic), (pct * 100.0) as u32);
        } else if pct > 0.65 {
            self.status_msg = format!("⚠ Compact: {} tok ({}%)", tokenizer::format_tokens(breakdown.episodic), (pct * 100.0) as u32);
        } else if pct > 0.50 {
            self.status_msg = format!("Compact: {} tok ({}%)", tokenizer::format_tokens(breakdown.episodic), (pct * 100.0) as u32);
        }

        // Inject matched skill bodies (Layer 2) after semantic memory
        if !self.active_skill_bodies.is_empty() && !prepared.is_empty() {
            // Insert after system prompt + semantic memory (at position 1 or 2)
            let insert_at = if prepared.len() > 1 && prepared[1].role == "system" { 2 } else { 1 };
            for (name, body) in &self.active_skill_bodies {
                let skill_msg = Message::system(&format!(
                    "## Skill: {}\n{}\n---\n",
                    name, body
                ));
                if insert_at <= prepared.len() {
                    prepared.insert(insert_at, skill_msg);
                }
            }
        }
        prepared
    }

    pub(crate) fn start_agent_loop(&mut self, stream_tx: mpsc::Sender<StreamEvent>) {
        crate::orchestrator::agent_loop::handle_start_agent_loop(self, stream_tx);
    }

    pub(crate) fn send_message(&mut self, tx: mpsc::Sender<StreamEvent>) {
        crate::orchestrator::agent_loop::handle_send_message(self, tx);
    }

    pub(crate) fn phase_check_tool(&mut self, tool_name: &str, tc_id: &str) -> bool {
        crate::orchestrator::gates::phase_check_tool(self, tool_name, tc_id)
    }

    pub(crate) fn pre_tool_health_check(&mut self, name: &str) -> Option<String> {
        crate::orchestrator::gates::pre_tool_health_check(self, name)
    }

    pub(crate) fn post_tool_health_record(&mut self, name: &str, success: bool) {
        crate::orchestrator::gates::post_tool_health_record(self, name, success)
    }

    pub(crate) fn tool_action(args: &str) -> String {
        crate::orchestrator::arg_parser::tool_action(args)
    }

    pub(crate) fn explore_gate(&mut self, tool_name: &str, tc_id: &str, args: &str) -> bool {
        crate::orchestrator::gates::explore_gate(self, tool_name, tc_id, args)
    }

    fn parse_file_arg(args: &str) -> Option<String> {
        crate::orchestrator::arg_parser::parse_file_arg(args)
    }

    fn parse_cmd_arg(args: &str) -> Option<String> {
        crate::orchestrator::arg_parser::parse_cmd_arg(args)
    }

    pub(crate) fn track_file_written(&mut self, args: &str) {
        crate::orchestrator::tracker::track_file_written(self, args)
    }

    fn last_assistant_content(&self) -> String {
        crate::orchestrator::tracker::last_assistant_content(self)
    }

    pub(crate) fn track_tool_code(&mut self, tool_name: &str, args: &str, result: &str) {
        crate::orchestrator::tracker::track_tool_code(self, tool_name, args, result)
    }

    pub(crate) fn read_active_plan(&self) -> String {
        crate::orchestrator::tracker::read_active_plan(self)
    }

    pub(crate) fn auto_extract_memory(&mut self, final_msg: &Message) {
        crate::orchestrator::learning::auto_extract_memory(self, final_msg)
    }

    fn capture_learning_entry(&self, final_msg: &Message, round: usize,
                               tools: &[String], files: &[String],
                               file_ops: &[(String, String, String)],
                               had_errors: bool,
                               emo: crate::health::AgentEmotion, label: &str) {
        crate::orchestrator::learning::capture_learning_entry(
            self, final_msg, round, tools, files, file_ops, had_errors, emo, label)
    }

    /// Repair inconsistent messages: add [ERROR] for tool_calls without matching tool results.
    /// Prevents API 400 when resuming a session that was interrupted mid-execution.
    /// Repair messages by re-importing through ContextAssembler.
    /// This automatically fixes orphan tool_use/tool_result and injects
    /// [ERROR] messages for unresolved tool calls.
    pub(crate) fn repair_messages(&mut self) {
        let msgs = std::mem::take(&mut self.messages);
        let (mut new_ctx, repairs) = ContextAssembler::from_legacy(msgs);
        // Preserve system messages and turns
        self.ctx = new_ctx;
        self.sync_messages();
        if !repairs.is_empty() {
            log::warn!("repair: {}", repairs.join("; "));
            self.status_msg = format!("Repaired: {} issue(s).", repairs.len());
        } else {
            self.status_msg = "Messages are clean.".into();
        }
    }

    pub(crate) fn maybe_save_session(&mut self) {
        self.repair_messages();
        let msgs = self.ctx.to_vec();
        if msgs.len() > 1 && !self.session_seed.is_empty() {
            session::finalize_session(
                &self.session_seed,
                &msgs,
                &self.config.model,
                self.config.effort.as_deref(),
                None, // [MEMORY FROZEN] semantic_memory
            );
        }
    }

}
impl App {
    pub(crate) fn update_phase(&mut self, reasoning: &str) {
        let (detected, _conf) = detect_phase(reasoning);
        if detected == self.phase {
            self.phase_count += 1;
        } else {
            self.phase_count = self.phase_count.saturating_sub(1);
            // Hysteresis: switch only after 2 consecutive same-phase detections
            if self.phase_count == 0 {
                self.phase = detected;
                self.phase_count = 1;
            }
        }
    }
}
