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
    AgentMode, CancellationToken, SandboxMode, ToolBody, ToolCallContext, ToolCallSource,
    ToolContentBlock, ToolDisplay, ToolError, ToolErrorCode, ToolErrorKind, ToolExecutionError,
    ToolHeader, ToolMeta, ToolProjection, TypedTool,
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
            "The parent directory may not exist. Use exec command \"ls -la\" to inspect it, and create the directory first."
        }
        ErrorKind::PermissionDenied => {
            "The target is not writable (read-only attribute or missing permissions). Check with exec command \"ls -la\", and remove the read-only flag if needed."
        }
        ErrorKind::IsADirectory => {
            "The target path is a directory, not a file. Use delete first, or write to a file path instead."
        }
        ErrorKind::StorageFull => "The disk is full. Free up space or choose another location.",
        _ => "Check disk space, file locks (another process may hold the file), and permissions.",
    };
    format!("[ERROR] Cannot write {path}: {error} [HINT] {hint}")
}

/// 文件变更工具的展示摘要：只承载路径 + 变更元信息。
///
/// 这里刻意不采用 `output` 首行：`output` 是模型面文本，可能自带工具名和
/// `[OK]` 终态标记；展示面已有 header 与结构化 state，再回捞文本必然重复。
pub fn mutation_change_summary(path: &str, added: u32, removed: u32) -> String {
    if added == 0 && removed == 0 {
        format!("{path} · no changes")
    } else {
        format!("{path} · +{added} -{removed}")
    }
}

pub fn mutation_error(
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
    let kind = if code == "stale_file" {
        ToolErrorKind::Conflict
    } else if code == "not_found" || code.ends_with("_not_found") {
        ToolErrorKind::NotFound
    } else {
        ToolErrorKind::Execution
    };
    let mut error = ToolError::new(kind, message);
    error.code = ToolErrorCode::parse_or_builtin(code, kind);
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
        "tool_error",
        write_error(path, error),
        None,
        json!({"path": path}),
    )
}

pub fn resolve_mutation_path(ctx: &ToolCallContext, raw_path: &str) -> String {
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

/// 回收站根目录。默认 `<workspace>/.qaqh/trash/`。
///
/// 为什么不默认搬到 `data_dir()` 下——尽管"备份放在工作区之外"才是一贯原则
/// （见 `qaqh-spy/src/store.rs::default_dir`）：`delete` 的快路径是 **rename**，
/// 而跨卷 rename 必然失败（`EXDEV`）。家目录与工程分处不同卷是常见部署
/// （Windows 上 `C:\Users` + `D:\project`；macOS 外接盘），默认搬出去会让**每一次**
/// 删除退化成整份复制（文件），或者干脆拒绝（目录，见下方 `cross_device_dir` 分支）
/// ——那比"备份留在工作区里"糟得多。
///
/// 留在工作区里之所以安全，是因为 PR-B1 已经把 `.qaqh` 纳入 spy 的 `exclude_dirs`，
/// 并把目录名匹配从全等改成「全等或 `{name}.` 前缀」：`target.1791993394` 这类
/// 带时间戳的 trash 副本不会再被当成工作区净变更快照/上报（曾一次报出 9337 个
/// 变更、单条注入 ≈1.2–3.5 MB）。
///
/// `QAQH_TRASH_DIR` 可覆盖：给"就是想把备份放到别处/别盘"的用法（自己承担跨卷
/// 复制代价），也给测试隔离。
fn trash_dir(ctx: &ToolCallContext) -> PathBuf {
    if let Ok(dir) = std::env::var("QAQH_TRASH_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    let workspace = ctx.workspace_root.to_string_lossy();
    let root = if !workspace.is_empty() && workspace != "." {
        ctx.workspace_root.clone()
    } else {
        qaqh_types::platform::data_dir().join("workspace")
    };
    root.join(".qaqh/trash")
}

/// trash 条目的展示路径。
///
/// 在工作区内时给相对形（`.qaqh/trash/x.<ts>`）——短、对模型友好，且与既有
/// 文案/断言口径一致；被 `QAQH_TRASH_DIR` 搬到工作区外时**只能**给绝对路径，
/// 因为相对形在那儿指向一个并不存在的落点（照旧硬编码 `.qaqh/trash/` 就是假线索）。
fn trash_display_path(trash: &Path, project_root: &Path) -> String {
    trash
        .strip_prefix(project_root)
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| trash.to_string_lossy().to_string())
}

pub fn mutation_display(
    raw_path: Option<&str>,
    fallback_path: &str,
    op: crate::tool_api::PathOp,
    label: &'static str,
    summary: Option<String>,
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
    display.summary = summary;
    display.diff = diff;
    display
}

// ── write ──

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteArgs {
    #[serde(alias = "file_path")]
    pub path: String,
    pub content: String,
    /// Append (default false)
    #[serde(default)]
    pub append: bool,
    /// Preview only
    #[serde(default)]
    pub dry_run: bool,
    /// Hash from prior read (optional)
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

    fn display(&self, args: &Value) -> ToolDisplay {
        let summary = if self.dry_run {
            format!(
                "{} · preview +{} -{}",
                self.path, self.lines_added, self.lines_removed
            )
        } else if self.created {
            format!(
                "{} · new · +{} -{}",
                self.path, self.lines_added, self.lines_removed
            )
        } else {
            mutation_change_summary(&self.path, self.lines_added, self.lines_removed)
        };
        mutation_display(
            crate::support::arg_path(args),
            &self.path,
            crate::tool_api::PathOp::Write,
            "write",
            Some(summary),
            &self.model_text,
            self.diff.clone(),
        )
        .with_lines(self.lines_added, self.lines_removed)
    }
}

pub struct WriteTool;

impl TypedTool for WriteTool {
    type Args = WriteArgs;
    type Output = WriteOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "write",
            "Write/overwrite/append a file. dry_run previews a diff; use edit for targeted changes.",
            crate::permission::ToolCategory::Write,
            ToolRisk::Write,
            Duration::from_secs(30),
        )
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
                "tool_error",
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
                    "stale_file",
                    "stale_file: File content changed since the referenced read",
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
                    "stale_file",
                    "stale_file: File was modified outside the tool since the last read/edit",
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

// ── delete ──

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteArgs {
    #[serde(alias = "file_path")]
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
    /// 展示面 unified diff（`删除前 → 空`）。
    ///
    /// 与 `write`/`edit`/`apply_patch` 同一个出口：只供 timeline 与前端 diff 抽屉
    /// 消费，绝不进模型投影。删除的语义就是整份内容被移出，所以这里是全 `-` 行
    /// （与 git 对删除的表示一致）。此前该槽恒为 `None`，抽屉里对删除只能显示
    /// 「没有可展示的对比内容」——文件列表有 `+0 −N` 指纹，点进去是空的。
    #[serde(skip)]
    #[schemars(skip)]
    diff: Option<String>,
}

impl ToolProjection for DeleteOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: self.content.clone(),
        }]
    }

    fn display(&self, args: &Value) -> ToolDisplay {
        mutation_display(
            crate::support::arg_path(args),
            &self.path,
            crate::tool_api::PathOp::Delete,
            "delete",
            Some(format!("{} · deleted", self.path)),
            &self.content,
            self.diff.clone(),
        )
    }
}

pub struct DeleteTool;

impl TypedTool for DeleteTool {
    type Args = DeleteArgs;
    type Output = DeleteOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "delete",
            "Move file to trash (.qaqh/trash/).",
            crate::permission::ToolCategory::Write,
            ToolRisk::Destructive,
            Duration::from_secs(15),
        )
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
                "not_found",
                format!("not_found: {path} does not exist"),
                Some("Use exec command \"ls -la\" to verify."),
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
                        "stale_file",
                        "stale_file: File was modified outside the tool since the last read/edit",
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
        // 展示面 diff：删除 = 整份内容移出，unified 视角就是全 `-` 行。
        // 盘上有可能是 CRLF，先归一到 LF 再算，否则整段会被渲染成「换行符差异」。
        // 读不出文本（二进制/权限）就不造假 diff，留空态。
        let diff = journal_before.as_deref().and_then(|before| {
            let (lf, _) = normalize_newlines(before);
            let text = unified_diff(&lf, "", &raw_path);
            (!text.trim().is_empty()).then(|| text.trim_end().to_string())
        });

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
                let trash_path = trash_display_path(&trash_path, project_root);
                Ok(DeleteOutput {
                    status: "ok".to_string(),
                    path: path.clone(),
                    content: format!("Moved to trash: {trash_path}"),
                    hint: Some(format!(
                        "Restore with exec command \"mv '{}' '{}'\"",
                        trash_path_abs.display(),
                        path
                    )),
                    trash_path,
                    diff: diff.clone(),
                })
            }
            Err(_error) => {
                if source.is_dir() {
                    return Err(mutation_error(
                        "cross_device_dir",
                        "Cannot trash directory across devices",
                        Some(&format!(
                            "Use exec command \"rm -rf '{}'\" for cross-device deletion.",
                            path
                        )),
                        json!({"path": &path}),
                    ));
                }
                if let Err(error) = std::fs::copy(source, &trash_path) {
                    return Err(mutation_error(
                        "copy_failed",
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
                        let trash_path = trash_display_path(&trash_path, project_root);
                        Ok(DeleteOutput {
                            status: "ok".to_string(),
                            path: path.clone(),
                            content: format!("Moved to trash (cross-device): {trash_path}"),
                            hint: Some(format!(
                                "Restore with exec command \"cp '{}' '{}'\"",
                                trash_path_abs.display(),
                                path
                            )),
                            trash_path,
                            diff,
                        })
                    }
                    Err(error) => Err(mutation_error(
                        "delete_failed",
                        format!("Copied to trash but could not remove original: {error}"),
                        Some(&format!(
                            "Original still at {path}; remove it manually or retry."
                        )),
                        json!({
                            "path": &path,
                            "trash_path": trash_display_path(&trash_path, project_root),
                            "copied_to_trash": true,
                        }),
                    )),
                }
            }
        }
    }
}

// ── Registration ──

pub fn register(mgr: &mut impl qaqh_tool_core::tool_api::RegistersTyped) {
    mgr.register_typed_tool(WriteTool);
    mgr.register_typed_tool(DeleteTool);
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

pub fn ambient_tool_context(call_id: &str, timeout: Duration) -> ToolCallContext {
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
    let ambient = crate::hooks::ambient();
    ToolCallContext {
        call_id: call_id.to_string(),
        session_id: crate::current_session().unwrap_or_default(),
        workspace_root: workspace_root.clone(),
        mode: match ambient.mode {
            1 => AgentMode::Plan,
            _ => AgentMode::Code,
        },
        permission_level: crate::permission::PermissionLevel::from_u8(ambient.permission_level),
        sandbox: if ambient.subagent_sandbox {
            SandboxMode::Subagent
        } else {
            SandboxMode::Main
        },
        sandbox_spec: crate::tool_api::SandboxSpec::workspace_write(workspace_root),
        exec_default_shell: None,
        timeout,
        cancellation,
        progress: None,
        exec_progress: None,
        official_web: None,
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
    use std::sync::{Mutex, OnceLock};

    /// 串行所有涉及 trash 环境变量/落点的用例：`QAQH_TRASH_DIR` 是进程级状态，
    /// 而 cargo 默认多线程跑测试。
    static DELETE_LOCK: Mutex<()> = Mutex::new(());

    /// Claude 标准拼写 `file_path`：接收端接受，模型面 schema 不变。
    #[test]
    fn claude_style_file_path_is_accepted_without_touching_the_schema() {
        let write: WriteArgs =
            serde_json::from_value(serde_json::json!({"file_path": "a.rs", "content": "x"}))
                .expect("write file_path");
        assert_eq!(write.path, "a.rs");
        let delete: DeleteArgs = serde_json::from_value(serde_json::json!({"file_path": "b.rs"}))
            .expect("delete file_path");
        assert_eq!(delete.path, "b.rs");

        for schema in [
            crate::tool_api::schema::schema_of::<WriteArgs>(),
            crate::tool_api::schema::schema_of::<DeleteArgs>(),
        ] {
            assert!(
                schema["properties"].get("file_path").is_none(),
                "alias 不得出现在模型面 schema: {schema}"
            );
            assert_eq!(schema["additionalProperties"], false);
        }
    }

    /// 测试隔离：写/删路径都会 `journal::record_change`，而 SMJ 落在
    /// `platform::data_dir()` —— 不设隔离就是往**真实用户数据根**里写测试记录。
    /// 口径同 `journal.rs` 的 `with_temp_journal`；本模块用例多且共用 `test_ctx`，
    /// 所以用进程级 `OnceLock` 一次钉住，不逐个包裹。
    static JOURNAL_ISOLATION: OnceLock<tempfile::TempDir> = OnceLock::new();

    fn isolate_journal_root() {
        JOURNAL_ISOLATION.get_or_init(|| {
            let dir = tempfile::tempdir().expect("journal tempdir");
            // SAFETY: 在本模块首次构造 ToolCallContext 之前执行，此后只读不改。
            unsafe { std::env::set_var("QAQH_JOURNAL_DIR", dir.path()) };
            dir
        });
    }

    fn write(args: Value) -> String {
        exec_write_file(&args).model_text().to_string()
    }

    fn test_ctx(root: &Path) -> ToolCallContext {
        isolate_journal_root();
        ToolCallContext {
            call_id: "test-call".to_string(),
            session_id: "test-session".to_string(),
            workspace_root: root.to_path_buf(),
            mode: AgentMode::Code,
            permission_level: TestPermissionLevel::ReadOnly,
            sandbox: TestSandboxMode::Main,
            sandbox_spec: crate::tool_api::SandboxSpec::workspace_write(root.to_path_buf()),
            exec_default_shell: None,
            timeout: Duration::from_secs(30),
            cancellation: TestCancellationToken::new(),
            progress: None,
            exec_progress: None,
            official_web: None,
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
        let mut manager = qaqh_workspace::ToolManager::new();
        register(&mut manager);
        for name in ["write", "delete"] {
            let tool = manager
                .builtin(name)
                .unwrap_or_else(|| panic!("{name} must be registered"));
            assert_eq!(
                tool.descriptor.name.as_str(),
                name,
                "{name} descriptor name"
            );
        }
        assert_eq!(
            manager.builtin("write").unwrap().descriptor.input_schema["additionalProperties"],
            serde_json::json!(false)
        );
        assert!(
            manager.builtin("write").unwrap().descriptor.input_schema["properties"]["path"]
                .is_object(),
            "write input schema 由 WriteArgs 类型生成"
        );
        assert!(
            manager.builtin("write").unwrap().descriptor.input_schema["properties"]["content"]
                .is_object(),
            "write input schema 由 WriteArgs 类型生成"
        );
        assert_eq!(
            manager.builtin("delete").unwrap().descriptor.input_schema["additionalProperties"],
            serde_json::json!(false)
        );
        assert!(
            manager.builtin("delete").unwrap().descriptor.input_schema["properties"]["path"]
                .is_object(),
            "delete input schema 由 DeleteArgs 类型生成"
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
        assert!(!out.contains("--- "), "diff body leaked: {out}");
        assert!(!out.contains("+++ "), "diff body leaked: {out}");
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
        let expected_summary = format!("{} · +1 -1", path.to_string_lossy());
        assert_eq!(
            outcome.display.summary.as_deref(),
            Some(expected_summary.as_str())
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
        let header_path = path.to_string_lossy();
        assert!(
            out.contains(&format!("--- {header_path}")),
            "dry_run must include diff: {out}"
        );
        assert!(
            out.contains(&format!("+++ {header_path}")),
            "dry_run must include diff: {out}"
        );
        assert!(
            !out.contains("a//") && !out.contains("b//"),
            "diff header must not double-slash absolute paths: {out}"
        );
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
        let _guard = DELETE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        // 默认落点在工作区内 → 展示给相对形（短，且与既有模型文案口径一致）。
        assert!(
            trash_path.starts_with(".qaqh/trash/"),
            "工作区内 trash 应给相对形，实得 {trash_path}"
        );
        assert!(
            dir.path().join(trash_path).exists(),
            "trash 副本必须真的存在：{trash_path}"
        );
        // 默认落点仍在工作区内：那是 rename 快路径的前提（跨卷 rename 会 EXDEV，
        // 默认搬出去 = 每次删除都退化成整份复制 / 目录直接拒删）。
        // 它不再污染变更审计，靠 PR-B1 的 `.qaqh` 排除 + `{name}.` 前缀匹配保证，
        // 那两层由 qaqh-spy 的 trashed_directory_copy_is_excluded 钉住。
        // 名字带时间戳后缀，同名连续删除不得互相覆盖。
        let file_name = Path::new(trash_path)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(
            file_name.starts_with("delete-me.txt."),
            "trash 条目名应带时间戳后缀，实得 {file_name}"
        );
    }

    /// 展示面契约：删除必须下发 `display.diff`。语义是「整份内容被移出」，
    /// 所以在 unified 视角里是全 `-` 行（与 `git diff` 对删除的表示一致）。
    /// 缺这一段，抽屉对删除就只能显示「没有可展示的对比内容」。
    #[test]
    fn delete_ships_a_diff_of_the_removed_content() {
        let _guard = DELETE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gone.txt");
        std::fs::write(&path, "keep\r\nbye\r\n").unwrap();
        let outcome = TypedToolAdapter::new(DeleteTool)
            .execute(test_ctx(dir.path()), json!({"path": path}))
            .expect("delete must not return fatal");
        let diff = outcome
            .display
            .diff
            .clone()
            .expect("delete 必须下发 display.diff");
        // CRLF 盘面规整成 LF 再算：删除就是两行 `-`，不该变成换行符差异。
        assert!(diff.contains("-keep"), "got: {diff}");
        assert!(diff.contains("-bye"), "got: {diff}");
        assert!(diff.contains("+0,0"), "删除用 @@ … +0,0 表达: {diff}");
        assert!(!diff.contains("\r"), "diff 必须是 LF 视图: {diff}");
        assert!(!diff.contains("diff --git"), "不给 git 头: {diff}");
    }

    /// `QAQH_TRASH_DIR` 生效时备份必须完全落在工作区之外——这是"我愿意承担跨卷
    /// 复制代价，就是不想让 trash 待在工程目录里"的显式出口。
    #[test]
    fn delete_honors_trash_dir_override() {
        let _guard = DELETE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let path = dir.path().join("out-trash.txt");
        std::fs::write(&path, "bye\n").unwrap();

        // SAFETY: DELETE_LOCK 串行本模块所有 trash 相关用例；用完立刻撤掉。
        unsafe { std::env::set_var("QAQH_TRASH_DIR", outside.path()) };
        let result = TypedToolAdapter::new(DeleteTool)
            .execute(test_ctx(dir.path()), json!({ "path": path }))
            .expect("delete must not return fatal")
            .to_tool_result();
        unsafe { std::env::remove_var("QAQH_TRASH_DIR") };

        assert!(result.is_success(), "{}", result.model_text());
        let trash_path = result.data["trash_path"].as_str().unwrap();
        let trash = Path::new(trash_path);
        // 搬出工作区后相对形就是假线索，必须给绝对路径。
        assert!(
            trash.is_absolute(),
            "工作区外的 trash 必须给绝对路径，实得 {trash_path}"
        );
        assert!(trash.exists(), "覆盖后的 trash 必须存在：{trash_path}");
        assert!(
            trash.starts_with(outside.path()),
            "QAQH_TRASH_DIR 必须生效，实得 {trash_path}"
        );
        assert!(
            !trash.starts_with(dir.path()),
            "覆盖后不得有任何副本留在工作区内：{trash_path}"
        );
        // 工作区里除了被删文件消失，不该多出 .qaqh/trash。
        assert!(
            !dir.path().join(".qaqh").join("trash").exists(),
            "覆盖生效后不得回退创建 .qaqh/trash"
        );
    }

    #[test]
    fn delete_missing_file_is_a_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.txt");
        let result = execute_delete(dir.path(), json!({"path": path}));
        assert!(!result.is_success());
        assert_eq!(result.error.as_ref().unwrap().code, "not_found");
        assert!(result.model_text().contains("not_found"));
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
        assert_eq!(result.error.as_ref().unwrap().code, "stale_file");
        assert!(path.exists());
    }
}
