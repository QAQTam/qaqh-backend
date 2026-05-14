//! Status line rendering for narrow terminal fallback.

use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use crate::tokenizer;

use super::state::RenderContext;
use super::color;

/// Status line for vertical (narrow) layout fallback.
pub fn render_status_line(frame: &mut ratatui::Frame, area: Rect, ctx: &RenderContext) {
    let ctx_used = tokenizer::format_tokens(ctx.tokens_used());
    let ctx_max = tokenizer::format_tokens(ctx.config_context_limit);
    let bal = ctx.balance.unwrap_or("");
    let status = format!(" {} │ {} / {} │ {} │ {}", ctx.config_model, ctx_used, ctx_max, bal,
        if ctx.auto_mode { "[AUTO]" } else { "" });
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(status, Style::default().fg(color::MUTED))))
            .style(Style::default().bg(Color::Black)),
        area,
    );
}
