//! ratatui rendering: session selector and chat transcript.
//!
//! Long transcript lines are re-wrapped on every draw against the current
//! body width (textwrap with unicode width); scroll is a simple "rows from
//! the bottom" offset maintained by `App`.

use std::io::Stdout;

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TLine, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::app::App;
use crate::backend::LOCKED_TOOL_MODE;
use crate::transcript::Role;

pub fn render(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
) -> std::io::Result<()> {
    terminal
        .draw(|frame| match app.screen {
            crate::app::Screen::Select => draw_select(frame, app),
            crate::app::Screen::Chat => draw_chat(frame, app),
        })
        .map(|_| ())
}

fn draw_select(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .areas::<3>(area);

    let header = Paragraph::new(format!(
        " sessions ({} ) — ↑/↓ or j/k · Enter open · Esc / Ctrl+C quit",
        app.items.len()
    ))
    .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    frame.render_widget(header, chunks[0]);

    let items: Vec<ListItem> = app
        .items
        .iter()
        .map(|item| {
            let seed = short(&item.seed, 10);
            let mode = if item.tool_mode.is_empty() {
                "standard".to_string()
            } else {
                item.tool_mode.clone()
            };
            let marker = if item.running { "●" } else { " " };
            let summary = item.summary.replace('\n', " ");
            let text = format!(
                "{marker} {seed:<10} {mode:<12} {turn:>3}t  {summary}",
                turn = item.turn_count
            );
            ListItem::new(TLine::from(Span::styled(text, list_item_style(item.tool_mode.as_str()))))
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(app.list_sel));
    let list = List::new(items)
        .block(
            Block::bordered()
                .title(" sessions ")
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, chunks[1], &mut state);

    let footer = Paragraph::new(app.notice.as_str()).style(Style::default().fg(Color::Yellow));
    frame.render_widget(footer, chunks[2]);
}

fn draw_chat(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas::<4>(area);

    let seed = app.seed.as_deref().unwrap_or("<none>");
    let mode_state = if app.running { "running" } else { "idle" };
    let header = Paragraph::new(format!(
        " qaqh-tui · {} · {} 🔒 · {} · {} ",
        short(seed, 14),
        LOCKED_TOOL_MODE,
        app.timeline_status,
        mode_state,
    ))
    .style(Style::default().fg(Color::Black).bg(Color::Cyan));
    frame.render_widget(header, chunks[0]);

    // ── Transcript body ────────────────────────────────────────────────
    let body = chunks[1];
    let inner_width = body.width.saturating_sub(2).max(1) as usize;
    let view_height = body.height.saturating_sub(2).max(1) as usize;
    let wrapped = wrap_lines(&app.transcript.lines, inner_width);
    let total = wrapped.len();
    app.scroll = app.scroll.min(total.saturating_sub(view_height));
    let start = total.saturating_sub(view_height + app.scroll);
    let visible: Vec<TLine> = wrapped
        .into_iter()
        .skip(start)
        .take(view_height)
        .collect();

    let transcript = Paragraph::new(visible)
        .block(
            Block::bordered()
                .title(format!(" {} ", seed))
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(transcript, body);

    // ── Input box ─────────────────────────────────────────────────────
    let input = Paragraph::new(TLine::from(input_spans(&app.input, app.input_cursor)))
        .block(
            Block::bordered()
                .title(" message ")
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(input, chunks[2]);

    // ── Footer: help + notice ─────────────────────────────────────────
    let help = " Enter send · Esc clear/quit · Ctrl+C cancel/quit · ↑↓ scroll · End=bottom ";
    let notice = if app.notice.is_empty() {
        format!(
            "{help} │ {} rows",
            app.transcript.lines.len()
        )
    } else {
        format!("{help} │ {}", app.notice)
    };
    let footer = Paragraph::new(notice).style(Style::default().fg(Color::Yellow));
    frame.render_widget(footer, chunks[3]);
}

fn role_style(role: Role) -> Style {
    match role {
        Role::User => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        Role::Assistant => Style::default().fg(Color::White),
        Role::Tool => Style::default().fg(Color::DarkGray),
        Role::Notice => Style::default().fg(Color::Yellow),
    }
}

fn list_item_style(tool_mode: &str) -> Style {
    if tool_mode == LOCKED_TOOL_MODE {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::White)
    }
}

fn wrap_lines(lines: &[crate::transcript::Line], width: usize) -> Vec<TLine<'static>> {
    let width = width.max(1);
    let mut out: Vec<TLine<'static>> = Vec::new();
    for line in lines {
        let style = role_style(line.role);
        for segment in line.text.split('\n') {
            for row in textwrap::wrap(segment, width) {
                out.push(TLine::from(Span::styled(row.to_string(), style)));
            }
        }
    }
    if out.is_empty() {
        out.push(TLine::from(Span::styled(
            "(empty session — type a message below)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    out
}

fn input_spans(text: &str, cursor: usize) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = vec![Span::styled(
        "❯ ",
        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
    )];
    let chars: Vec<char> = text.chars().collect();
    let at = cursor.min(chars.len());
    let before: String = chars[..at].iter().collect();
    let after: String = chars[at..].iter().collect();
    if !before.is_empty() {
        spans.push(Span::styled(before, Style::default().fg(Color::White)));
    }
    spans.push(Span::styled(
        "▍",
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));
    if !after.is_empty() {
        spans.push(Span::styled(after, Style::default().fg(Color::White)));
    }
    spans
}

fn short(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

// Referenced so the keycodes stay in one place for future hint lines.
#[allow(dead_code)]
const _HINTS: &[KeyCode] = &[KeyCode::Up, KeyCode::Down, KeyCode::Enter, KeyCode::Esc];
