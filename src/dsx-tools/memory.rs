//! Memory management: save, read, forget.

use crate::tools::CURRENT_SESSION;
use crate::types::ToolDef;
use super::{parse_arg, parse_opt};
// ── Memory management tools ──

#[allow(dead_code)]
pub(super) fn mem_save_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: crate::types::ToolFunction {
            name: "mem_save".into(),
            description: "Save a note to short-term memory. Key max 50 chars, no code blocks.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "tier": {"type": "string", "enum": ["short"], "description": "Memory tier: short (this session only)"},
                    "key": {"type": "string", "description": "Unique key, max 50 chars"},
                    "content": {"type": "string", "description": "Note content, max 200 chars"}
                },
                "required": ["tier", "key", "content"],
                "additionalProperties": false
            }),
        },
    }
}

pub(super) fn exec_mem_save(args: &str) -> String {
    let tier = parse_arg(args, "tier");
    let key = parse_arg(args, "key");
    let content = parse_arg(args, "content");

    if tier != "short" {
        return "[ERROR] mem_save: tier must be \"short\"".to_string();
    }
    if key.is_empty() || key.chars().count() > 50 {
        return "[ERROR] mem_save: key must be 1-50 chars".to_string();
    }
    if content.contains("```") {
        return "[ERROR] mem_save: content must not contain code blocks (```). Use mem_save to record decisions, not code.".to_string();
    }
    let truncated: String = content.chars().take(200).collect();

    let seed = CURRENT_SESSION.get().cloned().unwrap_or_default();
    if seed.is_empty() {
        return "[ERROR] mem_save: no active session. Start a conversation first.".to_string();
    }

    // Format: `- key: content`
    let entry = format!("- {}: {}", key, truncated);
    // Remove old entry with same key
    crate::session::forget_memory_key(&seed, &format!("{}: ", key));
    crate::session::append_memory(&seed, &tier, &entry);

    let len = if truncated.len() < content.len() { "(truncated)" } else { "" };
    format!("[OK] Saved to {}-mem.md: {}: {} {}", tier, key, truncated, len)
}

#[allow(dead_code)]
pub(super) fn mem_read_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: crate::types::ToolFunction {
            name: "mem_read".into(),
            description: "Read memory by tier (short|tasks) and optional key.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "tier": {"type": "string", "enum": ["short", "tasks"], "description": "Memory tier: short or tasks"},
                    "key": {"type": "string", "description": "Filter to entry matching this key"}
                },
                "required": ["tier"],
                "additionalProperties": false
            }),
        },
    }
}

pub(super) fn exec_mem_read(args: &str) -> String {
    let tier = parse_arg(args, "tier");
    let key = parse_opt(args, "key").unwrap_or_default();

    if tier != "short" && tier != "tasks" {
        return "[ERROR] mem_read: tier must be \"short\" or \"tasks\"".to_string();
    }

    let seed = CURRENT_SESSION.get().cloned().unwrap_or_default();
    if seed.is_empty() {
        return "[ERROR] mem_read: no active session".to_string();
    }

    let content = crate::session::read_memory(&seed, &tier);
    if content.is_empty() {
        return format!("[OK] {}-mem.md is empty.", tier);
    }

    if !key.is_empty() {
        // Filter lines containing the key
        let matches: String = content
            .lines()
            .filter(|l| l.contains(&key))
            .collect::<Vec<_>>()
            .join("\n");
        if matches.is_empty() {
            format!("[OK] No entry matching '{}' in {}-mem.md", key, tier)
        } else {
            format!("[OK] {}-mem.md matching '{}':\n{}", tier, key, matches)
        }
    } else {
        format!("[OK] {}-mem.md:\n{}", tier, content)
    }
}

#[allow(dead_code)]
pub(super) fn mem_forget_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: crate::types::ToolFunction {
            name: "mem_forget".into(),
            description: "Delete a memory entry by key from short/tasks tiers.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string", "description": "Key to delete"}
                },
                "required": ["key"],
                "additionalProperties": false
            }),
        },
    }
}

pub(super) fn exec_mem_forget(args: &str) -> String {
    let key = parse_arg(args, "key");
    if key.is_empty() {
        return "[ERROR] mem_forget: key is required".to_string();
    }

    let seed = CURRENT_SESSION.get().cloned().unwrap_or_default();
    if seed.is_empty() {
        return "[ERROR] mem_forget: no active session".to_string();
    }

    crate::session::forget_memory_key(&seed, &key);
    format!("[OK] Removed entries matching '{}' from memory files.", key)
}

// ── Recall: session-history search ──

#[allow(dead_code)]
pub(super) fn recall_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: crate::types::ToolFunction {
            name: "recall".into(),
            description: "Search session history for compacted past rounds. Filters: file|error|decision|round|all.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search term. Matches file paths, tool names, error text, or keywords."},
                    "kind": {"type": "string", "enum": ["file", "error", "decision", "round", "all"], "description": "Filter by entry type: file, error, decision, round, or all (default)"},
                    "max_results": {"type": "integer", "default": 10, "description": "Max results to return (1-20, default 10)"}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
    }
}

pub(super) fn exec_recall(args: &str) -> String {
    let query = parse_arg(args, "query");
    if query.is_empty() {
        return "[ERROR] recall: query is required".to_string();
    }
    let kind = parse_opt(args, "kind").unwrap_or_else(|| "all".to_string());
    let max: usize = parse_opt(args, "max_results")
        .and_then(|s| s.parse().ok())
        .unwrap_or(10)
        .min(20);

    let seed = CURRENT_SESSION.get().cloned().unwrap_or_default();
    if seed.is_empty() {
        return "[ERROR] recall: no active session".to_string();
    }

    let query_lower = query.to_lowercase();
    let mut results: Vec<String> = Vec::new();

    // Search short-mem.md for round entries
    let short = crate::session::read_memory(&seed, "short");
    for line in short.lines() {
        if line.to_lowercase().contains(&query_lower) {
            let trimmed = line.trim();
            if trimmed.starts_with("- R") {
                match &kind[..] {
                    "round" | "all" => results.push(trimmed.to_string()),
                    _ => {}, // skip round summaries for file/error/decision filters
                }
            }
        }
    }

    // Search long-mem.md for decisions, architecture, file changes
    let long = crate::session::read_memory(&seed, "long");
    for line in long.lines() {
        if line.to_lowercase().contains(&query_lower) {
            let trimmed = line.trim();
            if !trimmed.starts_with("- ") && !trimmed.starts_with("## ") { continue; }
            match &kind[..] {
                "decision" if trimmed.contains("→") => results.push(format!("D: {}", trimmed.trim_start_matches("- "))),
                "file" if trimmed.contains("`") => results.push(format!("F: {}", trimmed.trim_start_matches("- "))),
                "all" => results.push(trimmed.to_string()),
                _ => {},
            }
        }
    }

    // Search learning.md for reasoning + outcome entries
    let learning = crate::session::read_memory(&seed, "learning");
    if !learning.is_empty() {
        // Group by turn entry (### tN | ... blocks)
        let mut in_block = false;
        let mut block: Vec<&str> = Vec::new();
        for line in learning.lines() {
            if line.starts_with("### t") {
                // Push previous block
                if in_block && block.iter().any(|l| l.to_lowercase().contains(&query_lower)) {
                    match &kind[..] {
                        "all" => {
                            results.push(format!("L: {}", block[0].trim_start_matches("### ")));
                            for l in &block[1..] {
                                if !l.is_empty() { results.push(format!("  {}", l)); }
                            }
                        }
                        "error" if block[0].contains("[错误]") => {
                            results.push(format!("L: {}", block[0].trim_start_matches("### ")));
                            for l in &block[1..] {
                                if !l.is_empty() { results.push(format!("  {}", l)); }
                            }
                        }
                        "file" if block[0].contains("[操作]") => {
                            results.push(format!("L: {}", block[0].trim_start_matches("### ")));
                            for l in &block[1..] {
                                if !l.is_empty() { results.push(format!("  {}", l)); }
                            }
                        }
                        "decision" | "round" if block[0].contains("[思考]") => {
                            results.push(format!("L: {}", block[0].trim_start_matches("### ")));
                            for l in &block[1..] {
                                if !l.is_empty() { results.push(format!("  {}", l)); }
                            }
                        }
                        _ => {},
                    }
                }
                block.clear();
                block.push(line);
                in_block = true;
            } else if in_block {
                block.push(line);
            }
        }
        // Last block
        if in_block && block.iter().any(|l| l.to_lowercase().contains(&query_lower)) {
            match &kind[..] {
                "all" | "round" => {
                    results.push(format!("L: {}", block[0].trim_start_matches("### ")));
                    for l in &block[1..] {
                        if !l.is_empty() { results.push(format!("  {}", l)); }
                    }
                }
                _ => {},
            }
        }
    }

    // Dedup and limit
    results.sort();
    results.dedup();
    if results.len() > max {
        results.truncate(max);
    }

    if results.is_empty() {
        format!("[OK] No matches for '{}' in session history.", query)
    } else {
        format!("[OK] {} match(es) for '{}' (kind={}):\n{}",
            results.len(), query, kind,
            results.join("\n"))
    }
}

// ── Pitfall Guide tools (cross-session error learning) ──

#[allow(dead_code)]
pub(super) fn pitfall_save_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: crate::types::ToolFunction {
            name: "pitfall_save".into(),
            description: "Save a cross-session lesson from an error. Future sessions get warned.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "description": {"type": "string", "description": "What went wrong. Be specific about the error and context."},
                    "lesson": {"type": "string", "description": "How to avoid it next time. Keep it actionable."},
                    "tool": {"type": "string", "description": "Which tool was involved"},
                    "error_kind": {"type": "string", "enum": ["ToolParameter", "FileAccess", "ExecFailure", "NetworkFailure", "SudoFailure", "Timeout", "Panic", "SessionMissing", "ToolNotFound", "Unknown"], "description": "Error category"}
                },
                "required": ["description", "lesson"],
                "additionalProperties": false
            }),
        },
    }
}

pub(super) fn exec_pitfall_save(args: &str) -> String {
    let description = parse_arg(args, "description");
    let lesson = parse_arg(args, "lesson");
    let tool = parse_opt(args, "tool").unwrap_or_default();
    let error_kind_str = parse_opt(args, "error_kind").unwrap_or_else(|| "Unknown".to_string());

    if description.is_empty() {
        return "[ERROR] pitfall_save: description is required".to_string();
    }
    if lesson.is_empty() {
        return "[ERROR] pitfall_save: lesson is required".to_string();
    }

    let emotion = crate::health::AgentEmotion::Calm; // manually saved — no real-time emotion
    let kind = match error_kind_str.as_str() {
        "ToolParameter" => crate::health::ErrorKind::ToolParameter,
        "FileAccess" => crate::health::ErrorKind::FileAccess,
        "ExecFailure" => crate::health::ErrorKind::ExecFailure,
        "NetworkFailure" => crate::health::ErrorKind::NetworkFailure,
        "SudoFailure" => crate::health::ErrorKind::SudoFailure,
        "Timeout" => crate::health::ErrorKind::Timeout,
        "Panic" => crate::health::ErrorKind::Panic,
        "SessionMissing" => crate::health::ErrorKind::SessionMissing,
        "ToolNotFound" => crate::health::ErrorKind::ToolNotFound,
        _ => crate::health::ErrorKind::Unknown,
    };

    let mut guide = crate::session::load_pitfalls();
    guide.upsert(emotion, kind, &tool, &description, &lesson, &[]);
    crate::session::save_pitfalls(&guide);

    format!("[OK] Pitfall saved: {}\nTotal pitfalls: {}", description.chars().take(100).collect::<String>(), guide.entries.len())
}

#[allow(dead_code)]
pub(super) fn pitfall_guide_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: crate::types::ToolFunction {
            name: "pitfall_guide".into(),
            description: "Query cross-session error lessons. Filter by error_kind or tool.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "tool": {"type": "string", "description": "Filter by tool name"},
                    "error_kind": {"type": "string", "enum": ["ToolParameter", "FileAccess", "ExecFailure", "NetworkFailure", "SudoFailure", "Timeout", "Panic", "SessionMissing", "ToolNotFound", "Unknown"], "description": "Filter by error category"}
                },
                "required": [],
                "additionalProperties": false
            }),
        },
    }
}

pub(super) fn exec_pitfall_guide(args: &str) -> String {
    let filter_tool = parse_opt(args, "tool").unwrap_or_default();
    let filter_kind = parse_opt(args, "error_kind").unwrap_or_default();

    let guide = crate::session::load_pitfalls();
    let entries: Vec<&crate::memory::PitfallEntry> = guide.entries.iter()
        .filter(|e| {
            (filter_tool.is_empty() || e.tool == filter_tool)
            && (filter_kind.is_empty() || e.error_kind == filter_kind)
        })
        .collect();

    if entries.is_empty() {
        return format!("[OK] No pitfalls recorded yet{}. Use pitfall_save to record lessons from errors.",
            if filter_tool.is_empty() && filter_kind.is_empty() { "" } else { " matching your filter" });
    }

    let mut result = format!("[OK] Pitfall guide ({} entries):\n", entries.len());
    for e in entries.iter().take(15) {
        result.push_str(&format!(
            "- {}: {} — {}\n",
            e.error_kind, e.description, e.lesson
        ));
    }
    result
}
