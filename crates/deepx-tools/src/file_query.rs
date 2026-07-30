//! Query tools: file read, search, diff.

use std::process::Command;

use super::file_shared::{content_hash, is_binary_read_error, rust_grep, unified_diff};
use crate::{JsonArgs, ToolCallCtx, ToolHandler, ToolResult, ToolRisk, handler};

// ------ exec_read_file (from file_read.rs) ------

pub(super) fn exec_read_file(args: &serde_json::Value) -> ToolResult {
    // ------ Batch mode: read multiple files ------
    if let Some(paths) = args.get("paths").and_then(|v| v.as_array()) {
        let mut results = Vec::new();
        for p in paths {
            if let Some(pstr) = p.as_str() {
                let mut per = serde_json::json!({"path": pstr});
                if let Some(s) = args.get("start_line") {
                    per["start_line"] = s.clone();
                }
                if let Some(e) = args.get("end_line") {
                    per["end_line"] = e.clone();
                }
                results.push(exec_read_file(&per).content);
            }
        }
        return ToolResult::ok(format!("[{} files]\n\n{}", paths.len(), results.join("\n\n---\n\n")));
    }

    // ------ Single file mode ------
    let path = crate::resolve_workspace_path(&args.s("path"));
    let workspace = crate::CURRENT_WORKSPACE
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    if let Some(skill) = deepx_skills::managed_skill_for_path(
        std::path::Path::new(&workspace),
        std::path::Path::new(&path),
    ) {
        return ToolResult { success: false, content: crate::json_err(
            "USE_SKILLS_TOOL",
            format!("'{path}' is managed by skill '{skill}'"),
            "Use skills(action=activate|resource, name=...) instead of generic read.",
        ) };
    }
    let start: Option<usize> = args
        .get("start_line")
        .and_then(|v| v.as_u64())
        .map(|n| (n as usize).max(1));
    let end: Option<usize> = args
        .get("end_line")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);

    const MAX_READ_LINES: usize = 300;
    if let (Some(s), Some(e)) = (start, end) {
        if e >= s && e - s + 1 > MAX_READ_LINES {
            return ToolResult { success: false, content: serde_json::json!({
                "timeis": crate::now_utc8(),
                "status": "error",
                "path": path,
                "code": "RANGE_TOO_LARGE",
                "message": format!("Requested range too large ({} lines > {} max)", e - s, MAX_READ_LINES),
                "hint": "Use smaller range."
            }).to_string() };
        }
    }
    match std::fs::read_to_string(&path) {
        Ok(raw) => {
            let content = raw.replace("\r\n", "\n").replace('\r', "\n");

            // ------ Cache check: return "unchanged" if content matches previous read ------
            if start.is_none() && end.is_none() {
                if let Some(cached) = crate::file_cache::check(&path, &content) {
                    return ToolResult::ok(cached);
                }
            }

            let all_lines: Vec<&str> = content.lines().collect();
            let total = all_lines.len();
            let start_idx = start.map(|s| (s - 1).min(total)).unwrap_or(0);
            let end_idx = end.map(|e| e.min(total)).unwrap_or(total);
            let start_idx = start_idx.min(end_idx);
            let shown = end_idx - start_idx;
            let truncated = start.is_some() || end.is_some() || total > 200;

            let body: String = if total <= 200 && start.is_none() && end.is_none() {
                // Small file, full output ---no line numbers in body
                all_lines.join("\n")
            } else if start.is_some() || end.is_some() {
                // Range read
                all_lines[start_idx..end_idx].join("\n")
            } else {
                // Large file: head + tail, no line numbers
                let head = 50.min(total);
                let tail = 30.min(total - head);
                let mut s = all_lines[..head].join("\n");
                if total > head + tail {
                    s.push_str(&format!(
                        "\n[truncated: {} lines omitted. Call read again with start_line/end_line for the omitted range.]",
                        total - head - tail
                    ));
                }
                if tail > 0 {
                    s.push('\n');
                    s.push_str(&all_lines[total - tail..].join("\n"));
                }
                s
            };

            // ------ Cache: store full-file reads for future deduplication ------
            if start.is_none() && end.is_none() {
                crate::file_state::record_read(&path, &content, total);
            }

            ToolResult::ok(serde_json::json!({
                "timeis": crate::now_utc8(),
                "status": "ok",
                "path": path,
                "start_line": start_idx + 1,
                "end_line": start_idx + shown,
                "total_lines": total,
                "hash": content_hash(&content),
                "truncated": truncated,
                "content": body,
            })
            .to_string())
        }
        Err(e) => {
            if is_binary_read_error(&e.to_string()) {
                let meta = std::fs::metadata(&path);
                let size = meta
                    .as_ref()
                    .map(|m| format!("{}", m.len()))
                    .unwrap_or_default();
                ToolResult { success: false, content: serde_json::json!({
                    "timeis": crate::now_utc8(),
                    "status": "error",
                    "path": path,
                    "code": "BINARY_FILE",
                    "message": format!("Binary file ({}B), cannot display as text", size),
                    "hint": format!("Use exec with argv [\"file\", \"{}\"] to identify format, or [\"xxd\", \"{}\"] for a hex dump.", path, path),
                }).to_string() }
            } else {
                let url_hint = if path.contains("://")
                    || path.contains(".com")
                    || path.contains("www.")
                {
                    "\n[HINT] This looks like a URL — did you mean to call web with {\"url\": ...} instead?"
                } else {
                    ""
                };
                ToolResult { success: false, content: serde_json::json!({
                    "timeis": crate::now_utc8(),
                    "status": "error",
                    "path": path,
                    "code": "NOT_FOUND",
                    "message": e.to_string(),
                    "hint": format!("Use list on the parent directory to verify the file exists.{url_hint}"),
                }).to_string() }
            }
        }
    }
}

handler!(handle_read_file, exec_read_file);

// ------ exec_search ------

// ------ exec_search (from file_search.rs) ------

pub(super) fn exec_search(args: &serde_json::Value) -> ToolResult {
    let pattern = args.s("pattern");
    let glob = args.get("glob").and_then(|v| v.as_str()).map(String::from);
    let dir = crate::resolve_workspace_path(&args.s_or("path", "."));
    let workspace = crate::CURRENT_WORKSPACE
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    if let Some(skill) = deepx_skills::managed_skill_for_path(
        std::path::Path::new(&workspace),
        std::path::Path::new(&dir),
    ) {
        return ToolResult { success: false, content: crate::json_err(
            "USE_SKILLS_TOOL",
            format!("search target is managed by skill '{skill}'"),
            "Use skills(action=activate|resource, name=...) instead of generic search.",
        ) };
    }

    // Phase 1: try ripgrep (cross-platform, fast)
    let mut cmd = Command::new("rg");
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.arg("-n").arg("--no-heading");
    for excluded in ["!.deepx/skills/**", "!.agents/skills/**", "!skills/**"] {
        cmd.arg("-g").arg(excluded);
    }
    if let Some(ref g) = glob {
        cmd.arg("-g").arg(g);
    }
    cmd.arg(&pattern).arg(&dir);

    match cmd.output() {
        Ok(o) if o.status.success() => {
            let out = String::from_utf8_lossy(&o.stdout);
            let all_lines: Vec<&str> = out.lines().collect();
            let lines: Vec<&str> = all_lines.iter().take(100).copied().collect();
            if lines.is_empty() {
                return ToolResult::ok(crate::json_ok(
                    serde_json::json!({"pattern": pattern, "content": format!("No matches for '{}'", pattern)}),
                ));
            }
            let truncated = if all_lines.len() > 100 {
                format!(
                    "\n... [truncated: {} more matches. Call search again with a narrower pattern, glob, or path.]",
                    all_lines.len() - 100
                )
            } else {
                String::new()
            };
            return ToolResult::ok(crate::json_ok(
                serde_json::json!({"pattern": pattern, "matches": all_lines.len(), "content": format!("{}", lines.join("\n")) + &truncated}),
            ));
        }
        _ => {} // rg not installed or errored --?fall through to pure Rust
    }

    // Phase 2: pure Rust fallback
    match rust_grep(&pattern, &dir, true, true, glob.as_deref(), 100) {
        Ok(lines) => {
            if lines.is_empty() {
                ToolResult::ok(crate::json_ok(
                    serde_json::json!({"pattern": pattern, "content": format!("No matches for '{}'", pattern)}),
                ))
            } else {
                let result: Vec<&str> = lines.iter().take(100).map(|s| s.as_str()).collect();
                let truncated = if lines.len() > 100 {
                    format!(
                        "\n... [truncated: {} more matches. Call search again with a narrower pattern, glob, or path.]",
                        lines.len() - 100
                    )
                } else {
                    String::new()
                };
                ToolResult::ok(crate::json_ok(
                    serde_json::json!({"pattern": pattern, "matches": lines.len(), "content": format!("{}", result.join("\n")) + &truncated}),
                ))
            }
        }
        Err(e) => ToolResult { success: false, content: crate::json_err(
            "SEARCH_FAILED",
            &format!("search failed: {}", e),
            "Check the pattern or path.",
        ) },
    }
}

handler!(handle_search, exec_search);

// ------ exec_diff (from file_diff.rs) ------

pub(super) fn exec_diff(args: &serde_json::Value) -> ToolResult {
    let path_a = crate::resolve_workspace_path(&args.s("path_a"));
    let path_b = crate::resolve_workspace_path(&args.s("path_b"));

    let content_a = match std::fs::read_to_string(&path_a) {
        Ok(c) => c,
        Err(e) => {
            return ToolResult { success: false, content: crate::json_err(
                "READ_FAILED",
                &format!("Cannot read {}: {}", path_a, e),
                "Verify the file exists. Use list to check.",
            ) };
        }
    };
    let content_b = match std::fs::read_to_string(&path_b) {
        Ok(c) => c,
        Err(e) => {
            return ToolResult { success: false, content: crate::json_err(
                "READ_FAILED",
                &format!("Cannot read {}: {}", path_b, e),
                "Verify the file exists. Use list to check.",
            ) };
        }
    };

    if content_a == content_b {
        return ToolResult::ok(crate::json_ok(
            serde_json::json!({"path_a": path_a, "path_b": path_b, "identical": true, "content": "Files are identical"}),
        ));
    }

    ToolResult::ok(crate::json_ok(
        serde_json::json!({"path_a": path_a, "path_b": path_b, "identical": false, "content": unified_diff(&content_a, &content_b, &path_a)}),
    ))
}

handler!(handle_diff, exec_diff);

// ------ Registration ------

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register(ToolHandler {
        key: "read".to_string(),
        description: "Read one or more files. Use path for one file or paths for a batch. Use list for directories and start_line/end_line for a range. Returns a content hash; pass it as expected_hash to edit/write to prevent stale writes. Full files auto-truncate to head 50 + tail 30 lines (>200 lines); when truncated, call read again with a smaller range.",
        input_schema: serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"One file path, not a directory. Relative to workspace or absolute."},"paths":{"type":"array","items":{"type":"string"},"description":"Multiple file paths; cannot be combined with path."},"start_line":{"type":"integer","description":"First line to read (1-based, optional)"},"end_line":{"type":"integer","description":"Last line to read, inclusive (optional). Max range: 300 lines."}},"anyOf":[{"required":["path"]},{"required":["paths"]}],"additionalProperties":false}),
        handler: handle_read_file,
        risk: ToolRisk::ReadOnly,
        default_timeout: std::time::Duration::from_secs(15),
    });
    mgr.register(ToolHandler {
        key: "search".to_string(),
        description: "Regex search across files. Returns file:line matches.",
        input_schema: serde_json::json!({"type":"object","properties":{"pattern":{"type":"string","description":"Regex pattern"},"glob":{"type":"string","description":"File glob filter (e.g. *.rs)"},"path":{"type":"string","description":"Search directory","default":"."}},"required":["pattern"],"additionalProperties":false}),
        handler: handle_search,
        risk: ToolRisk::ReadOnly,
        default_timeout: std::time::Duration::from_secs(30),
    });
    mgr.register(ToolHandler {
        key: "diff".to_string(),
        description: "Compare two files line by line.",
        input_schema: serde_json::json!({"type":"object","properties":{"path_a":{"type":"string","description":"First file path"},"path_b":{"type":"string","description":"Second file path"}},"required":["path_a","path_b"],"additionalProperties":false}),
        handler: handle_diff,
        risk: ToolRisk::ReadOnly,
        default_timeout: std::time::Duration::from_secs(30),
    });
}
