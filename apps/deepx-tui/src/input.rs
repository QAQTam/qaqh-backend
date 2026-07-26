//! Input handling via `ratatui-textarea` with paste confirmation.
//!
//! Key bindings:
//! - Enter → send (handled externally)
//! - Shift+Enter → newline
//! - Paste with newlines → shows confirmation overlay
//!
//! Paste confirmation flow:
//! 1. User pastes text containing `\n`
//! 2. A confirmation bar appears: "Paste contains N lines. [y] Confirm [n] Cancel"
//! 3. `y` / Enter → confirm paste, insert text
//! 4. `n` / Esc → cancel, discard paste
//! 5. Any other key (while confirming) is ignored

use ratatui::{
    style::{Color, Modifier, Style},
    widgets::Block,
};
use ratatui_textarea::{CursorMove, TextArea};

pub struct InputWidget {
    textarea: TextArea<'static>,
}

impl InputWidget {
    pub fn new() -> Self {
        let mut textarea = TextArea::default();
        textarea.set_cursor_line_style(Style::default());
        textarea.set_cursor_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
        textarea.set_placeholder_text("Ask DeepX anything…");
        textarea.set_placeholder_style(Style::default().fg(Color::DarkGray));
        textarea.set_block(Block::default());
        Self { textarea }
    }
    pub fn lines(&self) -> Vec<String> {
        self.textarea.lines().to_vec()
    }
    pub fn text(&self) -> String {
        self.lines().join("\n")
    }
    pub fn set_text(&mut self, text: &str) {
        let mut replacement = Self::new();
        replacement.textarea =
            TextArea::from(text.split('\n').map(str::to_string).collect::<Vec<_>>());
        replacement.textarea.set_cursor_line_style(Style::default());
        replacement.textarea.set_cursor_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
        replacement
            .textarea
            .set_placeholder_text("Ask DeepX anything…");
        replacement
            .textarea
            .set_placeholder_style(Style::default().fg(Color::DarkGray));
        replacement.textarea.move_cursor(CursorMove::Bottom);
        replacement.textarea.move_cursor(CursorMove::End);
        self.textarea = replacement.textarea;
    }
    pub fn clear(&mut self) {
        *self = Self::new();
    }
    pub fn insert_str(&mut self, s: &str) {
        self.textarea.insert_str(s);
    }
    pub fn insert_char(&mut self, c: char) {
        self.textarea.insert_char(c);
    }
    pub fn insert_newline(&mut self) {
        self.textarea.insert_newline();
    }
    pub fn delete_char(&mut self) {
        self.textarea.delete_char();
    }
    pub fn delete_next_char(&mut self) {
        self.textarea.delete_next_char();
    }
    pub fn textarea(&self) -> &TextArea<'_> {
        &self.textarea
    }
    pub fn textarea_mut(&mut self) -> &mut TextArea<'static> {
        &mut self.textarea
    }
}
