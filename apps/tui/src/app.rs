//! UI state machine and event loop.
//!
//! Keyboard events are drained on a 50 ms render tick; async client events
//! arrive on the bridge channel. Both are folded into `App` and drawn by
//! `ui::render`.

use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use tokio::time::MissedTickBehavior;

use crate::backend::{LOCKED_TOOL_MODE, QaqhBridge, SessionItem, UiEvent};
use crate::transcript::Transcript;
use crate::ui;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartMode {
    /// Create a brand-new session (locked minimal:dsh).
    Start,
    /// Pick an existing session from the list (then re-pin minimal:dsh).
    Resume,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Select,
    Chat,
}

pub struct App {
    pub bridge: QaqhBridge,
    pub screen: Screen,
    pub items: Vec<SessionItem>,
    pub list_sel: usize,
    pub seed: Option<String>,
    pub transcript: Transcript,
    pub input: String,
    pub input_cursor: usize,
    pub running: bool,
    pub timeline_status: String,
    pub notice: String,
    /// Rows scrolled up from the bottom of the transcript (0 = pinned).
    pub scroll: usize,
    pub quit: bool,
}

impl App {
    pub async fn run(mode: StartMode) -> Result<(), String> {
        let bridge = QaqhBridge::connect()
            .await
            .map_err(|error| format!("daemon connect failed: {error}"))?;
        ratatui::init();
        let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))
            .map_err(|error| format!("terminal init failed: {error}"))?;

        let mut app = Self {
            bridge,
            screen: Screen::Select,
            items: Vec::new(),
            list_sel: 0,
            seed: None,
            transcript: Transcript::new(),
            input: String::new(),
            input_cursor: 0,
            running: false,
            timeline_status: "connecting".to_string(),
            notice: String::new(),
            scroll: 0,
            quit: false,
        };

        match mode {
            StartMode::Start => {
                if let Err(error) = app.begin_new_session().await {
                    app.notice = format!("create failed ({error}) — showing session list");
                    if let Err(error) = app.load_sessions().await {
                        app.notice = format!("session.list failed: {error}");
                    }
                }
            }
            StartMode::Resume => {
                if let Err(error) = app.load_sessions().await {
                    app.notice = format!("session.list failed: {error}");
                }
            }
        }

        let mut tick = tokio::time::interval(Duration::from_millis(50));
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        while !app.quit {
            tokio::select! {
                event = app.bridge.rx.recv() => {
                    match event {
                        Some(event) => app.on_ui_event(event),
                        None => app.quit = true,
                    }
                }
                _ = tick.tick() => {
                    app.drain_keys().await;
                    if let Err(error) = ui::render(&mut terminal, &mut app) {
                        app.notice = format!("render failed: {error}");
                        app.quit = true;
                    }
                }
            }
        }

        app.bridge.shutdown();
        ratatui::restore();
        Ok(())
    }

    // ── Startup flows ──────────────────────────────────────────────────

    async fn begin_new_session(&mut self) -> Result<(), String> {
        let cwd = std::env::current_dir()
            .ok()
            .map(|path| path.to_string_lossy().into_owned());
        self.notice = "creating session (minimal:dsh)…".to_string();
        self.bridge
            .create_session(cwd)
            .await
            .map_err(|error| format!("session.create: {error}"))?;

        // The new seed is broadcast on the control channel as
        // SessionStateChanged(Created); wait for it (bounded).
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        let seed = loop {
            if tokio::time::Instant::now() >= deadline {
                return Err("timed out waiting for created session seed".into());
            }
            let remaining = deadline - tokio::time::Instant::now();
            match tokio::time::timeout(remaining, self.bridge.rx.recv()).await {
                Ok(Some(UiEvent::ControlBatch(batch))) => {
                    if let Some(seed) = QaqhBridge::created_seed(&batch) {
                        break seed;
                    }
                }
                Ok(Some(UiEvent::TimelineStatus(status))) => {
                    self.timeline_status = status_text(&status);
                }
                Ok(Some(_)) => {}
                Ok(None) => return Err("bridge channel closed while creating session".into()),
                Err(_) => {}
            }
        };

        self.notice = format!("attaching {seed}…");
        self.bridge
            .attach(&seed)
            .await
            .map_err(|error| format!("attach: {error}"))?;
        self.enter_session(seed).await
    }

    async fn load_sessions(&mut self) -> Result<(), String> {
        self.notice = "loading sessions…".to_string();
        let mut items = self
            .bridge
            .list_sessions()
            .await
            .map_err(|error| format!("session.list: {error}"))?;
        items.retain(|item| !item.archived);
        self.list_sel = 0;
        self.items = items;
        self.screen = Screen::Select;
        if self.items.is_empty() {
            self.notice =
                "no sessions yet — press Enter to create one (or restart with --start)".to_string();
        } else {
            self.notice.clear();
        }
        Ok(())
    }

    async fn select_session(&mut self) -> Result<(), String> {
        let Some(item) = self.items.get(self.list_sel) else {
            return Ok(());
        };
        let seed = item.seed.clone();
        self.notice = format!("attaching {seed}…");
        self.bridge
            .attach(&seed)
            .await
            .map_err(|error| format!("attach: {error}"))?;
        self.notice = format!("locking {seed} to {LOCKED_TOOL_MODE}…");
        if let Err(error) = self.bridge.lock_minimal(&seed).await {
            self.notice = format!("lock minimal failed: {error}");
            return Err(format!("set_tool_mode: {error}"));
        }
        self.enter_session(seed).await
    }

    async fn enter_session(&mut self, seed: String) -> Result<(), String> {
        let page = self
            .bridge
            .activate_timeline(&seed)
            .await
            .map_err(|error| format!("activate timeline: {error}"))?;
        self.transcript.apply_snapshot(&page);
        self.seed = Some(seed);
        self.screen = Screen::Chat;
        self.input.clear();
        self.input_cursor = 0;
        self.scroll = 0;
        self.notice.clear();
        Ok(())
    }

    // ── Event / keyboard handling ─────────────────────────────────────

    fn on_ui_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::TimelineEntry(seed, entry) => {
                if self.seed.as_deref() != Some(seed.as_str()) {
                    return;
                }
                self.transcript.apply_entry(&entry);
                use qaqh_domain::TimelineEvent;
                match entry.event {
                    TimelineEvent::TurnOpened { .. } => self.running = true,
                    TimelineEvent::TurnSealed { .. } => {
                        self.running = false;
                        self.notice.clear();
                    }
                    _ => {}
                }
            }
            UiEvent::TimelineStatus(status) => {
                self.timeline_status = status_text(&status);
            }
            UiEvent::TimelineSnapshot(page) => {
                if self.seed.as_deref() == Some(page.seed.as_str()) {
                    self.transcript.apply_snapshot(&page);
                }
            }
            UiEvent::ControlBatch(_batch) => {
                // Session lifecycle events: the selector screen is rebuilt on
                // demand; creation-time seed resolution happens in
                // `begin_new_session` before this dispatcher runs.
            }
        }
    }

    async fn drain_keys(&mut self) {
        while event::poll(Duration::ZERO).unwrap_or(false) {
            let Ok(event) = event::read() else {
                break;
            };
            if let Event::Key(key) = event {
                self.on_key(key).await;
                if self.quit {
                    break;
                }
            }
        }
    }

    async fn on_key(&mut self, key: KeyEvent) {
        match self.screen {
            Screen::Select => self.on_key_select(key).await,
            Screen::Chat => self.on_key_chat(key).await,
        }
    }

    async fn on_key_select(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.list_sel = self.list_sel.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.list_sel = self
                    .list_sel
                    .saturating_add(1)
                    .min(self.items.len().saturating_sub(1));
            }
            KeyCode::Enter => {
                if self.items.is_empty() {
                    self.notice = "no sessions — use qaqh-tui --start to create one".to_string();
                    return;
                }
                if let Err(error) = self.select_session().await {
                    self.notice = error;
                }
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Esc => self.quit = true,
            _ => {}
        }
    }

    async fn on_key_chat(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    if self.running {
                        if let Some(seed) = self.seed.clone() {
                            if let Err(error) = self.bridge.cancel_turn(&seed).await {
                                self.notice = format!("cancel failed: {error}");
                            } else {
                                self.notice = "cancelling…".to_string();
                            }
                        }
                    } else {
                        self.quit = true;
                    }
                    return;
                }
                KeyCode::Char('d') => {
                    self.quit = true;
                    return;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    return;
                }
                if self.running {
                    self.notice =
                        "turn running — Ctrl+C cancels before sending another message".to_string();
                    return;
                }
                let Some(seed) = self.seed.clone() else {
                    self.notice = "no session attached".to_string();
                    return;
                };
                match self.bridge.send_message(&seed, &text).await {
                    Ok(ack) => {
                        if ack.status == qaqh_client::types::RingingCommandAckStatus::Accepted {
                            self.input.clear();
                            self.input_cursor = 0;
                            self.scroll = 0;
                            self.notice.clear();
                        } else {
                            self.notice = format!(
                                "command rejected: {}",
                                ack.message.unwrap_or_else(|| "no reason".into())
                            );
                        }
                    }
                    Err(error) => self.notice = format!("send failed: {error}"),
                }
            }
            KeyCode::Esc => {
                if !self.input.is_empty() {
                    self.input.clear();
                    self.input_cursor = 0;
                } else {
                    self.quit = true;
                }
            }
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete_forward(),
            KeyCode::Left => self.input_cursor = self.input_cursor.saturating_sub(1),
            KeyCode::Right => {
                self.input_cursor = self.input_cursor.saturating_add(1).min(self.char_len());
            }
            KeyCode::Home => self.input_cursor = 0,
            KeyCode::End => {
                self.input_cursor = self.char_len();
                self.scroll = 0;
            }
            KeyCode::Up => self.scroll = self.scroll.saturating_add(1),
            KeyCode::Down => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Char(ch) => self.insert_char(ch),
            _ => {}
        }
    }

    fn char_len(&self) -> usize {
        self.input.chars().count()
    }

    fn insert_char(&mut self, ch: char) {
        let mut chars: Vec<char> = self.input.chars().collect();
        let at = self.input_cursor.min(chars.len());
        chars.insert(at, ch);
        self.input = chars.into_iter().collect();
        self.input_cursor = at + 1;
    }

    fn backspace(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let mut chars: Vec<char> = self.input.chars().collect();
        chars.remove(self.input_cursor - 1);
        self.input = chars.into_iter().collect();
        self.input_cursor -= 1;
    }

    fn delete_forward(&mut self) {
        let mut chars: Vec<char> = self.input.chars().collect();
        if self.input_cursor < chars.len() {
            chars.remove(self.input_cursor);
            self.input = chars.into_iter().collect();
        }
    }
}

fn status_text(status: &qaqh_client::types::TimelineStatus) -> String {
    match status {
        qaqh_client::types::TimelineStatus::Connecting { seed } => format!("connecting {seed}"),
        qaqh_client::types::TimelineStatus::Open { cursor, .. } => {
            format!("live (cursor {cursor})")
        }
        qaqh_client::types::TimelineStatus::Reconnecting {
            retry_ms, cursor, ..
        } => format!("reconnecting (retry {retry_ms}ms, cursor {cursor})"),
        qaqh_client::types::TimelineStatus::Closed { reason, .. } => {
            format!("closed: {reason}")
        }
    }
}
