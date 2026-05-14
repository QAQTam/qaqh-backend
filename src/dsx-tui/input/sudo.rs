//! Sudo password prompt keyboard handler.

use crossterm::event::{self, KeyCode};
use std::time::Instant;
use tokio::sync::mpsc;

use crate::api::StreamEvent;
use crate::types::Message;

use super::super::app::App;
use super::super::utils::{char_boundary, prev_char_boundary};
use super::super::Mode;

pub(crate) fn handle_sudo_key(
    app: &mut App,
    key: event::KeyEvent,
    tx: mpsc::Sender<StreamEvent>,
) {
    match key.code {
        KeyCode::Esc => {
            // Cancel all sudo requests \u{2014} inject result so AI knows user refused
            let pending: Vec<_> = app.sudo_pending.drain(..).collect();
            for (tc, cmd) in pending {
                let msg = format!(
                    "[ERROR] exec requires sudo \u{2014} user did not provide password\n[HINT] Retry with a non-sudo approach, or ask the user to run `{}` manually.",
                    cmd);
                app.messages.push(Message::tool(&tc.id, &msg));
            }
            app.sudo_password.clear();
            app.mode = Mode::Chat;
            app.status_msg = "Sudo cancelled by user.".into();
            // Restart agent loop so AI can respond to the cancellation
            app.start_agent_loop(tx.clone());
        }
        KeyCode::Enter => {
            let pwd = app.sudo_password.clone();
            let pending: Vec<_> = app.sudo_pending.drain(..).collect();
            app.sudo_password.clear();
            app.mode = Mode::Chat;
                        app.status_msg = "\u{23f3} Running sudo command...".into();

            for (tc, cmd) in pending {
                // Set up bottom-left terminal panel
                app.exec_pending += 1;
                if app.exec_started_at.is_none() {
                    app.exec_started_at = Some(Instant::now());
                }
                app.exec_buffer.clear();
                app.tool_code_path = cmd.clone();
                app.tool_code_action = "exec".into();
                app.tool_code_content = format!("$ {}\n", cmd);
                // Use async streaming with sudo password
                let args =
                    format!("{{\"command\":\"{}\"}}", cmd.replace('"', "\\\""));
                crate::tools::spawn_exec_async_with_sudo(
                    &tc.id,
                    &args,
                    Some(&pwd),
                    tx.clone(),
                );
            }
        }
        KeyCode::Char(c) => {
            let pos =
                char_boundary(&app.sudo_password, app.sudo_password.len());
            app.sudo_password.insert(pos, c);
        }
        KeyCode::Backspace => {
            if !app.sudo_password.is_empty() {
                let prev = prev_char_boundary(
                    &app.sudo_password,
                    app.sudo_password.len(),
                );
                app.sudo_password.remove(prev);
            }
        }
        _ => {}
    }
}
