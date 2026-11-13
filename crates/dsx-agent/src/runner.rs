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
use crate::memory;
use crate::orchestrator::{gates, learning, phase_detector, tracker, turn_scorer, session_persistence};
use crate::router;
use crate::session;
use crate::tokenizer;
use crate::tools;
use dsx_types::{Message, ToolCall, FunctionCall, ToolDef};

// ── HP connection ──

fn hp_port() -> u16 {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let path = std::path::PathBuf::from(home).join(".dsx").join("hp.port");
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
        
            agent.short_mem = memory::ShortTermMemory::new(&agent.session_seed, &agent.config.model);
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
    agent.short_mem = memory::ShortTermMemory::new(&agent.session_seed, &agent.config.model);
    dsc_log::set_session(&agent.session_seed);
    tools::set_current_session(&agent.session_seed);
    session::save_live_snapshot(
        &agent.session_seed, &agent.ctx.to_vec(),
        &agent.config.model, agent.config.effort.as_deref(), None, None,
    );
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

// ── Message conversion ──

/// Parse DeepSeek v4 XML-style tool calls from text content.
///
/// Handles two formats:
/// 1. Explicit `<tool_use><tool_name>N</tool_name>JSON</tool_use>`
/// 2. Tool-specific tags: `<read>`, `<write>`, `<edit>`, `<exec>`, `<search>`, `<git>`, `<web>` etc.
///    Maps tag name to tool name and child elements to JSON args.
///
/// Returns (cleaned_content_without_xml, parsed_tool_calls).
fn parse_xml_tool_calls(content: &str, tool_names: &[String]) -> (String, Vec<ToolCall>) {
    let mut cleaned = String::with_capacity(content.len());
    let mut tool_calls = Vec::new();
    let mut tc_index = 0u32;

    // Build tag→name mapping dynamically from registered tool definitions.
    // Each tool name becomes an XML tag the model can use as shorthand.
    // Also add shorthand aliases: <read> → read_file, <write> → write_file, <edit> → edit_file
    let mut tag_map: Vec<(String, String)> = tool_names.iter()
        .map(|n| (n.clone(), n.clone()))
        .collect();
    for (alias, name) in &[("read", "read_file"), ("write", "write_file"), ("edit", "edit_file")] {
        if tool_names.iter().any(|n| n == name) {
            tag_map.push((alias.to_string(), name.to_string()));
        }
    }

    let mut remaining = content;

    'outer: while let Some(tc_start) = remaining.find('<') {
        // Check for explicit <tool_use> format first
        if remaining[tc_start..].starts_with("<tool_use>") {
            if let Some((name, args_text, rest)) = parse_tool_use_block(&remaining[tc_start..]) {
                cleaned.push_str(&remaining[..tc_start]);
                let arguments = normalize_args(&args_text);
                tool_calls.push(ToolCall {
                    id: format!("xml_tc_{tc_index}"),
                    call_type: "function".to_string(),
                    function: FunctionCall { name, arguments },
                });
                tc_index += 1;
                remaining = rest;
                continue 'outer;
            }
        }

        // Check for tool-specific tags
        let after_lt = &remaining[tc_start..];
        if let Some(end) = after_lt.find('>') {
            let tag_name = &after_lt[1..end];
            let closing = format!("</{tag_name}>");

            // Only handle known tool tags
            if let Some((_, tool_name)) = tag_map.iter().find(|(tag, _)| *tag == tag_name) {
                if let Some(close_pos) = after_lt[end..].find(&closing) {
                    let block_content = &after_lt[end + 1..end + close_pos];
                    let full_xml_len = end + close_pos + closing.len();

                    cleaned.push_str(&remaining[..tc_start]);

                    // Extract child elements as JSON args
                    let args = extract_child_args(block_content);
                    let arguments = normalize_args(&args);

                    tool_calls.push(ToolCall {
                        id: format!("xml_tc_{tc_index}"),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: tool_name.to_string(),
                            arguments,
                        },
                    });
                    tc_index += 1;
                    remaining = &after_lt[full_xml_len..];
                    continue 'outer;
                }
            }
        }

        // Not a tool tag — keep scanning
        cleaned.push_str(&remaining[..tc_start + 1]);
        remaining = &remaining[tc_start + 1..];
    }

    cleaned.push_str(remaining);
    (cleaned.trim().to_string(), tool_calls)
}

/// Parse a `<tool_use>...</tool_use>` block. Returns (tool_name, args_text, rest).
fn parse_tool_use_block(s: &str) -> Option<(String, String, &str)> {
    let prefix = "<tool_use>";
    let suffix = "</tool_use>";
    let s = s.strip_prefix(prefix)?;
    let end = s.find(suffix)?;
    let block = &s[..end];
    let rest = &s[end + suffix.len()..];

    let n_open = "<tool_name>";
    let n_close = "</tool_name>";
    let ns = block.find(n_open)?;
    let after_ns = &block[ns + n_open.len()..];
    let ne = after_ns.find(n_close)?;
    let name = after_ns[..ne].trim().to_string();
    let args_text = after_ns[ne + n_close.len()..].trim().to_string();
    Some((name, args_text, rest))
}

/// Extract child XML elements as JSON key-value pairs.
fn extract_child_args(xml: &str) -> String {
    let mut map = serde_json::Map::new();
    let mut s = xml.trim();
    while let Some(lt) = s.find('<') {
        if s[lt..].starts_with("</") || s[lt..].starts_with("<![") {
            // Skip closing tags and CDATA
            s = &s[lt + 1..];
            continue;
        }
        if let Some(gt) = s[lt..].find('>') {
            let tag = &s[lt + 1..lt + gt];
            let closing = format!("</{tag}>");
            if let Some(close) = s[lt + gt + 1..].find(&closing) {
                let value = s[lt + gt + 1..lt + gt + 1 + close].trim();
                map.insert(tag.to_string(), serde_json::json!(value));
                s = &s[lt + gt + 1 + close + closing.len()..];
            } else {
                s = &s[lt + 1..];
            }
        } else {
            break;
        }
    }
    serde_json::to_string(&map).unwrap_or_else(|_| "{}".into())
}

/// Normalize args: if valid JSON object, keep it; otherwise wrap as {"query": ...}.
fn normalize_args(args_text: &str) -> String {
    let trimmed = args_text.trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if v.is_object() {
            return trimmed.to_string();
        }
    }
    serde_json::json!({"query": trimmed}).to_string()
}

/// Convert HP tool_calls JSON (flat or nested) to ToolCall vec.
fn parse_tool_calls(tcs: &serde_json::Value) -> Vec<ToolCall> {
    let arr = match tcs.as_array() { Some(a) => a, None => return vec![] };
    arr.iter().filter_map(|tc| {
        let id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("");
        // Support both flat {name, arguments} and nested {function: {name, arguments}}
        let (name, arguments) = if let Some(func) = tc.get("function") {
            let n = func.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let a = func.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}");
            (n, a)
        } else {
            let n = tc.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let a = tc.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}");
            (n, a)
        };
        if id.is_empty() || name.is_empty() { return None; }
        Some(ToolCall {
            id: id.to_string(),
            call_type: "function".to_string(),
            function: FunctionCall { name: name.to_string(), arguments: arguments.to_string() },
        })
    }).collect()
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
                dsx_proto::write_line(tui_writer, &format!(r#"{{"type":"session_restored","seed":"{}","message_count":{},"summary":"{}"}}"#,
                    agent.session_seed, msg_count, summary.replace('"', "\\\"").replace('\n', "\\n")));
            }
            if agent.auto_mode {
                let phase = router::detect_initial_phase(text);
                let level = dsx_types::DebugLevel::Medium;
                router::set_phase(phase, level);
                agent.current_task_phase = phase;
                let pc = router::phase_config(phase, level);
                agent.config.model = pc.model.to_string();
                if let Some(e) = pc.effort { agent.config.effort = Some(e.to_string()); }
                agent.config.max_tokens = pc.max_tokens;
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
        &agent.config.model, agent.config.effort.as_deref(), None, None,
    );

    // ── Tool-calling loop ──
    for _round in 0..agent.max_tool_rounds {
        if agent.stream_cancelled {
            agent.stream_cancelled = false;
            agent.system_note("system", "用户终止了当前操作。".to_string());
            break;
        }

        // Repair + compact context
        
        let (system, _anthropic_msgs, breakdown) =
            crate::assembly::prepare_and_compact(agent);

        // Filter out system-role messages (sent as `system` field above)
        let flat = agent.ctx.to_vec();
        let msgs_no_system: Vec<&Message> = flat.iter().filter(|m| m.role != "system").collect();
        let messages_json = serde_json::to_value(&msgs_no_system).unwrap_or_default();
        let token_est = tokenizer::estimate_messages_tokens(&flat);
        log::debug!("turn round={} messages={} tokens={}", _round, agent.ctx.message_count(), token_est);

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
        let mut parsed: Vec<ToolCall> = parse_tool_calls(&tcs_raw);

        // DeepSeek v4 outputs <tool_use> XML blocks in text content instead of
        // native Anthropic tool_use blocks. Parse them as fallback.
        if parsed.is_empty() && (content.contains("<tool_use>") || content.contains("<read>") || content.contains("<exec>") || content.contains("<write>") || content.contains("<search>")) {
            let tool_names: Vec<String> = agent.tool_defs.iter().map(|t| t.function.name.clone()).collect();
            let (cleaned, xml_tcs) = parse_xml_tool_calls(&content, &tool_names);
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
                            let pc = router::phase_config(tp, level);
                            agent.config.model = pc.model.to_string();
                            if let Some(e) = pc.effort { agent.config.effort = Some(e.to_string()); }
                            agent.config.max_tokens = pc.max_tokens;
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
                    agent.turn_scores.push(turn_scorer::score_current_turn(agent));
                    agent.stream_content.clear();
                    agent.stream_reasoning.clear();

                    agent.extract_reasoning_insights();
                    learning::auto_extract_memory(agent, &assistant_msg);

                    agent.health.record_turn(false);
                    agent.health_status_line = health_status(agent);
                    agent.health.reset_turn();

                    session::save_live_snapshot(
                        &agent.session_seed, &agent.ctx.to_vec(),
                        &agent.config.model, agent.config.effort.as_deref(), None, None,
                    );
                    session_persistence::maybe_save_session(agent);

                    // Send structured response to TUI using proto type
                    let tui_resp = AgentToTui::ApiResponse {
                        content,
                        tool_calls: None,
                        stop_reason: None,
                        usage,
                        reasoning_content: reasoning_content.clone(),
                    };
                    let _ = dsx_proto::write_frame(tui_writer, &tui_resp);
                    return;
                }

                // ── Tool call round ──
                agent.tool_calls_this_turn += parsed.len() as u32;
                agent.monitor.tool_calls_this_turn = agent.tool_calls_this_turn;

                session::save_live_snapshot(
                    &agent.session_seed, &agent.ctx.to_vec(),
                    &agent.config.model, agent.config.effort.as_deref(), None, None,
                );

                for tc in &parsed {
                    let name = &tc.function.name;
                    let id = &tc.id;
                    let args = &tc.function.arguments;

                    // ── Gates ──
                    if gates::phase_check_tool(agent, name, id) { continue; }
                    if gates::explore_gate(agent, name, id, args) { continue; }
                    if let Some(err_msg) = gates::pre_tool_health_check(agent, name) {
                        let _ = agent.ctx.push_tool_result(id, &err_msg);
                        agent.health.record_error(name, &err_msg);
                        agent.tool_failures += 1;
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
                            agent.tool_results.push((id.to_string(), err));
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
                            let pc = router::phase_config(tp, level);
                            agent.config.model = pc.model.to_string();
                            agent.config.effort = pc.effort.map(|s| s.to_string());
                            agent.config.max_tokens = pc.max_tokens;
                        }
                        let phase_name = format!("{:?}", tp).to_lowercase();
                        let _ = dsx_proto::write_line(tui_writer, &format!(r#"{{"type":"phase_changed","phase":"{}"}}"#, phase_name));
                        let result = format!("[OK] Switched to {} mode", state);
                        let _ = agent.ctx.push_tool_result(id, &result);
                        agent.tool_results.push((id.to_string(), result));
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
                        continue; // Skip calling execute_tool
                    }

                    // ── Pre-cancel check ──
                    if crate::tools::CANCEL.load(std::sync::atomic::Ordering::SeqCst) {
                        crate::tools::CANCEL.store(false, std::sync::atomic::Ordering::SeqCst);
                        agent.system_note("system", "用户取消了工具执行。".to_string());
                        // Append a cancelled result so the turn isn't orphaned on session save/restore
                        let cancel_msg = "[CANCELLED] Tool execution cancelled by user.\n[HINT] This tool was not executed.";
                        let _ = agent.ctx.push_tool_result(&tc.id, cancel_msg);
                        continue;
                    }

                    // ── Execute tool via IPC (tools.rs owns the pipe) ──
                    let tr_content = crate::tools::execute_tool(name, "", args);
                    // If tools IPC is dead, append this error and stop — don't cascade
                    if tr_content.contains("tools IPC") || tr_content.contains("not initialised") {
                        let mut appender = ToolResultAppender::new(agent);
                        appender.append(name, &tc.id, args, &tr_content);
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
                    agent.ctx.annotate("System", "3 consecutive tool failures. Respond with analysis — do not call more tools.");
                    agent.ctx.flush_annotations();
                
                    agent.tool_failures = 0;
                }
        // Continue loop — next api_chat includes tool results
    }

    // Max rounds reached — do a final no-tools API call so the model can summarise.
    agent.ctx.annotate("System", &format!("Max tool rounds ({}) reached. Respond with what you have.", agent.max_tool_rounds));
    agent.ctx.flush_annotations();


    // Repair + compact context for the final response
    
    let (system, _anthropic_msgs, _breakdown) =
        crate::assembly::prepare_and_compact(agent);

    let flat = agent.ctx.to_vec();
    let msgs_no_system: Vec<&Message> = flat.iter().filter(|m| m.role != "system").collect();
    let messages_json = serde_json::to_value(&msgs_no_system).unwrap_or_default();

    // Send ONE MORE api_chat WITHOUT tools — the model can only respond in text
    let chat = AgentToHp::ApiChat {
        model: agent.config.model.clone(),
        system: Some(system),
        messages: messages_json,
        effort: agent.config.effort.clone(),
        max_tokens: Some(agent.config.max_tokens),
        tools: None,
    };
    let _ = dsx_proto::write_frame(hp, &chat);

    // ── Read HP response (streaming loop) ──
    let mut hp_reader = BufReader::new(&mut *hp);
    let mut content = String::new();
    let mut reasoning_content: Option<String> = None;
    let mut thinking_signature: Option<String> = None;
    let mut usage: Option<dsx_types::UsageInfo> = None;

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
                content: c, tool_calls: _,
                stop_reason: _,
                reasoning_content: rc, thinking_signature: ts, usage: u,
            } => {
                content = c;
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

    // Build assistant message and push to context
    let assistant_msg = Message {
        role: "assistant".into(),
        content: if content.is_empty() { None } else { Some(content.clone()) },
        name: None,
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: reasoning_content.clone(),
        thinking_signature,
    };

    // Clone before push in case we need it for restore + learning
    let msg_for_learning = assistant_msg.clone();
    if let Err(e) = agent.ctx.push_assistant(assistant_msg) {
        log::error!("push_assistant failed: {:?} — repairing", e);
                agent.ctx.push_assistant_restore(msg_for_learning.clone());
    }


    // Turn scoring, health, session save
    agent.turn_scores.push(turn_scorer::score_current_turn(agent));
    agent.stream_content.clear();
    agent.stream_reasoning.clear();

    agent.extract_reasoning_insights();
    learning::auto_extract_memory(agent, &msg_for_learning);

    agent.health.record_turn(false);
    agent.health_status_line = health_status(agent);
    agent.health.reset_turn();

    session::save_live_snapshot(
        &agent.session_seed, &agent.ctx.to_vec(),
        &agent.config.model, agent.config.effort.as_deref(), None, None,
    );
    session_persistence::maybe_save_session(agent);

    // Send final ApiResponse to TUI (no tool_calls)
    let tui_resp = AgentToTui::ApiResponse {
        content,
        reasoning_content: reasoning_content.clone(),
        tool_calls: None,
        stop_reason: None,
        usage,
    };
    let _ = dsx_proto::write_frame(tui_writer, &tui_resp);
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
    let home = std::env::var("HOME").unwrap_or_default();
    let port_path = std::path::PathBuf::from(&home).join(".dsx").join("hp.port");

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
                // Process input — if HP fails, try reconnect once
                let hp_failed = if let Some(ref mut hp) = hp_conn {
                    let hp_read = hp.get_mut();
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                        || handle_user_input(&mut agent, &text, hp_read, &mut tui_writer, &mut tui_reader)
                    ));
                    result.is_err()
                } else { false };

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

            TuiToAgent::ToolConfirm { id, approved } => {
                crate::tools::handle_tool_confirm(id, approved);
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
