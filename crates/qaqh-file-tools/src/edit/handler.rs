//! handler — 三字段 str_replace 入口（`path` / `old_str` / `new_str`）。

use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::ToolRisk;
use crate::edit::core::{ReplaceError, count_lf, render_ambiguous, render_not_found, run_replace};
use crate::file_mutate::{
    ambient_tool_context, mutation_display, mutation_error, mutation_error_with_retryable,
    resolve_mutation_path,
};
use crate::file_shared::{
    Ending, LineEndings, atomic_write, ending_at, normalize_newlines, raw_offset_for_lf_offset,
    unified_diff,
};
use crate::tool_api::{
    ErasedTool, ToolCallContext, ToolContentBlock, ToolDisplay, ToolExecutionError, ToolMeta,
    ToolProjection, TypedTool, TypedToolAdapter,
};

/// Replace an exact string in a file.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditArgs {
    /// Target file
    pub path: String,
    /// Exact text to replace (must appear exactly once)
    pub old_str: String,
    /// Replacement text
    pub new_str: String,
    /// Replace every occurrence (default false)
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EditOutput {
    pub timeis: String,
    pub status: String,
    pub path: String,
    pub replaced: usize,
    pub lines: Vec<usize>,
    pub lines_added: u32,
    pub lines_removed: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_changed_line: Option<u32>,
    #[serde(skip)]
    #[schemars(skip)]
    model_text: String,
    #[serde(skip)]
    #[schemars(skip)]
    diff: Option<String>,
}

impl ToolProjection for EditOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: self.model_text.clone(),
        }]
    }

    fn display(&self, args: &Value) -> ToolDisplay {
        mutation_display(
            args.get("path").and_then(Value::as_str),
            &self.path,
            crate::tool_api::PathOp::Edit,
            "edit",
            Some(crate::file_mutate::mutation_change_summary(
                &self.path,
                self.lines_added,
                self.lines_removed,
            )),
            &self.model_text,
            self.diff.clone(),
        )
        .with_lines(self.lines_added, self.lines_removed)
    }
}

pub struct EditTool;

impl TypedTool for EditTool {
    type Args = EditArgs;
    type Output = EditOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "edit",
            "Replace an exact string in a file. 'old_str' must appear exactly once; set 'replace_all' to true to replace every occurrence. Do not include read's 'L<n>: ' prefix. Failures return a diff against the closest match.",
            crate::permission::ToolCategory::Write,
            ToolRisk::Write,
            Duration::from_secs(60),
        )
    }

    #[allow(clippy::result_large_err)] // ToolExecutionError is the frozen typed boundary.
    fn run(
        &self,
        ctx: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        let EditArgs {
            path: raw_path,
            old_str: old_arg,
            new_str: new_arg,
            replace_all,
        } = args;
        if old_arg.is_empty() {
            return Err(mutation_error(
                "parse_error",
                "edit: 'old_str' must be non-empty (use write to create a file)",
                None,
                json!({}),
            ));
        }
        if old_arg == new_arg {
            return Err(mutation_error(
                "parse_error",
                "edit: 'old_str' and 'new_str' are identical",
                None,
                json!({}),
            ));
        }

        let path = resolve_mutation_path(ctx, &raw_path);
        // 写策略：拒绝符号链接（不替换链接、不穿透写）与设备/FIFO/目录。
        if let Err(guard) = crate::file_shared::ensure_writable_regular_target(&path) {
            let code = guard.code();
            let text = format!("[ERROR] edit {raw_path}\n  {code}: {}\n", guard.message());
            return Err(mutation_error(
                code,
                text,
                guard.hint().as_deref(),
                json!({}),
            ));
        }
        let raw = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(mutation_error(
                    "file_not_found",
                    format!("edit: {raw_path}: file not found (use write to create a new file)"),
                    None,
                    json!({}),
                ));
            }
            Err(error) => {
                return Err(mutation_error(
                    "read_failed",
                    format!("edit: cannot read {raw_path}: {error}"),
                    None,
                    json!({}),
                ));
            }
        };
        let raw = match String::from_utf8(raw) {
            Ok(raw) => raw,
            Err(_) => {
                return Err(mutation_error(
                    "not_utf8_text",
                    format!("edit: {raw_path} is not valid UTF-8 text (treated as binary)"),
                    None,
                    json!({}),
                ));
            }
        };

        // LF 规范视图：匹配与行号都在该视图上算；写回把命中区间映射回原始字节，
        // 只对插入文本按命中行的行尾还原，文件其余字节原样保留。
        let (content, endings) = normalize_newlines(&raw);
        let (old, _) = normalize_newlines(&old_arg);
        let (new, _) = normalize_newlines(&new_arg);

        let outcome = match run_replace(&content, &old, &new, replace_all) {
            Ok(outcome) => outcome,
            Err(ReplaceError::NotFound { nearest }) => {
                let text = render_not_found(&raw_path, &old, nearest.as_ref());
                return Err(mutation_error_with_retryable(
                    "not_found",
                    text,
                    Some(true),
                    Some("Re-read the file and retry with the exact text."),
                    json!({
                        "timeis": crate::now_utc8(),
                        "status": "error",
                        "code": "not_found",
                        "path": &raw_path,
                    }),
                ));
            }
            Err(ReplaceError::Ambiguous { total, occurrences }) => {
                let text = render_ambiguous(&raw_path, total, &occurrences);
                return Err(mutation_error_with_retryable(
                    "ambiguous_match",
                    text,
                    Some(true),
                    Some("Extend 'old_str' with surrounding lines to make it unique."),
                    json!({
                        "timeis": crate::now_utc8(),
                        "status": "error",
                        "code": "ambiguous_match",
                        "path": &raw_path,
                        "match_count": total,
                    }),
                ));
            }
        };

        let write_content = splice_raw(&raw, &new, &outcome.matches, endings);
        if let Err(error) = atomic_write(&path, &write_content) {
            return Err(mutation_error(
                "write_failed",
                format!(
                    "edit: atomic write failed for {raw_path}: {error} — the file on disk was NOT modified"
                ),
                None,
                json!({}),
            ));
        }
        // 台账 + 行号偏移链（delta=0 时 record_edit_with_shifts 内部跳过）。
        crate::file_state::record_edit_with_shifts(&path, &write_content, &outcome.shifts);
        crate::journal::record_change(
            &ctx.session_id,
            "",
            "edit",
            &raw_path,
            "replace",
            Some(&raw),
            Some(&write_content),
            "ok",
        );

        // 模型面：只回紧凑回执（pass）。diff 走展示面（with_diff），不进模型投影。
        let model_text = if outcome.replaced == 1 {
            let start = outcome.lines[0];
            let end = start + count_lf(&new);
            format!("[OK] edit {raw_path}\n  replaced 1 occurrence (L{start}-L{end})\n")
        } else {
            let shown = outcome
                .lines
                .iter()
                .take(5)
                .map(|line| format!("L{line}"))
                .collect::<Vec<_>>()
                .join(", ");
            let more = if outcome.lines.len() > 5 { ", …" } else { "" };
            format!(
                "[OK] edit {raw_path}\n  replaced {} occurrences ({shown}{more})\n",
                outcome.replaced
            )
        };
        let diff_text = unified_diff(&content, &outcome.edited, &raw_path);
        let has_diff = !diff_text.is_empty();
        let (lines_added, lines_removed, first_line) =
            crate::file_shared::diff_stats_between(&content, &outcome.edited);
        Ok(EditOutput {
            timeis: crate::now_utc8(),
            status: "ok".to_string(),
            path: raw_path,
            replaced: outcome.replaced,
            lines: outcome.lines,
            lines_added,
            lines_removed,
            first_changed_line: has_diff.then_some(first_line),
            model_text,
            diff: has_diff.then_some(diff_text),
        })
    }
}

/// 把 LF 视图上的命中区间映射回原始内容并替换；插入文本按命中行的行尾还原
/// （末行无行尾 → 文件首选行尾）。倒序替换，保证前面的偏移不受影响。
fn splice_raw(raw: &str, new_lf: &str, matches: &[(usize, usize)], endings: LineEndings) -> String {
    let mut out = raw.to_string();
    for &(start_lf, end_lf) in matches.iter().rev() {
        let (Some(start), Some(end)) = (
            raw_offset_for_lf_offset(raw, start_lf),
            raw_offset_for_lf_offset(raw, end_lf),
        ) else {
            continue;
        };
        let ending = ending_at(raw, start).unwrap_or(endings.preferred);
        let replacement = match ending {
            Ending::Lf => new_lf.to_string(),
            Ending::Crlf => new_lf.replace('\n', "\r\n"),
        };
        out.replace_range(start..end, &replacement);
    }
    out
}

pub fn register(mgr: &mut impl qaqh_tool_core::tool_api::RegistersTyped) {
    mgr.register_typed_tool(EditTool);
}

/// Compatibility entry retained for existing in-process callers/tests.
///
/// Production registration uses [`EditTool`] directly. The pre-check keeps the
/// legacy `PARSE_ERROR` shape for malformed calls while valid calls reuse the
/// typed implementation.
pub fn exec_edit(args: &Value) -> crate::ToolResult {
    if let Some(result) = legacy_parse_error(args) {
        return result;
    }
    let ctx = ambient_tool_context("edit-compat", Duration::from_secs(60));
    TypedToolAdapter::new(EditTool)
        .execute(ctx, args.clone())
        .unwrap_or_else(|fatal| panic!("edit tool fatal: {}", fatal.message))
        .to_tool_result()
}

fn legacy_parse_error(args: &Value) -> Option<crate::ToolResult> {
    let fail = |code: &str, message: String| {
        crate::ToolResult::error_data(code, message, false, None, json!({}))
    };
    let Some(raw_path) = args
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
    else {
        return Some(fail("parse_error", "edit: missing 'path'".to_string()));
    };
    let Some(old_arg) = args.get("old_str").and_then(Value::as_str) else {
        return Some(fail("parse_error", "edit: missing 'old_str'".to_string()));
    };
    let Some(new_arg) = args.get("new_str").and_then(Value::as_str) else {
        return Some(fail("parse_error", "edit: missing 'new_str'".to_string()));
    };
    if old_arg.is_empty() {
        return Some(fail(
            "parse_error",
            "edit: 'old_str' must be non-empty (use write to create a file)".to_string(),
        ));
    }
    if old_arg == new_arg {
        return Some(fail(
            "parse_error",
            "edit: 'old_str' and 'new_str' are identical".to_string(),
        ));
    }
    let _ = raw_path;
    None
}
