//! Mutation tools: write, delete（统一编辑入口见 edit/ 模块的 edit 工具）。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::file_shared::{
    atomic_write, content_hash, diff_stats_between, normalize_newlines, unified_diff,
};
use crate::ToolRisk;
use crate::tool_api::{
    AgentMode, CancellationToken, OutputBudget, SandboxMode, ToolBody, ToolCallContext,
    ToolCallSource, ToolContentBlock, ToolDescriptor, ToolDisplay, ToolError, ToolErrorCode,
    ToolErrorKind, ToolExecutionError, ToolExposure, ToolHeader, ToolName, ToolProjection,
    ToolSource, TypedTool,
};

// ── Shared helpers ──

/// 成功摘要行：模型视角默认不回传 diff 正文（省上下文），只给可验证的
/// 变更统计（路径、首行、+N -M，真实增减行数）。需要预览时用 dry_run=true 单独请求。
fn format_write_result(
    prefix: &str,
    path: &str,
    added: u32,
    removed: u32,
    first_line: u32,
    label: &str,
) -> String {
    format!("[{prefix}] {path}:{first_line} +{added} -{removed} | {label}")
}

/// write 失败消息：按 io 错误种类给针对性 hint（模型可直接执行，不猜测）。
fn write_error(path: &str, error: &std::io::Error) -> String {
    use std::io::ErrorKind;
    let hint = match error.kind() {
        ErrorKind::NotFound => {
            "The parent directory may not exist. Use exec with argv [\"ls\", \"-la\"] to inspect it, and create the directory first."
        }
        ErrorKind::PermissionDenied => {
            "The target is not writable (read-only attribute or missing permissions). Check with exec argv [\"ls\", \"-la\"], and remove the read-only flag if needed."
        }
        ErrorKind::IsADirectory => {
            "The target path is a directory, not a file. Use delete first, or write to a file path instead."
        }
        ErrorKind::StorageFull => "The disk is full. Free up space or choose another location.",
        _ => "Check disk space, file locks (another process may hold the file), and permissions.",
    };
    format!("[ERROR] Cannot write {path}: {error} [HINT] {hint}")
}

fn first_line(output: &str) -> Option<String> {
    output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(160).collect())
}

pub(crate) fn mutation_error(
    code: &str,
    message: impl Into<String>,
    hint: Option<&str>,
    details: Value,
) -> ToolExecutionError {
    mutation_error_with_retryable(code, message, None, hint, details)
}

pub(crate) fn mutation_error_with_retryable(
    code: &str,
    message: impl Into<String>,
    retryable: Option<bool>,
    hint: Option<&str>,
    details: Value,
) -> ToolExecutionError {
    let kind = if code == "STALE_FILE" {
        ToolErrorKind::Conflict
    } else if code == "NOT_FOUND" || code.ends_with("_NOT_FOUND") {
        ToolErrorKind::NotFound
    } else {
        ToolErrorKind::Execution
    };
    let mut error = ToolError::new(kind, message);
    error.code = ToolErrorCode::from_legacy(code);
    if let Some(retryable) = retryable {
        error.retryable = retryable;
    }
    if let Some(hint) = hint {
        error = error.with_hint(hint);
    }
    error.details = Some(details);
    ToolExecutionError::Recoverable(error)
}

fn write_io_error(path: &str, error: &std::io::Error) -> ToolExecutionError {
    mutation_error(
        "TOOL_ERROR",
        write_error(path, error),
        None,
        json!({"path": path}),
    )
}

pub(crate) fn resolve_mutation_path(ctx: &ToolCallContext, raw_path: &str) -> String {
    if raw_path.is_empty() {
        return String::new();
    }
    let path = Path::new(raw_path);
    if path.is_absolute() {
        return crate::permission::normalize_lexically(path)
            .to_string_lossy()
            .to_string();
    }
    let workspace = ctx.workspace_root.to_string_lossy();
    if workspace.is_empty() || workspace == "." {
        return raw_path.to_string();
    }
    let joined = ctx.workspace_root.join(path);
    crate::permission::normalize_lexically(&joined)
        .to_string_lossy()
        .to_string()
}

fn trash_dir(ctx: &ToolCallContext) -> PathBuf {
    let workspace = ctx.workspace_root.to_string_lossy();
    let root = if !workspace.is_empty() && workspace != "." {
        ctx.workspace_root.clone()
    } else {
        qaqh_types::platform::data_dir().join("workspace")
    };
    root.join(".qaqh/trash")
}

pub(crate) fn mutation_display(
    raw_path: Option<&str>,
    fallback_path: &str,
    op: crate::tool_api::PathOp,
    label: &'static str,
    output: &str,
    diff: Option<String>,
) -> ToolDisplay {
    let path = raw_path
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .unwrap_or(fallback_path);
    let header = if path.is_empty() {
        ToolHeader::Other {
            label: label.to_string(),
        }
    } else {
        ToolHeader::Path {
            path: path.to_string(),
            op,
        }
    };
    let (text, truncated) = crate::tool_api::display::clamp_display_body(output);
    let mut display = ToolDisplay::new(header, ToolBody::Text { text, truncated });
    display.summary = first_line(output);
    display.diff = diff;
    display
}

// ── write ──

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteArgs {
    pub path: String,
    pub content: String,
    #[serde(default)]
    pub append: bool,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub expected_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WriteOutput {
    pub status: String,
    pub path: String,
    pub operation: String,
    pub bytes: usize,
    pub lines: usize,
    pub lines_added: u32,
    pub lines_removed: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_changed_line: Option<u32>,
    pub created: bool,
    pub no_changes: bool,
    pub dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_id: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    model_text: String,
    #[serde(skip)]
    #[schemars(skip)]
    diff: Option<String>,
}

impl ToolProjection for WriteOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: self.model_text.clone(),
        }]
    }

    fn summary(&self) -> Option<String> {
        first_line(&self.model_text)
    }

    fn display(&self, args: &Value) -> ToolDisplay {
        mutation_display(
            args.get("path").and_then(Value::as_str),
            &self.path,
            crate::tool_api::PathOp::Write,
            "write",
            &self.model_text,
            self.diff.clone(),
        )
    }
}

pub struct WriteTool;

impl TypedTool for WriteTool {
    type Args = WriteArgs;
    type Output = WriteOutput;

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::new("write").expect("valid write tool name"),
            display_name: None,
            description: "Write/overwrite/append a file. dry_run previews a diff; use edit for targeted changes."
                .to_string(),
            input_schema: write_schema(),
            output_schema: serde_json::to_value(schemars::schema_for!(WriteOutput))
                .expect("write output schema"),
            category: crate::permission::ToolCategory::Write,
            risk: ToolRisk::Write,
            default_timeout: Duration::from_secs(30),
            exposure: ToolExposure::Direct,
            source: ToolSource::Builtin,
            output_budget: OutputBudget::default(),
            capabilities: crate::tool_capabilities::builtin_capabilities("write")
                .unwrap_or_default(),
        }
    }

    #[allow(clippy::result_large_err)] // ToolExecutionError is the frozen typed boundary.
    fn run(
        &self,
        ctx: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        let WriteArgs {
            path: raw_path,
            content,
            append,
            dry_run,
            expected_hash,
        } = args;
        let path = resolve_mutation_path(ctx, &raw_path);
        if let Err(guard) = crate::file_shared::ensure_writable_regular_target(&path) {
            let hint = guard.hint().unwrap_or_default();
            return Err(mutation_error(
                "TOOL_ERROR",
                format!(
                    "[ERROR] Cannot write {raw_path}: {} [HINT] {hint}",
                    guard.message()
                ),
                None,
                json!({"path": &path}),
            ));
        }

        if !dry_run && let Some(parent) = Path::new(&path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let bytes = content.len();
        let lines = content.lines().count();
        let old_content = std::fs::read_to_string(&path).ok();
        let normalized_old = old_content
            .as_deref()
            .map(normalize_newlines)
            .map(|(content, _)| content)
            .unwrap_or_default();
        let expected_hash = expected_hash.as_deref().unwrap_or("");

        if !expected_hash.is_empty() {
            let actual_hash = content_hash(&normalized_old);
            if actual_hash != expected_hash {
                return Err(mutation_error(
                    "STALE_FILE",
                    "STALE_FILE: File content changed since the referenced read",
                    Some("Use read to obtain current content and hash, then retry the edit."),
                    json!({
                        "path": &path,
                        "expected_hash": expected_hash,
                        "actual_hash": actual_hash,
                    }),
                ));
            }
        }

        // 工具侧账本自动防漂移（模型无需回传 hash）：未显式带 expected_hash 时，
        // 用最近一次 read/edit/write 记录的指纹校验。失配 = 文件在工具外被修改，
        // 覆盖会丢掉外部改动 → 拒绝并提示重新 read（read 后账本自动刷新）。
        if expected_hash.is_empty()
            && let Some(known) = crate::file_state::last_hash(&path)
        {
            let disk_lf_hash = content_hash(&normalized_old);
            if known != disk_lf_hash {
                return Err(mutation_error(
                    "STALE_FILE",
                    "STALE_FILE: File was modified outside the tool since the last read/edit",
                    Some("Use read to refresh the tool's view of the file, then retry the write."),
                    json!({
                        "path": &path,
                        "expected_hash": known,
                        "actual_hash": disk_lf_hash,
                    }),
                ));
            }
        }

        // 统一在 LF 视图计算 diff（dry_run 文本预览 + 展示平面共用，不重复计算）。
        let (old_norm, _) = normalize_newlines(old_content.as_deref().unwrap_or(""));
        let preview = if append {
            format!("{normalized_old}{content}")
        } else {
            content.clone()
        };
        let (new_norm, _) = normalize_newlines(&preview);
        let diff = unified_diff(&old_norm, &new_norm, &path);
        let diff_text = (!diff.trim().is_empty()).then(|| diff.trim_end().to_string());
        let no_changes = old_content.is_some() && diff.is_empty();

        if dry_run {
            let mut pending_args = serde_json::Map::new();
            pending_args.insert("path".to_string(), json!(raw_path));
            pending_args.insert("content".to_string(), json!(content));
            if append {
                pending_args.insert("append".to_string(), json!(true));
            }
            pending_args.insert(
                "expected_hash".to_string(),
                json!(content_hash(&normalized_old)),
            );
            let pending_id = crate::pending::store("write", &Value::Object(pending_args));
            let hint = format!(
                "\npending_id={pending_id} — confirm with confirm_apply {{\"pending_id\":\"{pending_id}\",\"action\":\"apply\"}}"
            );
            let (lines_added, lines_removed, first_changed_line, model_text) = if diff.is_empty() {
                (
                    0,
                    0,
                    None,
                    format!(
                        "[DRY RUN] {path} — {} bytes, {} lines (no changes would be made){hint}",
                        content.len(),
                        lines
                    ),
                )
            } else {
                let (added, removed, first_line) = diff_stats_between(&old_norm, &new_norm);
                (
                    added,
                    removed,
                    Some(first_line),
                    format!(
                        "{}\n\n{}{hint}",
                        format_write_result("DRY RUN", &path, added, removed, first_line, "write"),
                        diff.trim_end()
                    ),
                )
            };
            return Ok(WriteOutput {
                status: "ok".to_string(),
                path,
                operation: "dry_run".to_string(),
                bytes,
                lines,
                lines_added,
                lines_removed,
                first_changed_line,
                created: old_content.is_none(),
                no_changes,
                dry_run: true,
                pending_id: Some(pending_id),
                model_text,
                diff: diff_text,
            });
        }

        if append {
            use std::io::Write;
            let mut file = match std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&path)
            {
                Ok(file) => file,
                Err(error) => return Err(write_io_error(&path, &error)),
            };
            match file.write_all(content.as_bytes()) {
                Ok(_) => {
                    // 账本记录 append 后**整个文件**的指纹（磁盘上的真实状态）。
                    let full = match &old_content {
                        Some(old) => format!("{old}{content}"),
                        None => content.clone(),
                    };
                    crate::file_state::record_write(&path, &full);
                    crate::journal::record_change(
                        &ctx.session_id,
                        "",
                        "write",
                        &raw_path,
                        "append",
                        old_content.as_deref(),
                        Some(&full),
                        "ok",
                    );
                    let (lines_added, first_changed_line, model_text) = match &old_content {
                        Some(old) => {
                            let old_line_count = old.lines().count();
                            let first_line = if old_line_count == 0 {
                                1u32
                            } else {
                                old_line_count as u32 + 1
                            };
                            (
                                lines as u32,
                                Some(first_line),
                                format_write_result(
                                    "OK",
                                    &path,
                                    lines as u32,
                                    0,
                                    first_line,
                                    "write",
                                ),
                            )
                        }
                        None => (
                            lines as u32,
                            Some(1),
                            format!(
                                "[OK] {path} — {} bytes, {} lines (new file)",
                                content.len(),
                                lines
                            ),
                        ),
                    };
                    return Ok(WriteOutput {
                        status: "ok".to_string(),
                        path,
                        operation: "append".to_string(),
                        bytes,
                        lines,
                        lines_added,
                        lines_removed: 0,
                        first_changed_line,
                        created: old_content.is_none(),
                        no_changes,
                        dry_run: false,
                        pending_id: None,
                        model_text,
                        diff: diff_text,
                    });
                }
                Err(error) => return Err(write_io_error(&path, &error)),
            }
        }

        match atomic_write(&path, &content) {
            Ok(_) => {
                crate::file_state::record_write(&path, &content);
                crate::journal::record_change(
                    &ctx.session_id,
                    "",
                    "write",
                    &raw_path,
                    "overwrite",
                    old_content.as_deref(),
                    Some(&content),
                    "ok",
                );
                let (lines_added, lines_removed, first_changed_line, model_text) =
                    if let Some(ref old) = old_content {
                        let (old_norm, _) = normalize_newlines(old);
                        let (new_norm, _) = normalize_newlines(&content);
                        if old_norm == new_norm {
                            (
                                0,
                                0,
                                None,
                                format!(
                                    "[OK] {path} — {} bytes, {} lines (no changes)",
                                    content.len(),
                                    lines
                                ),
                            )
                        } else {
                            let (added, removed, first_line) =
                                diff_stats_between(&old_norm, &new_norm);
                            (
                                added,
                                removed,
                                Some(first_line),
                                format_write_result(
                                    "OK", &path, added, removed, first_line, "write",
                                ),
                            )
                        }
                    } else {
                        (
                            lines as u32,
                            0,
                            Some(1),
                            format!(
                                "[OK] {path} — {} bytes, {} lines (new file)",
                                content.len(),
                                lines
                            ),
                        )
                    };
                Ok(WriteOutput {
                    status: "ok".to_string(),
                    path,
                    operation: "overwrite".to_string(),
                    bytes,
                    lines,
                    lines_added,
                    lines_removed,
                    first_changed_line,
                    created: old_content.is_none(),
                    no_changes,
                    dry_run: false,
                    pending_id: None,
                    model_text,
                    diff: diff_text,
                })
            }
            Err(error) => Err(write_io_error(&path, &error)),
        }
    }
}

fn write_schema() -> Value {
    json!({
        "type":"object",
        "properties":{
            "path":{"type":"string","description":"File"},
            "content":{"type":"string","description":"Content"},
            "append":{"type":"boolean","description":"Append (default false)","default":false},
            "dry_run":{"type":"boolean","description":"Preview only","default":false},
            "expected_hash":{"type":"string","description":"Hash from prior read (optional)"}
        },
        "required":["path","content"],
        "additionalProperties":false
    })
}

// ── delete ──

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteArgs {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DeleteOutput {
    pub status: String,
    pub path: String,
    pub trash_path: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl ToolProjection for DeleteOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: self.content.clone(),
        }]
    }

    fn summary(&self) -> Option<String> {
        first_line(&self.content)
    }

    fn display(&self, args: &Value) -> ToolDisplay {
        mutation_display(
            args.get("path").and_then(Value::as_str),
            &self.path,
            crate::tool_api::PathOp::Delete,
            "delete",
            &self.content,
            None,
        )
    }
}

pub struct DeleteTool;

impl TypedTool for DeleteTool {
    type Args = DeleteArgs;
    type Output = DeleteOutput;

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::new("delete").expect("valid delete tool name"),
            display_name: None,
            description: "Move file to trash (.qaqh/trash/).".to_string(),
            input_schema: delete_schema(),
            output_schema: serde_json::to_value(schemars::schema_for!(DeleteOutput))
                .expect("delete output schema"),
            category: crate::permission::ToolCategory::Write,
            risk: ToolRisk::Destructive,
            default_timeout: Duration::from_secs(15),
            exposure: ToolExposure::Direct,
            source: ToolSource::Builtin,
            output_budget: OutputBudget::default(),
            capabilities: crate::tool_capabilities::builtin_capabilities("delete")
                .unwrap_or_default(),
        }
    }

    #[allow(clippy::result_large_err)] // ToolExecutionError is the frozen typed boundary.
    fn run(
        &self,
        ctx: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        let raw_path = args.path;
        let path = resolve_mutation_path(ctx, &raw_path);
        let source = Path::new(&path);
        if !source.exists() {
            return Err(mutation_error(
                "NOT_FOUND",
                format!("NOT_FOUND: {path} does not exist"),
                Some("Use exec with argv [\"ls\", \"-la\"] to verify."),
                json!({"path": &path}),
            ));
        }

        // 工具侧账本防漂移：删除是破坏性操作——若文件在工具外被修改，
        // 模型基于过期认知删除会丢失外部改动 → 拒绝并提示重新 read。
        // 读失败（二进制/权限）则跳过校验（账本里也不会有对应记录）。
        if let Ok(raw) = std::fs::read_to_string(&path) {
            let (lf, _) = normalize_newlines(&raw);
            if let Some(known) = crate::file_state::last_hash(&path) {
                let disk_lf_hash = content_hash(&lf);
                if known != disk_lf_hash {
                    return Err(mutation_error(
                        "STALE_FILE",
                        "STALE_FILE: File was modified outside the tool since the last read/edit",
                        Some(
                            "Use read to refresh the tool's view of the file, then retry the delete.",
                        ),
                        json!({
                            "path": &path,
                            "expected_hash": known,
                            "actual_hash": disk_lf_hash,
                        }),
                    ));
                }
            }
        }

        let trash_root = trash_dir(ctx);
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let workspace = ctx.workspace_root.to_string_lossy().to_string();
        let project_root = if !workspace.is_empty() && workspace != "." {
            Path::new(&workspace)
        } else {
            Path::new(".")
        };
        let rel = if let Ok(stripped) = source.strip_prefix(project_root) {
            stripped.to_string_lossy().to_string()
        } else if let Some(name) = source.file_name() {
            name.to_string_lossy().to_string()
        } else {
            path.replace(['/', '\\', ':'], "__")
        };
        let safe_name = rel.replace(['/', '\\', ':'], "__");
        let trash_path = trash_root.join(format!("{safe_name}.{ts}"));
        if let Some(parent) = trash_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let journal_before = std::fs::read_to_string(&path).ok();

        match std::fs::rename(source, &trash_path) {
            Ok(_) => {
                crate::file_state::record_delete(&path);
                crate::journal::record_change(
                    &ctx.session_id,
                    "",
                    "delete",
                    &raw_path,
                    "delete",
                    journal_before.as_deref(),
                    None,
                    "ok",
                );
                let trash_path_abs = trash_path.clone();
                let trash_path = format!(
                    ".qaqh/trash/{}",
                    trash_path.file_name().unwrap_or_default().to_string_lossy()
                );
                Ok(DeleteOutput {
                    status: "ok".to_string(),
                    path: path.clone(),
                    content: format!("Moved to trash: {trash_path}"),
                    hint: Some(format!(
                        "Restore with exec argv [\"mv\", \"{}\", \"{}\"]",
                        trash_path_abs.display(),
                        path
                    )),
                    trash_path,
                })
            }
            Err(_error) => {
                if source.is_dir() {
                    return Err(mutation_error(
                        "CROSS_DEVICE_DIR",
                        "Cannot trash directory across devices",
                        Some(&format!(
                            "Use exec with argv [\"rm\", \"-rf\", \"{}\"] for cross-device deletion.",
                            path
                        )),
                        json!({"path": &path}),
                    ));
                }
                if let Err(error) = std::fs::copy(source, &trash_path) {
                    return Err(mutation_error(
                        "COPY_FAILED",
                        error.to_string(),
                        Some("Check permissions and disk space."),
                        json!({"path": &path}),
                    ));
                }
                match std::fs::remove_file(source) {
                    Ok(_) => {
                        crate::file_state::record_delete(&path);
                        crate::journal::record_change(
                            &ctx.session_id,
                            "",
                            "delete",
                            &raw_path,
                            "delete",
                            journal_before.as_deref(),
                            None,
                            "ok",
                        );
                        let trash_path_abs = trash_path.clone();
                        let trash_path = format!(
                            ".qaqh/trash/{}",
                            trash_path.file_name().unwrap_or_default().to_string_lossy()
                        );
                        Ok(DeleteOutput {
                            status: "ok".to_string(),
                            path: path.clone(),
                            content: format!("Moved to trash (cross-device): {trash_path}"),
                            hint: Some(format!(
                                "Restore with exec argv [\"cp\", \"{}\", \"{}\"]",
                                trash_path_abs.display(),
                                path
                            )),
                            trash_path,
                        })
                    }
                    Err(error) => Err(mutation_error(
                        "DELETE_FAILED",
                        format!("Copied to trash but could not remove original: {error}"),
                        Some(&format!(
                            "Original still at {path}; remove it manually or retry."
                        )),
                        json!({
                            "path": &path,
                            "trash_path": format!(
                                ".qaqh/trash/{}",
                                trash_path
                                    .file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                            ),
                            "copied_to_trash": true,
                        }),
                    )),
                }
            }
        }
    }
}

fn delete_schema() -> Value {
    json!({
        "type":"object",
        "properties":{"path":{"type":"string","description":"File"}},
        "required":["path"],
        "additionalProperties":false
    })
}

// ── Registration ──

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_typed(WriteTool);
    mgr.register_typed(DeleteTool);
}

/// Compatibility entry retained until `confirm_apply` is typed (Wave 6).
///
/// Production registration uses [`WriteTool`] directly and therefore never
/// takes this bridge. This helper only preserves the existing in-process
/// confirm-apply call shape.
#[cfg(test)]
pub(super) fn exec_write_file(args: &Value) -> crate::ToolResult {
    use crate::tool_api::{ErasedTool, TypedToolAdapter};

    let ctx = ambient_tool_context("write-compat", Duration::from_secs(30));
    TypedToolAdapter::new(WriteTool)
        .execute(ctx, args.clone())
        .unwrap_or_else(|fatal| panic!("write tool fatal: {}", fatal.message))
        .to_tool_result()
}

pub(crate) fn ambient_tool_context(call_id: &str, timeout: Duration) -> ToolCallContext {
    let workspace = crate::current_workspace();
    let workspace_root = if workspace.is_empty() {
        PathBuf::from(".")
    } else {
        PathBuf::from(workspace)
    };
    let cancellation = CancellationToken::new();
    if crate::is_cancel() {
        cancellation.cancel();
    }
    ToolCallContext {
        call_id: call_id.to_string(),
        session_id: crate::current_session().unwrap_or_default(),
        workspace_root,
        mode: match crate::runtime::current_mode() {
            1 => AgentMode::Plan,
            _ => AgentMode::Code,
        },
        permission_level: crate::runtime::context()
            .map(|context| crate::permission::PermissionLevel::from_u8(context.permission_level))
            .unwrap_or(crate::permission::PermissionLevel::MaxLockdown),
        sandbox: if crate::authorization::is_subagent_sandbox() {
            SandboxMode::Subagent
        } else {
            SandboxMode::Main
        },
        timeout,
        cancellation,
        progress: None,
        source: ToolCallSource::Model,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::PermissionLevel as TestPermissionLevel;
    use crate::tool_api::{
        CancellationToken as TestCancellationToken, ErasedTool, SandboxMode as TestSandboxMode,
        ToolCallSource as TestToolCallSource, TypedToolAdapter,
    };

    fn write(args: Value) -> String {
        exec_write_file(&args).model_text().to_string()
    }

    fn test_ctx(root: &Path) -> ToolCallContext {
        ToolCallContext {
            call_id: "test-call".to_string(),
            session_id: "test-session".to_string(),
            workspace_root: root.to_path_buf(),
            mode: AgentMode::Code,
            permission_level: TestPermissionLevel::ReadFree,
            sandbox: TestSandboxMode::Main,
            timeout: Duration::from_secs(30),
            cancellation: TestCancellationToken::new(),
            progress: None,
            source: TestToolCallSource::Model,
        }
    }

    fn execute_write(root: &Path, args: Value) -> qaqh_types::ToolResult {
        TypedToolAdapter::new(WriteTool)
            .execute(test_ctx(root), args)
            .expect("write must not return fatal")
            .to_tool_result()
    }

    fn execute_delete(root: &Path, args: Value) -> qaqh_types::ToolResult {
        TypedToolAdapter::new(DeleteTool)
            .execute(test_ctx(root), args)
            .expect("delete must not return fatal")
            .to_tool_result()
    }

    #[test]
    fn write_and_delete_registration_carries_no_legacy_executor() {
        let mut manager = crate::ToolManager::new();
        register(&mut manager);
        for name in ["write", "delete"] {
            let tool = manager
                .builtins
                .get(name)
                .unwrap_or_else(|| panic!("{name} must be registered"));
            assert!(tool.legacy.is_none(), "{name} still has legacy executor");
        }
        assert_eq!(
            manager.builtins["write"].descriptor.input_schema,
            write_schema()
        );
        assert_eq!(
            manager.builtins["delete"].descriptor.input_schema,
            delete_schema()
        );
    }

    #[test]
    fn typed_dry_run_carries_pending_id_in_canonical_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dry-run.txt");
        std::fs::write(&path, "old\n").unwrap();
        let result = execute_write(
            dir.path(),
            json!({"path": path, "content": "new\n", "dry_run": true}),
        );
        assert!(result.is_success());
        assert_eq!(result.data["status"], json!("ok"));
        assert_eq!(result.data["dry_run"], json!(true));
        assert!(result.data["pending_id"].as_str().is_some());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old\n");
    }

    #[test]
    fn overwrite_returns_summary_without_diff_body() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "line1\nline2\nline3\n").unwrap();
        let out = write(json!({
            "path": path, "content": "line1\nCHANGED\nline3\n"
        }));
        assert!(out.starts_with("[OK] "), "got: {out}");
        assert!(out.contains("+1 -1"), "got: {out}");
        assert!(out.contains("| write"), "got: {out}");
        assert!(!out.contains("--- a/"), "diff body leaked: {out}");
        assert!(!out.contains("+++ b/"), "diff body leaked: {out}");
        assert!(!out.contains("CHANGED"), "content echo leaked: {out}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "line1\nCHANGED\nline3\n"
        );
    }

    #[test]
    fn typed_write_output_drives_model_display_and_canonical_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("typed.txt");
        std::fs::write(&path, "old\n").unwrap();
        let outcome = TypedToolAdapter::new(WriteTool)
            .execute(
                test_ctx(dir.path()),
                json!({"path": path, "content": "new\n"}),
            )
            .expect("write must not return fatal");
        assert_eq!(outcome.status, qaqh_types::ToolStatus::Ok);
        assert!(outcome.display.diff.is_some());
        let display_text = match &outcome.display.body {
            crate::tool_api::ToolBody::Text { text, .. } => text.clone(),
            other => panic!("unexpected write display body: {other:?}"),
        };
        assert_eq!(display_text, outcome.model.text);
        assert!(
            outcome
                .display
                .summary
                .as_deref()
                .is_some_and(|summary| summary.starts_with("[OK] "))
        );

        let result = outcome.to_tool_result();
        assert_eq!(result.data["status"], json!("ok"));
        assert_eq!(
            result.data["path"],
            json!(path.to_string_lossy().to_string())
        );
        assert_eq!(result.data["operation"], json!("overwrite"));
        assert_eq!(result.data["lines_added"], json!(1));
        assert_eq!(result.data["lines_removed"], json!(1));
        assert!(result.diff.is_some());
    }

    #[test]
    fn dry_run_previews_diff_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.txt");
        std::fs::write(&path, "old\n").unwrap();
        let out = write(json!({
            "path": path, "content": "new\n", "dry_run": true
        }));
        assert!(out.starts_with("[DRY RUN] "), "got: {out}");
        assert!(out.contains("--- a/"), "dry_run must include diff: {out}");
        assert!(out.contains("+++ b/"), "dry_run must include diff: {out}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old\n");
    }

    #[test]
    fn append_returns_summary_without_content_echo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.txt");
        std::fs::write(&path, "a\nb\n").unwrap();
        let out = write(json!({
            "path": path, "content": "appended-line\n", "append": true
        }));
        assert!(out.starts_with("[OK] "), "got: {out}");
        assert!(out.contains("+1 -0"), "got: {out}");
        assert!(!out.contains("appended-line"), "content echo leaked: {out}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "a\nb\nappended-line\n"
        );
    }

    #[test]
    fn write_error_classifies_by_io_kind() {
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such dir");
        let out = write_error("x/y.txt", &err);
        assert!(
            out.starts_with("[ERROR] Cannot write x/y.txt"),
            "got: {out}"
        );
        assert!(out.contains("[HINT]"), "got: {out}");
        assert!(
            out.contains("parent directory"),
            "kind-specific hint missing: {out}"
        );
        let err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let out = write_error("x/y.txt", &err);
        assert!(
            out.contains("read-only"),
            "kind-specific hint missing: {out}"
        );
    }

    #[test]
    fn write_to_directory_path_reports_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub");
        std::fs::create_dir(&path).unwrap();
        let out = write(json!({
            "path": path.to_string_lossy(), "content": "x"
        }));
        assert!(out.starts_with("[ERROR]"), "got: {out}");
        assert!(out.contains("[HINT]"), "got: {out}");
    }

    #[test]
    fn mixed_endings_read_then_write_has_no_false_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.txt");
        std::fs::write(&path, "a\r\nb\rc\n").unwrap();
        let raw_path = path.to_string_lossy().to_string();
        crate::file_state::record_read(&raw_path, "a\nb\nc\n", 3);
        let result = execute_write(
            dir.path(),
            json!({"path": raw_path, "content": "a\r\nB\rc\n"}),
        );
        assert!(result.is_success(), "{}", result.model_text());
    }

    #[cfg(unix)]
    #[test]
    fn write_symlink_target_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, "hello\n").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let out = write(json!({"path": link, "content": "WORLD\n"}));
        assert!(out.contains("symbolic link"), "got: {out}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello\n");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn delete_returns_typed_output_and_moves_file_to_trash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("delete-me.txt");
        std::fs::write(&path, "bye\n").unwrap();
        let outcome = TypedToolAdapter::new(DeleteTool)
            .execute(test_ctx(dir.path()), json!({"path": path}))
            .expect("delete must not return fatal");
        let display_text = match &outcome.display.body {
            crate::tool_api::ToolBody::Text { text, .. } => text.clone(),
            other => panic!("unexpected delete display body: {other:?}"),
        };
        assert_eq!(display_text, outcome.model.text);
        assert_eq!(outcome.status, qaqh_types::ToolStatus::Ok);
        let result = outcome.to_tool_result();
        assert!(result.is_success());
        assert_eq!(result.data["status"], json!("ok"));
        assert_eq!(
            result.data["path"],
            json!(path.to_string_lossy().to_string())
        );
        assert!(!path.exists());
        let trash_path = result.data["trash_path"].as_str().unwrap();
        assert!(dir.path().join(trash_path).exists());
    }

    #[test]
    fn delete_missing_file_is_a_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.txt");
        let result = execute_delete(dir.path(), json!({"path": path}));
        assert!(!result.is_success());
        assert_eq!(result.error.as_ref().unwrap().code, "NOT_FOUND");
        assert!(result.model_text().contains("NOT_FOUND"));
    }

    #[test]
    fn delete_stale_ledger_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stale.txt");
        std::fs::write(&path, "original\n").unwrap();
        let raw_path = path.to_string_lossy().to_string();
        crate::file_state::record_read(&raw_path, "original\n", 1);
        std::fs::write(&path, "changed outside\n").unwrap();
        let result = execute_delete(dir.path(), json!({"path": raw_path}));
        assert!(!result.is_success());
        assert_eq!(result.error.as_ref().unwrap().code, "STALE_FILE");
        assert!(path.exists());
    }
}
