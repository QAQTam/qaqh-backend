//! handler — 三字段 str_replace 入口（`path` / `old_str` / `new_str`）。

use crate::edit::core::{ReplaceError, count_lf, render_ambiguous, render_not_found, run_replace};
use crate::file_shared::{
    Ending, LineEndings, atomic_write, ending_at, normalize_newlines, raw_offset_for_lf_offset,
    unified_diff,
};
use crate::{ToolCallCtx, ToolHandler, ToolManager, ToolResult, ToolRisk};
use serde_json::{Value, json};

pub fn exec_edit(args: &serde_json::Value) -> ToolResult {
    let fail = |code: &str, message: String, retryable: bool, hint: Option<&str>| {
        ToolResult::error_data(
            code,
            message,
            retryable,
            hint.map(str::to_string),
            serde_json::json!({}),
        )
    };

    let raw_path = match args
        .get("path")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        Some(p) => p,
        None => return fail("PARSE_ERROR", "edit: missing 'path'".into(), false, None),
    };
    let old_arg = match args.get("old_str").and_then(Value::as_str) {
        Some(s) => s,
        None => return fail("PARSE_ERROR", "edit: missing 'old_str'".into(), false, None),
    };
    let new_arg = match args.get("new_str").and_then(Value::as_str) {
        Some(s) => s,
        None => return fail("PARSE_ERROR", "edit: missing 'new_str'".into(), false, None),
    };
    let replace_all = args
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if old_arg.is_empty() {
        return fail(
            "PARSE_ERROR",
            "edit: 'old_str' must be non-empty (use write to create a file)".into(),
            false,
            None,
        );
    }
    if old_arg == new_arg {
        return fail(
            "PARSE_ERROR",
            "edit: 'old_str' and 'new_str' are identical".into(),
            false,
            None,
        );
    }

    let path = crate::resolve_workspace_path(raw_path);
    // 写策略：拒绝符号链接（不替换链接、不穿透写）与设备/FIFO/目录。
    if let Err(guard) = crate::file_shared::ensure_writable_regular_target(&path) {
        let code = guard.code();
        let text = format!("[ERROR] edit {raw_path}\n  {code}: {}\n", guard.message());
        return ToolResult::error_data(code, text, false, guard.hint(), json!({}));
    }
    let raw = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return fail(
                "FILE_NOT_FOUND",
                format!("edit: {raw_path}: file not found (use write to create a new file)"),
                false,
                None,
            );
        }
        Err(e) => {
            return fail(
                "READ_FAILED",
                format!("edit: cannot read {raw_path}: {e}"),
                false,
                None,
            );
        }
    };
    let raw = match String::from_utf8(raw) {
        Ok(s) => s,
        Err(_) => {
            return fail(
                "NOT_UTF8_TEXT",
                format!("edit: {raw_path} is not valid UTF-8 text (treated as binary)"),
                false,
                None,
            );
        }
    };

    // LF 规范视图：匹配与行号都在该视图上算；写回把命中区间映射回原始字节，
    // 只对插入文本按命中行的行尾还原，文件其余字节原样保留。
    let (content, endings) = normalize_newlines(&raw);
    let (old, _) = normalize_newlines(old_arg);
    let (new, _) = normalize_newlines(new_arg);

    let outcome = match run_replace(&content, &old, &new, replace_all) {
        Ok(outcome) => outcome,
        Err(ReplaceError::NotFound { nearest }) => {
            let text = render_not_found(raw_path, &old, nearest.as_ref());
            let data = json!({
                "timeis": crate::now_utc8(),
                "status": "error",
                "code": "NOT_FOUND",
                "path": raw_path,
            });
            return ToolResult::error_data(
                "NOT_FOUND",
                text,
                true,
                Some("Re-read the file and retry with the exact text.".to_string()),
                data,
            );
        }
        Err(ReplaceError::Ambiguous { total, occurrences }) => {
            let text = render_ambiguous(raw_path, total, &occurrences);
            let data = json!({
                "timeis": crate::now_utc8(),
                "status": "error",
                "code": "AMBIGUOUS_MATCH",
                "path": raw_path,
                "match_count": total,
            });
            return ToolResult::error_data(
                "AMBIGUOUS_MATCH",
                text,
                true,
                Some("Extend 'old_str' with surrounding lines to make it unique.".to_string()),
                data,
            );
        }
    };

    let write_content = splice_raw(&raw, &new, &outcome.matches, endings);
    if let Err(e) = atomic_write(&path, &write_content) {
        return fail(
            "WRITE_FAILED",
            format!(
                "edit: atomic write failed for {raw_path}: {e} — the file on disk was NOT modified"
            ),
            false,
            None,
        );
    }
    // 台账 + 行号偏移链（delta=0 时 record_edit_with_shifts 内部跳过）。
    crate::file_state::record_edit_with_shifts(&path, &write_content, &outcome.shifts);
    crate::journal::record_change(
        &crate::journal::active_session(),
        "",
        "edit",
        raw_path,
        "replace",
        Some(&raw),
        Some(&write_content),
        "ok",
    );

    // 模型面：只回紧凑回执（pass）。diff 走展示面（with_diff），不进模型投影。
    let text = if outcome.replaced == 1 {
        let start = outcome.lines[0];
        let end = start + count_lf(&new);
        format!("[OK] edit {raw_path}\n  replaced 1 occurrence (L{start}-L{end})\n")
    } else {
        let shown = outcome
            .lines
            .iter()
            .take(5)
            .map(|l| format!("L{l}"))
            .collect::<Vec<_>>()
            .join(", ");
        let more = if outcome.lines.len() > 5 { ", …" } else { "" };
        format!(
            "[OK] edit {raw_path}\n  replaced {} occurrences ({shown}{more})\n",
            outcome.replaced
        )
    };
    let mut result = ToolResult::ok_data(
        json!({
            "timeis": crate::now_utc8(),
            "status": "ok",
            "path": raw_path,
            "replaced": outcome.replaced,
            "lines": outcome.lines,
        }),
        text,
    );
    let diff = unified_diff(&content, &outcome.edited, raw_path);
    if !diff.is_empty() {
        result = result.with_diff(diff);
    }
    result
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

pub(crate) fn handle_edit(ctx: ToolCallCtx) -> ToolResult {
    exec_edit(&ctx.args)
}

pub fn register(mgr: &mut ToolManager) {
    mgr.register_display("edit", crate::display::project_edit);
    mgr.register(ToolHandler {
        key: "edit".to_string(),
        description: "Replace an exact string in a file. 'old_str' must appear exactly once; set 'replace_all' to true to replace every occurrence. Do not include read's 'L<n>: ' prefix. Failures return a diff against the closest match.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Target file"},
                "old_str": {"type": "string", "description": "Exact text to replace (must appear exactly once)"},
                "new_str": {"type": "string", "description": "Replacement text"},
                "replace_all": {"type": "boolean", "default": false, "description": "Replace every occurrence (default false)"}
            },
            "required": ["path", "old_str", "new_str"],
            "additionalProperties": false
        }),
        handler: handle_edit,
        risk: ToolRisk::Write,
        category: crate::permission::ToolCategory::Write,
        default_timeout: std::time::Duration::from_secs(60),
    });
}
