//! Query tools: file read, diff.

use std::path::{Path, PathBuf};
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::file_shared::{LineIndex, content_hash, is_binary_read_error, normalize_newlines};
use crate::ToolRisk;
use crate::tool_api::{
    OutputBudget, ToolBody, ToolCallContext, ToolContentBlock, ToolDescriptor, ToolDisplay,
    ToolError, ToolErrorCode, ToolErrorKind, ToolExecutionError, ToolExposure, ToolHeader,
    ToolName, ToolProjection, ToolSource, TypedTool,
};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadRequest {
    #[serde(default)]
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_hash: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadArgs {
    #[serde(default)]
    pub requests: Option<Vec<ReadRequest>>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub start_line: Option<u64>,
    #[serde(default)]
    pub end_line: Option<u64>,
    #[serde(default)]
    pub if_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReadFileMetadata {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_lines: Option<usize>,
    pub hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corrected: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_lines: Option<Vec<usize>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_offset: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_modified: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReadOutput {
    pub files: Vec<ReadFileMetadata>,
    #[serde(skip, default)]
    #[schemars(skip)]
    body: String,
}

impl ToolProjection for ReadOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: self.body.clone(),
        }]
    }

    fn summary(&self) -> Option<String> {
        self.body.lines().next().map(str::to_string)
    }

    fn display(&self, args: &Value) -> ToolDisplay {
        read_display(args, &self.body)
    }
}

struct ReadPart {
    metadata: ReadFileMetadata,
    body: String,
}

pub struct ReadTool;

impl TypedTool for ReadTool {
    type Args = ReadArgs;
    type Output = ReadOutput;

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::new("read").expect("valid read tool name"),
            display_name: None,
            description: "Read files (L-prefixed lines, hash+line_count). Up to 8 files; dirs -> IS_DIRECTORY."
                .to_string(),
            input_schema: read_schema(),
            output_schema: serde_json::to_value(schemars::schema_for!(ReadOutput))
                .expect("read output schema"),
            category: crate::permission::ToolCategory::Read,
            risk: ToolRisk::ReadOnly,
            default_timeout: Duration::from_secs(15),
            exposure: ToolExposure::Direct,
            source: ToolSource::Builtin,
            output_budget: OutputBudget::default(),
            capabilities: crate::tool_capabilities::builtin_capabilities("read")
                .unwrap_or_default(),
        }
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        let requests = args.requests.unwrap_or_else(|| {
            vec![ReadRequest {
                path: args.path.unwrap_or_default(),
                start_line: args.start_line,
                end_line: args.end_line,
                if_hash: args.if_hash,
            }]
        });
        if requests.is_empty() || requests.len() > 8 {
            return Err(read_error(
                "INVALID_REQUEST_COUNT",
                "read accepts between 1 and 8 file requests",
                Some("Split the read into multiple calls."),
                json!({"max_requests": 8}),
            ));
        }

        let mut files = Vec::with_capacity(requests.len());
        let mut bodies = Vec::with_capacity(requests.len());
        let mut total_chars = 0usize;
        for request in requests {
            let part = read_one(ctx, &request)?;
            total_chars += part.body.chars().count();
            if total_chars > 48_000 {
                return Err(read_error(
                    "RANGE_TOO_LARGE",
                    "combined read result exceeds the 12k-token lap budget",
                    Some("Read fewer files or split the requests."),
                    json!({"max_tokens": 12_000}),
                ));
            }
            files.push(part.metadata);
            bodies.push(part.body);
        }
        Ok(ReadOutput {
            files,
            body: bodies.join("\n\n---\n\n"),
        })
    }
}

#[allow(clippy::result_large_err)] // ToolExecutionError is the frozen typed boundary.
fn read_one(ctx: &ToolCallContext, request: &ReadRequest) -> Result<ReadPart, ToolExecutionError> {
    const MAX_LINES: usize = crate::file_shared::READ_MAX_LINES;
    const MAX_MODEL_CHARS: usize = crate::file_shared::READ_MAX_CHARS;

    let path = resolve_read_path(ctx, &request.path);
    if path.is_empty() {
        return Err(read_error(
            "TOOL_ERROR",
            "read: path is required",
            None,
            json!({}),
        ));
    }
    let workspace = ctx.workspace_root.to_string_lossy().to_string();
    if let Some(skill) =
        qaqh_skills::managed_skill_for_path(Path::new(&workspace), Path::new(&path))
    {
        return Err(read_error(
            "USE_SKILLS_TOOL",
            format!("'{path}' is managed by skill '{skill}'"),
            Some("Use skills(action=activate|resource, name=...) instead."),
            json!({"path": path}),
        ));
    }
    if Path::new(&path).is_dir() {
        return Err(read_error(
            "IS_DIRECTORY",
            format!("'{path}' is a directory"),
            Some(
                "Use exec command \"rg --files\" (or \"ls -la\" / \"dir /b\" on cmd) to list directory contents.",
            ),
            json!({"path": path}),
        ));
    }
    if let Err(guard) = crate::file_shared::ensure_readable_regular_file(&path) {
        let code = guard.code();
        let message = guard.message();
        let hint = guard.hint();
        return Err(read_error(code, message, hint.as_deref(), json!({})));
    }
    if let Ok(meta) = std::fs::metadata(&path)
        && meta.is_file()
        && meta.len() > crate::file_shared::READ_MAX_BYTES
    {
        return Err(read_error(
            "FILE_TOO_LARGE",
            format!(
                "'{path}' is {} bytes (read limit {} bytes)",
                meta.len(),
                crate::file_shared::READ_MAX_BYTES
            ),
            Some("Use exec (rg/sed/head) to inspect large files."),
            json!({
                "path": path,
                "size": meta.len(),
                "max_bytes": crate::file_shared::READ_MAX_BYTES
            }),
        ));
    }

    let mut start = request
        .start_line
        .map(|value| value as usize)
        .map(|value| if value == 0 { 1 } else { value });
    let mut end = request
        .end_line
        .map(|value| value as usize)
        .map(|value| if value == 0 { 1 } else { value });
    if let (Some(start), Some(end)) = (start, end) {
        if end < start {
            return Err(read_error(
                "TOOL_ERROR",
                "end_line must be greater than or equal to start_line",
                None,
                json!({}),
            ));
        }
        if end - start + 1 > MAX_LINES {
            return Err(read_error(
                "RANGE_TOO_LARGE",
                format!("requested range exceeds {MAX_LINES} lines"),
                Some("Use smaller contiguous ranges."),
                json!({"max_lines": MAX_LINES}),
            ));
        }
    }

    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if is_binary_read_error(&error.to_string()) => {
            return Err(read_error(
                "BINARY_FILE",
                format!("'{path}' is binary and cannot be read as text"),
                Some("Use exec for a binary-aware inspection."),
                json!({"path": path}),
            ));
        }
        Err(error) => {
            return Err(read_error(
                "NOT_FOUND",
                format!("cannot read '{path}': {error}"),
                Some("Verify the path, then retry read."),
                json!({"path": path}),
            ));
        }
    };
    let (content, _endings) = normalize_newlines(&raw);
    let hash = content_hash(&content);
    if request.if_hash.as_deref() == Some(hash.as_str()) {
        return Ok(ReadPart {
            metadata: ReadFileMetadata {
                path,
                start_line: None,
                end_line: None,
                total_lines: None,
                hash,
                truncated: None,
                continuation: None,
                corrected: None,
                original_lines: None,
                line_offset: None,
                not_modified: Some(true),
            },
            body: "not modified".to_string(),
        });
    }
    let index = LineIndex::new(&content);
    let lines = index.lines();
    let total_lines = index.line_count();

    let mut corrected: Option<(usize, usize)> = None;
    let mut offset: Option<i64> = None;
    if let (Some(s), Some(e)) = (start, end)
        && let (Some((s2, ds)), Some((e2, de))) = (
            crate::file_state::correct_line(&path, s),
            crate::file_state::correct_line(&path, e),
        )
        && (s2 != s || e2 != e)
        && ds == de
    {
        corrected = Some((s, e));
        offset = Some(ds);
        start = Some(s2);
        end = Some(e2);
    }

    let explicit = start.is_some() || end.is_some();
    let first = start.unwrap_or(1).saturating_sub(1);
    if first > total_lines {
        return Err(read_error(
            "LINE_OUT_OF_RANGE",
            format!("requested lines are outside '{path}' ({total_lines} total lines)"),
            Some("Use the total_lines value and retry."),
            json!({"path": path, "total_lines": total_lines, "hash": hash}),
        ));
    }
    let requested_end = end.unwrap_or(total_lines).min(total_lines);
    let mut end_index = requested_end;
    if explicit && end_index.saturating_sub(first) > MAX_MODEL_CHARS / 40 {
        return Err(read_error(
            "RANGE_TOO_LARGE",
            "requested range exceeds the model output budget",
            Some("Split the range into smaller contiguous reads."),
            json!({"path": path, "max_chars": MAX_MODEL_CHARS}),
        ));
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
        return Err(read_error(
            "RANGE_TOO_LARGE",
            "requested range exceeds the model output budget",
            Some("Split the range into smaller contiguous reads."),
            json!({"path": path, "max_chars": MAX_MODEL_CHARS}),
        ));
    }
    let truncated = end_index < total_lines;
    let mut metadata = ReadFileMetadata {
        path,
        start_line: Some(first + 1),
        end_line: Some(end_index),
        total_lines: Some(total_lines),
        hash,
        truncated: Some(truncated),
        continuation: None,
        corrected: None,
        original_lines: None,
        line_offset: None,
        not_modified: None,
    };
    if truncated {
        let mut continuation = serde_json::to_value(request).unwrap_or_else(|_| json!({}));
        continuation["start_line"] = json!(end_index + 1);
        continuation["end_line"] = Value::Null;
        metadata.continuation = Some(continuation);
    }
    if let (Some((os, oe)), Some(delta)) = (corrected, offset) {
        metadata.corrected = Some(true);
        metadata.original_lines = Some(vec![os, oe]);
        metadata.line_offset = Some(delta);
    }
    crate::file_state::record_read(&metadata.path, &content, total_lines);
    Ok(ReadPart { metadata, body })
}

fn resolve_read_path(ctx: &ToolCallContext, raw_path: &str) -> String {
    if raw_path.is_empty() {
        return String::new();
    }
    let path = Path::new(raw_path);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else if ctx.workspace_root.as_os_str().is_empty() {
        PathBuf::from(raw_path)
    } else {
        ctx.workspace_root.join(path)
    };
    crate::permission::normalize_lexically(&joined)
        .to_string_lossy()
        .to_string()
}

fn read_display(args: &Value, output: &str) -> ToolDisplay {
    let paths = if let Some(requests) = args.get("requests").and_then(Value::as_array) {
        let paths = requests
            .iter()
            .filter_map(|request| request.get("path").and_then(Value::as_str))
            .filter(|path| !path.trim().is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        match paths.as_slice() {
            [] => None,
            [path] => Some(path.clone()),
            _ => Some(paths.join(", ")),
        }
    } else {
        args.get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .map(str::to_string)
    };
    match paths {
        Some(path) => {
            let request = args
                .get("requests")
                .and_then(Value::as_array)
                .and_then(|requests| requests.first())
                .unwrap_or(args);
            let start = request
                .get("start_line")
                .and_then(Value::as_u64)
                .unwrap_or(1);
            let summary = match request.get("end_line").and_then(Value::as_u64) {
                Some(end) => format!("{path} · L{start}-L{end}"),
                None => format!("{path} · L{start}+"),
            };
            let mut display = ToolDisplay::new(
                ToolHeader::Path {
                    path,
                    op: crate::tool_api::PathOp::Read,
                },
                ToolBody::Text {
                    text: output.to_string(),
                    truncated: false,
                },
            );
            display.summary = Some(summary);
            display
        }
        None => ToolDisplay::new(
            ToolHeader::Other {
                label: "read".to_string(),
            },
            ToolBody::Text {
                text: output.to_string(),
                truncated: false,
            },
        ),
    }
}

fn read_error(
    code: &str,
    message: impl Into<String>,
    hint: Option<&str>,
    details: Value,
) -> ToolExecutionError {
    let mut error = ToolError::new(ToolErrorKind::Execution, message);
    error.code = ToolErrorCode::from_legacy(code);
    if let Some(hint) = hint {
        error = error.with_hint(hint);
    }
    error.details = Some(details);
    ToolExecutionError::Recoverable(error)
}

fn read_schema() -> Value {
    json!({
        "type":"object",
        "properties": {
            "requests": {
                "type":"array", "maxItems":8,
                "description":"Batch (mirrors single-file fields)",
                "items": {"type":"object", "properties": {
                    "path":{"type":"string","description":"File"},
                    "start_line":{"type":"integer","minimum":1,"description":"Start line (1-based)"},
                    "end_line":{"type":"integer","minimum":1,"description":"End line inclusive"},
                    "if_hash":{"type":"string","description":"Hash from prior read; NOT_MODIFIED if unchanged"}
                }, "required":["path"], "additionalProperties":false}
            },
            "path":{"type":"string","description":"File"},
            "start_line":{"type":"integer","minimum":1,"description":"Start line (1-based)"},
            "end_line":{"type":"integer","minimum":1,"description":"End line inclusive"},
            "if_hash":{"type":"string","description":"Hash from prior read; NOT_MODIFIED if unchanged"}
        },
        "oneOf":[{"required":["requests"]},{"required":["path"]}],
        "additionalProperties":false
    })
}

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_typed(ReadTool);
}

#[cfg(test)]
pub(crate) fn exec_read(args: &Value) -> crate::ToolResult {
    use crate::tool_api::ErasedTool;
    let workspace = crate::current_workspace();
    let workspace_root = if workspace.is_empty() {
        PathBuf::from(".")
    } else {
        PathBuf::from(workspace)
    };
    let ctx = ToolCallContext {
        call_id: "read-test".to_string(),
        session_id: "read-test-session".to_string(),
        workspace_root: workspace_root.clone(),
        mode: crate::tool_api::AgentMode::Code,
        permission_level: crate::permission::PermissionLevel::ReadFree,
        sandbox: crate::tool_api::SandboxMode::Main,
        sandbox_spec: crate::tool_api::SandboxSpec::workspace_write(workspace_root),
        exec_default_shell: None,
        timeout: Duration::from_secs(15),
        cancellation: crate::tool_api::CancellationToken::new(),
        progress: None,
        source: crate::tool_api::ToolCallSource::Model,
    };
    crate::tool_api::TypedToolAdapter::new(ReadTool)
        .execute(ctx, args.clone())
        .expect("read must not return fatal")
        .to_tool_result()
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

        let result = exec_read(&serde_json::json!({
            "path": dir.path().to_string_lossy(),
        }));

        assert!(!result.is_success());
        assert_eq!(result.error.as_ref().unwrap().code, "IS_DIRECTORY");
    }

    #[test]
    fn read_range_is_contiguous_and_numbered() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.txt"), "one\ntwo\nthree\n").unwrap();

        let result = exec_read(&serde_json::json!({
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
        // 防呆闭环：响应必须带 hash（LF 视图 content_hash），供 edit 的 expected_hash 校验
        let hash = result.data["files"][0]["hash"]
            .as_str()
            .expect("read must return hash");
        assert_eq!(hash, crate::file_shared::content_hash("one\ntwo\nthree\n"));
    }
}

#[test]
fn zero_line_is_tolerated_as_one() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x.txt"), "one\ntwo\n").unwrap();

    // 0-based 混用（LSP 习惯）：0 视为 1，不再拒绝。
    let result = exec_read(&serde_json::json!({
        "path": dir.path().join("x.txt").to_string_lossy(),
        "start_line": 0,
        "end_line": 1,
    }));
    assert!(
        result.is_success(),
        "0-based tolerance: {}",
        result.model_text()
    );
    assert_eq!(result.model_text(), "L1: one");
    assert_eq!(result.data["files"][0]["start_line"], 1);
}

#[test]
fn out_of_range_end_truncates_instead_of_rejecting() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x.txt"), "one\ntwo\nthree\nfour\n").unwrap();

    // end 越界（行号漂移常见场景）：截断到文件尾 + truncated 标记，不拒绝。
    let result = exec_read(&serde_json::json!({
        "path": dir.path().join("x.txt").to_string_lossy(),
        "start_line": 2,
        "end_line": 99,
    }));
    assert!(
        result.is_success(),
        "end truncation should succeed: {}",
        result.model_text()
    );
    assert_eq!(result.model_text(), "L2: two\nL3: three\nL4: four");
    let meta = &result.data["files"][0];
    assert_eq!(meta["end_line"], 4);
    assert_eq!(meta["total_lines"], 4);
    assert!(!meta["truncated"].as_bool().unwrap());
}

#[test]
fn out_of_range_start_still_rejects() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x.txt"), "one\ntwo\n").unwrap();

    // start 越界 = 行号概念失效：仍硬拒绝（data 带 total_lines 可立即重试）。
    let result = exec_read(&serde_json::json!({
        "path": dir.path().join("x.txt").to_string_lossy(),
        "start_line": 10,
    }));
    assert!(!result.is_success());
    assert_eq!(result.error.as_ref().unwrap().code, "LINE_OUT_OF_RANGE");
    assert_eq!(result.data["total_lines"], 2);
    assert!(result.data["hash"].as_str().is_some());
}

#[test]
fn read_registration_is_typed_and_descriptor_keeps_legacy_schema() {
    let mut manager = crate::ToolManager::new();
    register(&mut manager);
    let registered = manager.builtins.get("read").expect("read registered");
    assert!(
        registered.legacy.is_none(),
        "read must not use legacy executor"
    );
    assert_eq!(
        registered.descriptor.input_schema["additionalProperties"],
        serde_json::json!(false)
    );
    assert_eq!(
        registered.descriptor.input_schema["oneOf"],
        serde_json::json!([{"required": ["requests"]}, {"required": ["path"]}])
    );
    assert_eq!(registered.descriptor.output_schema["type"], "object");
}

#[test]
fn ledger_corrects_stale_line_numbers_after_edit() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("y.txt");
    std::fs::write(&p, "one\ntwo\nthree\n").unwrap();
    let path = p.to_string_lossy().to_string();

    // 基线 read（清空偏移链）
    let r1 = exec_read(&serde_json::json!({ "path": path }));
    assert!(r1.is_success());

    // 账本 edit：在 L2 处插入两行（模拟 edit 的 record_edit_with_shifts）
    let new_content = "one\nINS1\nINS2\ntwo\nthree\n";
    crate::file_state::record_edit_with_shifts(&path, new_content, &[(2, 2)]);
    std::fs::write(&p, new_content).unwrap();

    // 模型仍用旧行号 L3（= 现在的 L5 three）盲定位 → 自动修正 + 透明回传
    let r2 = exec_read(&serde_json::json!({
        "path": path,
        "start_line": 3,
        "end_line": 3,
    }));
    assert!(
        r2.is_success(),
        "stale line correction: {}",
        r2.model_text()
    );
    assert_eq!(r2.model_text(), "L5: three");
    let meta = &r2.data["files"][0];
    assert_eq!(meta["corrected"], serde_json::json!(true));
    assert_eq!(meta["original_lines"], serde_json::json!([3, 3]));
    assert_eq!(meta["line_offset"], serde_json::json!(2));
}

#[test]
fn external_modification_is_not_corrected() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("z.txt");
    std::fs::write(&p, "one\ntwo\nthree\n").unwrap();
    let path = p.to_string_lossy().to_string();

    let r1 = exec_read(&serde_json::json!({ "path": path }));
    assert!(r1.is_success());

    // 外部直接改（不经账本）：偏移链为空 → 无法解释 → 不修正（hash 兜底）
    std::fs::write(&p, "one\nEXT\nthree\n").unwrap();
    let r2 = exec_read(&serde_json::json!({
        "path": path,
        "start_line": 3,
        "end_line": 3,
    }));
    assert!(r2.is_success());
    let meta = &r2.data["files"][0];
    assert!(
        meta.get("corrected").is_none(),
        "external change must not be corrected"
    );
}

#[test]
fn write_clears_shift_chain() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.txt");
    std::fs::write(&p, "one\ntwo\n").unwrap();
    let path = p.to_string_lossy().to_string();

    let r1 = exec_read(&serde_json::json!({ "path": path }));
    assert!(r1.is_success());
    crate::file_state::record_edit_with_shifts(&path, "one\ntwo\n", &[(1, 3)]);
    // write 全覆盖：行号全失效 → 清链
    crate::file_state::record_write(&path, "a\nb\nc\nd\n");
    std::fs::write(&p, "a\nb\nc\nd\n").unwrap();

    let r2 = exec_read(&serde_json::json!({
        "path": path,
        "start_line": 3,
        "end_line": 3,
    }));
    assert!(r2.is_success());
    assert_eq!(r2.model_text(), "L3: c");
    let meta = &r2.data["files"][0];
    assert!(
        meta.get("corrected").is_none(),
        "write semantics must clear the shift chain"
    );
}

#[test]
fn end_to_end_edit_then_stale_read_is_corrected() {
    // 真实工具链路：read（基线）→ edit（落账本偏移）→ 旧行号 read（自动修正）。
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("e2e.txt");
    std::fs::write(&p, "a\nb\nc\nd\ne\n").unwrap();
    let path = p.to_string_lossy().to_string();

    let r1 = exec_read(&serde_json::json!({ "path": path }));
    assert!(r1.is_success());

    // 真实 edit 工具：L3 的 c 替换为 c+C1+C2（+2 偏移，等价于 c 后插入两行）
    let e = crate::edit::exec_edit(&serde_json::json!({
        "path": path,
        "old_str": "c",
        "new_str": "c\nC1\nC2",
    }));
    assert!(e.is_success(), "edit: {}", e.model_text());

    // 模型仍用旧行号 L5（现在 = L7 e）→ 自动修正 + 透明回传
    let r2 = exec_read(&serde_json::json!({
        "path": path,
        "start_line": 5,
        "end_line": 5,
    }));
    assert!(r2.is_success(), "stale read: {}", r2.model_text());
    assert_eq!(r2.model_text(), "L7: e");
    let meta = &r2.data["files"][0];
    assert_eq!(meta["corrected"], serde_json::json!(true));
    assert_eq!(meta["original_lines"], serde_json::json!([5, 5]));
    assert_eq!(meta["line_offset"], serde_json::json!(2));
}

#[test]
fn multi_line_edit_records_shifts_for_later_reads() {
    // 行号偏移入账本：多行替换后，旧行号的 read 自动修正（strict 全事务）。
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("p.txt");
    std::fs::write(&p, "a\nb\nc\nd\n").unwrap();
    let path = p.to_string_lossy().to_string();
    let r1 = exec_read(&serde_json::json!({ "path": path }));
    assert!(r1.is_success());

    let e = crate::edit::exec_edit(&serde_json::json!({
        "path": path,
        "old_str": "b",
        "new_str": "B1\nB2",
    }));
    assert!(e.is_success(), "edit: {}", e.model_text());

    // 旧 L4 d → 新 L5 d（仅成功 hunk 的偏移生效）
    let r2 = exec_read(&serde_json::json!({
        "path": path,
        "start_line": 4,
        "end_line": 4,
    }));
    assert!(r2.is_success(), "{}", r2.model_text());
    assert_eq!(r2.model_text(), "L5: d");
    let meta = &r2.data["files"][0];
    assert_eq!(meta["corrected"], serde_json::json!(true));
    assert_eq!(meta["line_offset"], serde_json::json!(1));
}

#[test]
fn read_display_summary_is_range_metadata_not_body_first_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
    let path = path.to_string_lossy().to_string();
    let result = exec_read(&serde_json::json!({
        "path": path,
        "start_line": 2,
        "end_line": 3,
    }));
    assert!(result.is_success(), "{}", result.model_text());
    let summary = result
        .display()
        .and_then(|display| display.summary.as_deref())
        .expect("read display summary");
    assert_eq!(summary, format!("{path} · L2-L3"));
    assert_ne!(
        summary,
        result.model_text().lines().next().unwrap_or_default(),
        "read summary must not duplicate the body's first line"
    );
}

#[test]
fn read_normalizes_mixed_endings_consistently() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("mixed.txt");
    std::fs::write(&p, "a\r\nb\rc\n").unwrap();
    let r = exec_read(&serde_json::json!({ "path": p.to_string_lossy() }));
    assert!(r.is_success(), "{}", r.model_text());
    assert_eq!(r.model_text(), "L1: a\nL2: b\nL3: c");
    // 与账本/写侧同一 LF 视图 hash（write 的 STALE_FILE 不再误报）。
    let hash = r.data["files"][0]["hash"].as_str().unwrap();
    assert_eq!(hash, crate::file_shared::content_hash("a\nb\nc\n"));
}

#[test]
fn read_rejects_fifo_without_hanging() {
    #[cfg(unix)]
    {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        let path = fifo.to_string_lossy().to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = exec_read(&serde_json::json!({"path": path}));
            let _ = tx.send(r.model_text().to_string());
        });
        match rx.recv_timeout(std::time::Duration::from_secs(3)) {
            Ok(text) => assert!(text.contains("not a regular file"), "{text}"),
            Err(_) => panic!("read on FIFO hung (type guard regressed)"),
        }
    }
}

#[cfg(unix)]
#[test]
fn read_rejects_dev_null() {
    let r = exec_read(&serde_json::json!({"path": "/dev/null"}));
    assert!(!r.is_success());
    assert!(
        r.model_text().contains("not a regular file"),
        "{}",
        r.model_text()
    );
}

#[test]
fn read_rejects_oversized_regular_file() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("big.txt");
    let f = std::fs::File::create(&p).unwrap();
    f.set_len(crate::file_shared::READ_MAX_BYTES + 1).unwrap();
    let r = exec_read(&serde_json::json!({"path": p.to_string_lossy()}));
    assert!(!r.is_success());
    assert!(r.model_text().contains("read limit"), "{}", r.model_text());
}
