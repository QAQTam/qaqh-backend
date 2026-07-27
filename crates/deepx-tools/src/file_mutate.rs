//! Mutation tools: file write, edit, edit_diff, delete, move, copy.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::file_shared::{
    apply_diff_and_format, atomic_write, closest_line, diff_stats, disambiguate_match,
    is_binary_read_error, normalize_newlines, unified_diff, verify_expected_hash,
};
use crate::{handler_from_string, JsonArgs, ToolCallCtx, ToolHandler, ToolResult, ToolRisk};

// ── Shared helpers ──

fn format_diff_result(prefix: &str, path: &str, diff: &str, label: &str, _success: bool) -> String {
    let (added, removed, first_line) = diff_stats(diff);
    let summary = format!(
        "[{prefix}] {path}:{first_line} +{added} -{removed} | {label}",
        added = added.max(1),
        removed = removed.max(1)
    );
    // Always include the diff body — LLM context is truncated later in build_context_for_gate.
    // The frontend and audit trail need the full diff.
    format!("{}\n\n{}", summary, diff.trim_end())
}

fn write_error(path: &str, error: impl std::fmt::Display) -> String {
    format!(
        "[ERROR] Cannot write {}: {}\n[HINT] Verify the parent directory exists and is writable. Use list on its parent directory or exec with argv [\"ls\", \"-la\"] to check.",
        path, error
    )
}

// ── Strict unified patch tool ──

#[derive(Debug)]
struct PatchHunk {
    old_start: usize,
    old_len: usize,
    new_len: usize,
    lines: Vec<(char, String)>,
}

fn parse_hunk_range(value: &str) -> Result<(usize, usize), String> {
    let (start, len) = value.split_once(',').unwrap_or((value, "1"));
    let start = start.parse::<usize>().map_err(|_| "invalid hunk start")?;
    let len = len.parse::<usize>().map_err(|_| "invalid hunk length")?;
    if start == 0 && len != 0 {
        return Err("hunk start must be positive unless the range is empty".into());
    }
    Ok((start, len))
}

fn parse_unified_patch(patch: &str, expected_path: &str) -> Result<Vec<PatchHunk>, String> {
    let lines: Vec<&str> = patch.lines().collect();
    if lines.len() < 3 {
        return Err("patch must contain ---/+++ headers and at least one hunk".into());
    }
    let old = lines[0]
        .strip_prefix("--- a/")
        .ok_or("expected '--- a/<path>' header")?;
    let new = lines[1]
        .strip_prefix("+++ b/")
        .ok_or("expected '+++ b/<path>' header")?;
    if old != expected_path || new != expected_path {
        return Err("patch header path must exactly match path".into());
    }
    if expected_path.is_empty()
        || expected_path.contains('\\')
        || expected_path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err("path must be a non-empty workspace-relative slash-separated path".into());
    }

    let mut index = 2;
    let mut hunks = Vec::new();
    while index < lines.len() {
        let header = lines[index];
        if header.starts_with("\\ No newline") {
            return Err("patches without a terminal newline are not supported".into());
        }
        let body = header
            .strip_prefix("@@ -")
            .and_then(|value| value.split_once(" +"))
            .ok_or("invalid hunk header")?;
        let (old_start, old_len) = parse_hunk_range(body.0)?;
        let new_range = body.1.split_once(" @@").ok_or("invalid hunk header")?.0;
        let (_new_start, new_len) = parse_hunk_range(new_range)?;
        index += 1;
        let mut hunk_lines = Vec::new();
        let mut removed = 0;
        let mut added = 0;
        while index < lines.len() && !lines[index].starts_with("@@ -") {
            let line = lines[index];
            if line.starts_with("\\ No newline") {
                return Err("patches without a terminal newline are not supported".into());
            }
            let (kind, content) = line
                .chars()
                .next()
                .filter(|kind| matches!(kind, ' ' | '+' | '-'))
                .map(|kind| (kind, line[1..].to_string()))
                .ok_or("invalid hunk line")?;
            if kind != '+' {
                removed += 1;
            }
            if kind != '-' {
                added += 1;
            }
            hunk_lines.push((kind, content));
            index += 1;
        }
        if removed != old_len || added != new_len {
            return Err("hunk line counts do not match its header".into());
        }
        hunks.push(PatchHunk {
            old_start,
            old_len,
            new_len,
            lines: hunk_lines,
        });
    }
    if hunks.is_empty() {
        return Err("patch contains no hunks".into());
    }
    Ok(hunks)
}

fn apply_unified_patch(content: &str, hunks: &[PatchHunk]) -> Result<String, String> {
    let had_final_newline = content.ends_with('\n');
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
    let mut offset: isize = 0;
    for hunk in hunks {
        let base = if hunk.old_start == 0 {
            0
        } else {
            hunk.old_start - 1
        };
        let start = (base as isize + offset) as usize;
        if start > lines.len() || start + hunk.old_len > lines.len() {
            return Err("hunk target is outside the current file".into());
        }
        let mut cursor = start;
        let mut replacement = Vec::new();
        for (kind, expected) in &hunk.lines {
            match kind {
                ' ' | '-' => {
                    if lines.get(cursor) != Some(expected) {
                        return Err(format!(
                            "hunk does not match current file at line {}",
                            cursor + 1
                        ));
                    }
                    if *kind == ' ' {
                        replacement.push(expected.clone());
                    }
                    cursor += 1;
                }
                '+' => replacement.push(expected.clone()),
                _ => unreachable!("parser limits patch line kinds"),
            }
        }
        lines.splice(start..cursor, replacement);
        offset += hunk.new_len as isize - hunk.old_len as isize;
    }
    let mut output = lines.join("\n");
    if had_final_newline && !output.ends_with('\n') {
        output.push('\n');
    }
    Ok(output)
}

pub(super) fn exec_patch_file(args: &serde_json::Value) -> String {
    let requested_path = args.s("path");
    if requested_path.is_empty() || std::path::Path::new(&requested_path).is_absolute() {
        return "[ERROR] patch: path must be a non-empty workspace-relative path".into();
    }
    let expected_hash = args.s("expected_hash");
    if expected_hash.is_empty() {
        return "[ERROR] patch: expected_hash is required; call read first and use its hash".into();
    }
    let patch = args.s("patch");
    let hunks = match parse_unified_patch(&patch, &requested_path.replace('\\', "/")) {
        Ok(hunks) => hunks,
        Err(error) => return format!("[ERROR] patch: {error}"),
    };
    let workspace = crate::CURRENT_WORKSPACE
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let workspace = match std::fs::canonicalize(&workspace) {
        Ok(path) => path,
        Err(_) => return "[ERROR] patch: an existing workspace is required".into(),
    };
    let path = match std::fs::canonicalize(crate::resolve_workspace_path(&requested_path)) {
        Ok(path) if path.starts_with(&workspace) => path,
        Ok(_) => return "[ERROR] patch: target resolves outside the workspace".into(),
        Err(error) => return format!("[ERROR] Cannot read {requested_path}: {error}"),
    };
    let path = path.to_string_lossy().to_string();
    let raw = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) => return format!("[ERROR] Cannot read {path}: {error}"),
    };
    let (original, was_crlf) = normalize_newlines(&raw);
    if let Err(error) = verify_expected_hash(&path, &original, Some(&expected_hash)) {
        return format!("[ERROR] {error}");
    }
    let updated = match apply_unified_patch(&original, &hunks) {
        Ok(content) => content,
        Err(error) => return format!("[ERROR] patch: {error}\n[HINT] Read the file again and generate a patch with exact context."),
    };
    let diff = unified_diff(&original, &updated, &requested_path);
    if args.opt_bool("dry_run").unwrap_or(false) {
        return format_diff_result("DRY RUN", &path, &diff, "patch", false);
    }
    let write_content = if was_crlf {
        updated.replace('\n', "\r\n")
    } else {
        updated.clone()
    };
    match atomic_write(&path, &write_content) {
        Ok(_) => {
            crate::file_state::record_edit(&path, 0);
            format_diff_result("OK", &path, &diff, "patch", true)
        }
        Err(error) => format!("[ERROR] Cannot write {path}: {error}"),
    }
}

handler_from_string!(handle_patch_file, exec_patch_file);

// ── Helpers from file_edit ──

enum Match {
    Ok,
    NoMatch { msg: String },
    Error { msg: String },
}

fn build_fuzzy_hint(content: &str, old: &str) -> String {
    if let Some((line_no, line)) = closest_line(content, old) {
        return format!(
            "\n[HINT] String not found exactly. Closest match at line {line_no}: \"{}\"\n       Retry with edit_block start_line={} and old_lines set to the actual lines from the file.",
            line.chars().take(120).collect::<String>(),
            line_no
        );
    }
    "\n[HINT] String not found. Use read to check current file content, then retry.".to_string()
}

fn apply_one(
    content: &str,
    old: &str,
    new: &str,
    use_regex: bool,
    replace_all: bool,
    _path: &str,
) -> (String, Match) {
    if use_regex {
        let re = match regex::Regex::new(old) {
            Ok(r) => r,
            Err(e) => {
                return (
                    content.to_string(),
                    Match::Error {
                        msg: format!("Invalid regex: {e}"),
                    },
                );
            }
        };
        let count = re.find_iter(content).count();
        if count == 0 {
            return (
                content.to_string(),
                Match::NoMatch {
                    msg: format!("regex no matches"),
                },
            );
        }
        let escaped_new = new.replace('$', "$$");
        let new_content = if replace_all {
            re.replace_all(content, &escaped_new).to_string()
        } else {
            re.replacen(content, 1, &escaped_new).to_string()
        };
        let _msg = if replace_all {
            format!("regex replaced {count} matches")
        } else {
            "regex replaced 1 match".into()
        };
        (
            new_content,
            Match::Ok,
        )
    } else if replace_all {
        // Exact match first; fallback to trim_end for trailing-whitespace tolerance.
        let matcher: &str = if content.contains(old) {
            old
        } else {
            let trimmed = old.trim_end();
            if trimmed != old && content.contains(trimmed) {
                trimmed
            } else {
                let hint = build_fuzzy_hint(content, old);
                return (
                    content.to_string(),
                    Match::NoMatch {
                        msg: format!("no occurrences{hint}"),
                    },
                );
            }
        };
        let _count = content.matches(matcher).count();
        let new_content = content.replace(matcher, new);
        (new_content, Match::Ok)
    } else {
        match content.find(old) {
            Some(pos) => {
                let _line = content[..pos].lines().count() + 1;
                let new_content = content.replacen(old, new, 1);
                (
                    new_content,
                    Match::Ok,
                )
            }
            None => {
                let trimmed = old.trim_end();
                if trimmed != old {
                    match content.find(trimmed) {
                        Some(pos) => {
                            let _line = content[..pos].lines().count() + 1;
                            let new_content = content.replacen(trimmed, new, 1);
                            (
                                new_content,
                                Match::Ok,
                            )
                        }
                        None => {
                            let hint = build_fuzzy_hint(content, old);
                            (
                                content.to_string(),
                                Match::NoMatch {
                                    msg: format!("string not found{hint}"),
                                },
                            )
                        }
                    }
                } else {
                    let hint = build_fuzzy_hint(content, old);
                    (
                        content.to_string(),
                        Match::NoMatch {
                            msg: format!("string not found{hint}"),
                        },
                    )
                }
            }
        }
    }
}

// ── exec_write_file (from file_write.rs) ──

pub(super) fn exec_write_file(args: &serde_json::Value) -> String {
    let path = crate::resolve_workspace_path(&args.s("path"));
    let content = args.s("content");
    let append = args.opt_bool("append").unwrap_or(false);
    let expected_hash = args.s("expected_hash");
    if let Some(parent) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let line_count = content.lines().count();

    // Read old content if file exists (for diff on overwrite)
    let old_content = std::fs::read_to_string(&path).ok();
    let normalized_old = old_content
        .as_deref()
        .map(normalize_newlines)
        .map(|(content, _)| content)
        .unwrap_or_default();
    if let Err(error) = verify_expected_hash(&path, &normalized_old, Some(&expected_hash)) {
        return error;
    }

    if append {
        use std::io::Write;
        let mut file = match std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                return write_error(&path, e);
            }
        };
        match file.write_all(content.as_bytes()) {
            Ok(_) => {
                crate::file_state::record_write(&path, line_count);
                if let Some(ref old) = old_content {
                    let old_line_count = old.lines().count();
                    let first_line = if old_line_count == 0 {
                        1u32
                    } else {
                        old_line_count as u32 + 1
                    };
                    format!(
                        "[OK] {path}:{first_line} +{line_count} -0 | write_file\n\n+{content_trim}",
                        path = path,
                        first_line = first_line,
                        line_count = line_count,
                        content_trim = content.trim_end()
                    )
                } else {
                    format!(
                        "[OK] {} — appended {} bytes, {} lines (new file)",
                        path,
                        content.len(),
                        line_count
                    )
                }
            }
            Err(e) => write_error(&path, e),
        }
    } else {
        match atomic_write(&path, &content) {
            Ok(_) => {
                crate::file_state::record_write(&path, line_count);
                if let Some(ref old) = old_content {
                    // Overwrite: show full diff
                    let (old_norm, _) = normalize_newlines(old);
                    let (new_norm, _) = normalize_newlines(&content);
                    let diff = unified_diff(&old_norm, &new_norm, &path);
                    if diff.is_empty() {
                        format!(
                            "[OK] {} — {} bytes, {} lines (no changes)",
                            path,
                            content.len(),
                            line_count
                        )
                    } else {
                        format_diff_result("OK", &path, &diff, "write_file", true)
                    }
                } else {
                    format!(
                        "[OK] {} — {} bytes, {} lines (new file)",
                        path,
                        content.len(),
                        line_count
                    )
                }
            }
            Err(e) => write_error(&path, e),
        }
    }
}

handler_from_string!(handle_write_file, exec_write_file);

// ── exec_edit_file (from file_edit.rs) ──

pub(super) fn exec_edit_file(args: &serde_json::Value) -> String {
    let path = crate::resolve_workspace_path(&args.s("path"));
    if path.is_empty() {
        return "[ERROR] edit_file: no path specified\n[HINT] Provide 'path' (string) to the file."
            .into();
    }
    let old_str = args.s("old_string");
    if old_str.is_empty() {
        return "[ERROR] edit_file: no old_string specified\n[HINT] Provide 'old_string' (text to find).".into();
    }
    let new_str = args.s("new_string");
    let replace_all = args.opt_bool("replace_all").unwrap_or(false);
    let use_regex = args.opt_bool("regex").unwrap_or(false);
    let dry_run = args.opt_bool("dry_run").unwrap_or(false);
    let expected_hash = args.s("expected_hash");

    let raw = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            if is_binary_read_error(&e.to_string()) {
                return format!("[PARTIAL] {path} — binary file, edit_file works on text only");
            }
            return format!("[ERROR] Cannot read {path}: {e}");
        }
    };

    let (orig, was_crlf) = normalize_newlines(&raw);
    if let Err(error) = verify_expected_hash(&path, &orig, Some(&expected_hash)) {
        return error;
    }
    if was_crlf {
        log::info!("edit_file: {path} had CRLF, normalized to LF");
    }

    let old = old_str.replace("\r\n", "\n").replace('\r', "\n");
    let new = new_str.replace("\r\n", "\n").replace('\r', "\n");
    let (content, m) = apply_one(&orig, &old, &new, use_regex, replace_all, &path);
    match m {
        Match::Ok => {}
        Match::NoMatch { msg } => {
            return format!(
                "[PARTIAL] {path} — pattern did not match\n[HINT] {msg}\n       Use read to check current content, then retry."
            );
        }
        Match::Error { msg } => {
            return format!("[ERROR] {path}: {msg}");
        }
    }

    if dry_run {
        let diff = unified_diff(&orig, &content, &path);
        return format_diff_result("DRY RUN", &path, &diff, "edit_file", false);
    }

    let write_content = if was_crlf {
        content.replace('\n', "\r\n")
    } else {
        content.clone()
    };
    match atomic_write(&path, &write_content) {
        Ok(_) => {
            crate::file_state::record_edit(&path, 0);
            let diff = unified_diff(&orig, &content, &path);
            format_diff_result("OK", &path, &diff, "edit_file", true)
        }
        Err(e) => format!("[ERROR] Cannot write {path}: {e}"),
    }
}

handler_from_string!(handle_edit_file, exec_edit_file);

// ── exec_delete_file (from file_delete.rs) ──

fn trash_dir() -> std::path::PathBuf {
    let dir = crate::workspace::deepx_dir().join("trash");
    let _ = std::fs::create_dir_all(&dir); // ensure exists
    dir
}

pub(super) fn exec_delete_file(args: &serde_json::Value) -> String {
    let path = crate::resolve_workspace_path(&args.s("path"));
    let p = std::path::Path::new(&path);
    if !p.exists() {
        return serde_json::json!({
            "timeis": crate::now_utc8(),
            "status": "error",
            "code": "NOT_FOUND",
            "path": path,
            "message": format!("{} does not exist", path),
            "hint": "Use list to verify."
        })
        .to_string();
    }

    let trash_root = trash_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let ws = crate::CURRENT_WORKSPACE
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let project_root = if !ws.is_empty() && ws != "." {
        Path::new(&ws)
    } else {
        Path::new(".")
    };
    let rel = if let Ok(stripped) = p.strip_prefix(project_root) {
        stripped.to_string_lossy().to_string()
    } else if let Some(name) = p.file_name() {
        name.to_string_lossy().to_string()
    } else {
        path.replace(['/', '\\', ':'], "__")
    };
    let safe_name = rel.replace(['/', '\\', ':'], "__");
    let trash_path = trash_root.join(format!("{}.{}", safe_name, ts));

    if let Some(parent) = trash_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    match std::fs::rename(p, &trash_path) {
        Ok(_) => {
            crate::file_state::record_delete(&path);
            serde_json::json!({
            "timeis": crate::now_utc8(),
            "status": "ok",
            "path": path,
            "trash_path": format!(".deepx/trash/{}", trash_path.file_name().unwrap_or_default().to_string_lossy()),
            "content": format!("Moved to trash: .deepx/trash/{}", trash_path.file_name().unwrap_or_default().to_string_lossy()),
            "hint": format!("Restore with exec argv [\"mv\", \"{}\", \"{}\"]", trash_path.display(), path),
        }).to_string()
        }
        Err(_e) => {
            if p.is_dir() {
                serde_json::json!({
                    "timeis": crate::now_utc8(),
                    "status": "error",
                    "code": "CROSS_DEVICE_DIR",
                    "path": path,
                    "message": "Cannot trash directory across devices",
                    "hint": format!("Use exec with argv [\"rm\", \"-rf\", \"{}\"] for cross-device deletion.", path),
                }).to_string()
            } else if let Err(e2) = std::fs::copy(p, &trash_path) {
                serde_json::json!({
                    "timeis": crate::now_utc8(),
                    "status": "error",
                    "code": "COPY_FAILED",
                    "path": path,
                    "message": e2.to_string(),
                    "hint": "Check permissions and disk space."
                })
                .to_string()
            } else {
                match std::fs::remove_file(p) {
                    Ok(_) => {
                        crate::file_state::record_delete(&path);
                        serde_json::json!({
                        "timeis": crate::now_utc8(),
                        "status": "ok",
                        "path": path,
                        "trash_path": format!(".deepx/trash/{}", trash_path.file_name().unwrap_or_default().to_string_lossy()),
                        "content": format!("Moved to trash (cross-device): .deepx/trash/{}", trash_path.file_name().unwrap_or_default().to_string_lossy()),
                        "hint": format!("Restore with exec argv [\"cp\", \"{}\", \"{}\"]", trash_path.display(), path),
                }).to_string()
                    }
                    Err(e2) => serde_json::json!({
                        "timeis": crate::now_utc8(),
                        "status": "ok",
                        "path": path,
                        "warning": format!("Copied to trash but could not remove original: {}", e2),
                        "content": format!("Copied to trash, original still at {}", path),
                    })
                    .to_string(),
                }
            }
        }
    }
}

handler_from_string!(handle_delete_file, exec_delete_file);

// ── exec_edit_fuzzy (was file_edit_diff) ──

pub(super) fn exec_edit_fuzzy(args: &serde_json::Value) -> String {
    let path =
        crate::resolve_workspace_path(args.get("path").and_then(|v| v.as_str()).unwrap_or(""));
    if path.is_empty() {
        return format!(
            "[ERROR] edit_block: MISSING_PATH — path is required\n[HINT] Provide a file path to edit."
        );
    }
    let old_lines: Vec<String> = args
        .get("old_lines")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let new_lines: Vec<String> = args
        .get("new_lines")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let context_before: Vec<String> = args
        .get("context_before")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let context_after: Vec<String> = args
        .get("context_after")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let description = args
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let dry_run = args
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let start_line: Option<usize> = args
        .get("start_line")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let end_line: Option<usize> = args
        .get("end_line")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let expected_hash = args.s("expected_hash");
    let allow_fuzzy = args.opt_bool("allow_fuzzy").unwrap_or(false);

    let err = |code: &str, msg: &str, hint: &str| -> String {
        format!("[ERROR] {path}: {code} — {msg}\n[HINT] {hint}")
    };

    if old_lines.is_empty() && start_line.is_none() {
        return err(
            "MISSING_PARAM",
            "old_lines or start_line is required",
            "Provide old_lines for content matching or start_line for line-number editing.",
        );
    }
    if old_lines.len() > 100 && start_line.is_none() {
        return err(
            "TOO_LARGE",
            &format!("old_lines too large ({} lines, max 100)", old_lines.len()),
            "Reduce the diff scope, use write for full rewrites, or set start_line (no old_lines needed) for line-range replacement.",
        );
    }

    let raw = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            if is_binary_read_error(&e.to_string()) {
                return serde_json::json!({
                    "timeis": crate::now_utc8(),
                    "status": "error",
                    "code": "BINARY_FILE",
                    "path": path,
                    "message": "Binary file, cannot display as text",
                    "hint": "Use exec with hex dump tool."
                })
                .to_string();
            }
            return err("READ_FAILED", &e.to_string(), "Use list first.");
        }
    };
    let (content, was_crlf) = normalize_newlines(&raw);
    if let Err(error) = verify_expected_hash(&path, &content, Some(&expected_hash)) {
        return error;
    }
    if was_crlf {
        log::info!(
            "file_edit_diff: {} had CRLF, normalized to LF for matching",
            path
        );
    }
    let file_lines: Vec<&str> = content.lines().collect();

    if let Some(start) = start_line {
        if old_lines.is_empty() && expected_hash.is_empty() {
            return err(
                "UNVERIFIED_LINE_EDIT",
                "line-number editing requires old_lines or expected_hash",
                "Read the target range and pass its old_lines (preferred), or pass the hash returned by read.",
            );
        }
        let s = start.saturating_sub(1);
        let e = end_line.map(|n| n.saturating_sub(1)).unwrap_or(s);
        if s >= file_lines.len() {
            return err(
                "LINE_OUT_OF_RANGE",
                &format!(
                    "start_line {start} past end of file ({} lines)",
                    file_lines.len()
                ),
                "Use read to check the file length.",
            );
        }
        let e = e.min(file_lines.len().saturating_sub(1));
        if s > e {
            return err(
                "LINE_RANGE_INVALID",
                &format!(
                    "start_line {start} > end_line {}",
                    end_line.unwrap_or(start)
                ),
                "end_line must be >= start_line.",
            );
        }
        let win = e - s + 1;
        if !old_lines.is_empty() {
            let actual: Vec<&str> = file_lines[s..=e].iter().map(|l| *l).collect();
            let norm_actual: Vec<String> =
                actual.iter().map(|l| l.trim_end().to_string()).collect();
            let norm_old: Vec<String> =
                old_lines.iter().map(|l| l.trim_end().to_string()).collect();
            if norm_actual != norm_old {
                let mut ctx = String::new();
                for (i, line) in actual.iter().enumerate() {
                    if i >= norm_old.len() || line.trim_end() != norm_old[i] {
                        ctx.push_str(&format!("  L{} actual: {}\n", s + i + 1, line));
                        if i < norm_old.len() {
                            ctx.push_str(&format!("  L{} old_lines: {}\n", s + i + 1, norm_old[i]));
                        }
                    }
                }
                return crate::json_err(
                    "LINE_MISMATCH",
                    &format!(
                        "start_line={start}: old_lines do not match actual file content at lines {}-{}",
                        s + 1,
                        e + 1
                    ),
                    &format!(
                        "Mismatch:\n{ctx}File content has changed. Use read to re-read and retry with corrected old_lines."
                    ),
                );
            }
        }
        return apply_diff_and_format(
            &path,
            &file_lines,
            s,
            win,
            &new_lines,
            description,
            false,
            dry_run,
            was_crlf,
            content.ends_with('\n'),
        );
    }

    let norm_old: Vec<String> = old_lines.iter().map(|l| l.trim_end().to_string()).collect();
    let win = norm_old.len();
    if win > file_lines.len() {
        return err(
            "TOO_LARGE",
            &format!(
                "old_lines ({} lines) longer than file ({} lines)",
                win,
                file_lines.len()
            ),
            "Check the file content with read first.",
        );
    }

    let mut candidates: Vec<usize> = Vec::new();
    let mut was_fuzzy = false;
    for i in 0..=file_lines.len() - win {
        let window: Vec<String> = file_lines[i..i + win]
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect();
        if window == norm_old {
            candidates.push(i);
        }
    }
    if candidates.is_empty() && allow_fuzzy {
        was_fuzzy = true;
        for i in 0..=file_lines.len() - win {
            let window: Vec<String> = file_lines[i..i + win]
                .iter()
                .map(|l| l.trim_end().to_string())
                .collect();
            if window
                .iter()
                .zip(&norm_old)
                .all(|(w, o)| w.trim() == o.trim())
            {
                candidates.push(i);
            }
        }
    }
    if candidates.is_empty() {
        let first_old = old_lines.first().map(|l| l.trim()).unwrap_or("");
        if let Some((line_num, line_text)) = closest_line(&content, first_old) {
            return serde_json::json!({
                "timeis": crate::now_utc8(),
                "status": "error",
                "code": "NO_MATCH",
                "path": path,
                "message": "old_lines not found",
                "closest_line": line_num,
                "closest_text": line_text,
                "hint": format!("Use read first, then retry with start_line={line_num} or corrected old_lines."),
            }).to_string();
        }
        return err(
            "NO_MATCH",
            "old_lines not found",
            "Verify current file content or use start_line/end_line for line-number editing.",
        );
    }

    let match_idx = match disambiguate_match(
        &candidates,
        &context_before,
        &context_after,
        &file_lines,
        &path,
        win,
    ) {
        Ok(idx) => idx,
        Err(msg) => return msg,
    };

    apply_diff_and_format(
        &path,
        &file_lines,
        match_idx,
        win,
        &new_lines,
        description,
        was_fuzzy,
        dry_run,
        was_crlf,
        content.ends_with('\n'),
    )
}

handler_from_string!(handle_edit_file_diff, exec_edit_fuzzy);

// ── Registration ──

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register(ToolHandler {
        key: "patch".to_string(),
        description: "Apply one strict unified diff to an existing workspace file. Call read first and pass its hash as expected_hash. The ---/+++ patch headers must use the same relative path. Use dry_run=true to validate and preview before writing; patches with stale hashes or fuzzy context are rejected.",
        input_schema: serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"Workspace-relative target path, using / separators"},"patch":{"type":"string","description":"Strict unified diff for exactly this file, with --- a/<path> and +++ b/<path> headers"},"expected_hash":{"type":"string","description":"Required SHA-256 hash returned by read for the complete current file"},"dry_run":{"type":"boolean","description":"Validate and show the resulting diff without writing","default":false}},"required":["path","patch","expected_hash"],"additionalProperties":false}),
        handler: handle_patch_file,
        risk: ToolRisk::Write,
        default_timeout: std::time::Duration::from_secs(30),
    });
    mgr.register(ToolHandler {
        key: "write".to_string(),
        description: "Create, overwrite, or append to a file.",
        input_schema: serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"File path"},"content":{"type":"string","description":"Content to write"},"append":{"type":"boolean","description":"If true, append to file instead of overwriting","default":false},"expected_hash":{"type":"string","description":"Optional hash returned by read. Write fails safely if the file changed."}},"required":["path","content"],"additionalProperties":false}),
        handler: handle_write_file,
        risk: ToolRisk::Write,
        default_timeout: std::time::Duration::from_secs(30),
    });
    mgr.register(ToolHandler {
        key: "edit".to_string(),
        description: "String replacement in files. Supports exact match, regex (with capture groups). Set dry_run=true to preview the diff before applying. For fuzzy or line-number addressing use edit_block.",
        input_schema: serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"File path"},"old_string":{"type":"string","description":"Text to find"},"new_string":{"type":"string","description":"Replacement text"},"regex":{"type":"boolean","description":"Treat old_string as regex","default":false},"replace_all":{"type":"boolean","description":"Replace all occurrences","default":false},"dry_run":{"type":"boolean","description":"Preview diff only, do not write file. Use for complex edits; call again with false to apply.","default":false},"expected_hash":{"type":"string","description":"Optional hash returned by read. Edit fails safely if the file changed."}},"required":["path"],"additionalProperties":false}),
        handler: handle_edit_file,
        risk: ToolRisk::Write,
        default_timeout: std::time::Duration::from_secs(60),
    });
    mgr.register(ToolHandler {
        key: "edit_block".to_string(),
        description: "Multi-line edit with fuzzy matching. Provide new_lines to insert; use old_lines for content-based matching or start_line/end_line for line-number addressing. context_before/after disambiguate identical text.",
        input_schema: serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"File path"},"old_lines":{"type":"array","items":{"type":"string"},"description":"Lines to find and replace. Required for safe line-number edits unless expected_hash is supplied."},"new_lines":{"type":"array","items":{"type":"string"},"description":"Lines to insert. REQUIRED."},"context_before":{"type":"array","items":{"type":"string"},"description":"Lines just before the change, for disambiguation"},"context_after":{"type":"array","items":{"type":"string"},"description":"Lines just after the change, for disambiguation"},"start_line":{"type":"integer","description":"1-based line to start replacement at"},"end_line":{"type":"integer","description":"1-based line to end replacement at (inclusive, defaults to start_line)"},"expected_hash":{"type":"string","description":"Optional hash returned by read; required for line-number edits without old_lines."},"allow_fuzzy":{"type":"boolean","description":"Allow whitespace-normalized fallback matching. Default false for safety.","default":false},"dry_run":{"type":"boolean","description":"Preview diff only (default: false)","default":false},"description":{"type":"string","description":"Brief note explaining why this change is needed (optional)"}},"required":["path","new_lines"],"additionalProperties":false}),
        handler: handle_edit_file_diff,
        risk: ToolRisk::Write,
        default_timeout: std::time::Duration::from_secs(30),
    });
    mgr.register(ToolHandler {
        key: "delete".to_string(),
        description: "Move file to trash (.deepx/trash/) instead of permanent deletion.",
        input_schema: serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"File path to delete"}},"required":["path"],"additionalProperties":false}),
        handler: handle_delete_file,
        risk: ToolRisk::Destructive,
        default_timeout: std::time::Duration::from_secs(15),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(path: &std::path::Path, extra: serde_json::Value) -> serde_json::Value {
        let mut value = serde_json::json!({ "path": path.to_string_lossy() });
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        value
    }

    #[test]
    fn edit_block_preserves_terminal_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("example.txt");
        std::fs::write(&path, "first\nsecond\n").unwrap();

        let result = exec_edit_fuzzy(&args(
            &path,
            serde_json::json!({
                "old_lines": ["second"], "new_lines": ["changed"],
            }),
        ));

        assert!(result.starts_with("[OK]"), "{result}");
        assert_eq!(std::fs::read_to_string(path).unwrap(), "first\nchanged\n");
    }

    #[test]
    fn line_edit_without_content_or_hash_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("example.txt");
        std::fs::write(&path, "first\nsecond\n").unwrap();

        let result = exec_edit_fuzzy(&args(
            &path,
            serde_json::json!({
                "start_line": 2, "new_lines": ["changed"],
            }),
        ));

        assert!(result.contains("UNVERIFIED_LINE_EDIT"), "{result}");
        assert_eq!(std::fs::read_to_string(path).unwrap(), "first\nsecond\n");
    }

    #[test]
    fn stale_hash_prevents_string_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("example.txt");
        std::fs::write(&path, "before\n").unwrap();
        let stale_hash = super::super::file_shared::content_hash("before\n");
        std::fs::write(&path, "changed elsewhere\n").unwrap();

        let result = exec_edit_file(&args(
            &path,
            serde_json::json!({
                "old_string": "before", "new_string": "after", "expected_hash": stale_hash,
            }),
        ));

        assert!(result.contains("STALE_FILE"), "{result}");
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "changed elsewhere\n"
        );
    }
}
