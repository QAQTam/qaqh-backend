//! Keyboard event handling and action dispatch.
//!
//! Maps crossterm key/event to `AppAction` which is consumed by the main loop.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};

/// Actions that the TUI can perform.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub enum AppAction {
    /// Quit the application
    Quit,
    /// Move selection up (j/k in session list)
    SelectUp,
    /// Move selection down
    SelectDown,
    /// Confirm selection or send a chat message (Enter)
    Confirm,
    /// Send the current input as a message
    SendMessage,
    /// Go back to session list (Esc)
    BackToSessions,
    /// Newline in input
    Newline,
    /// Backspace
    Backspace,
    /// Delete forward
    Delete,
    /// Insert character
    Char(char),
    /// Move cursor left
    CursorLeft,
    /// Move cursor right
    CursorRight,
    /// Move cursor to start of line
    Home,
    /// Move cursor to end of line
    End,
    /// Scroll up
    ScrollUp,
    /// Scroll down
    ScrollDown,
    /// Page up
    PageUp,
    /// Page down
    PageDown,
    /// Scroll to bottom
    ScrollBottom,
    /// Paste text (from bracketed paste)
    Paste(String),
    /// Confirm paste (y)
    ConfirmPaste,
    /// Cancel paste (n / Esc)
    CancelPaste,
    /// Terminal resize
    Resize(u16, u16),
    /// Tick for redraw
    Tick,
    /// Delete current session
    DeleteSession,
    /// New session
    NewSession,
    /// Refresh session list
    RefreshSessions,
}

/// Convert a crossterm event into an AppAction.
pub fn event_to_action(event: Event) -> Option<AppAction> {
    match event {
        Event::Key(key) => key_to_action(key),
        Event::Paste(text) => Some(AppAction::Paste(text)),
        Event::Resize(w, h) => Some(AppAction::Resize(w, h)),
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => Some(AppAction::ScrollUp),
            MouseEventKind::ScrollDown => Some(AppAction::ScrollDown),
            _ => None,
        },
        _ => None,
    }
}

fn key_to_action(key: KeyEvent) -> Option<AppAction> {
    // Crossterm can emit Press, Repeat, and Release for a single physical key
    // on Windows and terminals with enhanced keyboard reporting. Treating all
    // three as text input is the source of duplicated characters.
    if key.kind != KeyEventKind::Press {
        return None;
    }

    // Ctrl+C always quits
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Some(AppAction::Quit);
    }

    // Esc: back to session list or cancel paste
    if key.code == KeyCode::Esc {
        return Some(AppAction::BackToSessions);
    }

    match (key.code, key.modifiers) {
        // Navigation
        (KeyCode::Up, _) => Some(AppAction::SelectUp),
        (KeyCode::Down, _) => Some(AppAction::SelectDown),

        // Enter variations
        (KeyCode::Enter, modifiers) if modifiers.contains(KeyModifiers::SHIFT) => {
            Some(AppAction::Newline)
        }
        (KeyCode::Enter, _) => Some(AppAction::Confirm),
        (KeyCode::Char('j'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Some(AppAction::SendMessage)
        }

        // Input editing
        (KeyCode::Backspace, _) => Some(AppAction::Backspace),
        (KeyCode::Delete, _) => Some(AppAction::Delete),
        (KeyCode::Left, _) => Some(AppAction::CursorLeft),
        (KeyCode::Right, _) => Some(AppAction::CursorRight),
        (KeyCode::Home, _) => Some(AppAction::Home),
        (KeyCode::End, modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            Some(AppAction::ScrollBottom)
        }
        (KeyCode::End, _) => Some(AppAction::End),

        // Scrolling
        (KeyCode::PageUp, _) => Some(AppAction::PageUp),
        (KeyCode::PageDown, _) => Some(AppAction::PageDown),
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => Some(AppAction::PageUp),
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => Some(AppAction::PageDown),
        // Character input
        (KeyCode::Char(c), modifiers)
            if !modifiers.intersects(
                KeyModifiers::CONTROL
                    | KeyModifiers::ALT
                    | KeyModifiers::SUPER
                    | KeyModifiers::HYPER
                    | KeyModifiers::META,
            ) =>
        {
            Some(AppAction::Char(c))
        }

        // Session management
        (KeyCode::Char('n'), KeyModifiers::CONTROL) => Some(AppAction::NewSession),
        (KeyCode::Char('r'), KeyModifiers::CONTROL) => Some(AppAction::RefreshSessions),

        // Delete (Ctrl+D) was handled above
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventState, MouseButton, MouseEvent};

    fn key(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers,
            kind,
            state: KeyEventState::NONE,
        })
    }

    #[test]
    fn accepts_only_press_events_for_text_input() {
        assert_eq!(
            event_to_action(key(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
                KeyEventKind::Press,
            )),
            Some(AppAction::Char('a'))
        );
        assert_eq!(
            event_to_action(key(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
                KeyEventKind::Repeat,
            )),
            None
        );
        assert_eq!(
            event_to_action(key(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            )),
            None
        );
    }

    #[test]
    fn enter_sends_and_shift_enter_inserts_a_newline() {
        assert_eq!(
            event_to_action(key(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Press,)),
            Some(AppAction::Confirm)
        );
        assert_eq!(
            event_to_action(key(
                KeyCode::Enter,
                KeyModifiers::SHIFT,
                KeyEventKind::Press,
            )),
            Some(AppAction::Newline)
        );
    }

    #[test]
    fn vim_letters_remain_regular_chat_input() {
        assert_eq!(
            event_to_action(key(
                KeyCode::Char('j'),
                KeyModifiers::NONE,
                KeyEventKind::Press,
            )),
            Some(AppAction::Char('j'))
        );
        assert_eq!(
            event_to_action(key(
                KeyCode::Char('k'),
                KeyModifiers::NONE,
                KeyEventKind::Press,
            )),
            Some(AppAction::Char('k'))
        );
    }

    #[test]
    fn mouse_wheel_maps_to_bounded_scroll_actions() {
        let mouse = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            })
        };
        assert_eq!(
            event_to_action(mouse(MouseEventKind::ScrollUp)),
            Some(AppAction::ScrollUp)
        );
        assert_eq!(
            event_to_action(mouse(MouseEventKind::ScrollDown)),
            Some(AppAction::ScrollDown)
        );
        assert_eq!(
            event_to_action(mouse(MouseEventKind::Down(MouseButton::Left))),
            None
        );
    }
}
