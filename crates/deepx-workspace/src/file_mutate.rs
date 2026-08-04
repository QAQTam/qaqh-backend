//! Mutation tools: write, delete（统一编辑入口见 file_edit.rs 的 edit_file）。

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::file_shared::{
    atomic_write, diff_stats, normalize_newlines, unified_diff, verify_expected_hash,
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
        "[ERROR] Cannot write {}: {}\n[HINT] Verify the parent directory exists and is writable. Use exec with argv [\"ls\", \"-la\"] to check.",
        path, error
    )
}

// ── Helpers from file_edit ──

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
                        "[OK] {path}:{first_line} +{line_count} -0 | write\n\n+{content_trim}",
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
                        format_diff_result("OK", &path, &diff, "write", true)
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
            "hint": "Use exec with argv [\"ls\", \"-la\"] to verify."
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

// ── Registration ──

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register(ToolHandler {
        key: "write".to_string(),
        description: "Create, overwrite, or append to a file. Use for whole-file creation/overwrite/append; use edit_file for targeted changes.",
        input_schema: serde_json::json!({"type":"object","properties":{"path":{"type":"string","description":"File path"},"content":{"type":"string","description":"Content to write"},"append":{"type":"boolean","description":"If true, append to file instead of overwriting","default":false},"expected_hash":{"type":"string","description":"Optional hash returned by read. Write fails safely if the file changed."}},"required":["path","content"],"additionalProperties":false}),
        handler: handle_write_file,
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
