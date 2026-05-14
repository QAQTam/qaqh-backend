//! Input box rendering: user input field with cursor, mode hints, placeholder.

use ratatui::{
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
};
use crate::tui::app::App;
use crate::tui::utils::{char_boundary, display_width, textwrap};
use crate::tui::Mode;

use super::color;

pub fn render_input(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let border_style = match app.mode {
        Mode::Streaming => Style::default().fg(color::ACCENT),
        Mode::ApiConfig => Style::default().fg(color::WARNING),
        _ => Style::default().fg(color::BORDER),
    };
    let title = match app.mode {
        Mode::Streaming => format!(" {} ", app.tt().streaming_title),
        Mode::ApiConfig => format!(" {} ", app.tt().session_setup),
        _ => format!(" {} ", app.tt().prompt_title),
    };
    let input_area = Block::default()
        .borders(Borders::TOP)
        .title(title)
        .title_alignment(Alignment::Left)
        .border_style(border_style);
    let inner = input_area.inner(area);
    frame.render_widget(input_area, area);

    // Determine what to show in the input line
    if !app.input.is_empty() {
        // User has typed — show multi-line wrapped text
        let style = Style::default().fg(color::TEXT);
        let w = inner.width.saturating_sub(2) as usize;
        let chunks = textwrap(&app.input, w.max(10));
        let lines: Vec<Line> = chunks.into_iter().map(|c| {
            Line::from(Span::styled(c, style))
        }).collect();
        frame.render_widget(Paragraph::new(Text::from(lines)), inner);
    } else if app.mode == Mode::Chat && !app.tt().input_placeholder.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                app.tt().input_placeholder.to_string(),
                Style::default().fg(color::MUTED).add_modifier(Modifier::DIM),
            ))),
            inner,
        );
    } else if app.mode == Mode::SudoPrompt {
        // Password input handled by render_auth_popup (centered yellow dialog)
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                app.tt().sudo_title,
                Style::default().fg(color::MUTED).add_modifier(Modifier::DIM),
            ))),
            inner,
        );
    } else if app.mode == Mode::ApiConfig {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " Type your API key (sk-...) and press Enter",
                Style::default().fg(color::MUTED).add_modifier(Modifier::DIM),
            ))),
            inner,
        );
    } else if app.mode == Mode::Streaming {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                app.tt().streaming_text,
                Style::default().fg(color::MUTED),
            ))),
            inner,
        );
    }

    // Cursor — use display width, not byte offset (CJK chars are 2 columns)
    let show_cursor = (!app.input.is_empty() && app.mode != Mode::Streaming)
        || app.mode == Mode::ApiConfig
        || app.mode == Mode::SudoPrompt;
    if show_cursor {
        let prefix = char_boundary(&app.input, app.input_cursor);
        let pre_text = &app.input[..prefix];
        let _col = display_width(pre_text);
        let input_w = inner.width.saturating_sub(2) as usize;
        // Calculate wrapped line position
        let wrapped_lines = textwrap(pre_text, input_w.max(10));
        let row = wrapped_lines.len().saturating_sub(1);
        let col_in_wrap = if wrapped_lines.is_empty() { 0 } else {
            display_width(wrapped_lines.last().unwrap())
        };
        let cx = inner.x + col_in_wrap.min(input_w.saturating_sub(1)) as u16;
        let cy = inner.y + row as u16;
        frame.set_cursor_position((cx.min(inner.right().saturating_sub(1)), cy));
    }
}
