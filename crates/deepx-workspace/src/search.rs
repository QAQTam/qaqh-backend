//! Bounded structured workspace search.
//!
//! Search is deliberately separate from `exec`: callers receive stable
//! path/line/column records and a continuation instead of shell-specific
//! output that another layer has to parse.

use crate::{ToolCallCtx, ToolHandler, ToolResult, ToolRisk};
use std::path::{Path, PathBuf};

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;
const MAX_MATCH_CHARS: usize = 16_000;
const MAX_FILES_VISITED: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchKind {
    Text,
    Files,
    Symbol,
}

fn handle_search(ctx: ToolCallCtx) -> ToolResult {
    let kind = match ctx.args.get("kind").and_then(|v| v.as_str()) {
        Some("text") => SearchKind::Text,
        Some("files") => SearchKind::Files,
        Some("symbol") => SearchKind::Symbol,
        _ => {
            return ToolResult::error_data(
                "INVALID_KIND",
                "search.kind must be text, files, or symbol",
                false,
                Some("Choose one of the three structured search modes.".into()),
                serde_json::json!({"kinds": ["text", "files", "symbol"]}),
            )
        }
    };
    let query = ctx
        .args
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if kind != SearchKind::Files && query.is_empty() {
        return ToolResult::error("search.query is required for text and symbol search");
    }
    let limit = ctx
        .args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT);
    let cursor = ctx
        .args
        .get("cursor")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let roots = roots(&ctx.args);
    let mut files = Vec::new();
    let mut visited = 0usize;
    for root in roots {
        collect_files(&root, &mut files, &mut visited);
        if visited >= MAX_FILES_VISITED {
            break;
        }
    }
    files.sort();
    files.dedup();

    let mut records = Vec::new();
    match kind {
        SearchKind::Files => {
            for path in files.iter().skip(cursor).take(limit) {
                records.push(serde_json::json!({"path": display(path)}));
            }
        }
        SearchKind::Text | SearchKind::Symbol => {
            for path in files.iter().skip(cursor) {
                let Ok(content) = std::fs::read_to_string(path) else { continue };
                for (line_index, line) in content.replace("\r\n", "\n").split('\n').enumerate() {
                    let Some(column) = line.find(query) else { continue };
                    records.push(serde_json::json!({
                        "path": display(path),
                        "line": line_index + 1,
                        "column": line[..column].chars().count() + 1,
                        "preview": line.trim().chars().take(240).collect::<String>(),
                    }));
                    if records.len() >= cursor + limit {
                        break;
                    }
                }
                if records.len() >= cursor + limit {
                    break;
                }
            }
            if cursor > 0 {
                records = records.into_iter().skip(cursor).collect();
            }
        }
    }

    let returned = records.len().min(limit);
    records.truncate(returned);
    let next_cursor = cursor.saturating_add(returned);
    let mut data = serde_json::json!({
        "kind": match kind { SearchKind::Text => "text", SearchKind::Files => "files", SearchKind::Symbol => "symbol" },
        "query": query,
        "matches": records,
        "returned": returned,
    });
    let has_more = match kind {
        SearchKind::Files => next_cursor < files.len(),
        SearchKind::Text | SearchKind::Symbol => returned == limit,
    };
    let text = data["matches"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    if kind == SearchKind::Files {
                        item["path"].as_str().unwrap_or_default().to_string()
                    } else {
                        format!(
                            "{}:{}:{} {}",
                            item["path"].as_str().unwrap_or_default(),
                            item["line"].as_u64().unwrap_or_default(),
                            item["column"].as_u64().unwrap_or_default(),
                            item["preview"].as_str().unwrap_or_default()
                        )
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let text = if text.chars().count() > MAX_MATCH_CHARS {
        text.chars().take(MAX_MATCH_CHARS).collect()
    } else {
        text
    };
    if has_more {
        let mut continuation = ctx.args.clone();
        continuation["cursor"] = serde_json::json!(next_cursor);
        data["continuation"] = continuation;
    }
    ToolResult::ok_data(data, text)
}

fn roots(args: &serde_json::Value) -> Vec<PathBuf> {
    let workspace = crate::CURRENT_WORKSPACE
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let default_root = if workspace.is_empty() { "." } else { &workspace };
    match args.get("paths").and_then(|v| v.as_array()) {
        Some(paths) if !paths.is_empty() => paths
            .iter()
            .filter_map(|value| value.as_str())
            .map(crate::resolve_workspace_path)
            .map(PathBuf::from)
            .collect(),
        _ => vec![PathBuf::from(default_root)],
    }
}

fn collect_files(path: &Path, output: &mut Vec<PathBuf>, visited: &mut usize) {
    if *visited >= MAX_FILES_VISITED || path.to_string_lossy().contains("\\.git\\") {
        return;
    }
    *visited += 1;
    let Ok(metadata) = std::fs::metadata(path) else { return };
    if metadata.is_file() {
        output.push(path.to_path_buf());
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else { return };
    for entry in entries.flatten() {
        collect_files(&entry.path(), output, visited);
        if *visited >= MAX_FILES_VISITED {
            break;
        }
    }
}

fn display(path: &Path) -> String {
    crate::display_path(&path.to_string_lossy())
}

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register(ToolHandler {
        key: "search".into(),
        description: "Search the workspace with structured text, symbol, or file queries. Returns bounded path/line/column matches and a continuation cursor.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {"type": "string", "enum": ["text", "symbol", "files"]},
                "query": {"type": "string", "description": "Literal text or symbol query; omitted for files."},
                "paths": {"type": "array", "items": {"type": "string"}},
                "limit": {"type": "integer", "minimum": 1, "maximum": 200},
                "cursor": {"type": "integer", "minimum": 0}
            },
            "required": ["kind"],
            "additionalProperties": false
        }),
        handler: handle_search,
        risk: ToolRisk::ReadOnly,
        default_timeout: std::time::Duration::from_secs(30),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_schema_has_bounded_modes() {
        let mut manager = crate::ToolManager::new();
        register(&mut manager);
        let definition = manager.lookup("search").unwrap().to_tool_def();
        assert_eq!(definition.function.parameters["required"][0], "kind");
        assert_eq!(definition.function.parameters["properties"]["kind"]["enum"][2], "files");
    }
}
