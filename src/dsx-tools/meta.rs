//! Skill listing and lookup.

use crate::session;
use crate::skills;
use crate::tools::CURRENT_SESSION;
use crate::types::ToolDef;
use super::parse_arg;

// ── Skills tools ──

#[allow(dead_code)]
pub(super) fn list_skills_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: crate::types::ToolFunction {
            name: "list_skills".into(),
            description: "List all installed skills and their descriptions.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
        },
    }
}

#[allow(dead_code)]
pub(super) fn read_skill_ref_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: crate::types::ToolFunction {
            name: "read_skill_ref".into(),
            description: "Read a skill's files to load domain knowledge for your task.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Skill name (e.g. postgresql)"},
                    "path": {"type": "string", "description": "File path within the skill (default: SKILL.md). Use references/<file> for reference docs."}
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        },
    }
}

pub(super) fn exec_list_skills(_args: &str) -> String {
    skills::tool_list_skills()
}

pub(super) fn exec_read_skill_ref(args: &str) -> String {
    skills::tool_read_skill_ref(args)
}

// ── Glance tool (file metadata preview) ──

#[allow(dead_code)]
pub(super) fn glance_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: crate::types::ToolFunction {
            name: "glance".into(),
            description: "Preview file metadata: symbols, purpose, language, size. Use after explore.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path to preview. Relative to project root."}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
    }
}

pub(super) fn exec_glance(args: &str) -> String {
    let path = parse_arg(args, "path");
    if path.is_empty() {
        return "[ERROR] glance: path is required".to_string();
    }

    let seed = CURRENT_SESSION.get().cloned().unwrap_or_default();
    if seed.is_empty() {
        return "[ERROR] glance: no active session. Start a conversation first.".to_string();
    }

    let short_content = session::read_memory(&seed, "short");
    let last_round = find_last_round_for_file(&short_content, &path);

    match session::read_semantic_entry(&seed, &path) {
        Some(entry) => {
            let lang = entry.language.as_deref().unwrap_or("?");
            let purpose = entry.purpose.as_deref().unwrap_or("-");
            let syms = if entry.symbols.is_empty() {
                "(none)".to_string()
            } else {
                entry.symbols.iter().take(10).cloned().collect::<Vec<_>>().join(", ")
            };
            let stale_note = match last_round {
                Some(r) => {
                    // Count total rounds
                    let total = short_content.lines()
                        .filter(|l| l.starts_with("- R") && l.contains(" → "))
                        .count();
                    if total.saturating_sub(r) >= 3 {
                        format!("\n⚠ Last read: R{} (now R{}) — content may be compressed. Consider read_file() if editing.", r, total)
                    } else {
                        format!("\nLast read: R{}", r)
                    }
                }
                None => String::new(),
            };
            format!(
                "[OK] {} | {} | {} symbols\nPurpose: {}\nSymbols: {}{}",
                path, lang, entry.symbols.len(), purpose, syms, stale_note
            )
        }
        None => {
            format!(
                "[OK] {} — No cached summary. Use read() to load and auto-cache, or explore() to scan the directory first.",
                path
            )
        }
    }
}

/// Find the last round number that touched a given file path.
/// Short-term memory format: "- R{N}: {tools} → {files} — {summary}"
fn find_last_round_for_file(short_mem: &str, file_path: &str) -> Option<usize> {
    for line in short_mem.lines().rev() {
        if !line.starts_with("- R") { continue; }
        if !line.contains(file_path) { continue; }
        if let Some(rest) = line.strip_prefix("- R") {
            if let Some(num_str) = rest.split(':').next() {
                if let Ok(n) = num_str.trim().parse::<usize>() {
                    return Some(n);
                }
            }
        }
    }
    None
}
