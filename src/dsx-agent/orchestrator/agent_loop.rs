use std::time::Instant;
use tokio::sync::mpsc;

use crate::api::{self, StreamEvent};
use crate::config;
use crate::health::gate::{check_gate, GateContext, GateResult};
use crate::session;
use crate::tools;
use crate::tui::app::App;
use crate::tui::Mode;
use crate::types::AgentPhase;

pub fn handle_start_agent_loop(app: &mut App, stream_tx: mpsc::Sender<StreamEvent>) {
    // Sync flat messages from Assembler before anything reads them
    app.sync_messages();
    // Guard: don't start a new request while one is in-flight
    if app.mode == Mode::Streaming {
        log::warn!("handle_start_agent_loop: already streaming, skipping duplicate request");
        return;
    }
    // Guard: don't restart while tools await confirmation
    if app.mode == Mode::ToolConfirm || app.mode == Mode::DangerPrompt {
        log::warn!("handle_start_agent_loop: tools pending confirmation, skipping");
        return;
    }
    // Guard: exec has been spawned but result not yet returned
    if app.exec_pending > 0 {
        log::warn!("handle_start_agent_loop: {} exec(s) pending, waiting for results", app.exec_pending);
        app.mode = Mode::Chat;
        app.status_msg = format!("Waiting for {} exec(s) to complete...", app.exec_pending);
        return;
    }
    // Pre-flight sandbox validation — intercept 400 risks before the context is polluted
    let gate_ctx = GateContext {
        assembler: &app.ctx,
        has_orphan_tool_uses: app.health.has_orphan_tool_uses,
    };
    match check_gate(&gate_ctx) {
        GateResult::Pass => {}
        GateResult::Block { reason, repairable } => {
            log::error!("sandbox blocked: {}", reason);
            app.mode = Mode::Chat;
            app.status_msg = if repairable {
                format!("⚠ Blocked: {} — type /fix to repair", reason)
            } else {
                format!("⚠ Blocked: {}", reason)
            };
            return;
        }
    }
    app.mode = Mode::Streaming;
    app.stream_content.clear();
    app.stream_reasoning.clear();
    app.stream_tool_progress.clear();
    app.stream_cancelled = false;
    app.status_msg = String::new();
    app.last_activity = Instant::now();
    app.health.reset_turn();
    app.flush_notes();

    let cfg = app.config.clone();
    let thinking = true;
    let uid = app.session_seed.clone();

    let mut prepared = app.prepare_and_compact();
    log::debug!("handle_start_agent_loop auto_mode={}", app.auto_mode);

    // Auto mode: override model/effort/tools/max_tokens based on AI-declared phase
    let (model, effort, tools, max_tokens) = if app.auto_mode {
        let phase = crate::router::read_phase();
        let level = crate::router::read_debug_level();
        app.current_task_phase = phase;
        let pc = crate::router::phase_config(phase, level);

        // Inject phase prompt suffix into the first system message
        if let Some(suffix) = crate::router::phase_prompt_suffix(phase, &app.config.prompt_lang) {
            for msg in prepared.iter_mut() {
                if msg.role == "system" {
                    if let Some(ref mut content) = msg.content {
                        content.push_str(suffix);
                    }
                    break;
                }
            }
        }

        let model = pc.model.to_string();
        let effort = pc.effort.map(|s| s.to_string());
        // Sync to config for top bar display (don't persist — auto-only)
        app.config.model = model.clone();
        app.config.effort = effort.clone();
        let tools = Some(tools::tools_for_phase(phase));
        let max_tokens = pc.max_tokens;
        log::info!("auto mode: {:?} {} {} {}K", phase, model, effort.as_deref().unwrap_or("?"), max_tokens / 1000);
        (model, effort, tools, max_tokens)
    } else {
        let model = app.config.model.clone();
        let effort = app.config.effort.as_deref().map(|s| s.to_string());
        let tools = if app.tools_enabled { Some(tools::all_tools()) } else { None };
        let max_tokens = app.config.max_tokens;
        (model, effort, tools, max_tokens)
    };

    let tc: Option<&str> = if tools.is_some() { Some("auto") } else { None };

    // Pre-flight: verify tool_use/tool_result pairing — log tool_use without results
    for (i, msg) in prepared.iter().enumerate() {
        if msg.role == "assistant" {
            if let Some(ref tcs) = msg.tool_calls {
                let next_has_result = prepared.get(i + 1).map(|m| m.role == "tool").unwrap_or(false);
                if !next_has_result {
                    let names: Vec<&str> = tcs.iter().map(|t| t.function.name.as_str()).collect();
                    log::warn!("pre-flight: assistant[{}] {} tool_use(s) [{}] with NO following tool_result", i, tcs.len(), names.join(","));
                }
            }
        }
    }

    let (system, anthropic_msgs) = api::convert_messages(&prepared);
    tokio::spawn(async move {
        let result = api::chat_stream(
            &cfg, &model, system, anthropic_msgs, thinking, effort.as_deref(),
            tools, tc, max_tokens, None, None, Some(&uid), stream_tx.clone(),
        ).await;

        if let Err(e) = result {
            let _ = stream_tx.send(StreamEvent::Error(format!("{}", e))).await;
        }
    });
}

pub fn handle_send_message(app: &mut App, tx: mpsc::Sender<StreamEvent>) {
    let input = std::mem::take(&mut app.input);
    app.input_cursor = 0;
    if input.trim().is_empty() { return; }

    // Guard: don't accept input while streaming
    if app.mode == Mode::Streaming {
        app.status_msg = "Waiting for response...".into();
        return;
    }

    // Guard: if tools are awaiting confirmation, don't send new input
    if !app.pending_tools.is_empty() {
        app.status_msg = "先确认或拒绝待处理的工具调用。".into();
        return;
    }

    log::info!("user: {} ({} chars)", input.chars().take(80).collect::<String>(), input.len());

    if config::is_command(&input) {
        crate::tui::commands::handle_slash_command(app, &input, tx);
        return;
    }

    // Generate seed on first message (not at session start)
    if app.session_seed.is_empty() {
        app.session_seed = session::generate_seed();
        crate::dsc_log::set_session(&app.session_seed);
        crate::tools::set_current_session(&app.session_seed);
        app.short_mem = crate::memory::ShortTermMemory::new(&app.session_seed, &app.config.model);
        app.session_start = session::now_epoch();
        // Auto mode: detect initial phase from first user message
        if app.auto_mode {
            let phase = crate::router::detect_initial_phase(&input);
            crate::router::set_phase(phase, crate::types::DebugLevel::Medium);
            log::info!("auto initial phase: {:?} from '{}'", phase, input.chars().take(40).collect::<String>());
        }
        session::save_live_snapshot(
            &app.session_seed, &app.messages,
            &app.config.model, app.config.effort.as_deref(), None,
            None, // [MEMORY FROZEN] semantic_memory
        );
    }

    // Refresh balance every turn
    if app.config.is_ready() {
        let cfg = app.config.clone();
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
    let matched = app.skill_index.match_skills(&input);
    app.active_skill_bodies.clear();
    for skill in matched {
        if let Some(body) = app.skill_index.load_skill_body(&skill.name) {
            app.active_skill_bodies.push((skill.name.clone(), body));
        }
    }
    let _ = app.ctx.push_user(&input);
    app.sync_messages();
    app.tool_results.clear();
    app.tool_code_content.clear();
    app.tool_code_path.clear();
    app.tool_code_action.clear();
    app.tool_code_status = None;
    app.phase = AgentPhase::Idle;
    app.phase_count = 0;
    app.auto_scroll = true;
    app.tool_failures = 0;
    app.tool_calls_this_turn = 0;
    app.files_written_this_turn.clear();
    handle_start_agent_loop(app, tx);
}
