//! dsx-agent — central process. Connects TUI ↔ Agent ↔ Tools ↔ HP.
//!
//! Wires all intelligence modules (session, memory, orchestrator, context,
//! skills, router, tokenizer, health) into the HP-bridged conversation loop.
//!
//! Uses shared dsx-proto types for all IPC channels (TUI, Tools, HP).

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};

use dsx_proto::{
    self, TuiToAgent, AgentToTui, AgentToTools, ToolsToAgent, AgentToHp, HpToAgent,
};

use crate::agent::{AgentState, ToolResultAppender};
use crate::config::Config;
use crate::dsc_log;
use crate::orchestrator::{gates, learning, phase_detector, tracker, turn_scorer, session_persistence};
use crate::router;
use crate::session;
use crate::tokenizer;
use crate::tools;
use crate::tool_parser;
use dsx_types::{Message, ToolCall, ToolDef};

// ── HP connection ──

fn hp_port() -> u16 {
    let path = dsx_types::platform::hp_port_path();
    std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

/// Connect to HP, register, and return the TCP stream.
/// Uses set_nonblocking(false) + single stream for reliable read/write.
fn connect_hp() -> Option<TcpStream> {
    let port = hp_port();
    if port == 0 {
        eprintln!("dsx-agent: HP not running (no hp.port) — continuing without AI");
        return None;
    }

    let mut stream = match TcpStream::connect(format!("127.0.0.1:{port}")) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("dsx-agent: cannot connect HP: {e}");
            return None;
        }
    };

    let pid = std::process::id();
    let reg = AgentToHp::Register {
        kind: "Agent".into(),
        name: "dsx-agent".into(),
        pid,
    };
    let _ = dsx_proto::write_frame(&mut stream, &reg);

    eprintln!("dsx-agent: connected to HP on port {port}");
    Some(stream)
}

// ── Session initialization ──

fn init_session(agent: &mut AgentState, restore_seed: Option<&str>) {
    // Check if a specific session seed was requested via CLI
    if let Some(seed) = restore_seed {
        if let Some(file) = session::load_session(seed) {
            agent.session_seed = file.seed.clone();
            agent.session_start = file.created_at;
            let (ctx, repairs) = crate::assembly::ContextAssembler::from_legacy(file.messages);
            agent.ctx = ctx;
        
            dsc_log::set_session(&agent.session_seed);
            tools::set_current_session(&agent.session_seed);
            eprintln!("dsx-agent: restored session {} ({} msgs)", agent.session_seed, agent.ctx.message_count());
            if !repairs.is_empty() { log::warn!("session restore: {:?} repairs", repairs); }
            return;
        }
        eprintln!("dsx-agent: session {seed} not found, creating new");
    }
    agent.session_seed = session::generate_seed();
    agent.session_start = session::now_epoch();
    dsc_log::set_session(&agent.session_seed);
    tools::set_current_session(&agent.session_seed);
    session::save_live_snapshot(
        &agent.session_seed, &agent.ctx.to_vec(),
        &agent.config.model, agent.config.effort.as_deref(), None);
    eprintln!("dsx-agent: new session {}", agent.session_seed);
}

fn health_status(agent: &AgentState) -> String {
    let assessment = agent.health.assess();
    format!(
        "[{} {} {} | tier:{:?} | {}% | t{}]",
        if assessment.level == crate::health::HealthLevel::Red { "RED" }
        else if assessment.level == crate::health::HealthLevel::Yellow { "YLW" }
        else { "OK" },
        assessment.emotion.emoji(),
        assessment.emotion.label(),
        agent.health.context_tier,
        (assessment.success_rate * 100.0) as u32,
        agent.health.turn,
    )
}

// ── Helper: apply phase config with user overrides ──

fn apply_phase_config(agent: &mut AgentState, phase: dsx_types::TaskPhase, level: dsx_types::DebugLevel) {
    let phase_name = format!("{:?}", phase).to_lowercase();
    if let Some(user_pc) = agent.config.phase_configs.get(&phase_name) {
        agent.config.model = user_pc.model.clone();
        agent.config.effort = user_pc.effort.clone().filter(|e| !e.is_empty());
        agent.config.max_tokens = user_pc.max_tokens;
        agent.config.context_limit = user_pc.context_limit;
    } else {
        let pc = router::phase_config(phase, level);
        agent.config.model = pc.model.to_string();
        agent.config.effort = pc.effort.map(|s| s.to_string());
        agent.config.max_tokens = pc.max_tokens;
    }
}

// ── Helper: write a tool_result JSON-LP frame to TUI ──

fn emit_tool_result(w: &mut impl Write, id: &str, name: &str, content: &str, success: bool) {
    let frame = serde_json::json!({
        "type": "tool_result",
        "id": id,
        "name": name,
        "content": content,
        "success": success,
    });
    let _ = writeln!(w, "{}", serde_json::to_string(&frame).unwrap_or_default());
    let _ = w.flush();
}

// ── User input handler (fully wired) ──

/// Handle a TuiToAgent::UserInput frame with full module integration.
fn handle_user_input(
    agent: &mut AgentState,
    text: &str,
    hp: &mut TcpStream,
    tui_writer: &mut impl Write,
    tui_reader: &mut impl BufRead,
) {
    // ── Handle ask_user response ──
    if let Some(tool_call_id) = agent.pending_ask_user.take() {
        let _ = agent.ctx.push_tool_result(&tool_call_id, text);
        // Skip session init, skill matching, push_user — this is a tool continuation
    } else {
        if text.is_empty() { return; }

        // ── Session init on first message ──
        if agent.session_seed.is_empty() {
            let seed = agent.resume_seed.clone();
            init_session(agent, seed.as_deref());
            if seed.is_some() {
                let msg_count = agent.ctx.message_count();
                let summary = agent.ctx.turns().last()
                    .and_then(|t| t.steps.last())
                    .and_then(|s| s.assistant.content.as_ref())
                    .map(|c| c.chars().take(100).collect::<String>())
                    .unwrap_or_default();
                let tokens_used = agent.token_estimate;
                let cache_hit_pct = agent.cache_hit_pct;
                dsx_proto::write_line(tui_writer, &format!(
                    r#"{{"type":"session_restored","seed":"{}","message_count":{},"summary":"{}","tokens_used":{},"cache_hit_pct":{}}}"#,
                    agent.session_seed, msg_count,
                    summary.replace('"', "\\\"").replace('\n', "\\n"),
                    tokens_used, cache_hit_pct,
                ));
            }
            if agent.auto_mode {
                let phase = router::detect_initial_phase(text);
                let level = dsx_types::DebugLevel::Medium;
                router::set_phase(phase, level);
                agent.current_task_phase = phase;
                apply_phase_config(agent, phase, level);
                let phase_name = format!("{:?}", phase).to_lowercase();
                let _ = dsx_proto::write_line(tui_writer, &format!(r#"{{"type":"phase_changed","phase":"{}"}}"#, phase_name));
                log::info!("auto initial phase: {:?} model={} effort={:?}", phase, agent.config.model, agent.config.effort);
            }
        }

        // ── Skill matching ──
        let matched = agent.skill_index.match_skills(text);
        agent.active_skill_bodies.clear();
        for skill in &matched {
            if let Some(body) = agent.skill_index.load_skill_body(&skill.name) {
                agent.active_skill_bodies.push((skill.name.clone(), body));
            }
        }

        // ── Push user message to ContextAssembler ──
        let _ = agent.ctx.push_user(text);
    }


    // ── Reset per-turn state ──
    agent.tool_results.clear();
    agent.tool_code_content.clear();
    agent.tool_code_path.clear();
    agent.tool_code_action.clear();
    agent.tool_code_status = None;
    agent.tool_failures = 0;
    agent.tool_calls_this_turn = 0;
    agent.files_written_this_turn.clear();
    agent.monitor.tool_calls_this_turn = 0;

    // Live snapshot after user message
    session::save_live_snapshot(
        &agent.session_seed, &agent.ctx.to_vec(),
        &agent.config.model, agent.config.effort.as_deref(), None);

    // ── Tool-calling loop ──
    for _round in 0..agent.max_tool_rounds {
        if agent.stream_cancelled {
            agent.stream_cancelled = false;
            agent.system_note("system", "用户终止了当前操作。".to_string());
            break;
        }

        // Repair + compact context
        
        let (system, messages, breakdown) =
            crate::assembly::build_context(agent);

        // Send cache prediction to TUI
        let _ = dsx_proto::write_frame(tui_writer, &AgentToTui::CachePrediction { hit_rate: agent.predicted_cache_hit_pct });

        // Filter out system-role messages (sent as `system` field above)
        let msgs_no_system: Vec<&Message> = messages.iter().filter(|m| m.role != "system").collect();
        let messages_json = serde_json::to_value(&msgs_no_system).unwrap_or_default();
        let token_est = tokenizer::estimate_messages_tokens(&messages);
        log::debug!("turn round={} messages={} tokens={}", _round, messages.len(), token_est);

        // Send api_chat to HP
        let chat = AgentToHp::ApiChat {
            model: agent.config.model.clone(),
            system: Some(system),
            messages: messages_json,
            effort: agent.config.effort.clone(),
            max_tokens: Some(agent.config.max_tokens),
            tools: Some(serde_json::to_value(&agent.tool_defs).unwrap_or_default()),
        };
        let _ = dsx_proto::write_frame(hp, &chat);

        // ── Read HP response (streaming loop) ──
        let mut hp_reader = BufReader::new(&mut *hp);
        let mut content = String::new();
        let mut reasoning_content: Option<String> = None;
        let mut thinking_signature: Option<String> = None;
        let mut usage: Option<dsx_types::UsageInfo> = None;
        let mut tcs_raw = serde_json::Value::Null;

        loop {
            let hp_resp: HpToAgent = match dsx_proto::read_frame(&mut hp_reader) {
                Ok(Some(r)) => r,
                Ok(None) => {
                    eprintln!("dsx-agent: HP connection closed (EOF)");
                    agent.health.record_api_error();
                    return;
                }
                Err(e) => {
                    eprintln!("dsx-agent: HP parse error: {e}");
                    agent.health.record_api_error();
                    return;
                }
            };

            match hp_resp {
                HpToAgent::ContentDelta { delta, reasoning } => {
                    if agent.stream_cancelled || crate::tools::CANCEL.load(std::sync::atomic::Ordering::SeqCst) { eprintln!("dsx-agent: streaming cancelled"); return; }
                    if _round == 0 { eprintln!("dsx DEBUG: hp.ContentDelta d={} r={}", delta.len(), reasoning.as_ref().map(|s| s.len()).unwrap_or(0)); }

                    let _ = dsx_proto::write_frame(tui_writer, &AgentToTui::ContentDelta {
                        delta: delta.clone(),
                        reasoning: reasoning.clone(),
                    });
                    // Accumulate for phase detection
                    if let Some(ref r) = reasoning {
                        agent.stream_reasoning.push_str(r);
                    }
                    agent.stream_content.push_str(&delta);
                }
                HpToAgent::ToolProgress { id, content: prog_content, stream_type } => {
                    let _ = dsx_proto::write_frame(tui_writer, &AgentToTui::ToolProgress {
                        id: id.clone(),
                        content: prog_content.clone(),
                        stream_type: stream_type.clone(),
                    });
                }
                HpToAgent::ApiResponse {
                    content: c, tool_calls, stop_reason: _,
                    reasoning_content: rc, thinking_signature: ts, usage: u,
                } => {
                    content = c;
                    reasoning_content = rc;
                    thinking_signature = ts;
                    usage = u;
                    tcs_raw = tool_calls.unwrap_or(serde_json::Value::Null);
                    break;
                }
                HpToAgent::Error { message } => {
                    let _ = dsx_proto::write_frame(tui_writer, &AgentToTui::Error { message: message.clone() });
                    agent.health.record_api_error();
                    return;
                }
                _ => { /* ignore non-stream frames */ }
            }
        }

        // ── Process final api_response ──
        agent.health.record_api_success(&agent.config.model);

        // Parse tool calls (native Anthropic tool_use blocks)
        let mut parsed: Vec<ToolCall> = tool_parser::parse_tool_calls(&tcs_raw);

        // DeepSeek v4 outputs <tool_use> XML blocks in text content instead of
        // native Anthropic tool_use blocks. Parse them as fallback.
        if parsed.is_empty() && (content.contains("<tool_use>") || content.contains("<read>") || content.contains("<exec>") || content.contains("<write>") || content.contains("<search>")) {
            let tool_names: Vec<String> = agent.tool_defs.iter().map(|t| t.function.name.clone()).collect();
            let (cleaned, xml_tcs) = tool_parser::parse_xml_tool_calls(&content, &tool_names);
            content = cleaned;
            parsed = xml_tcs;
        }

        let has_tools = !parsed.is_empty();

                // Usage tracking
                if let Some(ref u) = usage {
                    agent.api_usage = Some(u.clone());
                    agent.session_tokens += u.total_tokens as u64;
                }
                agent.token_estimate = breakdown.total;
                agent.token_breakdown = Some(breakdown);
                agent.health.context_tokens = agent.tokens_used();
                agent.health.context_tier = crate::health::ContextTier::from_tokens(
                    agent.health.context_tokens, agent.config.context_limit,
                );

                // Phase detection from reasoning
                if let Some(ref r) = reasoning_content {
                    if agent.auto_mode {
                        let tp = phase_detector::detect_task_phase_from_reasoning(r);
                        if tp != agent.current_task_phase {
                            agent.current_task_phase = tp;
                            router::set_phase(tp, router::read_debug_level());
                            // Apply phase-specific config
                            let level = dsx_types::DebugLevel::Medium;
                            apply_phase_config(agent, tp, level);
                            let phase_name = format!("{:?}", tp).to_lowercase();
                            let _ = dsx_proto::write_line(tui_writer, &format!(r#"{{"type":"phase_changed","phase":"{}"}}"#, phase_name));
                        }
                    }
                }

                // Build and push assistant Message
                let assistant_msg = Message {
                    role: "assistant".into(),
                    content: if content.is_empty() && !parsed.is_empty() { None } else { Some(content.clone()) },
                    name: None,
                    tool_calls: if parsed.is_empty() { None } else { Some(parsed.clone()) },
                    tool_call_id: None,
                    reasoning_content: reasoning_content.clone(),
                    thinking_signature,
                };

                if let Err(e) = agent.ctx.push_assistant(assistant_msg.clone()) {
                    log::error!("push_assistant failed: {:?} — repairing", e);
                                        agent.ctx.push_assistant_restore(assistant_msg.clone());
                }
            

                if !has_tools {
                    // ── Final response ──
                    let final_reasoning = (!agent.stream_reasoning.is_empty())
                        .then(|| agent.stream_reasoning.clone())
                        .or_else(|| reasoning_content.clone());
                    agent.turn_scores.push(turn_scorer::score_current_turn(agent));
                    agent.stream_content.clear();
                    agent.stream_reasoning.clear();

                    // extract_reasoning_insights removed with memory module
                    learning::auto_extract_memory(agent, &assistant_msg);

                    agent.health.record_turn(false);
                    agent.health_status_line = health_status(agent);
                    agent.health.reset_turn();

                    session::save_live_snapshot(
                        &agent.session_seed, &agent.ctx.to_vec(),
                        &agent.config.model, agent.config.effort.as_deref(), None,
                    );
                    session_persistence::maybe_save_session(agent);

                    // Send structured response to TUI using proto type
                    let tui_resp = AgentToTui::ApiResponse {
                        content,
                        tool_calls: None,
                        stop_reason: None,
                        usage,
                        reasoning_content: final_reasoning,
                    };
                    let _ = dsx_proto::write_frame(tui_writer, &tui_resp);
                    return;
                }

                // ── Tool call round ──
                agent.tool_calls_this_turn += parsed.len() as u32;
                agent.monitor.tool_calls_this_turn = agent.tool_calls_this_turn;

                session::save_live_snapshot(
                    &agent.session_seed, &agent.ctx.to_vec(),
                    &agent.config.model, agent.config.effort.as_deref(), None,
                );

                for tc in &parsed {
                    let name = &tc.function.name;
                    let id = &tc.id;
                    let args = &tc.function.arguments;

                    // ── Gates ──
                    if gates::phase_check_tool(agent, name, id) {
                        emit_tool_result(tui_writer, id, name, "[BLOCKED] Phase gate prevented this tool.", false);
                        continue;
                    }
                    if gates::explore_gate(agent, name, id, args) {
                        emit_tool_result(tui_writer, id, name, "[BLOCKED] Explore gate prevented this tool.", false);
                        continue;
                    }
                    if let Some(err_msg) = gates::pre_tool_health_check(agent, name) {
                        let _ = agent.ctx.push_tool_result(id, &err_msg);
                        agent.health.record_error(name, &err_msg);
                        agent.tool_failures += 1;
                        emit_tool_result(tui_writer, id, name, &err_msg, false);
                        continue;
                    }

                    // ── Intercept status — apply phase change directly ──
                    if name == "status" {
                        let args_val: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
                        let state = args_val.get("state").and_then(|v| v.as_str()).unwrap_or("coding");
                        // Reject removed modes with clear feedback
                        if state == "explore" || state == "chat" {
                            let err = format!("[ERROR] Mode '{state}' no longer exists. Use: plan, coding, debug");
                            let _ = agent.ctx.push_tool_result(id, &err);
                            agent.tool_results.push((id.to_string(), err.clone()));
                            emit_tool_result(tui_writer, id, name, &err, false);
                            continue;
                        }
                        let tp = match state {
                            "plan" => dsx_types::TaskPhase::Plan,
                            "coding" => dsx_types::TaskPhase::Coding,
                            "debug" => dsx_types::TaskPhase::Debug,
                            _ => dsx_types::TaskPhase::Coding,
                        };
                        let level = dsx_types::DebugLevel::Medium;
                        agent.current_task_phase = tp;
                        router::set_phase(tp, level);
                        if agent.auto_mode {
                            apply_phase_config(agent, tp, level);
                        }
                        let phase_name = format!("{:?}", tp).to_lowercase();
                        let _ = dsx_proto::write_line(tui_writer, &format!(r#"{{"type":"phase_changed","phase":"{}"}}"#, phase_name));
                        let result = format!("[OK] Switched to {} mode", state);
                        let _ = agent.ctx.push_tool_result(id, &result);
                        agent.tool_results.push((id.to_string(), result.clone()));
                        emit_tool_result(tui_writer, id, name, &result, true);
                        continue;
                    }

                    // ── Intercept ask_user — ask TUI instead of tools subprocess ──
                    if name == "ask_user" || name == "ask" {
                        let args_val: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
                        let question = args_val.get("question")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                            .unwrap_or_default();
                        let options = args_val.get("options")
                            .and_then(|v| v.as_array())
                            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>())
                            .filter(|v| !v.is_empty());

                        // Send AskUser frame to TUI
                        let frame = AgentToTui::AskUser {
                            id: id.to_string(),
                            question,
                            options,
                        };
                        let _ = dsx_proto::write_frame(tui_writer, &frame);

                        agent.pending_ask_user = Some(id.to_string());

                        // Add placeholder result to ctx so the model sees it
                        let _ = agent.ctx.push_tool_result(id, "[ASK_USER] Awaiting your response in the Tauri interface.");
                        agent.tool_results.push((id.to_string(), "[ASK_USER] Awaiting your response in the Tauri interface.".to_string()));
                        emit_tool_result(tui_writer, id, name, "[ASK_USER] Awaiting your response.", false);
                        continue; // Skip calling execute_tool
                    }

                    // ── Pre-cancel check ──
                    if crate::tools::CANCEL.load(std::sync::atomic::Ordering::SeqCst) {
                        crate::tools::CANCEL.store(false, std::sync::atomic::Ordering::SeqCst);
                        agent.system_note("system", "用户取消了工具执行。".to_string());
                        // Append a cancelled result so the turn isn't orphaned on session save/restore
                        let cancel_msg = "[CANCELLED] Tool execution cancelled by user.\n[HINT] This tool was not executed.";
                        let _ = agent.ctx.push_tool_result(&tc.id, cancel_msg);
                        emit_tool_result(tui_writer, id, name, cancel_msg, false);
                        continue;
                    }

                    // ── Execute tool via IPC (tools.rs owns the pipe) ──
                    let tr_content = crate::tools::execute_tool_with_id(name, "", args, id);
                    // If tools IPC is dead, append this error and stop — don't cascade
                    if tr_content.contains("tools IPC") || tr_content.contains("not initialised") {
                        let mut appender = ToolResultAppender::new(agent);
                        appender.append(name, &tc.id, args, &tr_content);
                        emit_tool_result(tui_writer, id, name, &tr_content, false);
                        break;
                    }
                    let tr_success = !tr_content.starts_with("[ERROR]") && !tr_content.starts_with("[FAIL]");
                    let failed = !tr_success;

                    // ── Health tracking ──
                    agent.health.record_tool_call(name);
                    agent.monitor.tool_calls_this_turn += 1;
                    if failed {
                        agent.tool_failures += 1;
                        agent.health.record_error(name, &tr_content);
                    }
                    gates::post_tool_health_record(agent, name, !failed);

                    // ── ToolResultAppender (single entry point) ──
                    {
                        let mut appender = ToolResultAppender::new(agent);
                        appender.append(name, &tc.id, args, &tr_content);
                    }
                    emit_tool_result(tui_writer, id, name, &tr_content, tr_success);

                    // ── Tracker ──
                    tracker::track_tool_code(agent, name, args, &tr_content);
                    if !failed && name == "file"
                        && crate::orchestrator::arg_parser::tool_action(args) == "write"
                    {
                        tracker::track_file_written(agent, args);
                    }

                    // ── Side effects ──
                    if name == "exec" && crate::orchestrator::arg_parser::tool_action(args) == "explore" {
                        agent.has_explored = true;
                    }
                    if name == "file" && crate::orchestrator::arg_parser::tool_action(args) == "read" {
                        agent.turns_since_last_read = 0;
                    }

                    // ── Emit ToolState after tool execution ──
                    let tool_state_frame = AgentToTui::ToolState {
                        explored: agent.has_explored,
                        declared_files: agent.ctx.to_vec().iter()
                            .filter_map(|m| m.tool_calls.as_ref())
                            .flatten()
                            .filter(|tc| tc.function.name == "explore")
                            .map(|tc| format!("{}: {}", tc.function.name, tc.function.arguments))
                            .collect(),
                        read_files: agent.ctx.to_vec().iter()
                            .filter_map(|m| m.tool_calls.as_ref())
                            .flatten()
                            .filter(|tc| tc.function.name.starts_with("read"))
                            .map(|tc| tc.function.arguments.clone())
                            .collect(),
                        written_this_turn: agent.files_written_this_turn.clone(),
                    };
                    let _ = dsx_proto::write_frame(tui_writer, &tool_state_frame);
                }

            

                // Safety gate: 3 consecutive failures
                if agent.tool_failures >= 3 {
                    log::warn!("safety gate: 3 consecutive tool failures");
                    agent.turn_scores.push(turn_scorer::score_current_turn(agent));
                    agent.turn_annotations.push("[System] 3 consecutive tool failures. Respond with analysis — do not call more tools.".to_string());
                
                    agent.tool_failures = 0;
                }
        // Continue loop — next api_chat includes tool results
    }

    // ── Post-tool-loop: one more API call to let the model wrap up ──
    // Even with tools:None, DeepSeek V4 may output DSML tool calls inline.
    // We parse DSML/XML here so raw markup doesn't appear in the chat,
    // and if valid tool calls are found, execute them in a mini-loop.

    agent.turn_annotations.push(format!("[System] Max tool rounds ({}) reached. Respond with what you have.", agent.max_tool_rounds));

    let max_post_rounds = 3u32;
    for _post_round in 0..max_post_rounds {
        let (system, messages, _breakdown) =
            crate::assembly::build_context(agent);

        // Send cache prediction to TUI
        let _ = dsx_proto::write_frame(tui_writer, &AgentToTui::CachePrediction { hit_rate: agent.predicted_cache_hit_pct });

        let messages_json = serde_json::to_value(&messages).unwrap_or_default();

        let chat = AgentToHp::ApiChat {
            model: agent.config.model.clone(),
            system: Some(system),
            messages: messages_json,
            effort: agent.config.effort.clone(),
            max_tokens: Some(agent.config.max_tokens),
            tools: None,
        };
        let _ = dsx_proto::write_frame(hp, &chat);

        let mut hp_reader = BufReader::new(&mut *hp);
        let mut content = String::new();
        let mut reasoning_content: Option<String> = None;
        let mut thinking_signature: Option<String> = None;
        let mut usage: Option<dsx_types::UsageInfo> = None;
        let mut tcs_raw = serde_json::Value::Null;

        loop {
            let hp_resp: HpToAgent = match dsx_proto::read_frame(&mut hp_reader) {
                Ok(Some(r)) => r,
                Ok(None) => {
                    eprintln!("dsx-agent: HP connection closed (EOF)");
                    agent.health.record_api_error();
                    return;
                }
                Err(e) => {
                    eprintln!("dsx-agent: HP parse error: {e}");
                    agent.health.record_api_error();
                    return;
                }
            };

            match hp_resp {
                HpToAgent::ContentDelta { delta, reasoning } => {
                    if agent.stream_cancelled || crate::tools::CANCEL.load(std::sync::atomic::Ordering::SeqCst) { eprintln!("dsx-agent: streaming cancelled"); return; }
                    let _ = dsx_proto::write_frame(tui_writer, &AgentToTui::ContentDelta {
                        delta: delta.clone(),
                        reasoning: reasoning.clone(),
                    });
                    if let Some(ref r) = reasoning {
                        agent.stream_reasoning.push_str(r);
                    }
                    agent.stream_content.push_str(&delta);
                }
                HpToAgent::ApiResponse {
                    content: c, tool_calls,
                    stop_reason: _,
                    reasoning_content: rc, thinking_signature: ts, usage: u,
                } => {
                    content = c;
                    tcs_raw = tool_calls.unwrap_or(serde_json::Value::Null);
                    reasoning_content = rc;
                    thinking_signature = ts;
                    usage = u;
                    break;
                }
                HpToAgent::Error { message } => {
                    let _ = dsx_proto::write_frame(tui_writer, &AgentToTui::Error { message: message.clone() });
                    agent.health.record_api_error();
                    return;
                }
                _ => { /* ignore non-stream frames */ }
            }
        }

        agent.health.record_api_success(&agent.config.model);

        // ── Parse DSML/XML tool calls from content ──
        let mut parsed: Vec<ToolCall> = tool_parser::parse_tool_calls(&tcs_raw);
        if content.contains("\u{ff5c}DSML\u{ff5c}tool_calls") {
            let (cleaned, dsml_tcs) = tool_parser::parse_dsml_tool_calls(&content, &agent.tool_defs);
            content = cleaned;
            parsed = dsml_tcs;
        }
        if parsed.is_empty() && (content.contains("<tool_use>") || content.contains("<read>") || content.contains("<exec>") || content.contains("<write>") || content.contains("<search>")) {
            let tool_names: Vec<String> = agent.tool_defs.iter().map(|t| t.function.name.clone()).collect();
            let (cleaned, xml_tcs) = tool_parser::parse_xml_tool_calls(&content, &tool_names);
            content = cleaned;
            parsed = xml_tcs;
        }

        if parsed.is_empty() {
            // No tool calls → this is the final answer
            let assistant_msg = Message {
                role: "assistant".into(),
                content: if content.is_empty() { None } else { Some(content.clone()) },
                name: None,
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: reasoning_content.clone(),
                thinking_signature,
            };

            let msg_for_learning = assistant_msg.clone();
            if let Err(e) = agent.ctx.push_assistant(assistant_msg) {
                log::error!("push_assistant failed: {:?} — repairing", e);
                agent.ctx.push_assistant_restore(msg_for_learning.clone());
            }

            let final_reasoning = (!agent.stream_reasoning.is_empty())
                .then(|| agent.stream_reasoning.clone())
                .or_else(|| reasoning_content.clone());
            agent.turn_scores.push(turn_scorer::score_current_turn(agent));
            agent.stream_content.clear();
            agent.stream_reasoning.clear();

            // extract_reasoning_insights removed with memory module
            learning::auto_extract_memory(agent, &msg_for_learning);

            agent.health.record_turn(false);

            let _ = dsx_proto::write_frame(tui_writer, &AgentToTui::ApiResponse {
                content,
                reasoning_content: final_reasoning,
                tool_calls: None,
                stop_reason: None,
                usage,
            });

            break; // exit post-tool-loop
        }

        // ── DSML/XML tool calls found — execute them ──
        // Do NOT send ApiResponse here — tools are still running.
        // The TUI already sees streaming content via ContentDelta frames.
        // ApiResponse is sent only when the model returns no more tools.

        let assistant_msg = Message {
            role: "assistant".into(),
            content: if content.is_empty() && !parsed.is_empty() { None } else { Some(content.clone()) },
            name: None,
            tool_calls: Some(parsed.clone()),
            tool_call_id: None,
            reasoning_content: reasoning_content.clone(),
            thinking_signature,
        };

        let msg_for_learning = assistant_msg.clone();
        if let Err(e) = agent.ctx.push_assistant(assistant_msg) {
            log::error!("push_assistant failed: {:?} — repairing", e);
            agent.ctx.push_assistant_restore(msg_for_learning.clone());
        }

        // Execute tools (same pattern as main loop)
        for tc in &parsed {
            let name = &tc.function.name;
            let id = &tc.id;
            let args = &tc.function.arguments;

            if gates::phase_check_tool(agent, name, id) {
                emit_tool_result(tui_writer, id, name, "[BLOCKED] Phase gate prevented this tool.", false);
                continue;
            }
            if gates::explore_gate(agent, name, id, args) {
                emit_tool_result(tui_writer, id, name, "[BLOCKED] Explore gate prevented this tool.", false);
                continue;
            }
            if let Some(err_msg) = gates::pre_tool_health_check(agent, name) {
                let _ = agent.ctx.push_tool_result(id, &err_msg);
                agent.health.record_error(name, &err_msg);
                agent.tool_failures += 1;
                emit_tool_result(tui_writer, id, name, &err_msg, false);
                continue;
            }

            if name == "status" {
                let args_val: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
                let state = args_val.get("state").and_then(|v| v.as_str()).unwrap_or("coding");
                if state == "explore" || state == "chat" {
                    let err = format!("[ERROR] Mode '{state}' no longer exists. Use: plan, coding, debug");
                    let _ = agent.ctx.push_tool_result(id, &err);
                    agent.tool_results.push((id.to_string(), err.clone()));
                    emit_tool_result(tui_writer, id, name, &err, false);
                    continue;
                }
                let tp = match state { "plan" => dsx_types::TaskPhase::Plan, "coding" => dsx_types::TaskPhase::Coding, "debug" => dsx_types::TaskPhase::Debug, _ => dsx_types::TaskPhase::Coding };
                let level = dsx_types::DebugLevel::Medium;
                agent.current_task_phase = tp;
                router::set_phase(tp, level);
                if agent.auto_mode {
                    apply_phase_config(agent, tp, level);
                }
                let _ = dsx_proto::write_frame(tui_writer, &AgentToTui::PhaseChanged { phase: format!("{:?}", tp).to_lowercase() });
                let result = format!("[OK] Switched to {} mode", state);
                let _ = agent.ctx.push_tool_result(id, &result);
                agent.tool_results.push((id.to_string(), result.clone()));
                emit_tool_result(tui_writer, id, name, &result, true);
                continue;
            }

            if name == "ask_user" || name == "ask" {
                let _ = agent.ctx.push_tool_result(id, "[ASK_USER] Awaiting your response in the Tauri interface.");
                emit_tool_result(tui_writer, id, name, "[ASK_USER] Awaiting your response.", false);
                continue;
            }

            if crate::tools::CANCEL.load(std::sync::atomic::Ordering::SeqCst) {
                crate::tools::CANCEL.store(false, std::sync::atomic::Ordering::SeqCst);
                let cancel_msg = "[CANCELLED] Tool execution cancelled by user.";
                let _ = agent.ctx.push_tool_result(&tc.id, cancel_msg);
                emit_tool_result(tui_writer, id, name, cancel_msg, false);
                continue;
            }

            let tr_content = crate::tools::execute_tool_with_id(name, "", args, id);
            if tr_content.contains("tools IPC") || tr_content.contains("not initialised") {
                let mut appender = ToolResultAppender::new(agent);
                appender.append(name, &tc.id, args, &tr_content);
                emit_tool_result(tui_writer, id, name, &tr_content, false);
                break;
            }
            let tr_success = !tr_content.starts_with("[ERROR]") && !tr_content.starts_with("[FAIL]");
            let failed = !tr_success;

            agent.health.record_tool_call(name);
            agent.monitor.tool_calls_this_turn += 1;
            if failed {
                agent.tool_failures += 1;
                agent.health.record_error(name, &tr_content);
            }
            gates::post_tool_health_record(agent, name, !failed);

            {
                let mut appender = ToolResultAppender::new(agent);
                appender.append(name, &tc.id, args, &tr_content);
            }
            emit_tool_result(tui_writer, id, name, &tr_content, tr_success);

            tracker::track_tool_code(agent, name, args, &tr_content);
        }

        // Loop back for another post-round (with updated context)
    }

    // End of post-tool-loop — finalize turn
    agent.health_status_line = health_status(agent);
    agent.health.reset_turn();

    session::save_live_snapshot(
        &agent.session_seed, &agent.ctx.to_vec(),
        &agent.config.model, agent.config.effort.as_deref(), None);
    session_persistence::maybe_save_session(agent);
}
// ── Tools subprocess management ──

/// Spawn the dsx-tools subprocess with piped stdin/stdout.
/// Returns (child, reader, writer) where reader/writer are boxed for
/// later handoff to `tools::init_tools_ipc()`.
fn spawn_tools_process(exe: &std::path::Path) -> (std::process::Child, Box<dyn BufRead + Send>, Box<dyn Write + Send>) {
    let mut tools_cmd = Command::new(
        if std::env::var("DSX_SINGLE_BINARY").is_ok() {
            exe.to_path_buf()
        } else {
            exe.with_file_name("dsx-tools")
        }
    );
    if std::env::var("DSX_SINGLE_BINARY").is_ok() {
        tools_cmd.arg("tools");
    }
    let mut child = tools_cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("dsx-agent: failed to spawn dsx-tools");

    let reader: Box<dyn BufRead + Send> = Box::new(BufReader::new(child.stdout.take().unwrap()));
    let writer: Box<dyn Write + Send> = Box::new(child.stdin.take().unwrap());
    (child, reader, writer)
}

/// Respawn the tools subprocess. Kills any existing process, spawns new one,
/// completes the Init/Ready handshake, and stores the child (with pipes consumed).
fn respawn_tools(child: &mut Option<std::process::Child>) -> bool {
    // Kill old process
    if let Some(mut c) = child.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
    // Spawn new
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return false,
    };
    let (mut new_child, mut reader, mut writer) = spawn_tools_process(&exe);

    // Send init and read Ready
    let init = AgentToTools::Init {
        allowed_tools: vec![], session_seed: "pipe".into(), auto_mode: false,
    };
    let init_ok = dsx_proto::write_frame(&mut writer, &init).is_ok();
    let ready = dsx_proto::read_frame::<ToolsToAgent>(&mut reader).ok().flatten();

    match ready {
        Some(ToolsToAgent::Ready { tools }) => {
            let essential: &[&str] = &["exec","read_file","write_file","edit_file","edit_file_diff","explore","search","list_dir","glance","ask_user","status","task_create","task_update","task_list","plan_create","plan_update","plan_read","plan_list","web_fetch","web_search","git","mem_save","mem_read","mem_forget","recall","pitfall_save","pitfall_guide"];
            let defs: Vec<ToolDef> = tools.iter().filter(|t| essential.contains(&t.function.name.as_str())).cloned().collect();
            tools::init_tools_ipc(reader, writer, defs);
            *child = Some(new_child);
            eprintln!("dsx-agent: tools respawned");
            true
        }
        _ => {
            eprintln!("dsx-agent: tools respawn failed (no Ready)");
            let _ = new_child.kill();
            let _ = new_child.wait();
            false
        }
    }
}

// ── HP reconnect helper ──

/// Try to (re)connect to HP daemon. If port file is stale, spawn a new HP.
fn try_reconnect_hp() -> Option<std::net::TcpStream> {
    let port_path = dsx_types::platform::hp_port_path();

    let port = std::fs::read_to_string(&port_path).ok()
        .and_then(|s| s.trim().parse::<u16>().ok());

    if let Some(p) = port {
        if let Ok(stream) = std::net::TcpStream::connect(format!("127.0.0.1:{p}")) {
            return Some(stream);
        }
        let _ = std::fs::write(&port_path, "");
    }

    // Try to spawn HP from known paths
    for dsx_path in &[
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().and_then(|p| p.parent())
            .map(|p| p.join("target").join("release").join("dsx"))
            .unwrap_or_default(),
        std::env::current_exe().ok()
            .and_then(|e| e.parent().map(|d| d.join("dsx")))
            .unwrap_or_default(),
    ] {
        if !dsx_path.exists() { continue; }
        if let Ok(mut child) = std::process::Command::new(dsx_path).arg("hp")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            for _ in 0..10 {
                std::thread::sleep(std::time::Duration::from_millis(500));
                if let Ok(s) = std::fs::read_to_string(&port_path) {
                    if let Ok(p) = s.trim().parse::<u16>() {
                        if let Ok(stream) = std::net::TcpStream::connect(format!("127.0.0.1:{p}")) {
                            return Some(stream);
                        }
                    }
                }
                if let Ok(Some(_)) = child.try_wait() { break; }
            }
        }
    }
    None
}

// ── Main ──

pub fn run() {
    eprintln!("dsx-agent starting");

    // ── 1. Initialize logging ──
    dsc_log::init();

    // ── 2. Load configuration ──
    let config = Config::load().unwrap_or_default();
    eprintln!("dsx-agent: model={} effort={:?} context_limit={}",
        config.model, config.effort, config.context_limit);

    // ── 3. Parse CLI args ──
    let args: Vec<String> = std::env::args().collect();
    let resume_seed = args.windows(2).find(|w| w[0] == "--session").and_then(|w| Some(w[1].clone()));
    if let Some(ref seed) = resume_seed {
        eprintln!("dsx-agent: resume request for session {seed}");
    }

    // ── 4. Initialize AgentState ──
    let mut agent = AgentState::new(config);
    agent.resume_seed = resume_seed;
    agent.health.context_limit = agent.config.context_limit;

    // ── 4. Connect to HP (single stream, no try_clone) ──
    let mut hp_conn: Option<BufReader<TcpStream>> = connect_hp().map(BufReader::new);

    // Drain register response
    if let Some(ref mut hp) = hp_conn {
        let _: Option<HpToAgent> = dsx_proto::read_frame(hp).ok().flatten();
    }

    // ── 5. Spawn dsx-tools ──
    let exe = std::env::current_exe().unwrap();
    let (mut tools_child, mut tools_reader, mut tools_writer) = spawn_tools_process(&exe);
    let mut tools_option: Option<std::process::Child> = Some(tools_child);

    // Send init frame and read Ready response
    let init = AgentToTools::Init {
        allowed_tools: vec![],
        session_seed: "pipe".into(),
        auto_mode: agent.auto_mode,
    };
    let _ = dsx_proto::write_frame(&mut tools_writer, &init);
    let ready: Option<ToolsToAgent> = dsx_proto::read_frame(&mut tools_reader).ok().flatten();
    if let Some(ToolsToAgent::Ready { tools }) = &ready {
        let essential: &[&str] = &["exec", "read_file", "write_file", "edit_file", "edit_file_diff", "explore", "search", "list_dir", "glance", "ask_user", "status", "task_create", "task_update", "task_list", "plan_create", "plan_update", "plan_read", "plan_list", "web_fetch", "web_search", "git", "mem_save", "mem_read", "mem_forget", "recall", "pitfall_save", "pitfall_guide"];
        agent.tool_defs = tools.iter().filter(|t| essential.contains(&t.function.name.as_str())).cloned().collect();
        eprintln!("dsx-agent: tools → {} (filtered from {})", agent.tool_defs.len(), tools.len());
    }

    // Hand pipes over to tools.rs for the compatibility execute_tool() layer
    tools::init_tools_ipc(tools_reader, tools_writer, agent.tool_defs.clone());

    // ── 6. Session check ──
    let lives = session::find_live_sessions();
    if !lives.is_empty() {
        eprintln!("dsx-agent: {} live session(s) available for resume", lives.len());
    }

    // ── 7. Main loop ──
    let stdin = std::io::stdin();
    let mut tui_reader = BufReader::new(stdin.lock());
    let mut tui_writer = std::io::stdout();

    loop {
        let frame: TuiToAgent = match dsx_proto::read_frame(&mut tui_reader) {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                eprintln!("dsx-agent: TUI parse error: {e}");
                continue;
            }
        };

        eprintln!("dsx-agent: tui ← {:?}", std::mem::discriminant(&frame));

        match frame {
            TuiToAgent::UserInput { text } => {
                // Respawn tools if IPC was lost
                if tools::all_tools().is_empty() {
                    eprintln!("dsx-agent: tools IPC dead, respawning...");
                    if respawn_tools(&mut tools_option) {
                        agent.tool_defs = tools::all_tools();
                        eprintln!("dsx-agent: tools IPC restored ({} tools)", agent.tool_defs.len());
                    } else {
                        eprintln!("dsx-agent: tools respawn FAILED");
                    }
                }
                // Process input — if HP not connected or fails, try reconnect once
                let hp_failed = if let Some(ref mut hp) = hp_conn {
                    let hp_read = hp.get_mut();
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                        || handle_user_input(&mut agent, &text, hp_read, &mut tui_writer, &mut tui_reader)
                    ));
                    result.is_err()
                } else {
                    // HP was never connected — try reconnect now
                    true
                };

                if hp_failed {
                    eprintln!("dsx-agent: HP failed, reconnecting...");
                    if let Some(stream) = try_reconnect_hp() {
                        let reader = std::io::BufReader::new(stream);
                        hp_conn = Some(reader);
                        eprintln!("dsx-agent: HP reconnected, retry input");
                        // Retry with new connection
                        if let Some(ref mut hp) = hp_conn {
                            let hp_read = hp.get_mut();
                            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                                || handle_user_input(&mut agent, &text, hp_read, &mut tui_writer, &mut tui_reader)
                            ));
                        }
                    } else {
                        eprintln!("dsx-agent: HP reconnect failed");
                        let _ = dsx_proto::write_frame(&mut tui_writer,
                            &AgentToTui::Error { message: "HP disconnected. Please try again.".into() });
                    }
                }
                let _ = dsx_proto::write_frame(&mut tui_writer, &AgentToTui::Done);
            }

            TuiToAgent::ToolCall { id: _, name, action, args } => {
                // Execute tool via IPC and forward result to TUI
                let args_str = args.to_string();
                let content = crate::tools::execute_tool(&name, &action, &args_str);
                let tui_resp = AgentToTui::ApiResponse {
                    content, reasoning_content: None, tool_calls: None, stop_reason: None, usage: None,
                };
                let _ = dsx_proto::write_frame(&mut tui_writer, &tui_resp);
                let _ = dsx_proto::write_frame(&mut tui_writer, &AgentToTui::Done);
            }

            TuiToAgent::SetPhase { phase } => {
                let task_phase = match phase.as_str() {
                    "plan" => dsx_types::TaskPhase::Plan,
                    "coding" | "code" => dsx_types::TaskPhase::Coding,
                    "debug" => dsx_types::TaskPhase::Debug,
                    _ => dsx_types::TaskPhase::Coding,
                };
                agent.current_task_phase = task_phase;
                router::set_phase(task_phase, router::read_debug_level());

                let _ = dsx_proto::write_line(&mut tui_writer, &format!(r#"{{"type":"phase_changed","phase":"{}"}}"#, phase));
            }

            TuiToAgent::ToolConfirm { .. } => {
                // Confirm flow removed — all tools auto-pass
            }

            TuiToAgent::Cancel => {
                crate::tools::CANCEL.store(true, std::sync::atomic::Ordering::SeqCst);
                agent.stream_cancelled = true;
                // Also send cancel to tools subprocess
                crate::tools::cancel_current_tool();
            }

            TuiToAgent::Shutdown => {
                session_persistence::maybe_save_session(&mut agent);
                let _ = dsx_proto::write_frame(&mut tui_writer, &AgentToTui::ShutdownAck);
                break;
            }

            _ => {} // Future variants — silently ignored
        }
    }

    // ── Cleanup ──
    crate::tools::shutdown_tools();
    if let Some(mut c) = tools_option.take() {
        let _ = c.kill();
        let _ = c.wait();
    }

    agent.maybe_save_session();

    if let Some(ref mut hp) = hp_conn {
        let unreg = AgentToHp::Unregister { pid: std::process::id() };
        let _ = dsx_proto::write_frame(hp.get_mut(), &unreg);
    }

    eprintln!("dsx-agent: shutdown complete (session {}, {} turns, {} tokens)",
        agent.session_seed, agent.ctx.turn_count(), agent.session_tokens);
}
