//! Tool confirmation keyboard handlers — pure state machines.
//!
//! Enter → ConfirmTools, Esc → RejectTools.

use crossterm::event::{self, KeyCode};

use super::action::InputAction;
use super::state::InputState;

pub fn handle_confirm_key(state: &mut InputState, key: event::KeyEvent) -> InputAction {
    match key.code {
        KeyCode::Enter => InputAction::ConfirmTools,
        KeyCode::Esc => InputAction::RejectTools,
        _ => InputAction::None,
    }
}

pub fn handle_danger_key(state: &mut InputState, key: event::KeyEvent) -> InputAction {
    match key.code {
        KeyCode::Enter => InputAction::ConfirmTools,
        KeyCode::Esc => InputAction::RejectTools,
        _ => InputAction::None,
    }
}
