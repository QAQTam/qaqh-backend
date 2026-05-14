//! Tool code panel rendering: bottom-left exec/code preview, status line.

use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
};
use crate::tui::app::App;
use super::color;
use super::truncate_path;

/// Render the tool code/exec preview panel (bottom-left).
pub fn render_tool_code(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let action_label = match app.tool_code_action.as_str() {
        "read" => ("Reading", color::SUCCESS),
        "edit" => ("Editing", color::WARNING),
        "write" => ("Writing", color::ACCENT),
        "exec" => {
            match app.tool_code_status {
                Some("OK") => ("Exec ✓", color::SUCCESS),
                Some("FAIL") => ("Exec ✗", color::ERROR),
                _ => ("Exec", color::SUCCESS),
            }
        },
        _ => ("", color::MUTED),
    };

    let is_running = app.exec_pending > 0 && app.tool_code_action == "exec";
    let spinner = if is_running {
        let frames = ["|", "/", "-", "\\", "|", "/", "-", "\\", "|", "/"];
        let idx = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap_or_default()
            .as_millis() / 120) as usize % frames.len();
        frames[idx]
    } else { "" };
    let elapsed = if is_running {
        if let Some(start) = app.exec_started_at {
            let secs = start.elapsed().as_secs();
            format!(" {}s", secs)
        } else { String::new() }
    } else { String::new() };

    let title = format!(" {} {}: {} {}",
        spinner, action_label.0,
        truncate_path(&app.tool_code_path, area.width.saturating_sub(24) as usize),
        elapsed);
    let border_color = if is_running { color::WARNING } else { action_label.1 };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(title)
        .title_alignment(Alignment::Left);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let max_lines = inner.height as usize;
    if max_lines == 0 { return; }
    if app.tool_code_action == "exec" {
        let total_lines: Vec<&str> = app.tool_code_content.lines().collect();
        let total = total_lines.len();
        let skip = if app.exec_scroll_offset == 0 {
            total.saturating_sub(max_lines) // auto-follow
        } else {
            total.saturating_sub(max_lines).saturating_sub(app.exec_scroll_offset)
        };
        let visible: Vec<&str> = total_lines.into_iter().skip(skip).take(max_lines).collect();
        let mut styled: Vec<Line> = visible.iter().map(|l| {
            Line::from(Span::styled(*l, Style::default().fg(color::TEXT)))
        }).collect();
        // Blinking cursor on last line if running
        if is_running && max_lines > 0 {
            if let Some(last) = styled.last_mut() {
                let cursor = if (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH).unwrap_or_default()
                    .as_millis() / 500) % 2 == 0 { "▊" } else { " " };
                last.push_span(Span::styled(cursor, Style::default().fg(color::ACCENT)));
            } else {
                styled.push(Line::from(Span::styled("▊", Style::default().fg(color::ACCENT))));
            }
        } else if styled.is_empty() {
            styled.push(Line::from(Span::styled("(no output)", Style::default().fg(color::MUTED))));
        }
        frame.render_widget(Paragraph::new(Text::from(styled)), inner);
    } else {
        // Code view: show first N lines with line numbers
        let lines: Vec<&str> = app.tool_code_content.lines().take(max_lines).collect();
        let styled: Vec<Line> = lines.iter().enumerate().map(|(i, l)| {
            let num = format!("{:>4} ", i + 1);
            Line::from(vec![
                Span::styled(num, Style::default().fg(color::MUTED)),
                Span::styled(*l, Style::default().fg(color::TEXT)),
            ])
        }).collect();
        frame.render_widget(Paragraph::new(Text::from(styled)), inner);
    }
}
