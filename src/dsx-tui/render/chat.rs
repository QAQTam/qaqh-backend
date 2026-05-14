//! Chat panel rendering: conversation display, thinking blocks, tool calls, message content.

use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};
use crate::types::SafetyLevel;

use crate::tui::app::App;
use crate::tui::utils::{char_boundary, display_width, format_bytes, pad_to_width, textwrap, truncate};
use crate::tui::Mode;

use super::color;

use super::DS_BANNER;

pub fn render_chat(frame: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let mut lines: Vec<Line> = Vec::new();
    let w = area.width.saturating_sub(4) as usize;

    // Welcome
    if app.messages.iter().filter(|m| m.role != "system").count() == 0 && app.mode != Mode::Streaming {
        for bl in DS_BANNER.lines() {
            if !bl.is_empty() { lines.push(Line::from(Span::styled(bl.to_string(),
                Style::default().fg(color::ACCENT).add_modifier(Modifier::BOLD)))); }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(app.tt().welcome_help.to_string(), Style::default().fg(color::MUTED))));
        lines.push(Line::from(""));
    }

    // Session picker
    if app.mode == Mode::SessionPicker {
        lines.clear();
        lines.push(Line::from(Span::styled(app.tt().resume_title.to_string(), Style::default().fg(color::ACCENT).add_modifier(Modifier::BOLD))));
        lines.push(Line::from(""));
        if app.session_list.is_empty() {
            lines.push(Line::from("  No saved sessions found."));
            lines.push(Line::from("  Start a session with: dsx"));
            lines.push(Line::from(""));
        } else {
            lines.push(Line::from(Span::styled(app.tt().resume_headers.to_string(), Style::default().fg(color::MUTED))));
            lines.push(Line::from(""));
            for (i, m) in app.session_list.iter().enumerate() {
                let is_selected = i == app.session_pick_idx;
                let style = if is_selected {
                    Style::default().fg(color::BG).bg(color::ACCENT).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(color::TEXT)
                };
                lines.push(Line::from(Span::styled(
                    format!("  {}  {:22}  {:>3}     {}",
                        m.seed,
                        truncate(&m.model, 22),
                        m.message_count,
                        truncate(&m.last_summary, 50),
                    ),
                    style,
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(app.tt().resume_help.to_string(), Style::default().fg(color::MUTED))));
            lines.push(Line::from(""));
        }
        let text = Text::from(lines);
        frame.render_widget(
            Paragraph::new(text)
                .block(Block::default().borders(Borders::NONE))
                .style(Style::default().bg(color::BG)),
            area,
        );
        return;
    }

    // Setup wizard — centered logo + key prompt
    if app.mode == Mode::ApiConfig {
        lines.clear();
        let banner = DS_BANNER.to_string();
        let container_w = area.width.saturating_sub(8) as usize;
        // Center the banner
        for bl in banner.lines() {
            if bl.is_empty() {
                lines.push(Line::from(""));
            } else {
                let pad = (container_w.saturating_sub(bl.len())) / 2;
                lines.push(Line::from(Span::styled(format!("{:pad$}{}", "", bl, pad = pad), Style::default().fg(color::ACCENT).add_modifier(Modifier::BOLD))));
            }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(format!("{:^container_w$}", "Enter your DeepSeek API key to begin"), Style::default().fg(color::MUTED))));
        lines.push(Line::from(Span::styled(format!("{:^container_w$}", "Get one at: https://platform.deepseek.com/api_keys"), Style::default().fg(color::MUTED).add_modifier(Modifier::DIM))));
        lines.push(Line::from(""));
        if app.input.len() >= 10 && app.input.starts_with("sk-") {
            lines.push(Line::from(Span::styled(format!("{:^container_w$}", "[Enter] start  [Esc] quit"), Style::default().fg(color::SUCCESS))));
        } else if !app.input.is_empty() {
            lines.push(Line::from(Span::styled(format!("{:^container_w$}", "Key must start with 'sk-'"), Style::default().fg(color::WARNING))));
        } else {
            lines.push(Line::from(Span::styled(format!("{:^container_w$}", "[Esc] quit"), Style::default().fg(color::MUTED))));
        }
    }

    // Pending tool confirmations
    if !app.pending_tools.is_empty() {
        lines.push(Line::from(""));
        for (tc, safety, prompt) in &app.pending_tools {
            let (style, icon) = match safety {
                SafetyLevel::Safe => (Style::default().fg(color::SUCCESS), "~"),
                SafetyLevel::Confirm => (Style::default().fg(color::WARNING), "?"),
                SafetyLevel::Danger => (Style::default().fg(color::ERROR), "!!"),
            };
            lines.push(Line::from(Span::styled(format!(" {} {}: {}", icon, tc.function.name, prompt), style)));
        }
        lines.push(Line::from(""));
    }

    // ── Messages ──
    let mut prev_role = "";
    for msg in &app.messages {
        if msg.role == "system" { continue; }
        let role = msg.role.as_str();
        // Add separator between user <-> assistant transitions
        if (prev_role == "user" && role == "assistant") || (prev_role == "assistant" && role == "user") {
            lines.push(Line::from(Span::styled("· · ·", Style::default().fg(color::MUTED).add_modifier(Modifier::DIM))));
        }
        prev_role = role;

        match role {
            "user" => {
                let user_content = msg.content.as_deref().unwrap_or("");
                // Wrap long lines, then right-align each
                for line in user_content.lines() {
                    let wrapped = textwrap(line, w.saturating_sub(4) as usize);
                    for ul in &wrapped {
                        lines.push(Line::from(vec![
                            Span::styled(ul.to_string(), Style::default().fg(color::TEXT)),
                            Span::styled(" ▌", Style::default().fg(color::ACCENT).add_modifier(Modifier::BOLD)),
                        ]).alignment(Alignment::Right));
                    }
                }
            }
            "assistant" => {
                // Thinking chain — full content in a bordered block
                if let Some(ref r) = msg.reasoning_content {
                    if !r.is_empty() {
                        lines.push(Line::from(""));
                        lines.extend(render_thinking_block(r, w, app.tt().thinking_block));
                    }
                }
                // Content with code blocks
                if let Some(ref c) = msg.content {
                    if !c.is_empty() {
                        lines.extend(render_message_content(c, w));
                    }
                }
                // Tool calls — bordered blocks with args
                if let Some(ref tc) = msg.tool_calls {
                    for t in tc {
                        lines.push(Line::from(""));
                        lines.extend(render_tool_call_block(&t.function.name, &t.function.arguments, w));
                    }
                }
                lines.push(Line::from(""));
            }
            "tool" => {
                if let Some(ref c) = msg.content {
                    lines.push(Line::from(""));
                    lines.extend(render_tool_result(c, w));
                }
            }
            _ => {}
        }
    }

    // ── Live streaming ──
    if app.mode == Mode::Streaming {
        if !app.stream_reasoning.is_empty() {
            lines.push(Line::from(""));
            let rlines: Vec<&str> = app.stream_reasoning.lines().collect();
            let start = rlines.len().saturating_sub(12);
            lines.extend(render_thinking_block(&rlines[start..].join("\n"), w, app.tt().thinking_block));
        }
        if !app.stream_content.is_empty() {
            let cd = if app.stream_content.len() > 2000 {
                let cut = char_boundary(&app.stream_content, app.stream_content.len().saturating_sub(2000));
                &app.stream_content[cut..]
            } else { &app.stream_content };
            lines.extend(render_message_content(cd, w));
        }
        // Tool progress — show as open-ended tool block
        for (name, args) in &app.stream_tool_progress {
            lines.push(Line::from(""));
            lines.extend(render_tool_call_block(name, args, w));
        }
        // Content being generated (after reasoning, before emission — hybrid streaming)
        if app.stream_content.is_empty() && !app.stream_reasoning.is_empty() && app.stream_tool_progress.is_empty() {
            let e = app.last_activity.elapsed().as_secs();
            let spinner = match e % 4 { 0 => "/", 1 => "-", 2 => "\\", 3 => "|", _ => "/" };
            lines.push(Line::from(Span::styled(
                format!("  {} {}", spinner, app.tt().generating),
                Style::default().fg(color::ACCENT),
            )));
        }
        // Spinner
        if app.stream_content.is_empty() && app.stream_reasoning.is_empty() && app.stream_tool_progress.is_empty() {
            let e = app.last_activity.elapsed().as_secs();
            let spinner = match e % 4 { 0 => "o", 1 => "O", 2 => "o", 3 => ".", _ => "o" };
            lines.push(Line::from(Span::styled(format!("  {} {}", spinner, app.tt().waiting), Style::default().fg(color::MUTED))));
        }
        lines.push(Line::from(Span::styled(format!(" {}", app.tt().esc_cancel), Style::default().fg(color::MUTED))));
    }


    // ── Async exec running indicator ──
    if app.exec_pending > 0 {
        let e = app.exec_started_at.map(|s| s.elapsed().as_secs()).unwrap_or(0);
        let spinner = match e % 4 { 0 => "|", 1 => "/", 2 => "-", 3 => "\\", _ => "|" };
        let cmd_hint = app.tool_code_path.lines().next().unwrap_or("...");
        let cmd_short = truncate(cmd_hint.trim_start_matches("$ "), 50);
        lines.push(Line::from(Span::styled(
            format!("  {} Running: {} ({}s)  [PageUp/Dn→scroll output]",
                spinner, cmd_short, e),
            Style::default().fg(color::ACCENT),
        )));
        lines.push(Line::from(""));
    }
    let total = lines.len();
    let visible = area.height.saturating_sub(1) as usize;
    let max_scroll = total.saturating_sub(visible);
    app.last_max_scroll = max_scroll;

    // Clamp scroll
    let scroll = if app.auto_scroll {
        max_scroll
    } else {
        app.scroll_offset.min(max_scroll)
    };

    // Scroll position indicator
    if !app.auto_scroll && total > visible {
        let pct = if max_scroll == 0 { 100 } else { (scroll * 100 / max_scroll).min(100) };
        lines.push(Line::from(Span::styled(
            format!("  {} {}% {}", app.tt().scroll_indicator, pct, app.tt().scroll_indicator),
            Style::default().fg(color::MUTED).add_modifier(Modifier::DIM),
        )));
    }

    let text = Text::from(lines);
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::default().borders(Borders::NONE))
            .scroll((scroll as u16, 0))
            .style(Style::default().bg(color::BG)),
        area,
    );
}

pub fn tool_icon(name: &str) -> &'static str {
    match name {
        "read_file" => "→", "write_file" => "←", "edit_file" => "←",
        "exec" => "$", "search" => "⌕", "list_dir" => "…",
        "git_status" => "⑂", "git_diff" => "±", "git_log" => "⑆", "git_commit" => "✓",
        "web_fetch" => "%", "web_search" => "◈",
        _ => "·",
    }
}

// ── Block helpers ──

/// Render thinking/reasoning as a bordered block
pub fn render_thinking_block(reasoning: &str, max_width: usize, title: &str) -> Vec<Line<'static>> {
    let w = max_width.min(100).max(30);
    let label = format!(" {} ", title);
    let bar_len = w.saturating_sub(label.len() + 2);
    let bar = "─".repeat(bar_len);
    let mut lines = vec![
        Line::from(Span::styled(
            format!("  ┌─{}{}", label, bar),
            Style::default().fg(color::MUTED).add_modifier(Modifier::DIM),
        )),
    ];
    let rlines: Vec<&str> = reasoning.lines().collect();
    let show = if rlines.len() > 20 { &rlines[rlines.len()-20..] } else { &rlines[..] };
    for rl in show {
        for chunk in textwrap(rl, w.saturating_sub(4)) {
            lines.push(Line::from(Span::styled(
                format!("  │ {}", chunk),
                Style::default().fg(color::MUTED).add_modifier(Modifier::ITALIC),
            )));
        }
    }
    lines.push(Line::from(Span::styled(
        format!("  └{}", "─".repeat(w.saturating_sub(2))),
        Style::default().fg(color::MUTED).add_modifier(Modifier::DIM),
    )));
    lines
}

/// Render a tool call as a bordered block with truncated args
pub fn render_tool_call_block(name: &str, args: &str, max_width: usize) -> Vec<Line<'static>> {
    let w = max_width.min(90).max(30);
    let icon = tool_icon(name);
    let title = format!(" {} {} ", icon, name);
    let bar_len = w.saturating_sub(title.len() + 1);
    let bar = "─".repeat(bar_len);
    let mut lines = vec![
        Line::from(Span::styled(
            format!("  ┌{} {}", title, bar),
            Style::default().fg(color::ACCENT),
        )),
    ];
    // Parse and pretty-print JSON args
    let pretty = if let Ok(v) = serde_json::from_str::<serde_json::Value>(args) {
        let s = serde_json::to_string_pretty(&v).unwrap_or_else(|_| args.to_string());
        s
    } else {
        args.to_string()
    };
    for aline in pretty.lines().take(12) {
        for chunk in textwrap(aline, w.saturating_sub(4)) {
            lines.push(Line::from(Span::styled(
                format!("  │ {}", chunk),
                Style::default().fg(color::MUTED),
            )));
        }
    }
    lines.push(Line::from(Span::styled(
        format!("  └{}", "─".repeat(w.saturating_sub(2))),
        Style::default().fg(color::ACCENT).add_modifier(Modifier::DIM),
    )));
    lines
}

/// Render a tool result — show up to 15 lines, dimmed
pub fn render_tool_result(content: &str, max_width: usize) -> Vec<Line<'static>> {
    let w = max_width.min(100).max(30);
    // Extract actual tool result (after "── name ──" wrapper line)
    let actual = content.find('\n').map(|i| &content[i+1..]).unwrap_or(content);
    let (icon, c) = if actual.starts_with("[ERROR]") {
        ("✗", color::ERROR)
    } else if actual.starts_with("[BLOCKED]") {
        ("🚫", color::WARNING)
    } else if actual.starts_with("[PARTIAL]") {
        ("◈", color::WARNING)
    } else if actual.starts_with("[OK]") || actual.starts_with("[PROJECT_MAP]") {
        ("✓", color::SUCCESS)
    } else if actual.starts_with("[FAIL]") {
        ("✗", color::WARNING)
    } else if actual.starts_with("[SUDO_REQUIRED]") {
        ("⚡", color::WARNING)
    } else {
        ("·", color::MUTED)
    };
    let bytes = content.len();
    let summary = if bytes > 1024 { format!("{}B", format_bytes(bytes)) } else { String::new() };
    let title = format!(" {} {} {}", icon, summary, "─".repeat(w.saturating_sub(5 + summary.len())));
    let mut lines = vec![
        Line::from(Span::styled(format!("  ┌{}", title), Style::default().fg(c).add_modifier(Modifier::DIM))),
    ];
    for rl in content.lines().take(15) {
        // After "  -  " or "  +  ", check if next char is a digit → diff line (has its own marker)
        let is_diff_line = |s: &str| {
            s.strip_prefix("  -  ").or_else(|| s.strip_prefix("  +  "))
                .map(|rest| rest.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false))
                .unwrap_or(false)
        };
        let (line_color, prefix) = if rl.starts_with("  -  ") && !is_diff_line(rl) {
            (color::ERROR, "  │-")
        } else if rl.starts_with("  +  ") && !is_diff_line(rl) {
            (color::SUCCESS, "  │+")
        } else {
            (if icon == "✗" { color::ERROR } else { color::MUTED }, "  │ ")
        };
        for chunk in textwrap(rl, w.saturating_sub(4)) {
            let mut spans = vec![Span::styled(prefix.to_string(), Style::default().fg(line_color))];
            let mut hl = highlight_tool_line(&chunk, Style::default().fg(line_color));
            spans.append(&mut hl.spans);
            lines.push(Line::from(spans));
        }
    }
    lines.push(Line::from(Span::styled(
        format!("  └{}", "─".repeat(w.saturating_sub(2))),
        Style::default().fg(c).add_modifier(Modifier::DIM),
    )));
    lines
}

/// Highlight a line of tool output text into colored spans.
/// Matches status tags, file paths, error keywords, and metadata.
pub fn highlight_tool_line(line: &str, default_style: Style) -> Line<'static> {
    use std::sync::OnceLock;
    static HL_RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = HL_RE.get_or_init(|| {
        regex::Regex::new(
            r"\[OK\]|\[BLOCKED\]|\[ERROR\]|\[PARTIAL\]|\[FAIL\]|\[PROJECT_MAP\]|\[SUDO_REQUIRED\]|[\w./-]+\.\w+(?:\s*\(.*?\))?"
        ).unwrap()
    });

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut last = 0;
    for m in re.find_iter(line) {
        // Plain text before this match
        if m.start() > last {
            let t = line[last..m.start()].to_string();
            spans.push(Span::styled(t, default_style));
        }
        let matched = m.as_str();
        let style = if matched.starts_with('[') {
            match matched {
                "[OK]" => Style::default().fg(color::SUCCESS),
                "[ERROR]" | "[FAIL]" => Style::default().fg(color::ERROR),
                "[PARTIAL]" => Style::default().fg(color::WARNING),
                "[SUDO_REQUIRED]" => Style::default().fg(Color::Yellow),
                "[PROJECT_MAP]" => Style::default().fg(Color::Cyan),
                _ => default_style,
            }
        } else if matched.contains("Error") || matched.contains("error[") {
            Style::default().fg(color::ERROR)
        } else if matched.contains('.') && matched.len() > 5 {
            // Looks like a file path
            Style::default().fg(Color::Cyan)
        } else {
            default_style
        };
        spans.push(Span::styled(matched.to_string(), style));
        last = m.end();
    }
    if last < line.len() {
        let t = line[last..].to_string();
        spans.push(Span::styled(t, default_style));
    }
    Line::from(spans)
}

// ── Code block / text rendering ──

pub fn render_message_content(content: &str, max_width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut in_code = false;
    let mut line_no: usize = 0;
    let mut code_lang: &str = "";
    let mut code_buf: String = String::new();
    let mut table_buf: Vec<&str> = Vec::new();

    // First pass: split content into segments at ``` fences
    for line in content.lines() {
        if line.starts_with("```") {
            if in_code {
                // End code block — render with syntax highlighting
                in_code = false;
                let lang = code_lang;
                let ld: String = if lang.is_empty() { " code ".into() } else { format!(" {} ", lang) };
                let bar = "─".repeat(max_width.saturating_sub(ld.len() + 3).min(50));
                lines.push(Line::from(Span::styled(
                    format!("  ┌{}{}", ld, bar),
                    Style::default().fg(color::CODE).add_modifier(Modifier::DIM),
                )));
                // Use tui-markdown for syntax-highlighted code
                if !code_buf.is_empty() {
                    let md_input = format!("```{}\n{}```", lang, code_buf);
                    let highlighted = tui_markdown::from_str(&md_input);
                    let tw = max_width.saturating_sub(8);
                    for hl_line in &highlighted.lines {
                        // Skip the ``` fences that tui-markdown adds
                        let text = hl_line.to_string();
                        if text.starts_with("```") { continue; }
                        line_no += 1;
                        let num = format!(" {:>3} ", line_no);
                        let patched = Style::default().fg(color::CODE).patch(hl_line.style);
                        let display_w = display_width(&text);
                        if display_w > tw {
                            for chunk in textwrap(&text, tw) {
                                let chunk = pad_to_width(&chunk, tw);
                                lines.push(Line::from(vec![
                                    Span::styled("  │", Style::default().fg(color::CODE).add_modifier(Modifier::DIM)),
                                    Span::styled(num.clone(), Style::default().fg(color::MUTED).add_modifier(Modifier::DIM)),
                                    Span::styled(chunk, patched),
                                ]));
                            }
                        } else {
                            lines.push(Line::from(vec![
                                Span::styled("  │", Style::default().fg(color::CODE).add_modifier(Modifier::DIM)),
                                Span::styled(num, Style::default().fg(color::MUTED).add_modifier(Modifier::DIM)),
                                Span::styled(pad_to_width(&text, tw), patched),
                            ]));
                        }
                        if line_no > 500 { break; }
                    }
                }
                code_buf.clear();
                lines.push(Line::from(Span::styled("  └", Style::default().fg(color::CODE).add_modifier(Modifier::DIM))));
                line_no = 0;
            } else {
                // Start code block
                in_code = true;
                line_no = 0;
                code_lang = line.trim_start_matches('`').trim();
                code_buf.clear();
            }
            continue;
        }
        if in_code {
            code_buf.push_str(line);
            code_buf.push('\n');
            continue;
        }

        // Pipe table: buffer consecutive | rows then render aligned
        let trimmed = line.trim_start();
        if trimmed.starts_with('|') && trimmed.contains('|') {
            table_buf.push(line);
            continue;
        }
        if !table_buf.is_empty() {
            lines.extend(render_pipe_table(&table_buf, max_width));
            table_buf.clear();
        }

        // Box-drawing table (┌ │ └ etc.)
        if is_table_row(&trimmed) {
            // Flush any buffered markdown
            // (tui-markdown handles tables poorly, so we collect and flush per-line)
            let style = if trimmed.chars().all(|c| c == '─' || c == '│' || c == '┌' || c == '┐' || c == '└' || c == '┘' || c == '├' || c == '┤' || c == '┬' || c == '┴' || c == '┼' || c == '|' || c == '-' || c == ':' || c == '+' || c == '=' || c == ' ' || c == '\t') {
                Style::default().fg(color::MUTED).add_modifier(Modifier::DIM)
            } else {
                Style::default().fg(color::TEXT)
            };
            lines.push(Line::from(Span::styled(format!("{}", line), style)));
            continue;
        }

        // Strip heading markers before tui-markdown (tui-markdown preserves ### but we don't want them visible)
        let trimmed_line = line.trim_start();
        let heading_level = if trimmed_line.starts_with("###### ") { 6 }
            else if trimmed_line.starts_with("##### ") { 5 }
            else if trimmed_line.starts_with("#### ") { 4 }
            else if trimmed_line.starts_with("### ") { 3 }
            else if trimmed_line.starts_with("## ") { 2 }
            else if trimmed_line.starts_with("# ") { 1 }
            else { 0 };
        let content_line = if heading_level > 0 {
            let marker_end = trimmed_line.find(' ').unwrap_or(0) + 1;
            let indent = line.len() - trimmed_line.len();
            &line[indent + marker_end..]
        } else { line };

        // Use tui-markdown for inline text (bold, italic, links, lists, etc.)
        let md = tui_markdown::from_str(content_line);
        for md_line in &md.lines {
            let tw = max_width.saturating_sub(2);
            let raw = md_line.to_string();
            let display_w = display_width(&raw);
            let line_style = if heading_level > 0 {
                // Apply heading style ourselves
                let col = match heading_level {
                    1..=3 => color::ACCENT,
                    _ => Color::White,
                };
                Style::default().fg(col).add_modifier(Modifier::BOLD)
            } else if md_line.style != Style::default() {
                md_line.style
            } else {
                Style::default().fg(color::TEXT)
            };

            if display_w > tw {
                for chunk in textwrap(&raw, tw) {
                    lines.push(Line::from(Span::styled(format!("  {}", chunk), line_style)));
                }
            } else {
                let mut spans: Vec<Span> = Vec::with_capacity(md_line.spans.len() + 1);
                spans.push(Span::from("  "));
                for s in &md_line.spans {
                    let content = s.content.clone();
                    let style = if s.style == Style::default() { line_style } else { s.style };
                    spans.push(Span::styled(content.to_string(), style));
                }
                lines.push(Line::from(spans));
            }
        }
    }

    if in_code && !code_buf.is_empty() {
        // Unclosed code block
        let lang = code_lang;
        let ld: String = if lang.is_empty() { " code ".into() } else { format!(" {} ", lang) };
        let bar = "─".repeat(max_width.saturating_sub(ld.len() + 3).min(50));
        lines.push(Line::from(Span::styled(
            format!("  ┌{}{}", ld, bar),
            Style::default().fg(color::CODE).add_modifier(Modifier::DIM),
        )));
        for cline in code_buf.lines() {
            line_no += 1;
            let tw = max_width.saturating_sub(8);
            for w in textwrap(cline, tw) {
                let w = pad_to_width(&w, tw);
                let num = format!(" {:>3} ", line_no);
                lines.push(Line::from(vec![
                    Span::styled("  │", Style::default().fg(color::CODE).add_modifier(Modifier::DIM)),
                    Span::styled(num, Style::default().fg(color::MUTED).add_modifier(Modifier::DIM)),
                    Span::styled(w, Style::default().fg(color::CODE)),
                ]));
            }
        }
        lines.push(Line::from(Span::styled("  └", Style::default().fg(color::CODE).add_modifier(Modifier::DIM))));
    }

    if !table_buf.is_empty() {
        lines.extend(render_pipe_table(&table_buf, max_width));
    }

    lines
}

/// Render a markdown pipe table (rows of | separated columns) with aligned columns.
/// `max_width` caps total table width; columns are proportionally shrunk if needed.
pub fn render_pipe_table(rows: &[&str], max_width: usize) -> Vec<Line<'static>> {
    if rows.is_empty() { return vec![]; }
    // Find separator row (|---|) to determine column boundaries
    let sep_idx = rows.iter().position(|r| {
        let t = r.trim();
        t.starts_with('|') && t.contains("---")
    });
    let (header_rows, _body_rows, col_count) = if let Some(idx) = sep_idx {
        // Count columns from separator row
        let cols = rows[idx].trim().matches('|').count().saturating_sub(1);
        (&rows[..idx], &rows[idx + 1..], cols)
    } else {
        (&[][..], rows, rows[0].trim().matches('|').count().saturating_sub(1))
    };

    if col_count == 0 { return vec![]; }

    // Calculate max display width per column across all rows
    let mut col_widths = vec![0usize; col_count];
    for row in rows {
        let trimmed = row.trim();
        if !trimmed.starts_with('|') { continue; }
        let cells: Vec<&str> = trimmed.split('|').skip(1).collect();
        for (i, cell) in cells.iter().enumerate() {
            if i >= col_count { break; }
            let w = display_width(cell.trim());
            if w > col_widths[i] { col_widths[i] = w; }
        }
    }

    // Cap total table width to max_width, with minimum 8 chars per column
    let total_w: usize = col_widths.iter().sum::<usize>() + col_count * 3 + 2;
    if total_w > max_width {
        let max_col = max_width.saturating_sub(col_count * 3 + 2).max(8) / col_count;
        for cw in col_widths.iter_mut() {
            if *cw > max_col { *cw = max_col; }
        }
    }

    let mut out = Vec::new();
    for (row_idx, row) in rows.iter().enumerate() {
        let trimmed = row.trim();
        if !trimmed.starts_with('|') { continue; }

        // Check if this is a separator row
        let is_sep = trimmed.contains("---");

        let cells: Vec<&str> = trimmed.split('|').skip(1).collect();
        let mut spans = Vec::new();
        spans.push(Span::styled("  │", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)));

        for (i, cell) in cells.iter().enumerate() {
            if i >= col_count { break; }
            let content = cell.trim();
            let target_w = col_widths[i];
            let content_w = display_width(content);
            let pad = target_w.saturating_sub(content_w);
            let padded = format!(" {}{} ", content, " ".repeat(pad));
            if is_sep {
                spans.push(Span::styled(padded, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)));
            } else if row_idx < header_rows.len() && !header_rows.is_empty() {
                spans.push(Span::styled(padded, Style::default().fg(color::ACCENT).add_modifier(Modifier::BOLD)));
            } else {
                spans.push(Span::styled(padded, Style::default().fg(color::TEXT)));
            }
            if i < col_count - 1 {
                spans.push(Span::styled("│", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)));
            }
        }
        spans.push(Span::styled("│", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)));
        out.push(Line::from(spans));
    }
    out
}

/// Check if a trimmed line looks like a table row (pipe or box-drawing characters).
pub fn is_table_row(s: &str) -> bool {
    if s.is_empty() { return false; }
    let first = s.chars().next().unwrap();
    // Starts with pipe | or box-drawing ─│┌┐└┘├┤┬┴┼
    matches!(first, '|' | '─' | '│' | '┌' | '┐' | '└' | '┘' | '├' | '┤' | '┬' | '┴' | '┼')
}
