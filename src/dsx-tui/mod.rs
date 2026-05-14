//! DSX TUI — main event loop.
//!
//! Lifecycle:
//!   1. Initialize AppState + AgentConnection + StreamBuffer
//!   2. Loop: poll keyboard → dispatch InputAction → poll IPC → render → snapshot
//!   3. Shutdown: flush session, terminate agent, restore terminal

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use std::time::{Duration, Instant};

use crate::api::{self, StreamEvent};
use crate::config::{self, Config};
use crate::orchestrator::stream_handler;
use crate::session;
use crate::types::Message;
use tokio::sync::mpsc;

pub(crate) mod utils;
pub(crate) mod app;  // legacy — kept for backward compat during migration
pub(crate) mod commands;
pub(crate) mod state;
mod input;
use input::*;
mod render;
use render::*;
mod mode;
pub(crate) use mode::{Mode, SubMenuState};
pub(crate) use state::{AppState, AgentConnection, AgentFrame, StreamBuffer, TuiFrame};

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

// ── Helpers ──

fn input_mode_for(mode: &Mode) -> Option<input::state::InputMode> {
    use input::state::InputMode as Im;
    match mode {
        Mode::Chat => Some(Im::Chat),
        Mode::ToolConfirm => Some(Im::Confirm),
        Mode::DangerPrompt => Some(Im::Danger),
        Mode::SudoPrompt => Some(Im::Sudo),
        Mode::IntentPicker => Some(Im::IntentPicker),
        Mode::SessionPicker => Some(Im::SessionPicker),
        Mode::Streaming | Mode::ApiConfig | Mode::GateBlock => None,
    }
}

/// Apply an InputAction to the system (mutate AppState, send IPC frames).
/// Returns true if the caller should break the event loop (exit).
fn dispatch_action(
    action: input::action::InputAction,
    app: &mut AppState,
    conn: &mut AgentConnection,
    buf: &mut StreamBuffer,
    tx: &mpsc::Sender<StreamEvent>,
) -> bool {
    use input::action::InputAction as A;
    match action {
        A::None => {}

        // ── Chat ──
        A::SendMessage(text) => {
            app.input_state.clear_chat();
            app.mode = Mode::Streaming;
            buf.clear();
            conn.send_frame(TuiFrame::UserInput {
                text,
                tools_enabled: app.tools_enabled,
            });
        }
        A::SetBuffer { text, cursor } => {
            app.input_state.chat.buffer = text;
            app.input_state.chat.cursor = cursor;
        }

        // ── Tool confirmation ──
        A::ConfirmTools => {
            conn.send_frame(TuiFrame::ConfirmTools { ids: vec![] });
            app.mode = Mode::Streaming;
        }
        A::RejectTools => {
            app.status_msg = "Tools rejected.".into();
            app.mode = Mode::Chat;
        }

        // ── Sudo ──
        A::SubmitSudo(password) => {
            conn.send_frame(TuiFrame::UserInput {
                text: format!("[SUDO_PASSWORD]{}", password),
                tools_enabled: false,
            });
            app.mode = Mode::Streaming;
        }
        A::CancelSudo => {
            app.status_msg = "Sudo cancelled by user.".into();
            app.mode = Mode::Chat;
        }

        // ── Intent picker ──
        A::SelectIntent(answer) => {
            conn.send_frame(TuiFrame::UserInput {
                text: answer,
                tools_enabled: app.tools_enabled,
            });
            app.mode = Mode::Streaming;
            buf.clear();
        }

        // ── Session picker ──
        A::LoadSession(idx) => {
            let pick_idx = app.input_state.session.pick_idx;
            if let Some(meta) = app.input_state.session.list.get(pick_idx) {
                let seed = meta.seed.clone();
                if let Some((file, _is_live)) = session::load_session_or_live(&seed) {
                    let msg_count = file.messages.len();
                    app.messages = file.messages;
                    app.session_seed = seed;
                    app.token_estimate =
                        crate::tokenizer::estimate_messages_tokens(&app.messages);
                    app.api_usage = None;
                    app.status_msg = format!("Resumed {} | {} msgs", file.seed, msg_count);
                    app.mode = Mode::Chat;
                    conn.send_frame(TuiFrame::ModeChange {
                        mode: format!("resume/{}", file.seed),
                    });
                } else {
                    app.status_msg = format!("Failed to load session {}", seed);
                }
            }
        }

        // ── Scroll ──
        A::ScrollUp(n) => { app.auto_scroll = false; app.scroll_offset = app.scroll_offset.saturating_sub(n); }
        A::ScrollDown(n) => { app.auto_scroll = false; app.scroll_offset = app.scroll_offset.saturating_add(n); }
        A::ScrollPageUp => { app.exec_scroll_offset = app.exec_scroll_offset.saturating_add(20); }
        A::ScrollPageDown => { app.exec_scroll_offset = app.exec_scroll_offset.saturating_sub(20); }
        A::ScrollToTop => { app.scroll_offset = 0; app.auto_scroll = false; }
        A::ScrollToBottom => { app.auto_scroll = true; }
        A::ToggleAutoScroll => { app.auto_scroll = !app.auto_scroll; }

        // ── Slash menu ──
        A::OpenSlashSubmenu { parent_cmd, items } => {
            app.input_state.slash.submenu = Some(input::state::SlashSubMenu {
                parent_cmd, items, idx: 0,
            });
        }

        // ── App control ──
        A::RequestExit => { app.input_state.exit_prompt = true; }
        A::ConfirmExit => return true,
        A::CancelExit => { app.input_state.exit_prompt = false; app.status_msg.clear(); }
        A::CancelStream => {
            conn.send_frame(TuiFrame::Cancel);
            buf.cancelled = true;
            app.status_msg = "Stream cancelled.".into();
        }
        A::CancelExec => {
            conn.send_frame(TuiFrame::Cancel);
            app.status_msg = "Exec cancelled.".into();
        }
    }
    false
}

/// Process a single AgentFrame, updating AppState and StreamBuffer.
fn process_agent_frame(
    frame: AgentFrame,
    app: &mut AppState,
    buf: &mut StreamBuffer,
    conn: &mut AgentConnection,
) {
    match frame {
        AgentFrame::StreamPayload { delta, reasoning, tool_progress } => {
            buf.apply_payload(&delta, reasoning.as_deref(), &tool_progress);
            if app.mode != Mode::Streaming {
                app.mode = Mode::Streaming;
            }
            app.dirty = true;
        }
        AgentFrame::StreamDone { usage } => {
            if buf.is_active() {
                app.push_message(buf.to_message());
            }
            app.mode = Mode::Chat;
            if let Some(u) = usage {
                app.api_usage = Some(u);
            }
            buf.clear();
            app.dirty = true;
        }
        AgentFrame::StreamError { message } => {
            app.status_msg = format!("Error: {}", message);
            app.mode = Mode::Chat;
            buf.clear();
            app.dirty = true;
        }
        AgentFrame::TuiModeRequest { mode, context } => {
            let new_mode = match mode.as_str() {
                "confirm" => Mode::ToolConfirm,
                "danger" => Mode::DangerPrompt,
                "sudo" => Mode::SudoPrompt,
                "intent" => Mode::IntentPicker,
                "api_config" => Mode::ApiConfig,
                _ => { log::warn!("unknown mode: {}", mode); Mode::Chat }
            };
            if new_mode != app.mode {
                app.mode = new_mode;
                app.dirty = true;
            }
            if let Some(ctx) = context { app.status_msg = ctx; }
        }
        AgentFrame::BalanceResult { balance } => {
            // First, try to parse as a numeric balance
            let bal_str = balance.trim();
            if bal_str == "netfail" {
                app.status_msg = "Network unreachable — check your connection.".into();
            } else if bal_str == "fail_auth" {
                app.status_msg = "API key rejected (401). Check your key.".into();
            } else if bal_str.starts_with("fail_") {
                app.status_msg = format!("Setup error: {}", &bal_str[5..]);
            } else if bal_str == "ok" {
                app.status_msg = "API key verified. Connected.".into();
                if app.mode == Mode::ApiConfig {
                    app.mode = Mode::Chat;
                }
            } else {
                app.balance = Some(balance);
            }
            app.dirty = true;
        }
    }
}

/// Crash recovery snapshot (every 3s when dirty).
fn take_snapshot(app: &AppState, buf: &StreamBuffer) {
    if app.session_seed.is_empty() { return; }
    let seed = app.session_seed.clone();
    let msgs = app.messages.clone();
    let stream_state = if matches!(app.mode, Mode::Streaming | Mode::ToolConfirm | Mode::DangerPrompt | Mode::IntentPicker) {
        Some(crate::types::StreamState {
            mode: format!("{:?}", app.mode),
            stream_content: buf.content.clone(),
            stream_reasoning: buf.reasoning.clone(),
            stream_tool_progress: buf.tool_progress.clone(),
        })
    } else { None };
    tokio::spawn(async move {
        session::save_live_snapshot(&seed, &msgs, "", None, stream_state, None);
    });
}

// ── Main Event Loop ──

async fn run_app(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    config: Config,
    resume: bool,
) -> Result<()> {
    // ── Initialize ──
    let session_list = if resume {
        let mut list = session::load_index();
        let live = session::find_live_sessions();
        for ls in live {
            if !list.iter().any(|s| s.seed == ls.seed) { list.push(ls); }
        }
        list.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        list
    } else { vec![] };

    let initial_mode = if resume { Mode::SessionPicker }
    else if config.is_ready() { Mode::Chat }
    else { Mode::ApiConfig };

    let mut app = AppState::new(initial_mode);
    app.input_state.session.list = session_list;
    app.auto_mode = config.auto_mode;

    let agent_bin = std::env::var("DSX_AGENT_BIN").ok();
    let mut conn = AgentConnection::spawn(agent_bin.as_deref(), config)
        .expect("failed to spawn agent");
    let mut buf = StreamBuffer::new();

    // Auto-query balance on startup
    if conn.config().is_ready() {
        conn.send_frame(TuiFrame::QueryBalance);
    }

    // Legacy StreamEvent channel — for backward compat during migration
    let (tx, mut rx) = mpsc::channel::<StreamEvent>(256);

    // ── Event Loop ──
    loop {
        // 1. Render
        terminal.draw(|f| render_all(f, &app, &conn, &buf))?;

        // 2. Poll timeout based on mode
        let timeout = if app.mode == Mode::Streaming {
            Duration::from_millis(20)
        } else {
            Duration::from_millis(150)
        };

        // 3. Keyboard events
        if event::poll(timeout)? {
            match event::read()? {
                Event::Paste(data) => {
                    if matches!(app.mode, Mode::Chat | Mode::ApiConfig) {
                        let pos = app.input_state.chat.buffer.char_indices()
                            .nth(app.input_state.chat.cursor)
                            .map(|(i, _)| i)
                            .unwrap_or(app.input_state.chat.buffer.len());
                        let cleaned = data.replace("\r\n", "\n");
                        app.input_state.chat.buffer.insert_str(pos, &cleaned);
                        app.input_state.chat.cursor = pos + cleaned.len();
                    }
                }
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    let mut actions: Vec<input::action::InputAction> = Vec::new();

                    match app.mode {
                        Mode::Streaming => {
                            let is_esc = key.code == KeyCode::Esc;
                            let is_ctrlc = key.code == KeyCode::Char('c')
                                && key.modifiers == KeyModifiers::CONTROL;
                            if is_esc || is_ctrlc {
                                actions.push(input::action::InputAction::CancelStream);
                            }
                        }
                        Mode::ApiConfig => {
                            handle_setup_key(&mut app, key, &mut conn, &tx);
                        }
                        Mode::GateBlock => {
                            let dismiss = match app.last_gate_block.as_ref().map(|g| g.repairable) {
                                Some(true) => key.code == KeyCode::Esc || key.code == KeyCode::Enter,
                                Some(false) => key.code == KeyCode::Enter,
                                None => true,
                            };
                            if dismiss {
                                app.last_gate_block = None;
                                app.mode = Mode::Chat;
                            }
                        }
                        _ => {
                            use input::state::InputMode as Im;
                            let im = input_mode_for(&app.mode).unwrap_or(Im::Chat);
                            let action = match im {
                                Im::Chat => input::chat::handle_chat_key(
                                    &mut app.input_state, key,
                                    &config::all_commands(conn.config().prompt_lang),
                                ),
                                Im::Confirm | Im::Danger => {
                                    input::confirm::handle_confirm_key(
                                        &mut app.input_state, key,
                                    )
                                }
                                Im::Sudo => input::sudo::handle_sudo_key(
                                    &mut app.input_state, key,
                                ),
                                Im::IntentPicker => input::picker::handle_intent_picker_key(
                                    &mut app.input_state, key,
                                ),
                                Im::SessionPicker => input::picker::handle_session_picker_key(
                                    &mut app.input_state, key,
                                ),
                            };
                            actions.push(action);
                        }
                    }

                    // Exit handling: Ctrl+C / Esc / Enter when exit_prompt is shown
                    let ctrl_c = key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL;
                    let esc = key.code == KeyCode::Esc;
                    let enter = key.code == KeyCode::Enter;
                    let exit_shown = app.input_state.exit_prompt;

                    if ctrl_c && exit_shown {
                        if dispatch_action(input::action::InputAction::ConfirmExit,
                            &mut app, &mut conn, &mut buf, &tx) { break; }
                    } else if ctrl_c && (app.mode == Mode::Chat || app.mode == Mode::ApiConfig) {
                        dispatch_action(input::action::InputAction::RequestExit,
                            &mut app, &mut conn, &mut buf, &tx);
                    } else if esc && exit_shown {
                        dispatch_action(input::action::InputAction::CancelExit,
                            &mut app, &mut conn, &mut buf, &tx);
                    } else if enter && exit_shown {
                        if dispatch_action(input::action::InputAction::ConfirmExit,
                            &mut app, &mut conn, &mut buf, &tx) { break; }
                    } else if !exit_shown {
                        for action in actions {
                            if dispatch_action(action, &mut app, &mut conn, &mut buf, &tx) { break; }
                        }
                    }
                }
                _ => {}
            }
        }

        // 4. IPC events from Agent (non-blocking drain)
        for frame in conn.drain_frames() {
            process_agent_frame(frame, &mut app, &mut buf, &mut conn);
        }

        // Legacy stream event compatibility
        while let Ok(event) = rx.try_recv() {
            stream_handler::handle_stream_event_legacy(&mut app, &mut buf, event, tx.clone());
        }

        // 5. Crash recovery snapshot
        if app.dirty && app.last_snapshot.elapsed() > Duration::from_secs(3) {
            app.dirty = false;
            app.last_snapshot = Instant::now();
            take_snapshot(&app, &buf);
        }
    }

    // ── Shutdown ──
    if app.messages.len() > 1 && !app.session_seed.is_empty() {
        let msgs = app.messages.clone();
        let seed = app.session_seed.clone();
        tokio::spawn(async move {
            session::finalize_session(&seed, &msgs, "", None, None);
        });
    }
    conn.shutdown();
    Ok(())
}

/// Inline API key setup handler (simple key-entry wizard).
fn handle_setup_key(
    app: &mut AppState,
    key: event::KeyEvent,
    conn: &mut AgentConnection,
    tx: &mpsc::Sender<StreamEvent>,
) {
    use tokio::net::TcpStream;
    match key.code {
        KeyCode::Esc => std::process::exit(0),
        KeyCode::Enter => {
            let mut key_str = app.input_state.chat.buffer.trim().to_string();
            if key_str.len() >= 10 {
                let key_lower = key_str.to_lowercase();
                while key_lower.starts_with("sk-sk-") {
                    key_str = key_str.replacen("sk-", "", 1);
                    break;
                }
                let key2 = if key_str.to_lowercase().starts_with("sk-") {
                    key_str
                } else {
                    format!("sk-{}", key_str)
                };
                let cfg = conn.config_mut();
                cfg.api_key = key2;
                cfg.save();
                app.status_msg = "Verifying...".into();
                let cfg2 = conn.config().clone();
                let tx2 = tx.clone();
                tokio::spawn(async move {
                    let net_ok = TcpStream::connect("api.deepseek.com:443")
                        .await.map(|_| ()).is_ok();
                    if !net_ok {
                        let _ = tx2.send(StreamEvent::BalanceResult("netfail".into())).await;
                        return;
                    }
                    let result = match api::list_models(&cfg2).await {
                        Ok(_) => "ok".to_string(),
                        Err(e) => {
                            let err_str = format!("{}", e);
                            if err_str.contains("401") { "fail_auth".into() }
                            else { format!("fail_{}", err_str.chars().take(40).collect::<String>()) }
                        }
                    };
                    let _ = tx2.send(StreamEvent::BalanceResult(result)).await;
                });
            } else {
                app.status_msg = "Key must be at least 10 characters".into();
            }
        }
        KeyCode::Char(c) => {
            let pos = app.input_state.chat.cursor;
            app.input_state.chat.buffer.insert(pos, c);
            app.input_state.chat.cursor += 1;
        }
        KeyCode::Backspace => {
            if app.input_state.chat.cursor > 0 && !app.input_state.chat.buffer.is_empty() {
                let prev = app.input_state.chat.cursor - 1;
                app.input_state.chat.buffer.remove(prev);
                app.input_state.chat.cursor = prev;
            }
        }
        _ => {}
    }
}
