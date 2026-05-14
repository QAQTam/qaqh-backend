//! Status line rendering for narrow terminal fallback.

use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use crate::tokenizer;

use crate::tui::app::App;
use super::color;

/// Status line for vertical (narrow) layout fallback.
pub fn render_status_line(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let ctx_used = tokenizer::format_tokens(app.tokens_used());
    let ctx_max = tokenizer::format_tokens(app.config.context_limit);
    let bal = app.balance.as_deref().unwrap_or("");
    let status = format!(" {} │ {} / {} │ {} │ {}", app.config.model, ctx_used, ctx_max, bal,
        if app.auto_mode { "[AUTO]" } else { "" });
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(status, Style::default().fg(color::MUTED))))
            .style(Style::default().bg(Color::Black)),
        area,
    );
}
