//! Chat mode keyboard handler — pure state machine.
//!
//! Receives `(&mut InputState, KeyEvent, &[(String, String)])` and returns `InputAction`.
//! No access to App, tx, or any async resource.

use crossterm::event::{self, KeyCode, KeyModifiers};

use super::action::{InputAction, NO_ACTION};
use super::state::{InputMode, InputState, SlashSubMenu};
use crate::tui::utils::{char_boundary, next_char_boundary, prev_char_boundary};

/// Handle a key event in Chat mode.
///
/// `commands` is the list of available slash commands from config,
/// injected by the caller (the event loop or dispatch layer).
pub fn handle_chat_key(
    state: &mut InputState,
    key: event::KeyEvent,
    commands: &[(String, String)],
) -> InputAction {
    state.active_mode = InputMode::Chat;

    // ── Slash submenu navigation (takes priority over main menu) ──
    if let Some(ref mut sub) = state.slash.submenu {
        match key.code {
            KeyCode::Esc => {
                state.slash.submenu = None;
                return NO_ACTION;
            }
            KeyCode::Up => {
                sub.idx = sub.idx.saturating_sub(1);
                return NO_ACTION;
            }
            KeyCode::Down => {
                if sub.idx + 1 < sub.items.len() {
                    sub.idx += 1;
                }
                return NO_ACTION;
            }
            KeyCode::Enter => {
                if let Some((val, _)) = sub.items.get(sub.idx) {
                    state.chat.buffer = format!("{} {} ", sub.parent_cmd, val);
                    state.chat.cursor = state.chat.buffer.len();
                }
                state.slash.submenu = None;
                state.slash.open = false;
                return NO_ACTION;
            }
            _ => return NO_ACTION,
        }
    }

    // ── Slash menu navigation ──
    if state.slash.open {
        // Space opens submenu
        if key.code == KeyCode::Char(' ') {
            if let Some((cmd, _)) = state.slash.items.get(state.slash.idx) {
                let items = build_submenu_items(cmd);
                if let Some(items) = items {
                    state.slash.submenu = Some(SlashSubMenu {
                        parent_cmd: cmd.clone(),
                        items,
                        idx: 0,
                    });
                }
            }
            return NO_ACTION;
        }

        match key.code {
            KeyCode::Esc => {
                state.slash.open = false;
                state.chat.buffer.clear();
                state.chat.cursor = 0;
                state.slash.esc_just_closed = true;
                return NO_ACTION;
            }
            KeyCode::Up => {
                state.slash.idx = state.slash.idx.saturating_sub(1);
                return NO_ACTION;
            }
            KeyCode::Down => {
                if state.slash.idx + 1 < state.slash.items.len() {
                    state.slash.idx += 1;
                }
                return NO_ACTION;
            }
            KeyCode::Enter | KeyCode::Tab => {
                if let Some((cmd, _)) = state.slash.items.get(state.slash.idx) {
                    state.chat.buffer = format!("{} ", cmd);
                    state.chat.cursor = state.chat.buffer.len();
                }
                state.slash.open = false;
                return NO_ACTION;
            }
            KeyCode::Char(c) => {
                let pos = char_boundary(&state.chat.buffer, state.chat.cursor);
                state.chat.buffer.insert(pos, c);
                state.chat.cursor = pos + c.len_utf8();
                state.slash.filter = state.chat.buffer.clone();
                update_slash_items(state, commands);
                state.slash.idx = 0;
                if state.slash.items.is_empty() {
                    state.slash.open = false;
                }
                return NO_ACTION;
            }
            KeyCode::Backspace => {
                if state.chat.cursor > 0 {
                    let prev = prev_char_boundary(&state.chat.buffer, state.chat.cursor);
                    state.chat.buffer.remove(prev);
                    state.chat.cursor = prev;
                    state.slash.filter = state.chat.buffer.clone();
                    update_slash_items(state, commands);
                    state.slash.idx = 0;
                }
                return NO_ACTION;
            }
            _ => return NO_ACTION,
        }
    }

    // ── Normal chat input ──
    match key.code {
        KeyCode::Esc => {
            if !state.chat.buffer.is_empty() {
                state.chat.buffer.clear();
                state.chat.cursor = 0;
            }
            NO_ACTION
        }
        KeyCode::Enter => {
            if key.modifiers == KeyModifiers::CONTROL {
                // Ctrl+Enter = insert newline
                let pos = char_boundary(&state.chat.buffer, state.chat.cursor);
                state.chat.buffer.insert(pos, '\n');
                state.chat.cursor = pos + 1;
                NO_ACTION
            } else {
                InputAction::SendMessage(state.chat.buffer.clone())
            }
        }
        KeyCode::Char(c) if key.modifiers == KeyModifiers::CONTROL => match c {
            'd' | '\x04' => InputAction::ScrollDown(12),
            'u' | '\x15' => InputAction::ScrollUp(12),
            _ => NO_ACTION,
        },
        KeyCode::Char(c) => {
            let pos = char_boundary(&state.chat.buffer, state.chat.cursor);
            state.chat.buffer.insert(pos, c);
            state.chat.cursor = pos + c.len_utf8();
            if c == '/' && state.chat.buffer.trim() == "/" {
                state.slash.open = true;
                state.slash.filter = state.chat.buffer.clone();
                update_slash_items(state, commands);
                state.slash.idx = 0;
            }
            NO_ACTION
        }
        KeyCode::Backspace => {
            if state.chat.cursor > 0 {
                let prev = prev_char_boundary(&state.chat.buffer, state.chat.cursor);
                state.chat.buffer.remove(prev);
                state.chat.cursor = prev;
            }
            NO_ACTION
        }
        KeyCode::Delete => {
            if state.chat.cursor < state.chat.buffer.len() {
                state.chat.buffer.remove(state.chat.cursor);
            }
            NO_ACTION
        }
        KeyCode::Left => {
            state.chat.cursor = prev_char_boundary(&state.chat.buffer, state.chat.cursor);
            NO_ACTION
        }
        KeyCode::Right => {
            state.chat.cursor = next_char_boundary(&state.chat.buffer, state.chat.cursor);
            NO_ACTION
        }
        KeyCode::Home => {
            state.chat.cursor = 0;
            NO_ACTION
        }
        KeyCode::End => {
            state.chat.cursor = state.chat.buffer.len();
            NO_ACTION
        }
        KeyCode::Up => InputAction::ScrollUp(3),
        KeyCode::Down => InputAction::ScrollDown(3),
        KeyCode::PageUp => InputAction::ScrollPageUp,
        KeyCode::PageDown => InputAction::ScrollPageDown,
        KeyCode::Tab => NO_ACTION,
        _ => NO_ACTION,
    }
}

/// Filter the slash commands list against the current input filter.
fn update_slash_items(state: &mut InputState, commands: &[(String, String)]) {
    let filter = state.slash.filter.trim().to_lowercase();
    state.slash.items = commands
        .iter()
        .filter(|(cmd, _)| cmd.to_lowercase().contains(&filter))
        .map(|(c, d)| (c.to_string(), d.to_string()))
        .collect();
}

/// Build submenu items for a given slash command (pure, static).
fn build_submenu_items(cmd: &str) -> Option<Vec<(String, String)>> {
    match cmd {
        "/effort" => Some(vec![
            ("high".into(), "High reasoning effort".into()),
            ("max".into(), "Maximum reasoning effort".into()),
        ]),
        "/model" => Some(vec![
            ("deepseek-v4-flash".into(), "Fast, cost-effective".into()),
            ("deepseek-v4-pro".into(), "High capability".into()),
        ]),
        "/lang" => Some(vec![
            ("zh".into(), "中文 UI + 中文提示词".into()),
            ("en".into(), "English UI + English prompt".into()),
        ]),
        _ => None,
    }
}
