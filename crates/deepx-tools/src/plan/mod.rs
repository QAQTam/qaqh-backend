//! PLAN.md reader: parse PLAN.md checklist items for importers (todo_import_plan).
//!
//! Format:
//! ```markdown
//! - [ ] P1: Title — Description。Deps: none。Effort: 2h
//! - [x] P2: Title — Description。Deps: P1。Effort: 4h | comment
//! ```
//!
//! This module no longer registers any Agent tools. Planning is done via
//! the unified `todo` tool, and PLAN.md serves only as a human-readable
//! plan of record that can be imported into todo.json via `todo_import_plan`.

mod types;
pub use types::PlanItem;

use std::path::Path;

fn plan_path() -> std::path::PathBuf {
    let dir = crate::workspace::deepx_dir();
    let new_path = dir.join("PLAN.md");

    if !new_path.exists() {
        let ws = crate::CURRENT_WORKSPACE
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if !ws.is_empty() && ws != "." {
            let old_path = Path::new(&ws).join("PLAN.md");
            if old_path.exists() {
                let _ = std::fs::create_dir_all(&dir);
                if std::fs::copy(&old_path, &new_path).is_ok() {
                    log::info!(
                        "plan: migrated PLAN.md from {} to {}",
                        old_path.display(),
                        new_path.display()
                    );
                }
            }
        }
    }

    new_path
}

pub fn read_plan() -> Result<String, String> {
    let path = plan_path();
    match std::fs::read_to_string(&path) {
        Ok(c) => Ok(c),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("read PLAN.md: {e}")),
    }
}

fn parse_plan(content: &str) -> Vec<PlanItem> {
    content.lines().filter_map(parse_plan_item).collect()
}

/// Public API: parse PLAN.md into structured items for importers.
pub fn parse_plan_items(content: &str) -> Vec<PlanItem> {
    parse_plan(content)
}

fn parse_plan_item(line: &str) -> Option<PlanItem> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("- [")?;
    let bracket_end = rest.find(']')?;
    let status = match &rest[..bracket_end] {
        "x" | "X" | "✓" => "approved",
        "-" => "rejected",
        _ => "pending",
    };
    let body = rest[bracket_end + 1..].trim();
    let (id_part, remainder) = body.split_once(": ")?;
    let id = id_part.trim().to_string();
    let (title_desc, comment) = if let Some((td, c)) = remainder.split_once(" | ") {
        (td.trim().to_string(), c.trim().to_string())
    } else {
        (remainder.trim().to_string(), String::new())
    };
    let (title, description) = if let Some((t, d)) = title_desc.split_once(" — ") {
        (t.trim().to_string(), d.trim().to_string())
    } else {
        (title_desc.clone(), String::new())
    };
    let mut deps = String::new();
    let mut effort = String::new();
    let mut clean_desc = String::new();
    for part in description.split("。") {
        let p = part.trim();
        if p.starts_with("Deps:") {
            deps = p.strip_prefix("Deps:").unwrap_or("").trim().to_string();
        } else if p.starts_with("Effort:") {
            effort = p.strip_prefix("Effort:").unwrap_or("").trim().to_string();
        } else if !p.is_empty() {
            if !clean_desc.is_empty() {
                clean_desc.push('。');
            }
            clean_desc.push_str(p);
        }
    }
    Some(PlanItem {
        id,
        title,
        description: clean_desc,
        status: status.to_string(),
        deps,
        effort,
        comment,
    })
}