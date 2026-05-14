# Input Processing Subsystem Design

## E02 — Input Processing

### 1. Problem Statement

Currently, input handlers are tightly coupled to the monolithic `App` struct. Each handler (`handle_chat_key`, `handle_confirm_key`, etc.) takes `&mut App` and directly mutates anything — state, side effects, spawning tasks. This makes testing impossible, mode transitions fragile, and the event loop (`mod.rs:103-137`) is cluttered with inline exit handling and mode-specific logic.

### 2. Architecture: State Machine + Action Queue

The core idea: **input handlers are pure(ish) state machines**. They receive `(&mut InputState, KeyEvent)`, update input-level state, and return `InputAction`. The event loop (or AppState owner, E01) maps actions to App-level side effects.

```
[KeyEvent]
    |
    v
InputHandler[mode]  ── mutates ──>  InputState
    |
    v
InputAction  ── consumed by ──>  EventLoop / AppState
                                        |
                                        v
                                  Side effects:
                                  - send_message(tx, ...)
                                  - start_agent_loop(tx)
                                  - load_session(...)
                                  - switch mode
```

### 3. InputState — All input-mode-local state in one struct

```rust
// File: input/state.rs (NEW)

use crate::types::SessionMeta;
use super::mode::InputMode;

/// All input-mode-local state, extracted from the monolithic App.
/// Lives inside AppState (E01's domain).
pub struct InputState {
    pub active_mode: InputMode,

    // ── Chat mode ──
    pub chat: ChatState,

    // ── Slash menu (embedded in chat) ──
    pub slash: SlashState,

    // ── Picker modes ──
    pub intent: IntentPickerState,
    pub session: SessionPickerState,

    // ── Sudo mode ──
    pub sudo: SudoState,

    // ── Exit confirmation ──
    pub exit_prompt: bool,
}

// ── Per-mode state structs ──

pub struct ChatState {
    pub buffer: String,        // was app.input
    pub cursor: usize,         // was app.input_cursor
}

pub struct SlashState {
    pub open: bool,
    pub filter: String,
    pub idx: usize,
    pub items: Vec<(String, String)>,
    pub submenu: Option<SlashSubMenu>,
    pub esc_just_closed: bool,
}

pub struct SlashSubMenu {
    pub parent_cmd: String,
    pub items: Vec<(String, String)>,
    pub idx: usize,
}

pub struct IntentPickerState {
    pub question: String,
    pub options: Vec<String>,
}

pub struct SessionPickerState {
    pub list: Vec<SessionMeta>,
    pub pick_idx: usize,
}

pub struct SudoState {
    pub password: String,
}
```

#### InputMode (replaces Mode for input purposes)

```rust
// File: input/mode.rs (NEW — replaces current mode.rs within input)

pub enum InputMode {
    Chat,
    Confirm,
    Danger,
    Sudo,
    IntentPicker,
    SessionPicker,
}
```

Note: `Mode::Streaming` and `Mode::ApiConfig` are NOT input modes — they don't have keyboard state machines (Streaming just captures Esc/Ctrl+C, ApiConfig is a wizard that reuses the chat handler). Streaming exit can be a trivial inline check in the event loop. ApiConfig can remain as an App-level mode with a simple inline handler, or be promoted to its own input mode later.

### 4. InputAction — The contract between E02 and E01

```rust
// File: input/action.rs (NEW)

use crate::types::SessionMeta;

/// Actions produced by input handlers, consumed by AppState/event loop.
pub enum InputAction {
    /// Key was consumed but produced no app-level effect
    None,

    // ── Chat ──
    SendMessage(String),            // buffer content to send

    // ── Tool confirmation ──
    ConfirmTools,
    RejectTools,

    // ── Sudo ──
    SubmitSudo(String),             // the password
    CancelSudo,                     // user declined, inject error messages

    // ── Intent picker ──
    SelectIntent(String),           // selected or typed intent

    // ── Session picker ──
    LoadSession(usize),             // index into session list

    // ── Scroll / Display ──
    ScrollUp(usize),
    ScrollDown(usize),
    ScrollPageUp,
    ScrollPageDown,
    ScrollToTop,
    ScrollToBottom,
    ToggleAutoScroll,

    // ── Slash menu ──
    OpenSubmenu(String, Vec<(String, String)>),
    CloseSubmenu,
    SelectSlashItem(usize),         // index into slash_items
    SelectSubmenuItem(usize),       // index into submenu items

    // ── App control ──
    RequestExit,                    // first Ctrl+C / Esc from exit prompt
    ConfirmExit,                    // second Ctrl+C / Enter from exit prompt
    CancelExit,                     // Esc from exit prompt
    CancelStream,
    CancelExec,
    SetInput(String, usize),        // paste: insert text at cursor
}
```

### 5. Handler Signatures

Each handler is a **free function** taking `(&mut InputState, KeyEvent, &[external]) -> InputAction`:

```rust
// chat.rs
pub fn handle_chat_key(
    state: &mut InputState,
    key: KeyEvent,
    commands: &[(String, String)],     // injected from config, not called internally
) -> InputAction;

// confirm.rs
pub fn handle_confirm_key(state: &mut InputState, key: KeyEvent) -> InputAction;
pub fn handle_danger_key(state: &mut InputState, key: KeyEvent) -> InputAction;

// sudo.rs
pub fn handle_sudo_key(state: &mut InputState, key: KeyEvent) -> InputAction;

// picker.rs
pub fn handle_intent_picker_key(state: &mut InputState, key: KeyEvent) -> InputAction;
pub fn handle_session_picker_key(state: &mut InputState, key: KeyEvent) -> InputAction;
```

**Key design rules:**
- Handlers NEVER access `tx`, `App`, or any async resource
- Handlers NEVER spawn tasks
- Handlers only mutate `InputState` and return `InputAction`
- External data (command list, session list) is passed in, not fetched inside

### 6. Integration with E01 (AppState)

E01 owns `InputState` inside `AppState`. The event loop pattern becomes:

```rust
// In event loop (replaces mod.rs:101-137):

fn handle_input(app: &mut AppState, key: KeyEvent) -> Vec<InputAction> {
    let input = &mut app.input_state;
    let action = match input.active_mode {
        InputMode::Chat => handle_chat_key(input, key, &app.config.commands()),
        InputMode::Confirm => handle_confirm_key(input, key),
        InputMode::Danger => handle_danger_key(input, key),
        InputMode::Sudo => handle_sudo_key(input, key),
        InputMode::IntentPicker => handle_intent_picker_key(input, key),
        InputMode::SessionPicker => handle_session_picker_key(input, key),
    };
    // Also handle exit-related actions (Esc/Ctrl+C/Enter when exit_prompt is active)
    // These can be handled uniformly via InputAction
    vec![action] // could be multiple with esc_just_closed cleanup
}

fn apply_action(app: &mut AppState, action: InputAction, tx: &mpsc::Sender<StreamEvent>) {
    match action {
        InputAction::SendMessage(text) => {
            app.send_message(text, tx);
        }
        InputAction::ConfirmTools => {
            tool_dispatch::handle_confirm_tools(app, tx);
        }
        InputAction::RejectTools => {
            tool_dispatch::handle_reject_tools(app);
        }
        InputAction::SubmitSudo(pwd) => {
            app.sudo_execute(pwd, tx);
        }
        InputAction::CancelSudo => {
            app.sudo_cancel(tx);
        }
        InputAction::SelectIntent(answer) => {
            app.finalize_intent(answer, tx);
        }
        InputAction::LoadSession(idx) => {
            app.load_session(idx);
        }
        InputAction::RequestExit => {
            app.exit_prompt = true;
        }
        InputAction::ConfirmExit => {
            // break from event loop
        }
        InputAction::CancelExit => {
            app.exit_prompt = false;
        }
        InputAction::CancelStream => {
            tool_dispatch::handle_cancel_stream(app);
        }
        InputAction::CancelExec => {
            tool_dispatch::handle_cancel_exec(app);
        }
        InputAction::SetInput(text, cursor) => {
            app.input_state.chat.buffer = text;
            app.input_state.chat.cursor = cursor;
        }
        // Scroll actions → app.scroll_offset changes
        InputAction::ScrollUp(n) => { app.scroll_offset = app.scroll_offset.saturating_sub(n); }
        InputAction::ScrollDown(n) => { app.scroll_offset = app.scroll_offset.saturating_add(n); }
        // etc.
        InputAction::None => {}
    }
}
```

**Data that flows from App → Input (injected):**
- `commands: &[(String, String)]` — for slash menu filtering (from `config::all_commands`)
- `session_list: &[SessionMeta]` — for session picker
- `intent_options: &[String]` — for intent picker

**Data that flows from Input → App (via InputAction):**
- Everything in the `InputAction` enum

### 7. utils.rs — What Stays vs. Moves

| Function | Stay/Move | Reason |
|---|---|---|
| `strip_ansi` | **STAY** in TUI | Pure display cleanup |
| `display_width` | **STAY** in TUI | Terminal column calculation |
| `format_bytes` | **STAY** in TUI | Display formatting only |
| `pad_to_width` | **STAY** in TUI | Display padding |
| `textwrap` | **STAY** in TUI | Terminal word wrap |
| `truncate` | **STAY** in TUI | Display truncation |
| `char_boundary` | **STAY** in TUI | Cursor positioning in chat buffer |
| `prev_char_boundary` | **STAY** in TUI | Cursor positioning in chat buffer |
| `next_char_boundary` | **STAY** in TUI | Cursor positioning in chat buffer |
| `detect_phase` | **MOVE** to Agent/Orohestrator | Analyzes AI reasoning to determine AgentPhase — Agent domain |
| `score` | **MOVE** to Agent/Orohestrator | Keyword scoring helper (only used by detect_phase) |
| `detect_task_phase_from_reasoning` | **MOVE** to Agent/Orohestrator | Analyzes AI reasoning to determine TaskPhase — Agent domain |

Functions that move become methods on the Agent's phase analyzer module (e.g., `orchestrator::phase_detector`).

### 8. tui_text.rs — i18n Belongs in TUI

**Decision: Keep in TUI, do NOT move to Agent.**

Rationale:
- `TuiText` is purely about visual display strings (labels, prompts, headers). It has zero influence on Agent behavior.
- The Agent should not decide UI language — that's a presentation-layer concern.
- The current interface is clean: `tui_text(lang: &str) -> &'static TuiText`. The `lang` value comes from `config.prompt_lang`, which is an App-level config field.
- Moving it to Agent would create a circular dependency: Agent depends on nothing from TUI, and TUI is the only consumer of these strings.

The one justified change: if the Agent emits localized strings (e.g., Chinese status messages that get injected into message history), then those specific strings should come from Agent. But `tui_text.rs` is all UI chrome — it stays here.

### 9. Mode Handling — Current State vs. Target

**Current** (mod.rs):
- `Mode` enum has 9 variants including `Streaming`, `ApiConfig`, etc.
- Event loop has inline checks: `if app.mode == Mode::Streaming { ... }`, `if app.mode == Mode::Chat || app.mode == Mode::ApiConfig { ... }`
- Exit handling is inline after the per-mode match

**Target**:
- `InputMode` (6 variants) covers only modes with keyboard state machines
- `Mode::Streaming` is handled separately — it only needs Esc/Ctrl+C capture, no state machine
- `Mode::ApiConfig` can be folded into `ChatState` with an `is_setup_wizard` flag, or become a separate lean handler
- Exit actions (`RequestExit`, `ConfirmExit`, `CancelExit`) are proper `InputAction` variants, not inline logic
- Paste is an `InputAction::SetInput` rather than inline buffer mutation

### 10. Testability

The refactoring makes input handlers testable:

```rust
#[test]
fn test_chat_text_input() {
    let mut state = InputState::default();
    let key = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE);
    let action = handle_chat_key(&mut state, key, &[]);
    assert_eq!(state.chat.buffer, "h");
    assert_eq!(state.chat.cursor, 1);
    assert!(matches!(action, InputAction::None));
}

#[test]
fn test_chat_enter_sends_message() {
    let mut state = InputState::default();
    state.chat.buffer = "hello".into();
    state.chat.cursor = 5;
    let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let action = handle_chat_key(&mut state, key, &[]);
    assert!(matches!(action, InputAction::SendMessage(text) if text == "hello"));
}

#[test]
fn test_confirm_enter() {
    let mut state = InputState::default();
    state.active_mode = InputMode::Confirm;
    let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    let action = handle_confirm_key(&mut state, key);
    assert!(matches!(action, InputAction::ConfirmTools));
}
```

### 11. Migration Path

1. Create `input/state.rs` with `InputState` and all per-mode state structs
2. Create `input/action.rs` with `InputAction` enum
3. Create `input/mode.rs` with `InputMode` enum
4. Refactor each handler to `(&mut InputState, KeyEvent, ...) -> InputAction` signature
5. Move `update_slash_items` into `handle_chat_key` or make it a method on `SlashState` with injected command list
6. Extract exit/paste/Ctrl+C handling from `mod.rs` into `InputAction` flow
7. Move `detect_phase`, `score`, `detect_task_phase_from_reasoning` out of `utils.rs` to Agent module
8. Remove now-unnecessary `App` field accesses from input handlers

### 12. File Manifest (after refactor)

```
input/
  mod.rs       — re-exports, pub API surface
  state.rs     — InputState + per-mode state structs (NEW)
  action.rs    — InputAction enum (NEW)
  mode.rs      — InputMode enum (NEW)
  chat.rs      — handle_chat_key (REFACTORED)
  confirm.rs   — handle_confirm_key, handle_danger_key (REFACTORED)
  sudo.rs      — handle_sudo_key (REFACTORED)
  picker.rs    — handle_intent_picker_key, handle_session_picker_key (REFACTORED)

utils.rs       — KEEP: strip_ansi, display_width, format_bytes,
                 pad_to_width, textwrap, truncate,
                 char_boundary, prev_char_boundary, next_char_boundary
                 REMOVE: detect_phase, score, detect_task_phase_from_reasoning

tui_text.rs    — KEEP as-is (TuiText, tui_text())
```
