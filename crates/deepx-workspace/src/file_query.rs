//! Query tools: file read, diff.

use super::file_shared::{content_hash, is_binary_read_error, unified_diff};
use crate::{JsonArgs, ToolCallCtx, ToolHandler, ToolResult, ToolRisk, handler};

// ------ exec_read_file (from file_read.rs) ------

pub(super) fn exec_read_file(args: &serde_json::Value) -> ToolResult {
    let requests = args
        .get("requests")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_else(|| vec![args.clone()]);
    if requests.is_empty() || requests.len() > 8 {
        return ToolResult::error_data(
            "INVALID_REQUEST_COUNT",
            "read accepts between 1 and 8 file requests",
            false,
            Some("Split the read into multiple calls.".into()),
            serde_json::json!({"max_requests": 8}),
        );
    }

    let mut outputs = Vec::with_capacity(requests.len());
    let mut metadata = Vec::with_capacity(requests.len());
    let mut total_chars = 0usize;
    for request in requests {
        let (result, meta) = read_one(&request);
        if !result.is_success() {
            return result;
        }
        let text = result.model_text().to_string();
        total_chars += text.chars().count();
        if total_chars > 48_000 {
            return ToolResult::error_data(
                "RANGE_TOO_LARGE",
                "combined read result exceeds the 12k-token lap budget",
                false,
                Some("Read fewer files or split the requests.".into()),
                serde_json::json!({"max_tokens": 12_000}),
            );
        }
        outputs.push(text);
        metadata.push(meta);
    }
    let text = outputs.join("\n\n---\n\n");
    ToolResult::ok_data(serde_json::json!({"files": metadata}), text)
}

fn read_one(args: &serde_json::Value) -> (ToolResult, serde_json::Value) {
    const MAX_LINES: usize = 400;
    const MAX_MODEL_CHARS: usize = 24_000;
    let path = crate::resolve_workspace_path(
        args.get("path")
            .and_then(|value| value.as_str())
            .unwrap_or_default(),
    );
    if path.is_empty() {
        return (
            ToolResult::error("read: path is required"),
            serde_json::json!({}),
        );
    }
    let workspace = crate::CURRENT_WORKSPACE
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    if let Some(skill) = deepx_skills::managed_skill_for_path(
        std::path::Path::new(&workspace),
        std::path::Path::new(&path),
    ) {
        return (
            ToolResult::error_data(
                "USE_SKILLS_TOOL",
                format!("'{path}' is managed by skill '{skill}'"),
                false,
                Some("Use skills(action=activate|resource, name=...) instead.".into()),
                serde_json::json!({"path": path}),
            ),
            serde_json::json!({}),
        );
    }
    if std::path::Path::new(&path).is_dir() {
        return (
            ToolResult::error_data(
                "IS_DIRECTORY",
                format!("'{path}' is a directory"),
                false,
                Some("Use exec with argv [\"rg\", \"--files\"] (or [\"ls\", \"-la\"] / [\"cmd\", \"/c\", \"dir\", \"/b\"]) to list directory contents.".into()),
                serde_json::json!({"path": path}),
            ),
            serde_json::json!({}),
        );
    }

    let start = args
        .get("start_line")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);
    let end = args
        .get("end_line")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);
    if start.is_some_and(|value| value == 0) || end.is_some_and(|value| value == 0) {
        return (
            ToolResult::error("read line numbers start at 1"),
            serde_json::json!({}),
        );
    }
    if let (Some(start), Some(end)) = (start, end) {
        if end < start {
            return (
                ToolResult::error("end_line must be greater than or equal to start_line"),
                serde_json::json!({}),
            );
        }
        if end - start + 1 > MAX_LINES {
            return (
                ToolResult::error_data(
                    "RANGE_TOO_LARGE",
                    format!("requested range exceeds {MAX_LINES} lines"),
                    false,
                    Some("Use smaller contiguous ranges.".into()),
                    serde_json::json!({"max_lines": MAX_LINES}),
                ),
                serde_json::json!({}),
            );
        }
    }

    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if is_binary_read_error(&error.to_string()) => {
            return (
                ToolResult::error_data(
                    "BINARY_FILE",
                    format!("'{path}' is binary and cannot be read as text"),
                    false,
                    Some("Use exec for a binary-aware inspection.".into()),
                    serde_json::json!({"path": path}),
                ),
                serde_json::json!({}),
            );
        }
        Err(error) => {
            return (
                ToolResult::error_data(
                    "NOT_FOUND",
                    format!("cannot read '{path}': {error}"),
                    false,
                    Some("Verify the path, then retry read.".into()),
                    serde_json::json!({"path": path}),
                ),
                serde_json::json!({}),
            );
        }
    };
    let content = raw.replace("\r\n", "\n").replace('\r', "\n");
    let hash = content_hash(&content);
    if args.get("if_hash").and_then(|v| v.as_str()) == Some(hash.as_str()) {
        let meta = serde_json::json!({"path": path, "not_modified": true, "hash": hash});
        return (ToolResult::ok_data(meta.clone(), "not modified"), meta);
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    let total_lines = lines.len();
    let explicit = start.is_some() || end.is_some();
    let first = start.unwrap_or(1).saturating_sub(1);
    if first > total_lines || end.is_some_and(|value| value > total_lines && explicit) {
        return (
            ToolResult::error_data(
                "LINE_OUT_OF_RANGE",
                format!("requested lines are outside '{path}' ({total_lines} total lines)"),
                false,
                Some("Use the total_lines value and retry.".into()),
                serde_json::json!({"path": path, "total_lines": total_lines, "hash": hash}),
            ),
            serde_json::json!({}),
        );
    }
    let requested_end = end.unwrap_or(total_lines).min(total_lines);
    let mut end_index = requested_end;
    if explicit && end_index.saturating_sub(first) > MAX_MODEL_CHARS / 40 {
        return (
            ToolResult::error_data(
                "RANGE_TOO_LARGE",
                "requested range exceeds the model output budget",
                false,
                Some("Split the range into smaller contiguous reads.".into()),
                serde_json::json!({"path": path, "max_chars": MAX_MODEL_CHARS}),
            ),
            serde_json::json!({}),
        );
    }
    if !explicit {
        let full_chars = lines
            .iter()
            .map(|line| line.chars().count() + 8)
            .sum::<usize>();
        if total_lines > MAX_LINES || full_chars > MAX_MODEL_CHARS {
            end_index = first;
            while end_index < total_lines {
                let next = end_index + 1;
                let chars = lines[first..next]
                    .iter()
                    .map(|line| line.chars().count() + 8)
                    .sum::<usize>();
                if chars > MAX_MODEL_CHARS {
                    break;
                }
                end_index = next;
            }
        }
    }
    let body = lines[first..end_index]
        .iter()
        .enumerate()
        .map(|(offset, line)| format!("L{}: {line}", first + offset + 1))
        .collect::<Vec<_>>()
        .join("\n");
    if explicit && body.chars().count() > MAX_MODEL_CHARS {
        return (
            ToolResult::error_data(
                "RANGE_TOO_LARGE",
                "requested range exceeds the model output budget",
                false,
                Some("Split the range into smaller contiguous reads.".into()),
                serde_json::json!({"path": path, "max_chars": MAX_MODEL_CHARS}),
            ),
            serde_json::json!({}),
        );
    }
    let truncated = end_index < total_lines;
    let mut meta = serde_json::json!({
        "path": path,
        "start_line": first + 1,
        "end_line": end_index,
        "total_lines": total_lines,
        "hash": hash,
        "truncated": truncated,
    });
    if truncated {
        let mut continuation = args.clone();
        continuation["start_line"] = serde_json::json!(end_index + 1);
        continuation["end_line"] = serde_json::Value::Null;
        meta["continuation"] = continuation;
    }
    // 任何 read（全文件或范围）都建立账本基线：模型后续用 start_line 盲定位时，
    // 工具凭账本自动防漂移，无需模型手动回传 hash。
    crate::file_state::record_read(&path, &content, total_lines);
    (ToolResult::ok_data(meta.clone(), body), meta)
}

handler!(handle_read_file, exec_read_file);

// ------ exec_diff (from file_diff.rs) ------

pub(super) fn exec_diff(args: &serde_json::Value) -> ToolResult {
    let path_a = crate::resolve_workspace_path(&args.s("path_a"));
    let path_b = crate::resolve_workspace_path(&args.s("path_b"));

    let content_a = match std::fs::read_to_string(&path_a) {
        Ok(c) => c,
        Err(e) => {
            return ToolResult::error(crate::json_err(
                "READ_FAILED",
                &format!("Cannot read {}: {}", path_a, e),
                "Verify the file exists. Use exec with argv [\"ls\", \"-la\"] to check.",
            ));
        }
    };
    let content_b = match std::fs::read_to_string(&path_b) {
        Ok(c) => c,
        Err(e) => {
            return ToolResult::error(crate::json_err(
                "READ_FAILED",
                &format!("Cannot read {}: {}", path_b, e),
                "Verify the file exists. Use exec with argv [\"ls\", \"-la\"] to check.",
            ));
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
    mgr.register_with_placement(ToolHandler {
        key: "read_file".to_string(),
        description: "Read up to eight files as precise contiguous ranges. Every returned line has a stable L<number> prefix, and each file includes its hash, total line count, and a directly executable continuation when the model budget is insufficient. Directories are rejected with IS_DIRECTORY; list directory contents with exec (e.g. argv [\"rg\", \"--files\"]).",
        input_schema: serde_json::json!({
            "type":"object",
            "properties": {
                "requests": {
                    "type":"array", "maxItems":8,
                    "items": {"type":"object", "properties": {
                        "path":{"type":"string"}, "start_line":{"type":"integer","minimum":1},
                        "end_line":{"type":"integer","minimum":1}, "if_hash":{"type":"string"}
                    }, "required":["path"], "additionalProperties":false}
                },
                "path":{"type":"string"}, "start_line":{"type":"integer","minimum":1},
                "end_line":{"type":"integer","minimum":1}, "if_hash":{"type":"string"}
            },
            "oneOf":[{"required":["requests"]},{"required":["path"]}],
            "additionalProperties":false
        }),
        handler: handle_read_file,
        risk: ToolRisk::ReadOnly,
        default_timeout: std::time::Duration::from_secs(15),
    },
    crate::ToolPlacement::Workspace,
);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_directory_is_an_explicit_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.path().join("b.md"), "# hi\n").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();

        let result = exec_read_file(&serde_json::json!({
            "path": dir.path().to_string_lossy(),
        }));

        assert!(!result.is_success());
        assert_eq!(result.error.as_ref().unwrap().code, "IS_DIRECTORY");
    }

    #[test]
    fn read_range_is_contiguous_and_numbered() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.txt"), "one\ntwo\nthree\n").unwrap();

        let result = exec_read_file(&serde_json::json!({
            "path": dir.path().join("x.txt").to_string_lossy(),
            "start_line": 2,
            "end_line": 3,
        }));

        assert!(
            result.is_success(),
            "range read should succeed: {}",
            result.model_text()
        );
        assert_eq!(result.model_text(), "L2: two\nL3: three");
        assert_eq!(result.data["files"][0]["start_line"], 2);
        assert_eq!(result.data["files"][0]["end_line"], 3);
        // 防呆闭环：响应必须带 hash（LF 视图 content_hash），供 edit_file 的 expected_hash 校验
        let hash = result.data["files"][0]["hash"]
            .as_str()
            .expect("read_file must return hash");
        assert_eq!(hash, crate::file_shared::content_hash("one\ntwo\nthree\n"));
    }
}
