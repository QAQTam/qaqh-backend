//! 独立补丁工具 `apply_patch`：**标准 git patch 语义**。
//!
//! 基于 libgit2（git2 crate）的 apply 引擎，行为与 git CLI 的 `git apply` 一致：
//! - **输入**：标准 unified diff（`git diff` / `format-patch` 输出，含
//!   `diff --git` 头）；裸 `---/+++` 格式自动补头（宽容输入）。
//! - **事务：整包**——任一 hunk 上下文不符 → 整个 patch 拒绝，磁盘零改动
//!   （libgit2 先全量校验再应用；与 edit_file 的每 op 独立事务不同）。
//! - **定位：行号 + 偏移容错**（git 语义）；patch 中的路径相对仓库根。
//! - 天然支持多文件：一个 patch 文本可含多个 `diff --git` 块。
//!
//! 与 `edit_file` 分工：edit_file 是结构化精确定位（内容锚定、严格拒绝歧义），
//! 适合模型逐步编辑；apply_patch 是标准 git 补丁合入，适合以 patch 文件/文本
//! 形式批量合入（模型输出 patch → 校验 → 合入），失败时整个 patch 需修正重发。

use crate::{ToolHandler, ToolResult, ToolRisk};

fn workspace_root() -> String {
    let ws = crate::CURRENT_WORKSPACE
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if ws.is_empty() { ".".to_string() } else { ws }
}

/// 执行 apply_patch：patch（必填）+ location（workdir/index/both）+ dry_run。
fn exec_apply_patch(args: &serde_json::Value) -> ToolResult {
    let patch = match args
        .get("patch")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
    {
        Some(p) => p,
        None => {
            return crate::ToolResult::error(serde_json::json!({
                "timeis": crate::now_utc8(),
                "status": "error",
                "code": "PARSE_ERROR",
                "message": "apply_patch: missing 'patch'",
                "hint": "Provide a standard unified diff (git diff / format-patch output, with 'diff --git' headers) in the patch parameter.",
            }).to_string());
        }
    };
    let location = args
        .get("location")
        .and_then(|x| x.as_str())
        .unwrap_or("workdir");
    let dry_run = args
        .get("dry_run")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);

    let ws = workspace_root();
    let result = if dry_run {
        crate::git::check_patch(&ws, patch)
    } else {
        crate::git::apply_patch(&ws, patch, location)
    };

    match result {
        Ok(meta) => {
            let v: serde_json::Value = serde_json::from_str(&meta).unwrap_or_default();
            let files = v["files"].as_u64().unwrap_or(0);
            let ins = v["insertions"].as_u64().unwrap_or(0);
            let del = v["deletions"].as_u64().unwrap_or(0);
            let offset = v["offset_used"].as_i64().unwrap_or(0);
            let touched = v["touched"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            // 账本同步：apply 直接走 libgit2 写盘，必须把 touched 文件的最新
            // 内容登记进 file_state，否则后续 edit_file 盲定位防漂移会误报。
            if !dry_run {
                for f in &touched {
                    let full = std::path::Path::new(&ws).join(f);
                    if let Ok(content) = std::fs::read_to_string(&full) {
                        crate::file_state::record_write(f, &content);
                    }
                }
            }

            let text = if dry_run {
                let pre = v["hunks"].as_array().map(|h| h.len()).unwrap_or(0);
                let mism = v["hunks"]
                    .as_array()
                    .map(|h| h.iter().filter(|x| x["context_match"] != true).count())
                    .unwrap_or(0);
                format!(
                    "[DRY RUN] apply_patch — patch parses: {files} file(s), +{ins} -{del}; {pre} hunk(s) pre-checked, {mism} context mismatch(es) (context is verified against the file; a real apply may still differ)\n"
                )
            } else if offset != 0 {
                format!(
                    "[OK] apply_patch — applied to {location}: {files} file(s), +{ins} -{del} (line numbers auto-corrected by {offset})\n"
                )
            } else {
                format!(
                    "[OK] apply_patch — applied to {location}: {files} file(s), +{ins} -{del}\n"
                )
            };

            let mut data = serde_json::json!({
                "timeis": crate::now_utc8(),
                "status": "ok",
                "dry_run": dry_run,
                "files": files,
                "insertions": ins,
                "deletions": del,
                "location": location,
            });
            if !dry_run {
                data["touched"] = serde_json::Value::Array(
                    touched
                        .iter()
                        .map(|s| serde_json::Value::String(s.clone()))
                        .collect(),
                );
                data["offset_used"] = serde_json::Value::Number(offset.into());
            } else if let Some(hunks) = v["hunks"].as_array() {
                data["hunks"] = serde_json::Value::Array(hunks.clone());
            }
            crate::ToolResult::ok_data(data, text)
        }
        Err(e) => {
            // git 语义失败：整包拒绝，磁盘零改动——错误已含诊断（文件/hunk/偏移）。
            crate::ToolResult::error(serde_json::json!({
                "timeis": crate::now_utc8(),
                "status": "error",
                "code": "APPLY_FAILED",
                "message": e,
                "hint": "The whole patch was rejected (no partial application). The diagnostics above tell you which file/hunk and by how much the line numbers are off. Fix the patch (line numbers and context lines) and resend the full corrected patch; or run dry_run=true for a pre-check with per-hunk context positions.",
            }).to_string())
        }
    }
}

fn handle_apply_patch(ctx: crate::ToolCallCtx) -> ToolResult {
    exec_apply_patch(&ctx.args)
}

// ─────────────────────────────────────────────────────────────
// Registration
// ─────────────────────────────────────────────────────────────

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_with_placement(ToolHandler {
        key: "apply_patch".to_string(),
        description: concat!(
            "Apply a standard git patch (unified diff, git diff / format-patch output) to the workspace repo — git apply semantics. ",
            "All-or-nothing: any hunk that does not apply rejects the whole patch with zero disk changes. ",
            "LLM-friendly auto-correction: hunk line-count declarations are recomputed if wrong, and line numbers within +/-5 are auto-corrected (offset search, like git CLI). ",
            "Paths are relative to the repo root; one patch may hold multiple 'diff --git' blocks (multi-file). Bare '---/+++' format is accepted. ",
            "Example:\n```\ndiff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -3,3 +3,3 @@\n line3\n-line4\n+LINE4\n line5\n```\n",
            "('@@ -3,3 +3,3 @@' = old file from line 3, 3 lines; new file same; ' ' = context, '-' = removed, '+' = added.) ",
            "dry_run=true pre-checks parsing and reports each hunk's context position without applying. ",
            "For structured single-file edits use edit_file."
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "patch": {"type": "string", "description": "Standard unified diff text (git diff / format-patch output with 'diff --git' headers). Multiple files allowed in one patch. Line numbers may be off by a few lines (auto-corrected); context lines must match the current file. Hunk line counts are auto-fixed if miscounted."},
                "location": {"type": "string", "enum": ["workdir", "index", "both"], "description": "Where to apply: working tree (default), index, or both", "default": "workdir"},
                "dry_run": {"type": "boolean", "description": "Pre-check parsing + per-hunk context positions; do not apply", "default": false}
            },
            "required": ["patch"],
            "additionalProperties": false
        }),
        handler: handle_apply_patch,
        risk: ToolRisk::Write,
        default_timeout: std::time::Duration::from_secs(60),
    },
    crate::ToolPlacement::Workspace,
);
}

// ─────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use git2::Repository;
    use std::path::Path;

    /// 建一个带初始 commit 的临时 git 仓库，返回 (tempdir, workspace path)。
    fn repo_with_commit(files: &[(&str, &str)]) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("temp repo");
        let repo = Repository::init(dir.path()).expect("init repo");
        let mut index = repo.index().expect("index");
        for (name, content) in files {
            let p = dir.path().join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, content).expect("write fixture");
            index.add_path(Path::new(name)).expect("stage fixture");
        }
        // 必须 write() 落盘：libgit2 的 commit 不会像 git CLI 那样自动刷新 index
        index.write().expect("write index");
        let tree_id = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_id).expect("find tree");
        let signature = git2::Signature::now("DeepX Test", "deepx-test@local").expect("signature");
        repo.commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .expect("initial commit");
        let ws = dir.path().to_str().unwrap().to_string();
        (dir, ws)
    }

    fn run_in(ws: &str, patch: &str, extra: serde_json::Value) -> serde_json::Value {
        let mut args = serde_json::json!({ "patch": patch });
        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                args[k] = v.clone();
            }
        }
        // 测试直接注入 workspace（避免依赖全局 CURRENT_WORKSPACE）
        crate::CURRENT_WORKSPACE
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clone_from(&ws.to_string());
        let result = exec_apply_patch(&args);
        let data = result.data.clone();
        if data.as_object().is_none_or(|o| o.is_empty()) {
            let raw = result.model_text();
            match serde_json::from_str::<serde_json::Value>(&raw) {
                Ok(v) if v.is_object() => v,
                _ => serde_json::json!({ "status": "error", "raw": raw }),
            }
        } else {
            data
        }
    }

    fn full_patch(path: &str, old_lines: &[&str], new_lines: &[&str], start: usize) -> String {
        // 构造标准 git 格式 patch（diff --git 头 + 行数声明）
        let mut s = format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n");
        s.push_str(&format!(
            "@@ -{start},{} +{start},{} @@\n",
            old_lines.len(),
            new_lines.len()
        ));
        for l in old_lines {
            s.push('-');
            s.push_str(l);
            s.push('\n');
        }
        for l in new_lines {
            s.push('+');
            s.push_str(l);
            s.push('\n');
        }
        s
    }

    #[test]
    fn patch_basic_apply() {
        let (dir, ws) = repo_with_commit(&[("a.txt", "line1\nline2\nline3\n")]);
        let patch = full_patch("a.txt", &["line2"], &["LINE2"], 2);
        let out = run_in(&ws, &patch, serde_json::json!({}));
        assert_eq!(out["status"], "ok", "got: {out}");
        assert_eq!(out["files"], 1);
        assert_eq!(out["insertions"], 1);
        assert_eq!(out["deletions"], 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "line1\nLINE2\nline3\n"
        );
    }

    #[test]
    fn patch_bare_format_is_normalized() {
        // 裸 ---/+++ 格式自动补 diff --git 头（宽容输入）
        let (dir, ws) = repo_with_commit(&[("a.txt", "one\ntwo\n")]);
        let patch = "--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n-one\n+ONE\n two\n";
        let out = run_in(&ws, &patch, serde_json::json!({}));
        assert_eq!(out["status"], "ok", "got: {out}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "ONE\ntwo\n"
        );
    }

    #[test]
    fn patch_multi_file_in_one_patch() {
        let (dir, ws) = repo_with_commit(&[("a.txt", "one\n"), ("b.txt", "two\n")]);
        let patch = format!(
            "{}{}",
            full_patch("a.txt", &["one"], &["ONE"], 1),
            full_patch("b.txt", &["two"], &["TWO"], 1)
        );
        let out = run_in(&ws, &patch, serde_json::json!({}));
        assert_eq!(out["status"], "ok", "got: {out}");
        assert_eq!(out["files"], 2);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "ONE\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "TWO\n"
        );
    }

    #[test]
    fn patch_context_mismatch_rejects_whole_patch() {
        // git 语义核心：任一 hunk 不符 → 整包拒绝，磁盘零改动（无部分应用）
        let (dir, ws) = repo_with_commit(&[("a.txt", "aaa\nbbb\nccc\n")]);
        let patch = full_patch("a.txt", &["WRONG_CONTEXT"], &["XXX"], 2);
        let out = run_in(&ws, &patch, serde_json::json!({}));
        assert_eq!(out["status"], "error");
        assert_eq!(out["code"], "APPLY_FAILED");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "aaa\nbbb\nccc\n"
        );
    }

    #[test]
    fn patch_bad_format_reports_error() {
        let (_dir, ws) = repo_with_commit(&[("a.txt", "x\n")]);
        let out = run_in(&ws, "this is not a patch", serde_json::json!({}));
        assert_eq!(out["status"], "error");
        assert!(
            out["message"]
                .as_str()
                .unwrap_or("")
                .contains("parse patch")
        );
    }

    #[test]
    fn patch_missing_field_is_error() {
        let (_dir, ws) = repo_with_commit(&[("a.txt", "x\n")]);
        let out = run_in(&ws, "", serde_json::json!({}));
        assert_eq!(out["status"], "error");
        assert_eq!(out["code"], "PARSE_ERROR");
    }

    #[test]
    fn patch_dry_run_parses_without_applying() {
        let (dir, ws) = repo_with_commit(&[("a.txt", "line1\nline2\n")]);
        let patch = full_patch("a.txt", &["line2"], &["LINE2"], 2);
        let out = run_in(&ws, &patch, serde_json::json!({ "dry_run": true }));
        assert_eq!(out["status"], "ok");
        assert_eq!(out["dry_run"], true);
        assert_eq!(out["insertions"], 1);
        // 未应用
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "line1\nline2\n"
        );
    }

    #[test]
    fn patch_offset_auto_retry() {
        // 行号偏移在 ±MAX_OFFSET 内自动修正（对齐 git CLI 的 offset 语义）：
        // 实际修改在 L1-3，hunk 声明 L5（偏移 -4）→ 重试后应用成功并报告 offset_used
        let (dir, ws) = repo_with_commit(&[("a.txt", "l1\nl2\nl3\nl4\nl5\n")]);
        let patch = "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -5,3 +5,3 @@\n l1\n-l2\n+L2\n l3\n";
        let out = run_in(&ws, &patch, serde_json::json!({}));
        assert_eq!(out["status"], "ok", "got: {out}");
        assert_eq!(out["offset_used"], -4, "got: {out}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "l1\nL2\nl3\nl4\nl5\n"
        );
    }

    #[test]
    fn patch_offset_beyond_range_still_rejected() {
        // 偏移超出 ±MAX_OFFSET → 整包拒绝（不错误应用），且诊断提示实际位置
        let (dir, ws) = repo_with_commit(&[("a.txt", "l1\nl2\nl3\n")]);
        let patch = "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -99,3 +99,3 @@\n l1\n-l2\n+L2\n l3\n";
        let out = run_in(&ws, &patch, serde_json::json!({}));
        assert_eq!(out["status"], "error", "got: {out}");
        assert_eq!(out["code"], "APPLY_FAILED");
        // 诊断包含锚行实际位置
        let msg = out["message"].as_str().unwrap_or("");
        assert!(msg.contains("a.txt"), "diagnostics missing file: {msg}");
        assert!(
            msg.contains("actually at L1"),
            "diagnostics missing position: {msg}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "l1\nl2\nl3\n"
        );
    }

    #[test]
    fn patch_wrong_hunk_counts_auto_fixed() {
        // hunk 头行数声明错误（`@@ -1,2 +1,2 @@` 但实际 3 行）→ 自动重算后应用
        let (dir, ws) = repo_with_commit(&[("a.txt", "l1\nl2\nl3\n")]);
        let patch = "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n l1\n-l2\n+L2\n l3\n";
        let out = run_in(&ws, &patch, serde_json::json!({}));
        assert_eq!(out["status"], "ok", "got: {out}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "l1\nL2\nl3\n"
        );
    }

    #[test]
    fn patch_ledger_updated_after_apply() {
        // 账本同步：apply 后 touched 文件的最新 hash 已登记，edit_file 盲定位不误报
        let (dir, ws) = repo_with_commit(&[("a.txt", "one\ntwo\n")]);
        let patch = full_patch("a.txt", &["two"], &["TWO"], 2);
        let out = run_in(&ws, &patch, serde_json::json!({}));
        assert_eq!(out["status"], "ok", "got: {out}");
        assert_eq!(out["touched"], serde_json::json!(["a.txt"]));
        // 账本 hash 必须与磁盘一致（LF canonical 视图）
        let disk = std::fs::read_to_string(dir.path().join("a.txt")).unwrap();
        let expected =
            crate::file_shared::content_hash(&crate::file_shared::normalize_newlines(&disk).0);
        assert_eq!(crate::file_state::last_hash("a.txt"), Some(expected));
    }

    #[test]
    fn patch_missing_file_diagnostic() {
        // 路径错误 → 诊断提示文件不存在
        let (_dir, ws) = repo_with_commit(&[("a.txt", "x\n")]);
        let patch = full_patch("nope.txt", &["x"], &["X"], 1);
        let out = run_in(&ws, &patch, serde_json::json!({}));
        assert_eq!(out["status"], "error");
        let msg = out["message"].as_str().unwrap_or("");
        assert!(msg.contains("nope.txt"), "got: {msg}");
        assert!(msg.contains("does not exist"), "got: {msg}");
    }

    #[test]
    fn patch_dry_run_reports_hunk_positions() {
        // dry_run 预检：报告每个 hunk 的上下文锚行实际位置与偏移
        let (dir, ws) = repo_with_commit(&[("a.txt", "l1\nl2\nl3\n")]);
        let patch = "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -5,3 +5,3 @@\n l1\n-l2\n+L2\n l3\n";
        let out = run_in(&ws, &patch, serde_json::json!({ "dry_run": true }));
        assert_eq!(out["status"], "ok");
        let hunks = out["hunks"].as_array().expect("hunks in dry_run");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0]["declared_line"], 5);
        assert_eq!(hunks[0]["anchor_actual_line"], 1);
        assert_eq!(hunks[0]["offset"], -4);
        assert_eq!(hunks[0]["context_match"], true);
        // 未应用
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "l1\nl2\nl3\n"
        );
    }

    #[test]
    fn patch_location_index() {
        let (dir, ws) = repo_with_commit(&[("a.txt", "one\n")]);
        let patch = full_patch("a.txt", &["one"], &["ONE"], 1);
        let out = run_in(&ws, &patch, serde_json::json!({ "location": "index" }));
        assert_eq!(out["status"], "ok", "got: {out}");
        assert_eq!(out["location"], "index");
        // index 已更新为 ONE，workdir 仍是 one（patch 应用到 index）
        let repo = Repository::open(dir.path()).unwrap();
        let head_tree = repo.head().unwrap().peel_to_tree().unwrap();
        let mut opts = git2::DiffOptions::new();
        opts.pathspec("a.txt");
        let diff = repo
            .diff_tree_to_index(Some(&head_tree), None, Some(&mut opts))
            .unwrap();
        assert_eq!(diff.deltas().len(), 1, "index must differ from HEAD");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt"))
                .unwrap()
                .replace("\r\n", "\n"),
            "one\n"
        );
    }

    #[test]
    fn patch_invalid_location() {
        let (_dir, ws) = repo_with_commit(&[("a.txt", "x\n")]);
        let patch = full_patch("a.txt", &["x"], &["X"], 1);
        let out = run_in(&ws, &patch, serde_json::json!({ "location": "elsewhere" }));
        assert_eq!(out["status"], "error");
        assert!(
            out["message"]
                .as_str()
                .unwrap_or("")
                .contains("invalid apply location")
        );
    }
}
