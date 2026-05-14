//! Sudo password prompt keyboard handler — pure state machine.

use crossterm::event::{self, KeyCode};

use super::action::InputAction;
use super::state::InputState;
use crate::tui::utils::{char_boundary, prev_char_boundary};

pub fn handle_sudo_key(state: &mut InputState, key: event::KeyEvent) -> InputAction {
    match key.code {
        KeyCode::Esc => InputAction::CancelSudo,
        KeyCode::Enter => {
            let pwd = state.sudo.password.clone();
            state.sudo.password.clear();
            InputAction::SubmitSudo(pwd)
        }
        KeyCode::Char(c) => {
            let pos = char_boundary(&state.sudo.password, state.sudo.password.len());
            state.sudo.password.insert(pos, c);
            InputAction::None
        }
        KeyCode::Backspace => {
            if !state.sudo.password.is_empty() {
                let prev = prev_char_boundary(&state.sudo.password, state.sudo.password.len());
                state.sudo.password.remove(prev);
            }
            InputAction::None
        }
        _ => InputAction::None,
    }
}
