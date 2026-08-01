//! Query tools: file read, diff.

use super::file_shared::{content_hash, is_binary_read_error, unified_diff};
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

    // ── Directory fallback: reading a directory lists its entries instead of
    // erroring (start_line/end_line are meaningless for a directory). There is
    // no separate `list` tool, so this is the only way the model can browse
    // the workspace without exec ls.
    if let Ok(mut entries) = std::fs::read_dir(&path) {
            const MAX_ENTRIES: usize = 200;
            let mut lines: Vec<String> = Vec::new();
            let mut count = 0usize;
            let mut skipped = 0usize;
            let mut total = 0usize;
            while let Some(entry) = entries.next() {
                total += 1;
                if lines.len() >= MAX_ENTRIES {
                    skipped = total - MAX_ENTRIES;
                    break;
                }
                match entry {
                    Ok(e) => {
                        let name = e.file_name().to_string_lossy().to_string();
                        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                        let size = e.metadata().ok().map(|m| m.len());
                        let size_s = size
                            .map(|b| {
                                if b >= 1024 * 1024 {
                                    format!("{:.1}M", b as f64 / 1048576.0)
                                } else if b >= 1024 {
                                    format!("{:.1}K", b as f64 / 1024.0)
                                } else {
                                    format!("{b}B")
                                }
                            })
                            .unwrap_or_else(|| "?".to_string());
                        lines.push(format!("{}{}  {size_s}", if is_dir { "📁 " } else { "   " }, name));
                        count += 1;
                    }
                    Err(_) => count += 0,
                }
            }
            let mut text = lines.join("\n");
            if skipped > 0 {
                text.push_str(&format!("\n... [{skipped} more entries omitted]"));
            }
            return ToolResult::ok(serde_json::json!({
                "timeis": crate::now_utc8(),
                "status": "ok",
                "path": path,
                "is_dir": true,
                "entry_count": total,
                "shown_entries": count,
                "content": text,
            })
            .to_string());
    }

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
                    "hint": format!("Use exec with argv [\"ls\", \"-la\"] on the parent directory to verify the file exists.{url_hint}"),
                }).to_string() }
            }
        }
    }
}

handler!(handle_read_file, exec_read_file);

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
                "Verify the file exists. Use exec with argv [\"ls\", \"-la\"] to check.",
            ) };
        }
    };
    let content_b = match std::fs::read_to_string(&path_b) {
        Ok(c) => c,
        Err(e) => {
            return ToolResult { success: false, content: crate::json_err(
                "READ_FAILED",
                &format!("Cannot read {}: {}", path_b, e),
                "Verify the file exists. Use exec with argv [\"ls\", \"-la\"] to check.",
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
        description: "Read one or more files. Use path for one file or paths for a batch. Passing a directory lists its entries; use start_line/end_line for a range. Returns a content hash; pass it as expected_hash to edit/write to prevent stale writes. Full files auto-truncate to head 50 + tail 30 lines (>200 lines); when truncated, call read again with a smaller range.",
        input_schema: serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"One file path, not a directory. Relative to workspace or absolute."},"paths":{"type":"array","items":{"type":"string"},"description":"Multiple file paths; cannot be combined with path."},"start_line":{"type":"integer","description":"First line to read (1-based, optional)"},"end_line":{"type":"integer","description":"Last line to read, inclusive (optional). Max range: 300 lines."}},"anyOf":[{"required":["path"]},{"required":["paths"]}],"additionalProperties":false}),
        handler: handle_read_file,
        risk: ToolRisk::ReadOnly,
        default_timeout: std::time::Duration::from_secs(15),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_directory_lists_entries_instead_of_erroring() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.path().join("b.md"), "# hi\n").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();

        let result = exec_read_file(&serde_json::json!({
            "path": dir.path().to_string_lossy(),
        }));

        assert!(result.success, "directory read should succeed: {}", result.content);
        let v: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(v["is_dir"], true);
        assert_eq!(v["entry_count"], 3);
        let text = v["content"].as_str().unwrap();
        assert!(text.contains("a.rs"), "entries should list a.rs: {text}");
        assert!(text.contains("b.md"), "entries should list b.md: {text}");
        assert!(text.contains("sub"), "entries should list sub: {text}");
        assert!(text.contains('📁'), "directories should be marked: {text}");
    }

    #[test]
    fn read_directory_with_range_still_lists_entries() {
        // start_line/end_line are irrelevant for a directory — the fallback
        // still applies (range reads of a directory make no sense).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.txt"), "x\n").unwrap();

        let result = exec_read_file(&serde_json::json!({
            "path": dir.path().to_string_lossy(),
            "start_line": 1,
            "end_line": 5,
        }));

        assert!(result.success, "directory read should succeed: {}", result.content);
        let v: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(v["is_dir"], true);
    }
}
