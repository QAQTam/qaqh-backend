//! App state machine and message model for deepx-tui.

use chrono::Utc;
use deepx_proto::{Agent2Ui, ControlServerMessage, RoundDeltaKind, SessionActivityState};
use serde_json::Value;

use crate::input::InputWidget;

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    SessionList,
    Chat,
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub seed: String,
    pub name: String,
    pub preview: String,
    pub updated_at: u64,
    pub turn_count: usize,
    pub message_count: usize,
    pub model: String,
    pub running: bool,
}

pub type MsgId = usize;

#[derive(Debug, Clone)]
pub struct ChatMessage {
    #[allow(dead_code)]
    pub id: MsgId,
    #[allow(dead_code)]
    pub turn_id: String,
    pub round_num: Option<u32>,
    pub kind: MessageKind,
    #[allow(dead_code)]
    pub timestamp: chrono::DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub enum MessageKind {
    User {
        text: String,
    },
    Thinking {
        content: String,
        complete: bool,
    },
    Answer {
        content: String,
        complete: bool,
    },
    ToolUse {
        #[allow(dead_code)]
        tool_call_id: String,
        tool_name: String,
        params: Value,
    },
    ToolResult {
        #[allow(dead_code)]
        tool_call_id: String,
        output: String,
        #[allow(dead_code)]
        success: bool,
    },
    ToolExecDelta {
        tool_call_id: String,
        delta: String,
    },
    Block {
        block_type: String,
        content: String,
    },
    TurnEnd {
        stop_reason: String,
        usage: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum PasteState {
    None,
    Confirm { line_count: usize, text: String },
}

pub struct App {
    pub mode: Mode,
    pub connected: bool,
    pub sessions: Vec<SessionInfo>,
    pub selection_index: usize,
    pub current_session_seed: Option<String>,
    pub current_session_name: String,
    pub messages: Vec<ChatMessage>,
    next_msg_id: usize,
    pub input: InputWidget,
    pub paste_state: PasteState,
    pub current_turn: u64,
    pub tokens_used: u64,
    #[allow(dead_code)]
    pub tokens_limit: u64,
    pub status_message: String,
    pub scroll_offset: usize,
    pub scroll_max: usize,
    pub follow_tail: bool,
    pub terminal_height: usize,
    pub terminal_width: usize,
}

impl App {
    pub fn new() -> Self {
        Self {
            mode: Mode::SessionList,
            connected: false,
            sessions: Vec::new(),
            selection_index: 0,
            current_session_seed: None,
            current_session_name: String::new(),
            messages: Vec::new(),
            next_msg_id: 0,
            input: InputWidget::new(),
            paste_state: PasteState::None,
            current_turn: 0,
            tokens_used: 0,
            tokens_limit: 200_000,
            status_message: String::from("Connecting..."),
            scroll_offset: 0,
            scroll_max: 0,
            follow_tail: true,
            terminal_height: 24,
            terminal_width: 80,
        }
    }

    fn next_id(&mut self) -> MsgId {
        let id = self.next_msg_id;
        self.next_msg_id += 1;
        id
    }

    fn push_message(&mut self, kind: MessageKind, turn_id: String, round_num: Option<u32>) {
        let id = self.next_id();
        self.messages.push(ChatMessage {
            id,
            turn_id,
            round_num,
            kind,
            timestamp: Utc::now(),
        });
    }

    fn push_delta(&mut self, turn_id: String, round_num: u32, kind: MessageKind, delta: &str) {
        let found = self.messages.iter_mut().rev().find(|m| {
            m.turn_id == turn_id
                && m.round_num == Some(round_num)
                && std::mem::discriminant(&m.kind) == std::mem::discriminant(&kind)
                && matches!(
                    &m.kind,
                    MessageKind::Thinking {
                        complete: false,
                        ..
                    } | MessageKind::Answer {
                        complete: false,
                        ..
                    }
                )
        });
        if let Some(msg) = found {
            match &mut msg.kind {
                MessageKind::Thinking { content, .. } | MessageKind::Answer { content, .. } => {
                    content.push_str(delta)
                }
                _ => {}
            }
        } else {
            // No existing delta message — create one with the first chunk as content
            let mut k = kind;
            match &mut k {
                MessageKind::Thinking { content, .. } | MessageKind::Answer { content, .. } => {
                    content.push_str(delta);
                }
                _ => {}
            }
            self.push_message(k, turn_id, Some(round_num));
        }
    }

    fn finalize_round(&mut self, round_num: u32) {
        for msg in &mut self.messages {
            if msg.round_num == Some(round_num) {
                match &mut msg.kind {
                    MessageKind::Thinking { complete, .. }
                    | MessageKind::Answer { complete, .. } => *complete = true,
                    _ => {}
                }
            }
        }
    }

    // ── Control message handler ──────────────────────────────��───────

    pub fn handle_control_message(&mut self, msg: ControlServerMessage) {
        match msg {
            ControlServerMessage::ServerHello { .. } => {
                self.connected = true;
                self.status_message = String::from("Connected");
            }

            ControlServerMessage::Snapshot { snapshot, .. } => {
                self.connected = true;
                self.status_message = String::from("Connected");
                self.set_sessions(snapshot.sessions.iter().map(parse_session_info).collect());

                // Restore events for current session
                let seed_opt = self.current_session_seed.clone();
                if let Some(seed) = seed_opt
                    && let Some(events) = snapshot.session_events.get(&seed)
                {
                    for ev in events {
                        let ev = ev.clone();
                        self.handle_agent_event_inner(ev);
                    }
                }
            }

            ControlServerMessage::SessionActivity { activity } => {
                self.status_message = match activity.state {
                    SessionActivityState::Starting => {
                        format!("Session {} starting...", activity.seed)
                    }
                    SessionActivityState::Idle => String::from("Idle"),
                    SessionActivityState::Working => String::from("Working..."),
                    SessionActivityState::WaitingUser => String::from("Waiting"),
                    SessionActivityState::Disconnected => {
                        format!("Session {} disconnected", activity.seed)
                    }
                };
            }

            ControlServerMessage::Event { event, .. } => {
                self.handle_agent_event_inner(event);
            }

            ControlServerMessage::Response { result, .. } => {
                self.handle_response(result);
            }

            ControlServerMessage::Shutdown { .. } => {
                self.connected = false;
                self.status_message = String::from("Disconnected");
            }

            ControlServerMessage::Error { message, .. } => {
                self.status_message = format!("Error: {message}");
            }

            _ => {}
        }
    }

    fn handle_agent_event_inner(&mut self, event: Agent2Ui) {
        match event {
            Agent2Ui::TurnStart { turn_id, user_text } => {
                self.current_turn += 1;
                self.push_message(MessageKind::User { text: user_text }, turn_id, None);
            }

            Agent2Ui::RoundDelta {
                turn_id,
                round_num,
                kind,
                delta,
            } => {
                let msg_kind = match kind {
                    RoundDeltaKind::Thinking => MessageKind::Thinking {
                        content: String::new(),
                        complete: false,
                    },
                    _ => MessageKind::Answer {
                        content: String::new(),
                        complete: false,
                    },
                };
                self.push_delta(turn_id, round_num, msg_kind, &delta);
            }

            Agent2Ui::RoundComplete {
                turn_id,
                round_num,
                thinking,
                answer,
                tool_calls,
                blocks,
                ..
            } => {
                // Clean up streamed Thinking deltas for this round.
                // Only push a complete Thinking as legacy fallback when
                // blocks (preferred) are not available.
                let mut i = self.messages.len();
                while i > 0 {
                    i -= 1;
                    if self.messages[i].round_num == Some(round_num)
                        && matches!(
                            &self.messages[i].kind,
                            MessageKind::Thinking { .. }
                        )
                    {
                        self.messages.remove(i);
                    }
                }
                if blocks.is_empty() {
                    if let Some(t) = &thinking
                        && !t.is_empty()
                    {
                        self.push_message(
                            MessageKind::Thinking {
                                content: t.clone(),
                                complete: true,
                            },
                            turn_id.clone(),
                            Some(round_num),
                        );
                    }
                }

                // Clean up streamed Answer deltas. Same legacy-fallback
                // strategy.
                if let Some(a) = &answer
                    && !a.is_empty()
                {
                    self.messages.retain(|m| {
                        m.round_num != Some(round_num)
                            || !matches!(m.kind, MessageKind::Answer { .. })
                    });
                    if blocks.is_empty() {
                        self.push_message(
                            MessageKind::Answer {
                                content: a.clone(),
                                complete: true,
                            },
                            turn_id.clone(),
                            Some(round_num),
                        );
                    }
                }

                for tc in &tool_calls {
                    self.push_message(
                        MessageKind::ToolUse {
                            tool_name: tc.name.clone(),
                            tool_call_id: tc.id.clone(),
                            params: serde_json::Value::String(tc.args_json.clone()),
                        },
                        turn_id.clone(),
                        Some(round_num),
                    );
                }

                for blk in &blocks {
                    let (block_type_str, content) = match blk {
                        deepx_proto::RoundBlock::Reasoning { content } => {
                            ("reasoning", content.clone())
                        }
                        deepx_proto::RoundBlock::Text { content } => {
                            ("text", content.clone())
                        }
                        deepx_proto::RoundBlock::Tool { card: _ } => continue,
                    };
                    self.push_message(
                        MessageKind::Block {
                            block_type: block_type_str.to_string(),
                            content,
                        },
                        turn_id.clone(),
                        Some(round_num),
                    );
                }

                self.push_message(
                    MessageKind::TurnEnd {
                        stop_reason: String::new(),
                        usage: String::new(),
                    },
                    turn_id.clone(),
                    None,
                );
            }
            Agent2Ui::ToolExecDelta {
                tool_call_id,
                delta,
                ..
            } => {
                let found = self.messages.iter_mut().rev().find(|m| {
                    matches!(&m.kind, MessageKind::ToolExecDelta { tool_call_id: tid, .. } if tid == &tool_call_id)
                });
                if let Some(msg) = found {
                    if let MessageKind::ToolExecDelta {
                        delta: existing, ..
                    } = &mut msg.kind
                    {
                        existing.push_str(&delta);
                    }
                } else {
                    self.push_message(
                        MessageKind::ToolExecDelta {
                            tool_call_id,
                            delta,
                        },
                        String::new(),
                        None,
                    );
                }
            }

            Agent2Ui::SessionRestored {
                turns, tokens_used, ..
            } => {
                // This event is an authoritative baseline. It may arrive once
                // from the attach snapshot and again when the agent finishes
                // resuming, so replace rather than append the transcript.
                self.messages.clear();
                self.current_turn = turns.len() as u64;
                self.tokens_used = tokens_used as u64; // tokens_used is u32
                for turn in turns {
                    // Reconstruct TurnStart
                    let turn_id = turn.turn_id.clone();
                    self.push_message(
                        MessageKind::User {
                            text: turn.user_text,
                        },
                        turn_id.clone(),
                        None,
                    );
                    for rd in &turn.rounds {
                        if !rd.blocks.is_empty() {
                            // Preferred: ordered blocks from LLM output
                            for blk in &rd.blocks {
                                let (block_type_str, content) = match blk {
                                    deepx_proto::RoundBlock::Reasoning { content } => {
                                        ("reasoning", content.clone())
                                    }
                                    deepx_proto::RoundBlock::Text { content } => {
                                        ("text", content.clone())
                                    }
                                    deepx_proto::RoundBlock::Tool { card: _ } => continue,
                                };
                                self.push_message(
                                    MessageKind::Block {
                                        block_type: block_type_str.to_string(),
                                        content,
                                    },
                                    turn_id.clone(),
                                    Some(rd.round_num),
                                );
                            }
                        } else {
                            // Legacy fallback: thinking / answer fields
                            if let Some(t) = &rd.thinking
                                && !t.is_empty()
                            {
                                self.push_message(
                                    MessageKind::Thinking {
                                        content: t.clone(),
                                        complete: true,
                                    },
                                    turn_id.clone(),
                                    Some(rd.round_num),
                                );
                            }
                            if let Some(a) = &rd.answer
                                && !a.is_empty()
                            {
                                self.push_message(
                                    MessageKind::Answer {
                                        content: a.clone(),
                                        complete: true,
                                    },
                                    turn_id.clone(),
                                    Some(rd.round_num),
                                );
                            }
                        }
                    }
                }
            }
            Agent2Ui::SessionCreated { .. } => {
                self.messages.clear();
                self.current_turn = 0;
                self.tokens_used = 0;
                self.scroll_to_bottom();
            }

            Agent2Ui::MoreTurns { .. } => {}
            _ => {}
        }
    }

    fn handle_response(&mut self, result: Value) {
        // `session.list` returns a raw JSON array in the canonical daemon
        // protocol. Keep support for the legacy `{ sessions: [...] }` shape so
        // older daemons remain usable, but never interpret an arbitrary
        // `{ seed }` response (for example an attach acknowledgement) as a new
        // navigation command.
        let sessions = result
            .as_array()
            .or_else(|| result.get("sessions").and_then(Value::as_array));
        if let Some(sessions) = sessions {
            self.set_sessions(sessions.iter().map(parse_session_info).collect());
        }
    }

    fn set_sessions(&mut self, sessions: Vec<SessionInfo>) {
        self.sessions = sessions;
        self.selection_index = self
            .selection_index
            .min(self.sessions.len().saturating_sub(1));
    }

    pub fn replace_sessions_from_value(&mut self, result: Value) {
        self.handle_response(result);
    }

    // ── Actions ────────────────────────────────────────────────────────

    pub fn open_session(&mut self, seed: String) {
        if let Some(name) = self
            .sessions
            .iter()
            .find(|s| s.seed == seed)
            .map(|s| s.name.clone())
        {
            self.open_session_with_name(seed, name);
        }
    }

    pub fn open_new_session(&mut self, seed: String) {
        self.open_session_with_name(seed, String::from("New session"));
    }

    fn open_session_with_name(&mut self, seed: String, name: String) {
        self.current_session_seed = Some(seed);
        self.current_session_name = name.clone();
        self.messages.clear();
        self.scroll_offset = 0;
        self.scroll_max = 0;
        self.follow_tail = true;
        self.mode = Mode::Chat;
        self.input.clear();
        self.paste_state = PasteState::None;
        self.status_message = format!("Opened: {name}");
    }

    pub fn back_to_sessions(&mut self) {
        self.mode = Mode::SessionList;
        self.input.clear();
        self.paste_state = PasteState::None;
    }

    pub fn select_up(&mut self) {
        if self.selection_index > 0 {
            self.selection_index -= 1;
        }
    }
    pub fn select_down(&mut self) {
        if self.selection_index + 1 < self.sessions.len() {
            self.selection_index += 1;
        }
    }
    pub fn selected_session(&self) -> Option<&SessionInfo> {
        self.sessions.get(self.selection_index)
    }

    pub fn input_newline(&mut self) {
        if self.paste_state != PasteState::None {
            return;
        }
        self.input.insert_newline();
    }
    pub fn input_char(&mut self, c: char) {
        if self.paste_state != PasteState::None {
            return;
        }
        self.input.insert_char(c);
    }
    pub fn input_backspace(&mut self) {
        if self.paste_state != PasteState::None {
            return;
        }
        self.input.delete_char();
    }
    pub fn input_delete(&mut self) {
        if self.paste_state != PasteState::None {
            return;
        }
        self.input.delete_next_char();
    }
    pub fn input_cursor_left(&mut self) {
        self.input
            .textarea_mut()
            .move_cursor(ratatui_textarea::CursorMove::Back);
    }
    pub fn input_cursor_right(&mut self) {
        self.input
            .textarea_mut()
            .move_cursor(ratatui_textarea::CursorMove::Forward);
    }
    pub fn input_cursor_up(&mut self) {
        self.input
            .textarea_mut()
            .move_cursor(ratatui_textarea::CursorMove::Up);
    }
    pub fn input_cursor_down(&mut self) {
        self.input
            .textarea_mut()
            .move_cursor(ratatui_textarea::CursorMove::Down);
    }
    pub fn input_home(&mut self) {
        self.input
            .textarea_mut()
            .move_cursor(ratatui_textarea::CursorMove::Head);
    }
    pub fn input_end(&mut self) {
        self.input
            .textarea_mut()
            .move_cursor(ratatui_textarea::CursorMove::End);
    }

    pub fn paste_detected(&mut self, text: &str) {
        let nc = text.chars().filter(|&c| c == '\n').count();
        if nc > 0 {
            self.paste_state = PasteState::Confirm {
                line_count: nc + 1,
                text: text.to_string(),
            };
        } else {
            self.input.insert_str(text);
        }
    }
    pub fn confirm_paste(&mut self) {
        if let PasteState::Confirm { text, .. } = &self.paste_state {
            let t = text.clone();
            self.paste_state = PasteState::None;
            self.input.insert_str(&t);
        }
    }
    pub fn cancel_paste(&mut self) {
        self.paste_state = PasteState::None;
    }
    pub fn take_input(&mut self) -> String {
        let t = self.input.text();
        self.input.clear();
        self.paste_state = PasteState::None;
        t
    }

    pub fn scroll_up(&mut self) {
        self.follow_tail = false;
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
    }
    pub fn scroll_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(1).min(self.scroll_max);
        self.follow_tail = self.scroll_offset == self.scroll_max;
    }
    pub fn page_up(&mut self) {
        self.follow_tail = false;
        self.scroll_offset = self
            .scroll_offset
            .saturating_sub((self.terminal_height / 2).max(5));
    }
    pub fn page_down(&mut self) {
        self.scroll_offset = self
            .scroll_offset
            .saturating_add((self.terminal_height / 2).max(5))
            .min(self.scroll_max);
        self.follow_tail = self.scroll_offset == self.scroll_max;
    }
    pub fn scroll_to_bottom(&mut self) {
        self.follow_tail = true;
        self.scroll_offset = self.scroll_max;
    }
    pub fn update_scroll_bounds(&mut self, max: usize) {
        self.scroll_max = max.min(u16::MAX as usize);
        if self.follow_tail {
            self.scroll_offset = self.scroll_max;
        } else {
            self.scroll_offset = self.scroll_offset.min(self.scroll_max);
        }
    }
    pub fn resize(&mut self, w: u16, h: u16) {
        self.terminal_width = w as usize;
        self.terminal_height = h as usize;
    }
}

fn parse_session_info(s: &Value) -> SessionInfo {
    let seed = s.get("seed").and_then(Value::as_str).unwrap_or_default();
    let summary = normalize_summary(
        s.get("preview")
            .and_then(Value::as_str)
            .or_else(|| s.get("last_summary").and_then(Value::as_str))
            .unwrap_or_default(),
    );
    let explicit_name = s
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| s.get("title").and_then(Value::as_str))
        .map(str::trim)
        .filter(|name| !name.is_empty());
    let name = explicit_name
        .map(str::to_owned)
        .or_else(|| (!summary.is_empty()).then(|| summary.clone()))
        .unwrap_or_else(|| seed.chars().take(8).collect());

    SessionInfo {
        seed: seed.to_string(),
        name: truncate(&name, 52),
        preview: truncate(&summary, 120),
        updated_at: number_or_string(s.get("updated_at")),
        turn_count: s
            .get("turn_count")
            .and_then(Value::as_u64)
            .unwrap_or_default() as usize,
        message_count: s
            .get("message_count")
            .and_then(Value::as_u64)
            .unwrap_or_default() as usize,
        model: s
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        running: s.get("running").and_then(Value::as_bool).unwrap_or(false),
    }
}

fn truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        let keep = max.saturating_sub(1);
        format!("{}…", s.chars().take(keep).collect::<String>())
    }
}

fn normalize_summary(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn number_or_string(value: Option<&Value>) -> u64 {
    value
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_raw_session_list_uses_last_summary_and_numeric_metadata() {
        let mut app = App::new();
        app.replace_sessions_from_value(json!([
            {
                "seed": "abc12345",
                "updated_at": 1_725_000_000_u64,
                "model": "gpt-test",
                "message_count": 9,
                "turn_count": 4,
                "last_summary": "  修复终端输入，并改善会话列表布局。 ",
                "running": true
            }
        ]));

        assert_eq!(app.sessions.len(), 1);
        let session = &app.sessions[0];
        assert_eq!(session.seed, "abc12345");
        assert!(session.name.contains("修复终端输入"));
        assert_eq!(session.preview, "修复终端输入，并改善会话列表布局。");
        assert_eq!(session.updated_at, 1_725_000_000);
        assert_eq!(session.turn_count, 4);
        assert_eq!(session.message_count, 9);
        assert_eq!(session.model, "gpt-test");
        assert!(session.running);
    }

    #[test]
    fn legacy_wrapped_session_list_and_string_timestamp_still_work() {
        let mut app = App::new();
        app.selection_index = 99;
        app.replace_sessions_from_value(json!({
            "sessions": [{
                "seed": "seed0001",
                "name": "Named session",
                "preview": "Useful description",
                "updated_at": "42"
            }]
        }));

        assert_eq!(app.selection_index, 0);
        assert_eq!(app.sessions[0].name, "Named session");
        assert_eq!(app.sessions[0].preview, "Useful description");
        assert_eq!(app.sessions[0].updated_at, 42);
    }

    #[test]
    fn unicode_summary_truncation_is_char_boundary_safe() {
        let summary = "终端界面".repeat(40);
        let parsed = parse_session_info(&json!({
            "seed": "unicode1",
            "last_summary": summary
        }));

        assert!(parsed.name.ends_with('…'));
        assert!(parsed.preview.ends_with('…'));
        assert_eq!(parsed.name.chars().count(), 52);
        assert_eq!(parsed.preview.chars().count(), 120);
    }

    #[test]
    fn scroll_is_always_clamped_and_tail_following_is_explicit() {
        let mut app = App::new();
        app.update_scroll_bounds(100);
        assert_eq!((app.scroll_offset, app.scroll_max), (100, 100));

        app.scroll_up();
        assert_eq!(app.scroll_offset, 99);
        assert!(!app.follow_tail);

        app.update_scroll_bounds(20);
        assert_eq!((app.scroll_offset, app.scroll_max), (20, 20));
        assert!(!app.follow_tail);

        app.page_down();
        assert_eq!(app.scroll_offset, 20);
        assert!(app.follow_tail);

        app.update_scroll_bounds(35);
        assert_eq!(app.scroll_offset, 35);
        app.scroll_down();
        assert_eq!(app.scroll_offset, 35);
    }

    #[test]
    fn textarea_deletion_respects_cursor_and_unicode_characters() {
        let mut app = App::new();
        app.input.set_text("你a");
        app.input_backspace();
        assert_eq!(app.input.text(), "你");
        app.input_cursor_left();
        app.input_delete();
        assert_eq!(app.input.text(), "");
    }
}
