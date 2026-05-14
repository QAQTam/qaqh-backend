//! InputState — all input-mode-local state, extracted from the monolithic App.
//!
//! Each mode has its own state struct. Handlers mutate their mode's state
//! and return `InputAction`. The event loop applies the action.
//!
//! InputState lives inside AppState (see state.rs).

use crate::types::SessionMeta;

/// Active input mode — subset of the full Mode enum covering
/// only modes with keyboard state machines.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputMode {
    Chat,
    Confirm,
    Danger,
    Sudo,
    IntentPicker,
    SessionPicker,
}

// ── Chat ──

#[derive(Debug, Clone)]
pub struct ChatState {
    pub buffer: String,
    pub cursor: usize,
}

impl Default for ChatState {
    fn default() -> Self {
        Self {
            buffer: String::new(),
            cursor: 0,
        }
    }
}

// ── Slash menu — embedded in Chat, has its own navigation state ──

#[derive(Debug, Clone)]
pub struct SlashSubMenu {
    pub parent_cmd: String,
    pub items: Vec<(String, String)>,
    pub idx: usize,
}

#[derive(Debug, Clone)]
pub struct SlashState {
    pub open: bool,
    pub filter: String,
    pub idx: usize,
    pub items: Vec<(String, String)>,
    pub submenu: Option<SlashSubMenu>,
    pub esc_just_closed: bool,
}

impl Default for SlashState {
    fn default() -> Self {
        Self {
            open: false,
            filter: String::new(),
            idx: 0,
            items: Vec::new(),
            submenu: None,
            esc_just_closed: false,
        }
    }
}

// ── Intent picker ──

#[derive(Debug, Clone)]
pub struct IntentState {
    pub question: String,
    pub options: Vec<String>,
}

impl Default for IntentState {
    fn default() -> Self {
        Self {
            question: String::new(),
            options: Vec::new(),
        }
    }
}

// ── Session picker ──

#[derive(Debug, Clone)]
pub struct SessionState {
    pub list: Vec<SessionMeta>,
    pub pick_idx: usize,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            list: Vec::new(),
            pick_idx: 0,
        }
    }
}

// ── Sudo ──

#[derive(Debug, Clone)]
pub struct SudoState {
    pub password: String,
}

impl Default for SudoState {
    fn default() -> Self {
        Self {
            password: String::new(),
        }
    }
}

// ── Top-level InputState ──

#[derive(Debug, Clone)]
pub struct InputState {
    pub active_mode: InputMode,
    pub chat: ChatState,
    pub slash: SlashState,
    pub intent: IntentState,
    pub session: SessionState,
    pub sudo: SudoState,
    pub exit_prompt: bool,
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            active_mode: InputMode::Chat,
            chat: ChatState::default(),
            slash: SlashState::default(),
            intent: IntentState::default(),
            session: SessionState::default(),
            sudo: SudoState::default(),
            exit_prompt: false,
        }
    }
}

impl InputState {
    /// Reset per-send state after a message is submitted
    pub fn clear_chat(&mut self) {
        self.chat.buffer.clear();
        self.chat.cursor = 0;
    }

    /// Switch input mode, resetting mode-local state as needed
    pub fn switch_mode(&mut self, mode: InputMode) {
        self.active_mode = mode;
        // Clear transient state that shouldn't persist across mode switches
        match mode {
            InputMode::Sudo => { /* keep password in case user retries */ }
            InputMode::IntentPicker => { /* keep question/options */ }
            InputMode::SessionPicker => { /* keep list */ }
            InputMode::Chat => {
                self.exit_prompt = false;
            }
            InputMode::Confirm | InputMode::Danger => {}
        }
    }
}
