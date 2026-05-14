//! Keyboard input handlers for each TUI mode.
//!
//! Each handler is a pure state machine: `(&mut InputState, key, deps) -> InputAction`.
//! The event loop (via `dispatch_action`) maps actions to App state mutations and IPC.

mod chat;
mod confirm;
mod sudo;
mod picker;

pub(crate) mod state;
pub(crate) mod action;

// Re-export handler functions (used by the event loop)
pub(crate) use chat::handle_chat_key;
pub(crate) use confirm::{handle_confirm_key, handle_danger_key};
pub(crate) use sudo::handle_sudo_key;
pub(crate) use picker::{handle_intent_picker_key, handle_session_picker_key};
