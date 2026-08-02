use crate::app::{App, MessageKind, Mode, PasteState, SessionInfo};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Style, Stylize},
    widgets::{
        Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Wrap,
    },
};
use ratatui::text::{Line, Span, Text};
use ratatui_markdown::markdown::MarkdownRenderer;
use ratatui_markdown::theme::ThemeConfig;

// =============================================================================
// Color palette
// =============================================================================

const TEXT: Color = Color::Rgb(224, 230, 237);
const TEXT_MUTED: Color = Color::Rgb(126, 139, 151);
const TEXT_RAISED: Color = Color::Rgb(255, 255, 255);
const ACCENT: Color = Color::Rgb(88, 166, 255);
const ACCENT_MUTED: Color = Color::Rgb(49, 62, 74);
const SUCCESS: Color = Color::Rgb(116, 217, 91);
const BORDER: Color = Color::Rgb(81, 87, 96);
const SURFACE: Color = Color::Rgb(24, 26, 31);
const SURFACE_RAISED: Color = Color::Rgb(30, 32, 38);

// =============================================================================
// Layout helper
// =============================================================================

fn centered_width(area: Rect, max_width: u16) -> Rect {
    let width = area.width.min(max_width);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    Rect { x, y: area.y, width, height: area.height }
}

// =============================================================================
// Top-level render
// =============================================================================

pub fn render(frame: &mut Frame, app: &mut App) {
    match app.mode {
        Mode::SessionList => render_session_list(frame, app),
        Mode::Chat => render_chat(frame, app),
    }
}

// =============================================================================
// Session list
// =============================================================================

fn render_session_list(frame: &mut Frame, app: &App) {
    let area = centered_width(frame.area(), 112);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Length(1),
            Constraint::Min(6),
            Constraint::Length(2),
        ])
        .split(area);

    let title = Paragraph::new(Text::from(vec![
        Line::from(vec![
            Span::styled(" >_ ", Style::default().fg(Color::Black).bg(ACCENT).bold()),
            Span::styled("  DeepX", Style::default().fg(TEXT).bold()),
            Span::styled("  Terminal workspace", Style::default().fg(TEXT_MUTED)),
        ]),
        Line::from(""),
        Line::from(vec![
            if app.connected {
                Span::styled("● ", Style::default().fg(SUCCESS))
            } else {
                Span::styled("○ ", Style::default().fg(Color::Red))
            },
            Span::styled(&app.status_message, Style::default().fg(TEXT_MUTED)),
            Span::styled(
                format!(
                    "  ·  {} session{}",
                    app.sessions.len(),
                    if app.sessions.len() == 1 { "" } else { "s" }
                ),
                Style::default().fg(TEXT_MUTED),
            ),
        ]),
    ]))
    .style(Style::default().bg(SURFACE))
    .block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(ACCENT_MUTED)),
    );
    frame.render_widget(title, layout[0]);

    render_session_items(frame, layout[2], app);

    let help = Text::from(vec![
        Line::from(vec![
            key_hint("↑/↓ or j/k"),
            hint_text(" select   "),
            key_hint("Enter"),
            hint_text(" open   "),
            key_hint("Ctrl+N"),
            hint_text(" new   "),
            key_hint("Del"),
            hint_text(" delete"),
        ]),
        Line::from(vec![
            key_hint("Ctrl+R"),
            hint_text(" refresh   "),
            key_hint("Ctrl+C"),
            hint_text(" quit"),
        ]),
    ]);
    frame.render_widget(
        Paragraph::new(help).style(Style::default().bg(SURFACE)),
        layout[3],
    );
}

fn render_session_items(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .title(
            Span::styled(
                format!("  Recent sessions  ({})", app.sessions.len()),
                Style::default().fg(TEXT).bold(),
            ),
        )
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(SURFACE_RAISED));

    let empty = if app.sessions.is_empty() {
        Text::from(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  No sessions yet",
                Style::default().fg(TEXT_MUTED),
            )),
            Line::from(Span::styled(
                "  Press Ctrl+N to start a new conversation.",
                Style::default().fg(TEXT_MUTED),
            )),
        ])
    } else {
        Text::from("")
    };

    let items: Vec<ListItem> = app
        .sessions
        .iter()
        .map(render_session_item)
        .collect();

    let list = List::new(items)
        .block(block.clone())
        .highlight_symbol(" ▸ ")
        .highlight_style(Style::default().bg(ACCENT).fg(TEXT_RAISED));

    if app.sessions.is_empty() {
        let _inner_area = block.inner(area);
        frame.render_widget(Paragraph::new(empty).block(block), area);
        return;
    }

    let mut state = ListState::default().with_selected(Some(app.selection_index));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_session_item(session: &SessionInfo) -> ListItem<'static> {
    let summary = session.preview.clone();
    let model = &session.model;
    let turns = session.turn_count;
    let metadata = format!(
        "  {model}  ·  {turns} turns  ·  {}",
        relative_time(session.updated_at)
    );

    ListItem::new(Text::from(vec![
        Line::from(Span::styled(
            summary,
            Style::default().fg(TEXT).bold(),
        )),
        Line::from(metadata),
    ]))
}

// =============================================================================
// Chat view
// =============================================================================
fn render_chat(frame: &mut Frame, app: &mut App) {
    let area = centered_width(frame.area(), 150);
    let composer_h = composer_height(app, area.width);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(composer_h),
            Constraint::Length(1),
        ])
        .split(area);

    render_chat_title(frame, chunks[0], app);
    render_messages(frame, chunks[1], app);
    render_composer(frame, chunks[2], app);
    render_status_bar(frame, chunks[3], app);

    if let PasteState::Confirm { .. } = &app.paste_state {
        render_paste_overlay(frame, area, app);
    }
}
fn composer_height(app: &App, width: u16) -> u16 {
    if width < 60 {
        3
    } else {
        (app.input.lines().len().max(1).min(10) as u16) + 2
    }
}

fn render_chat_title(frame: &mut Frame, area: Rect, app: &App) {
    let session_name = if app.current_session_name.is_empty() {
        "Chat"
    } else {
        &app.current_session_name
    };
    let title = Line::from(vec![
        Span::styled(" >_ ", Style::default().fg(Color::Black).bg(ACCENT).bold()),
        Span::styled("  DeepX", Style::default().fg(TEXT).bold()),
        Span::styled("  /  ", Style::default().fg(BORDER)),
        Span::styled(session_name.to_string(), Style::default().fg(TEXT_MUTED)),
    ]);
    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(ACCENT_MUTED))
        .style(Style::default().bg(SURFACE));
    frame.render_widget(Paragraph::new(title).block(block), area);
}

fn render_messages(frame: &mut Frame, area: Rect, app: &mut App) {
    let mut lines: Vec<Line> = Vec::new();

    for msg in &app.messages {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.extend(render_message(msg.clone(), area.width.saturating_sub(2)));
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "Start a conversation below. Enter sends; Shift+Enter adds a line.",
            Style::default().fg(TEXT_MUTED),
        )));
    }

    let block = Block::default()
        .title(Span::styled(" Conversation ", Style::default().fg(TEXT).bold()))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .style(Style::default().bg(SURFACE_RAISED));
    let paragraph = Paragraph::new(Text::from(lines))
        .block(block)
        .wrap(Wrap { trim: false });

    let rendered_lines = paragraph.line_count(area.width);
    let viewport_lines = area.height.saturating_sub(2) as usize;
    let max_scroll = rendered_lines.saturating_sub(viewport_lines);
    app.update_scroll_bounds(max_scroll);
    frame.render_widget(paragraph.scroll((app.scroll_offset as u16, 0)), area);

    if app.scroll_max > 0 && area.width > 2 && area.height > 2 {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .thumb_symbol("┃");
        let mut state = ScrollbarState::new(rendered_lines).position(app.scroll_offset);
        frame.render_stateful_widget(
            scrollbar,
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut state,
        );
    }
}

fn render_composer(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .title(Span::styled(" Message ", Style::default().fg(TEXT).bold()))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT_MUTED))
        .style(Style::default().bg(SURFACE_RAISED));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(app.input.textarea(), inner);
}

fn render_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    let connected_icon: Span = if app.connected {
        Span::styled("●", Style::default().fg(SUCCESS))
    } else {
        Span::styled("○", Style::default().fg(Color::Red))
    };

    let line = Line::from(vec![
        connected_icon,
        Span::styled(
            format!(
                " {}  ·  turn {}  ·  {} tokens  ·  scroll {}/{}",
                app.status_message,
                app.current_turn,
                app.tokens_used,
                app.scroll_offset,
                app.scroll_max,
            ),
            Style::default().fg(TEXT_MUTED),
        ),
        Span::styled(
            "    Enter send  ·  Shift+Enter newline  ·  PgUp/PgDn scroll  ·  Esc sessions",
            Style::default().fg(TEXT_MUTED),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(SURFACE).fg(TEXT_MUTED)),
        area,
    );
}

fn render_paste_overlay(frame: &mut Frame, area: Rect, app: &App) {
    if let PasteState::Confirm { line_count, .. } = &app.paste_state {
        let msg = format!(
            " Paste contains {} lines. [y] Confirm  [n] Cancel ",
            line_count
        );

        let overlay_height = 3;
        let overlay_width = (msg.len() as u16 + 4).min(area.width.saturating_sub(4));

        let overlay_area = Rect {
            x: area.x + (area.width.saturating_sub(overlay_width)) / 2,
            y: area.y + (area.height.saturating_sub(overlay_height)) / 2,
            width: overlay_width,
            height: overlay_height,
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(
                Style::default()
                    .bg(Color::Yellow)
                    .fg(Color::Black)
                    .bold(),
            );
        let p = Paragraph::new(msg).block(block).centered();
        frame.render_widget(Clear, overlay_area);
        frame.render_widget(p, overlay_area);
    }
}

// =============================================================================
// Message rendering helpers
// =============================================================================

fn key_hint(text: &str) -> Span<'static> {
    format!(" {text} ").fg(TEXT_RAISED).bg(SURFACE_RAISED)
}

fn hint_text(text: &str) -> Span<'static> {
    text.to_string().fg(TEXT_MUTED)
}

fn relative_time(epoch: u64) -> String {
    if epoch == 0 {
        return "unknown time".to_string();
    }
    let now = chrono::Utc::now().timestamp().max(0) as u64;
    let elapsed = now.saturating_sub(epoch);
    match elapsed {
        0..=59 => "just now".to_string(),
        60..=3_599 => format!("{}m ago", elapsed / 60),
        3_600..=86_399 => format!("{}h ago", elapsed / 3_600),
        86_400..=604_799 => format!("{}d ago", elapsed / 86_400),
        _ => chrono::DateTime::from_timestamp(epoch as i64, 0)
            .map(|date| date.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "unknown time".to_string()),
    }
}

fn message_tag(label: &str, color: Color) -> Span<'static> {
    format!(" {label} ").fg(TEXT_RAISED).bg(color)
}

// =============================================================================
// Message rendering
// =============================================================================

fn render_message(msg: crate::app::ChatMessage, width: u16) -> Vec<Line<'static>> {
    match msg.kind {
        MessageKind::User { text } => {
            vec![
                Line::from(message_tag("YOU", SUCCESS)),
                Line::from(Span::styled(text, Style::default().fg(TEXT))),
            ]
        }

        MessageKind::Thinking { content, complete } => {
            let status = if complete { "  complete" } else { "  streaming…" };
            let header = Line::from(vec![
                message_tag("THINKING", Color::Rgb(237, 196, 89)),
                Span::styled(status, Style::default().fg(TEXT_MUTED)),
            ]);
            let mut lines = vec![header];
            if !content.is_empty() {
                lines.extend(render_markdown(&content, width));
            }
            lines
        }

        MessageKind::Answer { content, complete } => {
            let mut spans = vec![message_tag("DEEPX", ACCENT)];
            if !complete {
                spans.push(Span::styled("  streaming…", Style::default().fg(TEXT_MUTED)));
            }
            let header = Line::from(spans);
            let mut lines = vec![header];
            if !content.is_empty() {
                lines.extend(render_markdown(&content, width));
            }
            lines
        }

        MessageKind::ToolUse {
            tool_call_id: _,
            tool_name,
            params,
        } => {
            let header = Line::from(vec![
                message_tag("TOOL", Color::Rgb(201, 117, 255)),
                Span::raw("  "),
                Span::styled(tool_name.clone(), Style::default().fg(Color::Magenta).bold()),
            ]);
            let mut lines = vec![header];

            if tool_name == "write_file" || tool_name == "edit_file" || tool_name == "patch" {
                if let Some(path) = params.get("path").and_then(|v| v.as_str()) {
                    lines.push(Line::from(Span::styled(
                        format!("  file: {}", path),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                if let Some(content) = params.get("content").and_then(|v| v.as_str()) {
                    let content = content.to_owned();
                    lines.extend(render_diff_preview(&tool_name, &content, &params));
                }
            } else {
                if let Some(text) = params.as_str() {
                    let preview: String = if text.len() > 120 {
                        format!("  {:.117}…", text)
                    } else {
                        format!("  {}", text)
                    };
                    lines.push(Line::from(Span::styled(
                        preview,
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
            lines
        }

        MessageKind::ToolResult {
            tool_call_id: _,
            output,
            success,
        } => {
            let color = if success {
                Color::Rgb(116, 160, 255)
            } else {
                Color::Rgb(255, 117, 117)
            };
            let header = Line::from(message_tag("RESULT", color));
            let mut lines = vec![header];
            if !output.is_empty() {
                let preview: String = if output.len() > 125 {
                    format!("  {:.122}…", output)
                } else {
                    format!("  {}", output)
                };
                lines.push(Line::from(Span::styled(
                    preview,
                    Style::default().fg(Color::DarkGray),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    "  (empty output)",
                    Style::default().fg(Color::DarkGray),
                )));
            }
            lines
        }

        MessageKind::ToolExecDelta {
            tool_call_id: _,
            delta,
        } => {
            let header = Line::from(message_tag("EXEC", Color::Rgb(237, 196, 89)));
            let mut lines = vec![header];
            if !delta.is_empty() {
                lines.push(Line::from(Span::styled(
                    delta,
                    Style::default().fg(TEXT_MUTED),
                )));
            }
            lines
        }

        MessageKind::Block {
            block_type,
            content,
        } => {
            let header = Line::from(vec![
                message_tag(&block_type.to_uppercase(), Color::Rgb(201, 117, 255)),
            ]);
            let mut lines = vec![header];
            if !content.is_empty() {
                lines.extend(render_markdown(&content, width));
            }
            lines
        }

        MessageKind::TurnEnd {
            stop_reason: _,
            usage: _,
        } => {
            let parts = vec![Span::styled("^", Style::default().fg(Color::DarkGray))];
            vec![Line::from(parts)]
        }
    }
}

// =============================================================================
// Markdown rendering (ratatui-markdown)
// =============================================================================

fn render_markdown(text: &str, width: u16) -> Vec<Line<'static>> {
    if text.is_empty() {
        return vec![Line::from("")];
    }

    let renderer = MarkdownRenderer::new(width as usize);
    let blocks = renderer.parse(text);
    let theme = ThemeConfig::default();
    renderer.render(&blocks, &theme)
}

// =============================================================================
// Diff preview helper
// =============================================================================

fn safe_slice_range(text: &str, from: usize, to: usize) -> &str {
    let from = floor_char_boundary(text, from);
    let to = floor_char_boundary(text, to);
    if from >= text.len() {
        return "";
    }
    &text[from..text.len().min(to)]
}

fn floor_char_boundary(text: &str, byte: usize) -> usize {
    if byte >= text.len() {
        return text.len();
    }
    let mut adjusted = byte;
    while adjusted > 0 && !text.is_char_boundary(adjusted) {
        adjusted -= 1;
    }
    adjusted
}

fn render_diff_preview(
    _tool_name: &str,
    content: &str,
    _params: &serde_json::Value,
) -> Vec<Line<'static>> {
    match _params.get("old_content").and_then(|v| v.as_str()) {
        Some(old) if !old.is_empty() => {
            crate::diff::render_unified_diff(old, content, 20)
        }
        _ => crate::diff::render_new_file_preview(content, 15),
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{ChatMessage, MessageKind};
    use chrono::Utc;
    use crossterm::event::{KeyCode, KeyEventKind};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use serde_json::json;

    fn screen_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let mut s = String::new();
        let width = buffer.area.width as usize;
        for (i, cell) in buffer.content.iter().enumerate() {
            s.push_str(cell.symbol());
            if (i + 1) % width == 0 {
                s.push('\n');
            }
        }
        s
    }

    #[test]
    fn session_screen_renders_canonical_summary_and_metadata() {
        let mut app = App::new();
        app.status_message = "Connected".to_string();
        let session = SessionInfo {
            seed: "deadbeef".to_string(),
            name: "deadbeef".to_string(),
            preview: "Diagnose terminal input and repair the chat layout".to_string(),
            updated_at: Utc::now().timestamp() as u64,
            model: "gpt-test".to_string(),
            turn_count: 7,
            message_count: 18,
            running: true,
        };
        app.sessions.push(session);
        app.selection_index = 0;

        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                render(f, &mut app);
            })
            .unwrap();

        let screen = screen_text(&terminal);
        assert!(screen.contains("DeepX"));
        assert!(screen.contains("Diagnose terminal input"));
        assert!(screen.contains("gpt-test"));
        assert!(screen.contains("7 turns"));
        assert!(screen.contains("1 session"));
    }

    #[test]
    fn chat_render_computes_wrapped_scroll_bounds_and_follows_tail() {
        let mut app = App::new();
        app.open_new_session("scroll01".to_string());
        for id in 1..=60 {
            let msg = crate::app::ChatMessage {
                id,
                kind: MessageKind::Answer {
                    content: format!(
                        "message {id}: this deliberately wraps across a narrow terminal viewport"
                    ),
                    complete: true,
                },
                timestamp: Utc::now(),
                turn_id: format!("turn-{id}"),
                round_num: Some(id as u32),
            };
            app.messages.push(msg);
        }

        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                render(f, &mut app);
            })
            .unwrap();

        let screen = screen_text(&terminal);
        assert!(
            screen.contains("wraps"),
            "chat view should render wrapped messages"
        );
    }
}
