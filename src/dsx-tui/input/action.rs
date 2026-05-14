//! Actions produced by input handlers, consumed by the event loop.
//!
//! Each handler receives `(&mut InputState, KeyEvent)` and returns `InputAction`.
//! The event loop maps `InputAction` to AppState side effects (send message,
//! confirm tools, cancel stream, etc.) — handlers never access IPC or async resources.

use crate::types::SessionMeta;

/// Neutral action: no app-level side effect needed
pub const NO_ACTION: InputAction = InputAction::None;

#[derive(Debug, Clone, PartialEq)]
pub enum InputAction {
    /// Key was consumed without producing any app-level effect
    None,

    // ── Chat ──

    /// Send the current chat buffer as a user message
    SendMessage(String),

    /// Set the chat buffer to a new value and cursor position (paste, etc.)
    SetBuffer { text: String, cursor: usize },

    // ── Tool confirmation ──

    /// Accept all pending tool calls
    ConfirmTools,

    /// Reject all pending tool calls
    RejectTools,

    // ── Sudo ──

    /// Submit the sudo password to execute commands
    SubmitSudo(String),

    /// Cancel sudo prompt and notify AI of refusal
    CancelSudo,

    // ── Intent picker ──

    /// Select an intent option (a string from the picker or custom input)
    SelectIntent(String),

    // ── Session picker ──

    /// Resume a session at the given index
    LoadSession(usize),

    // ── Scroll ──

    ScrollUp(usize),
    ScrollDown(usize),
    ScrollPageUp,
    ScrollPageDown,
    ScrollToTop,
    ScrollToBottom,
    ToggleAutoScroll,

    // ── Slash menu ──

    /// Open a submenu for a slash command
    OpenSlashSubmenu { parent_cmd: String, items: Vec<(String, String)> },

    // ── App mode ──

    /// Request exit (first Ctrl+C)
    RequestExit,

    /// Confirm exit (second Ctrl+C or Enter when exit prompt shown)
    ConfirmExit,

    /// Cancel exit (Esc when exit prompt shown)
    CancelExit,

    /// Cancel active stream (Esc during streaming)
    CancelStream,

    /// Cancel running exec subprocess (Esc/Ctrl+C during exec)
    CancelExec,
}
