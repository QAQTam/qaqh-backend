//! Tool confirmation keyboard handlers.

use crossterm::event::{self, KeyCode};
use tokio::sync::mpsc;

use crate::api::StreamEvent;
use crate::orchestrator::tool_dispatch;

use super::super::app::App;

pub(crate) fn handle_confirm_key(
    app: &mut App,
    key: event::KeyEvent,
    tx: mpsc::Sender<StreamEvent>,
) {
    match key.code {
        KeyCode::Enter => tool_dispatch::handle_confirm_tools(app, tx),
        KeyCode::Esc => tool_dispatch::handle_reject_tools(app),
        _ => {}
    }
}

pub(crate) fn handle_danger_key(
    app: &mut App,
    key: event::KeyEvent,
    tx: mpsc::Sender<StreamEvent>,
) {
    match key.code {
        KeyCode::Enter => tool_dispatch::handle_confirm_tools(app, tx),
        KeyCode::Esc => tool_dispatch::handle_reject_tools(app),
        _ => {}
    }
}
