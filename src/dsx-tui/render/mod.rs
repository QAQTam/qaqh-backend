//! Render entry point: layout, main render_all dispatch, center_rect.

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, BorderType, Borders},
};
use super::app::App;
use super::utils::textwrap;

// ── OpenCode-inspired color palette ──
pub mod color {
    use ratatui::style::Color;
    pub const BG: Color = Color::Black;
    pub const PANEL: Color = Color::DarkGray;
    pub const BORDER: Color = Color::Gray;
    pub const TEXT: Color = Color::White;
    pub const MUTED: Color = Color::DarkGray;
    pub const ACCENT: Color = Color::Rgb(79, 107, 237);
    pub const SUCCESS: Color = Color::Green;
    pub const WARNING: Color = Color::Yellow;
    pub const ERROR: Color = Color::Red;
    pub const CODE: Color = Color::Yellow;
}

pub(crate) const DS_BANNER: &str = r#"
  ████████████████████████████████ ██████▀▀████████████████████████████████████████████████  ██████
  ████████████████████████████████ ██████  ████████████████████████████████████████████████  ██████
  ████████▀▀██▀▀▀▀▀████▀ █████████ ███▀▀█  ██▀▀▀▀████▀▀▀▀███▀█▀▀▀████▀▀▀▀████▀▀▀▀████▀▀▀███  ███▀▀█
  ███▀▀           █████    ▀█▀▀▀▀  ██   █  █▀     ██▀     ██      ██  ▄   ██     ▀██     ██  ██  ██
  ██                ▀███         ▄ █  ███  █ ▄███ ▀█ ▄██▄ ██ ▄███ ██ ▄██▄▄█▀ ███  █▀ ███  █  █▀ ▄██
  █                   ▀██▄     ▄██ █ ████  █ ██▀▀  █ █▀▀▀ ██ ████ ██  ▀▀▀██  █▀▀▀ █  █▀▀▀ █  █ ▄███
    ▄██▄▄▄         ▄    ▀▀   █████ █ ████  █ ██▄▄▄▄█ █▄▄▄▄██ ████ ███▄▄   █  █▄▄▄▄█  █▄▄▄▄█  █  ███
    ████████▄      ▀██      ▄█████ █ ▀███  █ ▀███▀██ ▀███▀██ ███▀ ██▀███  █▄ ███▀▀█  ███▀▀█  █▄ ▀██
     █████████▄     ▀██     ██████ █▄  ▀   █▄     ██▄     ██ █   ▄██  ▀▀  ██     ▄██     ▄█  ██  ██
  █  ▀██████████▄          ███████ ██▄▄▄▄▄▄██▄▄▄▄████▄▄▄▄███ █▄▄█████▄▄▄▄████▄▄▄▄████▄▄▄▄██▄▄███▄▄█
  █▄   █████▀▀███▄        ████████ ██████████████████████████████████████████████████████████████████
  ███    ▀███  ▀▀██▄     ▀████████ ██████████████████████████████████████████████████████████████████
"#;

pub mod chat;
pub mod popups;
pub mod input;
pub mod workspace;
pub mod tool_code;
pub mod status;
pub use chat::*;
pub use popups::*;
pub use input::*;
pub use workspace::*;
pub use tool_code::*;
pub use status::*;

/// Centered popup rectangle.
pub(super) fn center_rect(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(4));
    let x = (area.width.saturating_sub(w)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    Rect { x, y, width: w, height }
}

pub(super) fn truncate_path(path: &str, max_w: usize) -> String {
    if path.len() <= max_w { return path.to_string(); }
    // Keep the filename, truncate the middle
    let sep = std::path::MAIN_SEPARATOR;
    let parts: Vec<&str> = path.split(sep).collect();
    if parts.len() <= 2 { return path.chars().take(max_w - 3).collect::<String>() + "..."; }
    let last = parts.last().unwrap_or(&"");
    format!("{}...{}{}", parts[0], sep, last)
}

pub(super) fn render_all(frame: &mut ratatui::Frame, app: &mut App) {
    let area = frame.area();

    // Fall back to vertical layout on narrow terminals (< 100 cols)
    if area.width < 100 {
        return render_all_vertical(frame, app);
    }

    // ── 3-zone layout ──
    // Top 65%: chat(left) + workspace(right)
    // Bottom 35%: exec panel (full width, permanent)
    let top_h = (area.height as f64 * 0.65) as u16;
    let [top_area, exec_area] = Layout::vertical([
        Constraint::Length(top_h),
        Constraint::Min(1),
    ]).areas(area);

    // Top row: chat 65%, workspace 35%
    let [chat_area, right_col] = Layout::horizontal([
        Constraint::Percentage(65),
        Constraint::Percentage(35),
    ]).areas(top_area);

    // ── Chat border (blue) ──
    let chat_border = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(Color::Blue));
    let chat_inner = chat_border.inner(chat_area);
    frame.render_widget(chat_border, chat_area);
    render_chat(frame, chat_inner, app);

    // ── Workspace border (cyan) ──
    let ws_border = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(Color::Cyan));
    let ws_inner = ws_border.inner(right_col);
    frame.render_widget(ws_border, right_col);

    // Right side inside border: workspace + info panel
    let [w_area, info_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(4),
    ]).areas(ws_inner);

    let input_w = ws_inner.width.saturating_sub(2) as usize;
    let wrapped = textwrap(&app.input, input_w.max(20));
    let input_h = (wrapped.len().max(app.input.lines().count().max(1)) + 1).max(3) as u16;
    let input_h = input_h.min(w_area.height.saturating_sub(5));
    let [plan_area, input_area] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(input_h),
    ]).areas(w_area);

    render_workspace(frame, plan_area, app);
    render_input(frame, input_area, app);
    render_info_panel(frame, info_area, app);

    // ── Exec panel (bottom, always visible) ──
    render_tool_code(frame, exec_area, app);

    render_popups(frame, app);
}

/// Fallback vertical layout for narrow terminals.
pub(super) fn render_all_vertical(frame: &mut ratatui::Frame, app: &mut App) {
    let area = frame.area();
    let wrapped = textwrap(&app.input, area.width.saturating_sub(4) as usize);
    let input_h = (wrapped.len().max(app.input.lines().count().max(1)) + 1).max(3) as u16;
    let input_h = input_h.min(area.height.saturating_sub(3));

    let [chat_area, status_area, input_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(input_h),
    ]).areas(area);

    render_chat(frame, chat_area, app);
    render_status_line(frame, status_area, app);
    render_input(frame, input_area, app);

    render_popups(frame, app);
}
