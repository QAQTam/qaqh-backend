use crate::api::{self, StreamEvent};
use crate::config::Config;
use crate::session;
use crate::types::{Message, ToolCall};
use crate::orchestrator::stream_handler;
use crate::orchestrator::tool_dispatch;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub(crate) mod utils;
pub(crate) mod app;
use app::App;
pub(crate) mod commands;
mod input;
use input::*;
mod render;
use render::*;
mod mode;
pub(crate) use mode::{Mode, SubMenuState};

// ── Entry point ──

pub async fn run(config: Config, resume: bool) -> Result<()> {
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::cursor::Hide,
        crossterm::event::EnableBracketedPaste,
    )?;

    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = run_app(&mut terminal, config, resume).await;

    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::cursor::Show,
        crossterm::event::DisableBracketedPaste,
    )?;
    result
}

// ── Main Event Loop ──

async fn run_app(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    config: Config,
    resume: bool,
) -> Result<()> {
    let mut app = App::new(config, resume);
    let (tx, mut rx) = mpsc::channel::<StreamEvent>(256);

    // Auto-query balance on startup if API key is configured
    if app.config.is_ready() {
        let cfg = app.config.clone();
        let btx = tx.clone();
        tokio::spawn(async move {
            let r = api::get_balance(&cfg).await;
            let msg = match r {
                Ok(info) => info.balance_infos.first()
                    .map(|b| format!(" {} {}", b.total_balance, b.currency))
                    .unwrap_or_else(|| "N/A".into()),
                Err(_) => String::new(),
            };
            let _ = btx.send(StreamEvent::BalanceResult(msg)).await;
        });
    }

    loop {
        terminal.draw(|f| render_all(f, &mut app))?;

        let timeout = if app.mode == Mode::Streaming {
            Duration::from_millis(20)  // fast poll during streaming
        } else {
            Duration::from_millis(150)
        };

        if event::poll(timeout)? {
            match event::read()? {
                Event::Paste(data) => {
                    // Only handle paste in input-friendly modes
                    match app.mode {
                        Mode::Chat | Mode::ApiConfig => {
                            let pos = app.input.char_indices()
                                .nth(app.input_cursor)
                                .map(|(i, _)| i)
                                .unwrap_or(app.input.len());
                            let cleaned = data.replace("\r\n", "\n");
                            app.input.insert_str(pos, &cleaned);
                            app.input_cursor = pos + cleaned.len();
                        }
                        _ => {}
                    }
                }
                Event::Key(key) => {
                    if key.kind == KeyEventKind::Press {
                        match app.mode {
                            Mode::Chat | Mode::ApiConfig => handle_chat_key(&mut app, key, tx.clone()),
                            Mode::ToolConfirm => handle_confirm_key(&mut app, key, tx.clone()),
                            Mode::DangerPrompt => handle_danger_key(&mut app, key, tx.clone()),
                            Mode::IntentPicker => handle_intent_picker_key(&mut app, key, tx.clone()),
                            Mode::SessionPicker => handle_session_picker_key(&mut app, key),
                            Mode::SudoPrompt => handle_sudo_key(&mut app, key, tx.clone()),
                            Mode::Streaming => {
                                if key.code == KeyCode::Esc ||
                                   (key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL)
                                {
                                    tool_dispatch::handle_cancel_stream(&mut app);
                                }
                            }
                        }
                        // Ctrl+C: first = exit prompt, second = exit
                        // Esc: dismiss exit prompt (or clear input in chat)
                        // Both Esc/Ctrl+C during active exec: cancel exec instead
                        let ctrl_c = key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL;
                        let exit_shown = app.exit_prompt;
                        if app.exec_pending > 0 && (key.code == KeyCode::Esc || ctrl_c) {
                            tool_dispatch::handle_cancel_exec(&mut app);
                        } else if ctrl_c && exit_shown {
                            break; // second Ctrl+C → confirm exit
                        } else if ctrl_c && (app.mode == Mode::Chat || app.mode == Mode::ApiConfig) {
                            app.exit_prompt = true; // first Ctrl+C → show prompt
                        } else if key.code == KeyCode::Esc && exit_shown {
                            app.exit_prompt = false;
                            app.status_msg = String::new();
                        } else if key.code == KeyCode::Enter && exit_shown {
                            break; // Enter → confirm exit
                        } else {
                            if exit_shown { /* consume the key */ }
                            app.esc_just_closed_menu = false;
                        }
                    }
                }
                _ => {}
            }
        }

        // Process stream events (in all modes, to not miss any)
        while let Ok(event) = rx.try_recv() {
            // ExecStarted fires before stream_cancelled — always store PID
            if let StreamEvent::ExecStarted(id, pid) = &event {
                app.exec_child_pids.push(*pid);
                continue;
            }
            if app.stream_cancelled {
                if matches!(event, StreamEvent::Done { .. } | StreamEvent::Error(_)) {
                    continue;
                }
                continue;
            }
            // Forward UI events (balance/model list / exec progress) regardless of mode
            match &event {
                StreamEvent::BalanceResult(_) | StreamEvent::ModelListResult(_)
                | StreamEvent::ExecProgress(_) => {
                    stream_handler::handle_stream_event(&mut app, event, tx.clone());
                    continue;
                }
                StreamEvent::SudoDone(results) => {
                    let results = results.clone();
                    for (id, result) in results {
                        let _ = app.ctx.push_tool_result_for(&id, &result);
                    }
                    app.status_msg = "Sudo commands executed. Continuing...".into();
                    app.stream_content.clear();
                    app.stream_reasoning.clear();
                    let was_streaming = app.mode == Mode::Streaming;
                    app.mode = Mode::Chat;
                    if !was_streaming {
                        app.start_agent_loop(tx.clone());
                    }
                    continue;
                }
                StreamEvent::ExecDone(id, result) => {
                    let id = id.clone();
                    let result = result.clone();
                    // Orphan guard: scan flat messages for matching tool_use
                    let has_tool_use = app.messages.iter().any(|m|
                        m.role == "assistant" && m.tool_calls.as_ref()
                            .map(|t| t.iter().any(|tc| tc.id == id)).unwrap_or(false));
                    if !has_tool_use {
                        app.exec_pending = app.exec_pending.saturating_sub(1);
                        if app.exec_pending == 0 { app.exec_started_at = None; }
                        log::warn!("exec orphan: no matching tool_use for {}, discarding result", id);
                        continue;
                    }
                    app.exec_pending = app.exec_pending.saturating_sub(1);
                    if app.exec_pending == 0 {
                        app.exec_started_at = None;
                    }
                    // Check if this is a sudo-needed signal, not actual exec result
                    if result.starts_with("[SUDO_REQUIRED]") {
                        let sudo_cmd = result[16..].trim().to_string();
                        let tc = ToolCall {
                            id: id.clone(),
                            call_type: "function".into(),
                            function: crate::types::FunctionCall {
                                name: "exec".into(),
                                arguments: format!("{{\"command\":\"{}\"}}", sudo_cmd),
                            },
                        };
                        app.sudo_pending.push((tc, sudo_cmd));
                        app.mode = Mode::SudoPrompt;
                        app.status_msg = "Sudo password required. [Enter] to submit.".into();
                        continue;
                    }
                    // Extract exit status from result prefix
                    app.tool_code_status = if result.starts_with("[OK]") { Some("OK") }
                        else if result.starts_with("[FAIL]") || result.starts_with("[ERROR]") { Some("FAIL") }
                        else { None };
                    // Finalize bottom-left terminal panel
                    if app.tool_code_action == "exec" {
                        let raw = result.strip_prefix("[OK] ").or_else(|| result.strip_prefix("[ERROR] ")).unwrap_or(&result);
                        let output = if let Some(pos) = raw.find("── output ──\n") {
                            raw[pos + "── output ──\n".len()..].to_string()
                        } else if let Some(pos) = raw.find('\n') {
                            raw[pos + 1..].to_string()
                        } else {
                            raw.to_string()
                        };
                        let output = output.trim().to_string();
                        // Don't overwrite streaming content with a summary placeholder
                        if !output.is_empty()
                            && output != "(no output)"
                            && output != "(streaming output shown above)"
                        {
                            if !app.tool_code_content.ends_with(&output) {
                                app.tool_code_content = format!("$ {}\n{}",
                                    &app.tool_code_path, output);
                            }
                        }
                    }
                    // Build structured summary with exit status
                    let exit_code = result.rsplit("exit ").next()
                        .and_then(|s| s.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse::<i32>().ok())
                        .unwrap_or(0);
                    let status_label = app.tool_code_status.unwrap_or("OK");
                    let summary = if result.starts_with("[ERROR]") {
                        format!("{} > exec failed to start", status_label)
                    } else {
                        let has_streaming_content = app.tool_code_content.lines().count() > 1;
                        if has_streaming_content {
                            let output = app.tool_code_content.trim();
                            format!("{} > exit {}\n{}", status_label, exit_code, output)
                        } else {
                            // Sync exec — result has structured output already
                            let raw = result.strip_prefix("[OK] ").or_else(|| result.strip_prefix("[FAIL] ")).or_else(|| result.strip_prefix("[ERROR] ")).unwrap_or(&result);
                            if let Some(pos) = raw.find("── output ──\n") {
                                format!("$ {}\n{}", app.tool_code_path, &raw[pos + "── output ──\n".len()..])
                            } else {
                                raw.to_string()
                            }
                        }
                    };
                    // Push through Assembler — it finds the correct step by tool_call_id
                    let _ = app.ctx.push_tool_result_for(&id, &summary);
                    app.status_msg = format!("Exec completed ({} remaining).", app.exec_pending);
                    app.stream_content.clear();
                    app.stream_reasoning.clear();
                    if app.exec_pending == 0 {
                        // Flush any safety gate message that was delayed due to pending execs
                        if let Some(msg) = app.gate_message.take() {
                            app.ctx.annotate("SYSTEM", &msg);
                            app.ctx.flush_annotations();
                            app.maybe_save_session();
                        }
                        // Don't restart agent loop if waiting for user input
                        let waiting_for_user = matches!(app.mode, Mode::IntentPicker | Mode::SudoPrompt);
                        if !waiting_for_user {
                            let was_streaming = app.mode == Mode::Streaming;
                            app.mode = Mode::Chat;
                            if !was_streaming {
                                app.start_agent_loop(tx.clone());
                            }
                        }
                    }
                    continue;
                }
                _ => {}
            }
            if app.mode == Mode::Streaming {
                stream_handler::handle_stream_event(&mut app, event, tx.clone());
            }
        }

        // ── Live snapshot for crash recovery (every 3s when dirty) ──
        if app.dirty && app.last_snapshot.elapsed() > Duration::from_secs(3) {
            app.dirty = false;
            app.last_snapshot = Instant::now();
            let seed = app.session_seed.clone();
            let msgs = app.messages.clone();
            let model = app.config.model.clone();
            let effort = app.config.effort.clone();
            let sem_mem = app.semantic_memory.clone();
            let stream_state = if app.mode == Mode::Streaming || app.mode == Mode::ToolConfirm || app.mode == Mode::DangerPrompt || app.mode == Mode::IntentPicker {
                Some(crate::types::StreamState {
                    mode: format!("{:?}", app.mode),
                    stream_content: app.stream_content.clone(),
                    stream_reasoning: app.stream_reasoning.clone(),
                    stream_tool_progress: app.stream_tool_progress.clone(),
                })
            } else { None };
            tokio::spawn(async move {
                session::save_live_snapshot(&seed, &msgs, &model, effort.as_deref(), stream_state, Some(sem_mem));
            });
        }
    }

    // Auto-save session on exit (only if user actually sent messages)
    if app.messages.len() > 1 && !app.session_seed.is_empty() {
        app.maybe_save_session();
    }

    Ok(())
}


