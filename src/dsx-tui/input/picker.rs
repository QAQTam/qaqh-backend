//! Picker keyboard handlers (intent and session) — pure state machines.

use crossterm::event::{self, KeyCode};

use super::action::InputAction;
use super::state::{InputMode, InputState};
use crate::tui::utils::{char_boundary, prev_char_boundary};

pub fn handle_intent_picker_key(state: &mut InputState, key: event::KeyEvent) -> InputAction {
    match key.code {
        KeyCode::Esc => {
            state.active_mode = InputMode::Chat;
            InputAction::None
        }
        KeyCode::Char(ref c)
            if c.is_ascii_alphabetic() && c.to_ascii_lowercase() >= 'a' && c.to_ascii_lowercase() <= 'f' =>
        {
            let idx = (c.to_ascii_lowercase() as usize).saturating_sub(97);
            if idx < state.intent.options.len() {
                let answer = state.intent.options[idx].clone();
                InputAction::SelectIntent(answer)
            } else {
                // Typing: append to buffer
                let pos = char_boundary(&state.chat.buffer, state.chat.cursor);
                state.chat.buffer.insert(pos, *c);
                state.chat.cursor = pos + c.len_utf8();
                InputAction::None
            }
        }
        KeyCode::Enter => {
            let answer = if state.chat.buffer.is_empty() {
                "keep going".to_string()
            } else {
                state.chat.buffer.trim().to_string()
            };
            state.chat.buffer.clear();
            state.chat.cursor = 0;
            InputAction::SelectIntent(answer)
        }
        KeyCode::Char(c) => {
            let pos = char_boundary(&state.chat.buffer, state.chat.cursor);
            state.chat.buffer.insert(pos, c);
            state.chat.cursor = pos + c.len_utf8();
            InputAction::None
        }
        KeyCode::Backspace => {
            if state.chat.cursor > 0 && !state.chat.buffer.is_empty() {
                let byte_cursor = state
                    .chat
                    .buffer
                    .char_indices()
                    .nth(state.chat.cursor)
                    .map(|(i, _)| i)
                    .unwrap_or(state.chat.buffer.len());
                let prev_start = state.chat.buffer[..byte_cursor]
                    .char_indices()
                    .rev()
                    .next()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                state.chat.buffer.drain(prev_start..byte_cursor);
                state.chat.cursor = state.chat.cursor.saturating_sub(1);
            }
            InputAction::None
        }
        _ => InputAction::None,
    }
}

pub fn handle_session_picker_key(state: &mut InputState, key: event::KeyEvent) -> InputAction {
    match key.code {
        KeyCode::Esc => InputAction::ConfirmExit, // Session picker Esc = quit app
        KeyCode::Up => {
            state.session.pick_idx = state.session.pick_idx.saturating_sub(1);
            InputAction::None
        }
        KeyCode::Down => {
            if state.session.pick_idx + 1 < state.session.list.len() {
                state.session.pick_idx += 1;
            }
            InputAction::None
        }
        KeyCode::Enter => InputAction::LoadSession(state.session.pick_idx),
        _ => InputAction::None,
    }
}
