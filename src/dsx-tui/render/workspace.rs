//! Workspace panel + info panel rendering.
//!
//! E03: replace with full implementation from the original dsx-tui/workspace.rs
//! adapted to use `ctx: &RenderContext` instead of `&App`.

use ratatui::layout::Rect;
use crate::tui::state::RenderContext;
use super::color;

/// Render the workspace panel (project info, token stats, health, memory).
pub fn render_workspace(frame: &mut ratatui::Frame, area: Rect, ctx: &RenderContext) {
    // TODO(E03): implement workspace panel
    let _ = frame;
    let _ = area;
    let _ = ctx;
}

/// Render the bottom-right info panel (model, tokens, balance, health).
pub fn render_info_panel(frame: &mut ratatui::Frame, area: Rect, ctx: &RenderContext) {
    use ratatui::{
        style::Style,
        text::{Line, Span},
        widgets::Paragraph,
    };
    use crate::tokenizer;

    let ctx_used = tokenizer::format_tokens(ctx.tokens_used());
    let ctx_max = tokenizer::format_tokens(ctx.config_context_limit);
    let bal = ctx.balance.unwrap_or("");
    let status = format!(" {} │ {} / {} │ {}", ctx.config_model, ctx_used, ctx_max, bal);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(status, Style::default().fg(color::MUTED))))
            .style(Style::default().bg(color::BG)),
        area,
    );
}
