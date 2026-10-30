//! Formatting of explicit workspace and task snapshots for compact requests.

use crate::todo::{TodoMode, TodoStatus, TodoStore};

/// Render only the supplied snapshots; no ambient workspace or filesystem access.
pub fn render(file_summary: &str, todos: Option<&TodoStore>) -> String {
    let mut out = String::new();
    if !file_summary.is_empty() {
        out.push_str(
            "Current file state (files touched this session; use these paths and \
             line counts instead of re-reading):\n",
        );
        out.push_str(file_summary);
        out.push('\n');
    }
    if let Some(store) = todos.filter(|store| !store.items.is_empty()) {
        let current = store.current_id.as_deref();
        out.push_str("Task checklist (session-scoped; preserve this in the summary):\n");
        if store.mode == TodoMode::Goal {
            out.push_str("  mode: goal (autonomous execution)\n");
        }
        for item in &store.items {
            let mark = match item.status {
                TodoStatus::Completed => "x",
                TodoStatus::InProgress => ">",
                TodoStatus::Cancelled => "-",
                TodoStatus::Pending => " ",
            };
            let cursor = if Some(item.id.as_str()) == current {
                "  <- current"
            } else {
                ""
            };
            out.push_str(&format!(
                "  [{}] T{}: {}{}\n",
                mark, item.id, item.title, cursor
            ));
        }
    }
    out
}
