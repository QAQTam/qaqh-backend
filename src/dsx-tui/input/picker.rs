//! Picker keyboard handlers (intent and session).

use crossterm::event::{self, KeyCode};
use tokio::sync::mpsc;

use crate::api::StreamEvent;
use crate::tokenizer;
use crate::router;
use crate::session;
use crate::types::Message;

use super::super::app::App;
use super::super::Mode;

pub(crate) fn handle_intent_picker_key(
    app: &mut App,
    key: event::KeyEvent,
    tx: mpsc::Sender<StreamEvent>,
) {
    match key.code {
        KeyCode::Esc => {
            app.mode = Mode::Chat;
            app.status_msg = "Intent picker cancelled.".into();
        }
        KeyCode::Char(ref c)
            if c.is_ascii_alphabetic()
                && c.to_ascii_lowercase() >= 'a'
                && c.to_ascii_lowercase() <= 'f' =>
        {
            let idx = (c.to_ascii_lowercase() as usize).saturating_sub(97);
            if idx < app.intent_options.len() {
                let answer =
                    format!("\u{9009}\u{62e9}\u{4e86}: {}", app.intent_options[idx]);
                finalize_intent(app, answer, tx);
            } else {
                app.input.push(*c);
                app.input_cursor += 1;
            }
        }
        KeyCode::Enter => {
            let answer = if app.input.is_empty() {
                "keep going".to_string()
            } else {
                format!("{}", app.input.trim())
            };
            app.input.clear();
            app.input_cursor = 0;
            finalize_intent(app, answer, tx);
        }
        KeyCode::Char(c) => {
            if app.input_cursor < app.input.len() {
                app.input.insert(app.input_cursor, c);
            } else {
                app.input.push(c);
            }
            app.input_cursor += 1;
        }
        KeyCode::Backspace => {
            if app.input_cursor > 0 && !app.input.is_empty() {
                // Convert char-count cursor to byte position via char_indices
                let byte_cursor = app
                    .input
                    .char_indices()
                    .nth(app.input_cursor)
                    .map(|(i, _)| i)
                    .unwrap_or(app.input.len());
                let prev_start = app.input[..byte_cursor]
                    .char_indices()
                    .rev()
                    .next()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                app.input.drain(prev_start..byte_cursor);
                app.input_cursor -= 1;
            }
        }
        _ => {}
    }
}

pub(crate) fn finalize_intent(
    app: &mut App,
    answer: String,
    tx: mpsc::Sender<StreamEvent>,
) {
    app.system_note("user", answer);
    app.intent_question.clear();
    app.intent_options.clear();
    app.mode = Mode::Chat;
    app.stream_content.clear();
    app.stream_reasoning.clear();
    app.start_agent_loop(tx);
}

pub(crate) fn handle_session_picker_key(
    app: &mut App,
    key: event::KeyEvent,
) {
    match key.code {
        KeyCode::Esc => {
            std::process::exit(0);
        }
        KeyCode::Up => {
            app.session_pick_idx = app.session_pick_idx.saturating_sub(1);
        }
        KeyCode::Down => {
            if app.session_pick_idx + 1 < app.session_list.len() {
                app.session_pick_idx += 1;
            }
        }
        KeyCode::Enter => {
            if let Some(meta) = app.session_list.get(app.session_pick_idx) {
                let seed = meta.seed.clone();
                if let Some((file, is_live)) =
                    session::load_session_or_live(&seed)
                {
                    let msg_count = file.messages.len();
                    let was_interrupted = is_live
                        || meta
                            .last_summary
                            .contains(app.tt().resume_interrupted);
                    let sys = app.messages.first().cloned();
                    app.messages = file.messages;
                    app.repair_messages();
                    if let Some(sys_msg) = sys {
                        if app.messages.first().map(|m| m.role.as_str())
                            == Some("system")
                        {
                            app.messages[0] = sys_msg;
                        } else {
                            app.messages.insert(0, sys_msg);
                        }
                    }
                    // If session had partial AI response, append it as
                    // [interrupted]
                    if let Some(ref ss) = file.stream_state {
                        if !ss.stream_content.is_empty()
                            || !ss.stream_reasoning.is_empty()
                        {
                            let mut partial = Message::assistant_empty();
                            if !ss.stream_content.is_empty() {
                                partial.content = Some(format!(
                                    "[interrupted] {}",
                                    ss.stream_content
                                ));
                            }
                            if !ss.stream_reasoning.is_empty() {
                                partial.reasoning_content =
                                    Some(ss.stream_reasoning.clone());
                            }
                            app.messages.push(partial);
                        }
                    }
                    app.session_seed = file.seed.clone();
                    crate::tools::set_current_session(&app.session_seed);
                    app.short_mem = crate::memory::ShortTermMemory::new(
                        &app.session_seed,
                        &file.model,
                    );
                    app.session_start = file.created_at;
                    app.config.model = file.model.clone();
                    app.config.effort = file.effort.clone();
                    app.semantic_memory =
                        file.semantic_memory.unwrap_or_default();
                    if let Some(phase) = file.task_phase {
                        app.current_task_phase = phase;
                        crate::router::set_phase(
                            phase,
                            crate::types::DebugLevel::Medium,
                        );
                    }
                    app.token_estimate =
                        tokenizer::estimate_messages_tokens(&app.messages);
                    app.api_usage = None;
                    if was_interrupted {
                        app.status_msg = format!(
                            "Resumed {} (interrupted) | {} msgs. Re-send last message to continue.",
                            file.seed, msg_count
                        );
                    } else {
                        app.status_msg = format!(
                            "Resumed {} | {} | {} msgs",
                            file.seed, file.model, msg_count
                        );
                    }
                    app.mode = Mode::Chat;
                    // Save initial live snapshot for this resumed session
                    session::save_live_snapshot(
                        &app.session_seed,
                        &app.messages,
                        &app.config.model,
                        app.config.effort.as_deref(),
                        None,
                        Some(app.semantic_memory.clone()),
                    );
                } else {
                    app.status_msg =
                        format!("Failed to load session {}", seed);
                }
            }
        }
        _ => {}
    }
}
