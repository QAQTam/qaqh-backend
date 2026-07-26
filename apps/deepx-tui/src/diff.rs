//! Diff block rendering for TUI.
//!
//! Renders unified diffs as colored lines for inline display in the chat.
//! Used when the agent performs `write_file`, `edit_file`, or `patch` operations.
//!
//! Color scheme: + green (additions), - red (deletions), @@ blue (hunk headers).

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Render a unified diff from old and new content.
///
/// Returns styled lines suitable for display in a `Paragraph`.
/// Limited to `max_lines` lines to avoid flooding the terminal.
#[allow(dead_code)]
pub fn render_unified_diff(
    old_content: &str,
    new_content: &str,
    max_lines: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    // Header
    lines.push(Line::from(Span::styled(
        "─── diff ───",
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    )));

    if old_content == new_content {
        lines.push(Line::from(Span::styled(
            "  (no changes)",
            Style::default().fg(Color::DarkGray),
        )));
        return lines;
    }

    let diff = similar::TextDiff::from_lines(old_content, new_content);
    let mut count = 0;

    // similar doesn't expose hunk headers directly; use change grouping
    for group in diff.grouped_ops(3).into_iter() {
        if count >= max_lines {
            break;
        }
        for op in group {
            if count >= max_lines {
                break;
            }
            for change in diff.iter_changes(&op) {
                if count >= max_lines {
                    break;
                }
                let (marker, style) = match change.tag() {
                    similar::ChangeTag::Equal => ("  ", Style::default().fg(Color::DarkGray)),
                    similar::ChangeTag::Delete => ("- ", Style::default().fg(Color::Red)),
                    similar::ChangeTag::Insert => ("+ ", Style::default().fg(Color::Green)),
                };
                lines.push(Line::from(Span::styled(
                    format!("  {}{}", marker, change.value().trim_end()),
                    style,
                )));
                count += 1;
            }
        }
    }

    if count >= max_lines {
        lines.push(Line::from(Span::styled(
            "  ... (truncated)",
            Style::default().fg(Color::DarkGray),
        )));
    }

    lines
}

/// Render a "new file" preview (for write_file without old content).
pub fn render_new_file_preview(content: &str, max_lines: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    lines.push(Line::from(Span::styled(
        "─── new file ───",
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    )));

    let total = content.lines().count();
    for l in content.lines().take(max_lines) {
        lines.push(Line::from(Span::styled(
            format!("  + {}", l),
            Style::default().fg(Color::Green),
        )));
    }

    if total > max_lines {
        lines.push(Line::from(Span::styled(
            format!("  ... +{} more lines", total - max_lines),
            Style::default().fg(Color::DarkGray),
        )));
    }

    lines
}
