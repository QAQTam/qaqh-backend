//! Workspace panel rendering: project info, token stats, health status, memory overview.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Clear, Paragraph},
};
use crate::tokenizer;

use super::color;
use super::truncate_path;
use crate::tui::app::App;

pub fn render_workspace(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    if area.height < 3 { return; }
    let mut lines: Vec<Line> = Vec::new();

    // Section header
    lines.push(Line::from(Span::styled(
        "Workspace",
        Style::default().fg(color::ACCENT).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        "─".repeat(area.width.saturating_sub(4) as usize),
        Style::default().fg(color::BORDER),
    )));

    // Low balance warning
    if let Some(ref bal) = app.balance {
        let bal_str = bal.trim();
        if let Some(num_str) = bal_str.split_whitespace().next() {
            if let Ok(val) = num_str.parse::<f64>() {
                if val < 1.0 && val > 0.0 {
                    let warn = if app.config.prompt_lang == "zh" {
                        "⚠ 余额不足，请及时前往 platform.deepseek.com 续费"
                    } else {
                        "⚠ Insufficient balance — top up at platform.deepseek.com"
                    };
                    lines.push(Line::from(Span::styled(warn, Style::default().fg(color::ERROR).add_modifier(Modifier::BOLD))));
                    lines.push(Line::from(Span::styled(
                        "─".repeat(area.width.saturating_sub(4) as usize),
                        Style::default().fg(color::BORDER),
                    )));
                }
            }
        }
    }

    // Health/status warning (when active)
    if !app.status_msg.is_empty() {
        let warn = &app.status_msg;
        let (warn_color, prefix) = if warn.starts_with("⚠ ") {
            (color::WARNING, "")
        } else {
            (color::MUTED, "")
        };
        lines.push(Line::from(Span::styled(
            format!("{}{}", prefix, warn),
            Style::default().fg(warn_color).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            "─".repeat(area.width.saturating_sub(4) as usize),
            Style::default().fg(color::BORDER),
        )));
    }

    // Active plan status
    if !app.session_seed.is_empty() {
        let plans = crate::session::list_plans(&app.session_seed);
        let active = plans.iter().find(|(_, s, _)| s == "active" || s == "draft");
        match active {
            Some((name, status, _)) => {
                let icon = if status == "active" { "●" } else { "○" };
                let color = if status == "active" { color::SUCCESS } else { color::MUTED };
                lines.push(Line::from(Span::styled(
                    format!("{} Plan: {} [{}]", icon, name, status),
                    Style::default().fg(color),
                )));
            }
            None => {
                lines.push(Line::from(Span::styled(
                    "○ No active plan",
                    Style::default().fg(color::MUTED),
                )));
            }
        }
    }

    // Task summary
    if !app.session_seed.is_empty() {
        let content = crate::session::read_memory(&app.session_seed, "tasks");
        let task_lines: Vec<&str> = content.lines()
            .filter(|l| l.contains("[pending]") || l.contains("[in_progress]") || l.contains("[completed]"))
            .collect();
        if !task_lines.is_empty() {
            let pending = task_lines.iter().filter(|l| l.contains("[pending]")).count();
            let active = task_lines.iter().filter(|l| l.contains("[in_progress]")).count();
            let done = task_lines.iter().filter(|l| l.contains("[completed]")).count();
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("Tasks: {} pending · {} active · {} done", pending, active, done),
                Style::default().fg(color::TEXT),
            )));
            // Show in-progress and pending tasks (max 5)
            let active_tasks: Vec<&&str> = task_lines.iter()
                .filter(|l| l.contains("[in_progress]") || l.contains("[pending]"))
                .take(5)
                .collect();
            for t in &active_tasks {
                let t_clean = t.trim_start_matches("- ");
                let is_active = t.contains("[in_progress]");
                let style = if is_active { Style::default().fg(color::SUCCESS) }
                    else { Style::default().fg(color::MUTED) };
                let short = if t_clean.len() > 50 {
                    let mut end = 50.min(t_clean.len());
                    while end > 0 && !t_clean.is_char_boundary(end) { end -= 1; }
                    format!("{}...", &t_clean[..end])
                } else { t_clean.to_string() };
                lines.push(Line::from(Span::styled(format!("  {}", short), style)));
            }
        }
    }

    // File changes this session
    let changes = &app.long_mem.file_changes;
    if !changes.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Changes:",
            Style::default().fg(color::TEXT).add_modifier(Modifier::BOLD),
        )));
        for (path, added, removed) in changes.iter().rev().take(5) {
            let short_p = truncate_path(path, 30);
            lines.push(Line::from(Span::styled(
                format!("  {} +{}/-{}", short_p, added, removed),
                Style::default().fg(color::MUTED),
            )));
        }
    }

    // Health errors
    let error_count = app.health.recent_errors.len();
    if error_count > 0 {
        lines.push(Line::from(""));
        let err_style = match app.health.overall_level {
            crate::health::HealthLevel::Red => Style::default().fg(color::ERROR).add_modifier(Modifier::BOLD),
            crate::health::HealthLevel::Yellow => Style::default().fg(color::WARNING),
            _ => Style::default().fg(color::MUTED),
        };
        let pitfall_count = app.pitfall_guide.entries.len();
        lines.push(Line::from(Span::styled(
            format!("Health: {} errors | {:?} | {} pitfalls learned", error_count, app.health.overall_level, pitfall_count),
            err_style,
        )));
    }

    frame.render_widget(
        Paragraph::new(Text::from(lines)).style(Style::default().fg(color::TEXT)),
        area,
    );
}

// ── Bottom-Right: Info Panel (status, tokens, cost, logo) ──

pub fn render_info_panel(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let bg = Style::default().bg(Color::Black);
    frame.render_widget(Clear, area);
    let block = Block::default().style(bg);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let _ctx_used = tokenizer::format_tokens(app.tokens_used());
    let pct = app.context_pct();
    let bar_w = 16usize.min(inner.width as usize / 3);
    let filled = ((bar_w as f64 * pct).round() as usize).min(bar_w);
    let pct_color = if pct > 0.65 { color::ERROR } else if pct > 0.50 { color::WARNING } else { color::ACCENT };

    let filled_bar = "━".repeat(filled);
    let empty_bar = "━".repeat(bar_w.saturating_sub(filled));

    // Line 1: model + phase + effort
    let auto_tag = if app.auto_mode { "[AUTO]" } else { "" };
    let think = match app.config.effort.as_deref() {
        Some("max") => "MAX",
        _ => "HIGH",
    };
    let phase_str = match app.current_task_phase {
        crate::types::TaskPhase::Explore => "EXPLORE",
        crate::types::TaskPhase::Plan => "PLAN",
        crate::types::TaskPhase::Chat => "CHAT",
        crate::types::TaskPhase::Coding => "CODE",
        crate::types::TaskPhase::Debug => "DEBUG",
    };
    let health_icon = match app.health.overall_level {
        crate::health::HealthLevel::Red => " ●",
        crate::health::HealthLevel::Yellow => " ◐",
        _ => "",
    };
    let emotion = app.health.infer_emotion();
    let emotion_str = format!(" {} {}", emotion.emoji(), emotion.label());
    let tier_str = match app.health.context_tier {
        crate::health::ContextTier::Danger => " DGR",
        crate::health::ContextTier::Healthy => "",
        crate::health::ContextTier::Premium => "",
    };
    let skip_tag = if app.skip_all { " ⛌" } else { "" };
    let exec_tag = if app.exec_pending > 0 {
        let frames = ["|", "/", "-", "\\", "|", "/", "-", "\\", "|", "/"];
        let idx = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap_or_default()
            .as_millis() / 120) as usize % frames.len();
        format!(" {}", frames[idx])
    } else { String::new() };
    let mode_line = format!("{} {} {} {}{}{}{}{}{}", app.config.model, auto_tag, think, phase_str, health_icon, tier_str, emotion_str, skip_tag, exec_tag);
    let pct_label = format!("{:.0}%", pct * 100.0);

    // Line 2: token context usage + cost
    let bal = app.balance.as_deref().unwrap_or("").trim();
    let bal_color = if let Some(num_str) = bal.split_whitespace().next() {
        if let Ok(val) = num_str.parse::<f64>() {
            if val < 1.0 { color::ERROR } else if val < 5.0 { color::WARNING } else { color::MUTED }
        } else { color::MUTED }
    } else { color::MUTED };
    let tokens_line = format!("{}", bal);

    // Line 3: session time + version
    let elapsed = if app.session_start > 0 {
        let secs = crate::session::now_epoch().saturating_sub(app.session_start);
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        if h > 0 { format!("{}h{:02}m{:02}s", h, m, s) } else if m > 0 { format!("{}m{:02}s", m, s) } else { format!("{}s", s) }
    } else { "--".into() };
    let local = chrono::Local::now();
    let time_line = format!("runtime:{}  {}  dsx v{}", elapsed, local.format("%H:%M:%S"), env!("CARGO_PKG_VERSION"));

    // Health status line (per-turn, from auto_extract_memory)
    let health_line = if !app.health_status_line.is_empty() {
        let hl_color = if app.health_status_line.contains("RED") { color::ERROR }
            else if app.health_status_line.contains("YLW") { color::WARNING }
            else { color::SUCCESS };
        vec![Line::from(Span::styled(&app.health_status_line, Style::default().fg(hl_color)))]
    } else {
        vec![]
    };

    let spans = vec![
        Line::from(Span::styled(mode_line, Style::default().fg(color::TEXT).add_modifier(Modifier::BOLD))),
        Line::from(vec![
            Span::styled(filled_bar, Style::default().fg(pct_color).add_modifier(Modifier::BOLD)),
            Span::styled(empty_bar, Style::default().fg(Color::DarkGray)),
            Span::styled(format!(" {}", pct_label), Style::default().fg(color::MUTED)),
        ]),
        Line::from(Span::styled(tokens_line, Style::default().fg(bal_color))),
    ]
    .into_iter()
    .chain(health_line)
    .chain(std::iter::once(Line::from(Span::styled(time_line, Style::default().fg(color::MUTED)))))
    .collect::<Vec<_>>();

    frame.render_widget(Paragraph::new(Text::from(spans)).style(bg), inner);
}
