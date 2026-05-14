//! Popup rendering: confirm, danger, sudo, exit, intent picker, slash menu, submenu.

use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Clear, Paragraph},
};
use crate::types::SafetyLevel;

use super::color;
use super::center_rect;
use crate::tui::app::App;
use crate::tui::Mode;

/// Render all popup overlays (shared by horizontal and vertical layout).
pub fn render_popups(frame: &mut ratatui::Frame, app: &App) {
    if app.exit_prompt { render_exit_prompt(frame, app); }
    if app.mode == Mode::ToolConfirm || app.mode == Mode::DangerPrompt || app.mode == Mode::SudoPrompt { render_auth_popup(frame, app); }
    if app.mode == Mode::IntentPicker { render_intent_picker(frame, app); }
    if app.slash_open && !app.slash_items.is_empty() { render_slash_popup(frame, app); }
    if app.slash_submenu.is_some() { render_submenu_popup(frame, app); }
    // Orphan tool_use warning (400 detection)
    if app.health.has_orphan_tool_uses && app.mode != Mode::SessionPicker {
        let area = frame.area();
        let popup_rect = center_rect(area, 60, 3);
        frame.render_widget(Clear, popup_rect);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .title(" 400 Error ")
            .style(Style::default().bg(Color::Red).fg(Color::White));
        let inner = block.inner(popup_rect);
        frame.render_widget(block, popup_rect);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " Orphan tool_use → 400. Type /fix to repair ",
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ))).alignment(Alignment::Center),
            inner,
        );
    }
}

pub fn render_exit_prompt(frame: &mut ratatui::Frame, app: &App) {
    let tt = app.tt();
    let area = frame.area();
    let popup_rect = center_rect(area, 50, 5);

    frame.render_widget(Clear, popup_rect);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(" Exit ")
        .style(Style::default().bg(Color::Red).fg(Color::White))
        .border_style(Style::default().fg(Color::Red));
    let inner = block.inner(popup_rect);
    frame.render_widget(block, popup_rect);

    let text = vec![
        Line::from(Span::styled(tt.exit_title, Style::default().fg(Color::White).add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from(Span::styled("  Enter=confirm    Esc=cancel", Style::default().fg(Color::White).add_modifier(Modifier::BOLD))),
    ];
    frame.render_widget(Paragraph::new(Text::from(text)).alignment(Alignment::Center), inner);
}

pub fn render_auth_popup(frame: &mut ratatui::Frame, app: &App) {
    let tt = app.tt();
    let area = frame.area();
    let danger_mode = app.mode == Mode::DangerPrompt;
    let confirm_mode = app.mode == Mode::ToolConfirm;

    let (title, base_h) = if confirm_mode {
        (" Confirm ", 6u16)
    } else if danger_mode {
        (tt.danger_title, 9u16)
    } else {
        (tt.sudo_title, 8u16)
    };
    let tool_lines = app.pending_tools.len() as u16;
    let h = (base_h + tool_lines).min(area.height.saturating_sub(2));
    let popup_rect = center_rect(area, 50, h);

    // Clear background first
    frame.render_widget(Clear, popup_rect);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(format!(" {} ", title))
        .style(Style::default().bg(color::WARNING).fg(Color::White));
    let inner = block.inner(popup_rect);
    frame.render_widget(block, popup_rect);

    let mut lines: Vec<Line> = Vec::new();
    if confirm_mode {
        lines.push(Line::from(Span::styled("The AI wants to run these tools:", Style::default().fg(Color::White).add_modifier(Modifier::BOLD))));
        lines.push(Line::from(""));
        for (tc, safety, _) in &app.pending_tools {
            let icon = match safety {
                SafetyLevel::Danger => " !!",
                SafetyLevel::Confirm => "  ?",
                SafetyLevel::Safe => "  ~",
            };
            lines.push(Line::from(Span::styled(
                format!("{}  {}", icon, tc.function.name),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("  Enter=confirm  Esc=cancel", Style::default().fg(Color::White).add_modifier(Modifier::BOLD))));
    } else if danger_mode {
        lines.push(Line::from(Span::styled(tt.danger_warning, Style::default().fg(Color::White).add_modifier(Modifier::BOLD))));
        lines.push(Line::from(""));
        for (tc, _, _) in &app.pending_tools {
            let cmd_str = serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                .ok()
                .and_then(|v| v.get("command").and_then(|c| c.as_str().map(|s| s.to_string())))
                .unwrap_or_else(|| tc.function.arguments.clone());
            lines.push(Line::from(Span::styled(
                format!("  {}: {}", tt.tools_on, cmd_str),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(tt.danger_enter, Style::default().fg(Color::White).add_modifier(Modifier::BOLD))));
        lines.push(Line::from(Span::styled(tt.danger_esc, Style::default().fg(Color::White).add_modifier(Modifier::BOLD))));
    } else {
        let masked = if app.sudo_password.is_empty() {
            String::new()
        } else {
            "*".repeat(app.sudo_password.len())
        };
        lines.push(Line::from(Span::styled(
            tt.sudo_mask,
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            if masked.is_empty() { "  ".into() } else { format!("  {} ", masked) },
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            tt.sudo_enter,
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            tt.sudo_esc,
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        )));
    }

    frame.render_widget(
        Paragraph::new(Text::from(lines)).alignment(Alignment::Center),
        inner,
    );
}

pub fn render_intent_picker(frame: &mut ratatui::Frame, app: &App) {
    let area = frame.area();
    let opt_count = app.intent_options.len().max(1);
    let h = (opt_count + 4).min(14) as u16;
    let popup_rect = center_rect(area, 56, h);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(" Choose ")
        .style(Style::default().bg(color::PANEL).fg(Color::White));

    frame.render_widget(ratatui::widgets::Clear, popup_rect);
    frame.render_widget(block.clone(), popup_rect);

    let inner = block.inner(popup_rect);
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(&app.intent_question, Style::default().fg(Color::White).add_modifier(Modifier::BOLD))));
    lines.push(Line::from(""));
    for (i, opt) in app.intent_options.iter().enumerate() {
        let letter = (b'a' + i as u8) as char;
        let style = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
        lines.push(Line::from(Span::styled(format!("  [{}] {}", letter.to_uppercase(), opt), style)));
    }
    lines.push(Line::from(""));
    if !app.input.is_empty() {
        lines.push(Line::from(Span::styled(format!("  Custom: {}", app.input), Style::default().fg(Color::Yellow))));
    } else {
        lines.push(Line::from(Span::styled("  [A-F] select option  [Enter] custom  [Esc] cancel", Style::default().fg(Color::White).add_modifier(Modifier::BOLD))));
    }

    frame.render_widget(
        Paragraph::new(Text::from(lines)).style(Style::default().bg(color::PANEL)),
        inner,
    );
}

pub fn render_slash_popup(frame: &mut ratatui::Frame, app: &App) {
    let items = &app.slash_items;
    if items.is_empty() { return; }

    let max_visible = 8usize.min(items.len());
    let popup_h = max_visible as u16 + 2;
    let popup_w = 52u16;

    // Position above input in workspace (right column)
    let area = frame.area();
    let input_y = area.y + area.height.saturating_sub(3);
    let x = (area.width as f64 * 0.58) as u16 + 2; // right column
    let y = input_y.saturating_sub(popup_h);

    if y <= area.y { return; } // Not enough space

    let popup_rect = Rect { x, y, width: popup_w, height: popup_h };

    // Title shows filter text
    let title = format!(" Commands: {} ", app.slash_filter);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title)
        .style(Style::default().bg(color::PANEL).fg(color::TEXT));

    frame.render_widget(ratatui::widgets::Clear, popup_rect);
    frame.render_widget(block.clone(), popup_rect);

    let inner = block.inner(popup_rect);

    // Compute scroll window — center selection in visible area
    let start_idx = if items.len() <= max_visible {
        0
    } else if app.slash_idx < max_visible / 2 {
        0
    } else if app.slash_idx + max_visible / 2 >= items.len() {
        items.len() - max_visible
    } else {
        app.slash_idx - max_visible / 2
    };
    let end_idx = (start_idx + max_visible).min(items.len());

    let mut lines: Vec<Line> = Vec::new();
    for i in start_idx..end_idx {
        let (cmd, desc) = &items[i];
        let is_selected = i == app.slash_idx;
        let style = if is_selected {
            Style::default().fg(color::BG).bg(color::ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(color::TEXT).bg(color::PANEL)
        };
        lines.push(Line::from(Span::styled(
            format!(" {}  {}", cmd, desc),
            style,
        )));
    }

    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

pub fn render_submenu_popup(frame: &mut ratatui::Frame, app: &App) {
    let sub = match &app.slash_submenu {
        Some(s) => s,
        None => return,
    };
    let items = &sub.items;
    if items.is_empty() { return; }

    let max_visible = 6usize.min(items.len());
    let popup_h = max_visible as u16 + 2;
    let popup_w = 40u16;

    // Position to the right of the main slash popup (which is at x+2, width 52)
    let area = frame.area();
    let input_y = area.y + area.height.saturating_sub(3);
    let main_popup_h = 8usize.min(app.slash_items.len().max(1)) as u16 + 2;
    let y = input_y.saturating_sub(main_popup_h);
    let x = area.x + 2 + 54;

    if y <= area.y { return; }
    let popup_rect = Rect { x, y, width: popup_w, height: popup_h };

    let title = format!(" {} options ", sub.parent_cmd);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title)
        .style(Style::default().bg(color::PANEL).fg(color::TEXT));

    frame.render_widget(ratatui::widgets::Clear, popup_rect);
    frame.render_widget(block.clone(), popup_rect);

    let inner = block.inner(popup_rect);
    let start_idx = if items.len() <= max_visible { 0 }
        else if sub.idx < max_visible / 2 { 0 }
        else if sub.idx + max_visible / 2 >= items.len() { items.len() - max_visible }
        else { sub.idx - max_visible / 2 };
    let end_idx = (start_idx + max_visible).min(items.len());

    let mut lines: Vec<Line> = Vec::new();
    for i in start_idx..end_idx {
        let (val, desc) = &items[i];
        let is_selected = i == sub.idx;
        let style = if is_selected {
            Style::default().fg(color::BG).bg(color::ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(color::TEXT).bg(color::PANEL)
        };
        lines.push(Line::from(Span::styled(
            format!(" {}  {}", val, desc),
            style,
        )));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}
