//! Agent loop: build context, gate-check, spawn API request.
//!
//! In the new architecture, this module owns the orchestration logic and
//! operates on AgentState, NOT on the TUI's `App` struct. TUI-only concerns
//! (mode management, input cursor, streaming display) are handled by the
//! TUI side or the IPC bridge.
//!
//! The functions here are called by the IPC bridge (or, temporarily, by
//! the TUI's App forwarding methods).

use std::time::Instant;
use tokio::sync::mpsc;

use crate::api::{self, StreamEvent};
use crate::config;
use crate::health::gate::{check_gate, GateContext, GateResult};
use crate::session;
use crate::tools;

use crate::agent::AgentState;


/// Start a new agent loop turn: build context, run gate checks, spawn API request.
///
/// Preconditions (caller must ensure):
///   - Not already streaming (TUI won't call during streaming)
///   - No pending tools awaiting confirmation (TUI won't call during confirm mode)
///   - The caller has already cleared its streaming display buffers
///
/// Returns Ok(()) after the API task is spawned. Errors are reported via
/// the StreamEvent channel.
pub fn handle_start_agent_loop(state: &mut AgentState, stream_tx: mpsc::Sender<StreamEvent>) {
    // Sync flat messages from Assembler before anything reads them


    // Guard: exec has been spawned but result not yet returned
    if state.exec_pending > 0 {
        log::warn!("handle_start_agent_loop: {} exec(s) pending, waiting for results", state.exec_pending);
        return;
    }

    // Pre-flight sandbox validation — intercept 400 risks before the context is polluted
    let gate_ctx = GateContext {
        assembler: &state.ctx,
        has_orphan_tool_uses: state.health.has_orphan_tool_uses,
    };
    match check_gate(&gate_ctx) {
        GateResult::Pass => {}
        GateResult::Block { reason, repairable } => {
            log::error!("sandbox blocked: {}", reason);
            let _ = stream_tx.blocking_send(StreamEvent::Error(format!(
                "Gate blocked: {} — {}",
                reason,
                if repairable { "type /fix to repair" } else { "not repairable" }
            )));
            return;
        }
    }

    // Reset per-turn health counters
    state.health.reset_turn();
    state.stream_cancelled = false;
    state.last_activity = Instant::now();

    let cfg = state.config.clone();
    let uid = state.session_seed.clone();

    // ── Build context (returns OpenAI-format messages) ──
    let (system, messages, _breakdown) = crate::assembly::build_context(state);
    debug_assert!(
        crate::health::gate::validate_anthropic_messages(&messages).is_ok(),
        "prepare_and_compact produced invalid message sequence — would cause API 400",
    );
    log::debug!("handle_start_agent_loop auto_mode={}", state.auto_mode);

    // Auto mode: override model/effort/tools/max_tokens based on AI-declared phase
    let (model, effort, tools, max_tokens) = if state.auto_mode {
        let phase = crate::router::read_phase();
        let level = crate::router::read_debug_level();
        state.current_task_phase = phase;
        let pc = crate::router::phase_config(phase, level);

        // User-configured phase overrides take precedence over hardcoded defaults
        let phase_name = format!("{:?}", phase).to_lowercase();
        let (model, effort, max_tokens) = if let Some(user_pc) = state.config.phase_configs.get(&phase_name) {
            state.config.context_limit = user_pc.context_limit;
            (user_pc.model.clone(), user_pc.effort.clone().filter(|e| !e.is_empty()), user_pc.max_tokens)
        } else {
            (pc.model.to_string(), pc.effort.map(|s| s.to_string()), pc.max_tokens)
        };
        // Sync to config for top bar display (don't persist — auto-only)
        state.config.model = model.clone();
        state.config.effort = effort.clone();
        state.config.max_tokens = max_tokens;
        let tools = Some(tools::tools_for_phase(phase));
        log::info!("auto mode: {:?} {} {} {}K", phase, model, effort.as_deref().unwrap_or("?"), max_tokens / 1000);
        (model, effort.clone(), tools, max_tokens)
    } else {
        let model = state.config.model.clone();
        let effort = state.config.effort.as_deref().map(|s| s.to_string());
        let tools = if state.tools_enabled { Some(tools::all_tools()) } else { None };
        let max_tokens = state.config.max_tokens;
        (model, effort, tools, max_tokens)
    };

    let tc: Option<&str> = if tools.is_some() { Some("auto") } else { None };

    let system = if system.is_empty() { None } else { Some(system) };

    // Spawn the API request. Messages come from prepare_and_compact in OpenAI format.
    tokio::spawn(async move {
        let result = api::chat_stream(
            &cfg, &model, system, messages, true, effort.as_deref(),
            tools, tc, max_tokens, None, Some(&uid), stream_tx.clone(),
        ).await;

        if let Err(e) = result {
            let _ = stream_tx.send(StreamEvent::Error(format!("{}", e))).await;
        }
    });
}

/// Process user message input: session init, skill matching, context push, start loop.
///
/// Precondition: input is non-empty and not a slash command (slash commands
/// are handled by the TUI side before calling this).
pub fn handle_send_message(state: &mut AgentState, input: &str, tx: mpsc::Sender<StreamEvent>) {
    if input.trim().is_empty() { return; }

    // Guard: if tools are awaiting confirmation, don't send new input
    if !state.pending_tools.is_empty() {
        let _ = tx.blocking_send(StreamEvent::Error(
            "Please confirm or reject pending tool calls first.".into()
        ));
        return;
    }

    log::info!("user: {} ({} chars)", input.chars().take(80).collect::<String>(), input.len());

    if config::is_command(input) {
        let result = match input.trim().split(' ').next().unwrap_or(input.trim()) {
            "/model" => config::handle_model_command(input, &mut state.config),
            "/effort" => config::handle_effort_command(input, &mut state.config),
            "/lang" | "/prompt" => config::handle_lang_command(input, &mut state.config),
            "/profile" => config::handle_profile_command(input, &mut state.config),
            "/reset" => config::handle_reset_command(input, &mut state.config),
            "/re-config" => config::handle_reconfig_command(input, &mut state.config),
            "/fix" => {
                state.health.has_orphan_tool_uses = false;
                let removed = state.ctx.remove_last_step_if_incomplete();
                let mut repairs: Vec<String> = Vec::new();
                if removed { repairs.push("removed incomplete step".into()); }
                if let Err(e) = state.ctx.validate() { repairs.push(e); }
                state.tool_failures = 0;
                state.tool_calls_this_turn = 0;
                Some(if repairs.is_empty() { "Context OK — no repair needed.".into() }
                    else { format!("Context repaired: {}", repairs.join("; ")) })
            }
            "/clear" => {
                state.ctx = crate::assembly::ContextAssembler::new();
                Some("Conversation cleared.".into())
            }
            "/tools" => {
                state.tools_enabled = !state.tools_enabled;
                Some(format!("Tools: {}", if state.tools_enabled { "enabled" } else { "disabled" }))
            }
            "/auto" => {
                state.config.auto_mode = !state.config.auto_mode;
                state.auto_mode = state.config.auto_mode;
                crate::tools::AUTO_MODE.store(state.auto_mode, std::sync::atomic::Ordering::Relaxed);
                state.config.save();
                Some(format!("Auto mode: {}", if state.auto_mode { "on" } else { "off" }))
            }
            "/dev" => {
                state.dev_mode = !state.dev_mode;
                Some(format!("Dev mode: {}", if state.dev_mode { "on" } else { "off" }))
            }
            _ => Some(format!("Unknown command: '{}'", input.trim())),
        };
        if let Some(msg) = result {
            let _ = tx.blocking_send(StreamEvent::ContentDelta(msg));
        }
        return;
    }

    // Generate seed on first message (not at session start)
    if state.session_seed.is_empty() {
        state.session_seed = session::generate_seed();
        crate::dsc_log::set_session(&state.session_seed);
        crate::tools::set_current_session(&state.session_seed);

        state.session_start = session::now_epoch();
        // Auto mode: detect initial phase from first user message
        if state.auto_mode {
            let phase = crate::router::detect_initial_phase(input);
            crate::router::set_phase(phase, dsx_types::DebugLevel::Medium);
            log::info!("auto initial phase: {:?} from '{}'", phase, input.chars().take(40).collect::<String>());
        }
        session::save_live_snapshot(
            &state.session_seed, &state.ctx.to_vec(),
            &state.config.model, state.config.effort.as_deref(), None,
        );
    }

    // Refresh balance every turn
    if state.config.is_ready() {
        let cfg = state.config.clone();
        let btx = tx.clone();
        tokio::spawn(async move {
            if let Ok(info) = api::get_balance(&cfg).await {
                if let Some(b) = info.balance_infos.first() {
                    let _ = btx.send(StreamEvent::BalanceResult(format!(" {} {}", b.total_balance, b.currency))).await;
                }
            }
        });
    }

    // Match skills against user input
    let matched = state.skill_index.match_skills(input);
    state.active_skill_bodies.clear();
    for skill in matched {
        if let Some(body) = state.skill_index.load_skill_body(&skill.name) {
            state.active_skill_bodies.push((skill.name.clone(), body));
        }
    }

    // Push user message to context
    let _ = state.ctx.push_user(input);


    // Reset per-turn state
    state.tool_results.clear();
    state.tool_code_content.clear();
    state.tool_code_path.clear();
    state.tool_code_action.clear();
    state.tool_code_status = None;
    state.tool_failures = 0;
    state.tool_calls_this_turn = 0;
    state.files_written_this_turn.clear();

    handle_start_agent_loop(state, tx);
}
