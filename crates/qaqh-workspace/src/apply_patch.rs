//! 独立补丁工具 `apply_patch`：**Codex 格式补丁**（`*** Begin Patch`）。
//!
//! 自研内容匹配引擎（`apply_patch_engine`，移植自 OpenAI codex-rs，
//! Apache-2.0）：**无行号**、四级匹配（精确→去尾空白→trim→Unicode 归一化）、
//! `@@` 上下文锚定、`*** End of File` 文件尾锚定。模型手写友好。
//!
//! 事务语义：**按序应用**——任一 hunk 失败即停，已写入的文件保留（与上游
//! Codex 一致）；失败前先 `dry_run=true` 可预检全部 hunk。路径相对 workspace
//! 根解析，`..` 逃逸/外部绝对路径被拒绝。
//!
//! 与 `edit` 分工：edit 是结构化精确定位（内容锚定、严格拒绝歧义），
//! 适合模型逐步编辑；apply_patch 适合批量补丁合入（模型输出 patch → 校验 → 合入），
//! 失败时**只重发未生效的部分**——已生效的 hunk 已落在盘上，重发完整 patch
//! 会对它们二次 `NO_MATCH`（失败响应会列出「已生效 / 未生效」两份路径）。

use crate::apply_patch_engine::EngineError;
use crate::{ToolHandler, ToolResult, ToolRisk};

fn workspace_root() -> String {
    let ws = crate::current_workspace();
    if ws.is_empty() { ".".to_string() } else { ws }
}

/// 执行 apply_patch：patch（必填，`*** Begin Patch` Codex 格式）+ dry_run。
pub(super) fn exec_apply_patch(args: &serde_json::Value) -> ToolResult {
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
                "hint": "Provide a Codex-format patch: '*** Begin Patch' ... '*** End Patch' (see the tool description for the format).",
            }).to_string());
        }
    };
    let dry_run = args
        .get("dry_run")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);

    let ws = workspace_root();
    let mut result = exec_engine_patch(&ws, patch, dry_run);
    // dry-run 通过 → 暂存参数供 confirm_apply 内存直提（模型无需重发 patch）。
    if dry_run && result.status == crate::ToolStatus::Ok {
        let mut pending_args = args.clone();
        if let Some(obj) = pending_args.as_object_mut() {
            obj.remove("dry_run");
        }
        let pending_id = crate::pending::store("apply_patch", &pending_args);
        result.data["pending_id"] = serde_json::json!(pending_id);
        result.data["dry_run"] = serde_json::json!(true);
        result.push_hint(&format!(
            "pending_id={pending_id} — confirm with confirm_apply {{\"pending_id\":\"{pending_id}\",\"action\":\"apply\"}}"
        ));
    }
    result
}

/// 内容匹配引擎：无行号、四级匹配、按序应用（任一 hunk 失败即停，已写入
/// 的文件保留——与上游 Codex 一致；失败前先 `dry_run=true` 预检全部 hunk）。
fn exec_engine_patch(ws: &str, patch: &str, dry_run: bool) -> ToolResult {
    use crate::apply_patch_engine::{apply_patch_engine, dry_run_patch_engine};

    let outcome = if dry_run {
        dry_run_patch_engine(patch, std::path::Path::new(ws))
    } else {
        apply_patch_engine(patch, std::path::Path::new(ws), Default::default())
    };

    match outcome {
        Ok(outcome) => {
            let mut ins = 0usize;
            let mut del = 0usize;
            for d in &outcome.deltas {
                match (&d.old, &d.new) {
                    (None, Some(new)) => ins += new.lines().count(),
                    (Some(old), None) => del += old.lines().count(),
                    (Some(old), Some(new)) => {
                        let (a, r, _) = crate::file_shared::diff_stats_between(old, new);
                        ins += a as usize;
                        del += r as usize;
                    }
                    (None, None) => {}
                }
                // 账本同步：引擎直接写盘，touched 文件的最新内容登记进
                // file_state，否则后续 edit 盲定位防漂移会误报。
                if !dry_run {
                    if let Some(new) = &d.new {
                        // 账本键用解析后的绝对路径（与 read/edit/write 同键），
                        // 否则同一文件会有两套键，STALE_FILE 校验看不到本写入。
                        crate::file_state::record_write(&d.resolved_path, new);
                    }
                    let op = if d.old.is_none() {
                        "add"
                    } else if d.new.is_none() {
                        "delete"
                    } else {
                        "update"
                    };
                    crate::journal::record_change(
                        &crate::journal::active_session(),
                        "",
                        "apply_patch",
                        &d.path,
                        op,
                        d.old.as_deref(),
                        d.new.as_deref(),
                        "ok",
                    );
                }
            }
            let n = outcome.affected.added.len()
                + outcome.affected.modified.len()
                + outcome.affected.deleted.len();
            // `*** Add File:` 打到已存在路径 = 整文件覆盖（上游语义，fixture
            // 011 依赖）。引擎已把旧内容记进 `FileDelta.old`；这里再显式列出，
            // 免得响应体看起来像一次普通新增（BUG-2026-09-16-10）。
            let overwritten: Vec<String> = outcome
                .deltas
                .iter()
                .filter(|d| {
                    d.old.is_some()
                        && d.new.is_some()
                        && outcome.affected.added.iter().any(|a| a == &d.path)
                })
                .map(|d| d.path.clone())
                .collect();
            let text = if dry_run {
                format!(
                    "[DRY RUN] apply_patch — patch parses: {n} file(s), +{ins} -{del}; engine pre-checked every hunk against current file contents (a real apply may still differ)\n"
                )
            } else if overwritten.is_empty() {
                format!("[OK] apply_patch — applied: {n} file(s), +{ins} -{del}\n")
            } else {
                format!(
                    "[OK] apply_patch — applied: {n} file(s), +{ins} -{del}; OVERWROTE existing: {} (previous contents recorded for rollback)\n",
                    overwritten.join(", ")
                )
            };
            let mut data = serde_json::json!({
                "timeis": crate::now_utc8(),
                "status": "ok",
                "format": "codex",
                "dry_run": dry_run,
                "files": n,
                "insertions": ins,
                "deletions": del,
                "added": outcome.affected.added,
                "modified": outcome.affected.modified,
                "deleted": outcome.affected.deleted,
            });
            if !overwritten.is_empty() {
                data["overwritten"] = serde_json::json!(overwritten);
            }
            if dry_run {
                data["touched"] = serde_json::Value::Array(
                    outcome
                        .deltas
                        .iter()
                        .map(|d| serde_json::Value::String(d.path.clone()))
                        .collect(),
                );
            }
            crate::ToolResult::ok_data(data, text)
        }
        Err(e) => {
            let (code, hint) = error_code_and_hint(&e);
            crate::json_err(code, e.to_string(), hint)
        }
    }
}

/// Map an engine error to its `(code, hint)` pair for the **display plane**.
///
/// ⚠ 通道事实（2026-09-17 实测）：模型读到的是 `ToolResult::render_xml_envelope()`，
/// 其 body 取 `model.text`，而 `model.text` 来自 `error_with` 的 `message` 参数
/// （即 [`EngineError`] 的 `Display`，上限 `TOOL_MODEL_MAX_CHARS`）。
/// `ToolError.hint` **不进入该通道**（`project_for_model` 也不携带它），
/// 所以本函数返回的 hint 只服务前端/展示面，且被 `TOOL_SUMMARY_MAX_CHARS`(512) 截断。
///
/// 因此「已生效 / 未生效」清单必须走 [`EngineError::Partial`] 的 `Display`
/// （见 `apply_patch_engine::EngineError` 的 `fmt`），**不能只写在 hint 里**——
/// 否则模型看到的仍是一句「找不到上下文」，照旧重发整个 patch
/// （BUG-2026-09-16-09）。这里只保留一句短的行动指引，避免 512 截断吃掉它。
fn error_code_and_hint(e: &EngineError) -> (&'static str, String) {
    // ⚠ 可达性（N-2①）：`WouldOverwrite` **只可能由 dry-run 产生**——真 apply 的
    // `AddFile` 分支没有守卫，保持上游覆盖语义（见 `apply_patch_engine::apply_hunk`，
    // 旧内容记进 `FileDelta.old`）。评审担心的「真 apply 走到这里时前序 hunk 已
    // 落盘、重发同 patch 会 NO_MATCH」在 HEAD 上不成立，故 hint 只需标明这是
    // dry-run 的发现，不必再分两种场景写两套文案。
    match e {
        EngineError::Partial { error, .. } => {
            let (code, hint) = error_code_and_hint(error);
            (
                code,
                format!(
                    "{hint} NOTE: hunks are applied one at a time and are NOT atomic — the already-applied files keep their changes. Fix and re-send ONLY the failed part, or verify them with `git diff`/read first."
                ),
            )
        }
        EngineError::Parse(_) => (
            "PARSE_ERROR",
            "The patch does not follow the Codex apply-patch format: start with '*** Begin Patch', end with '*** End Patch'; hunk lines start with '+' (add), '-' (remove), ' ' (context); '@@' starts a chunk (optionally with a context line).".to_string(),
        ),
        EngineError::Compute(_) => (
            "NO_MATCH",
            "The engine could not find the expected lines in the target file (4-tier matching: exact → trailing-whitespace → trimmed → Unicode-normalised). Check the 'old' lines against the file; use '@@ <context>' to anchor the chunk, or '*** End of File' for end-of-file hunks.".to_string(),
        ),
        EngineError::EmptyPatch => (
            "EMPTY_PATCH",
            "The patch parsed to zero hunks; at least one '*** Add File: / *** Delete File: / *** Update File:' section is required.".to_string(),
        ),
        EngineError::Io { .. } => (
            "IO_ERROR",
            "A filesystem operation failed (read/write/remove).".to_string(),
        ),
        EngineError::PathOutsideWorkspace { .. } => (
            "PATH_OUTSIDE_WORKSPACE",
            "Every patch path must resolve inside the workspace root; '..' escapes and absolute paths outside the workspace are rejected.".to_string(),
        ),
        EngineError::WouldOverwrite { .. } => (
            "WOULD_OVERWRITE",
            "dry-run finding: '*** Add File:' would replace an existing file wholesale. Use '*** Update File:' to edit it in place (or '*** Delete File:' first). To overwrite deliberately, re-send the same patch without dry_run — a real apply keeps the upstream overwrite semantics and records the replaced contents in the delta/journal for rollback.".to_string(),
        ),
    }
}

fn handle_apply_patch(ctx: crate::ToolCallCtx) -> ToolResult {
    exec_apply_patch(&ctx.args)
}

// ─────────────────────────────────────────────────────────────
// Registration
// ─────────────────────────────────────────────────────────────

/// 工具描述。**必须**保留「重复上下文取首个命中」的警示：匹配器不做歧义
/// 拒绝（`seek_sequence` exact 循环直接返回首个 `i`，与上游 codex 一致），
/// 上下文不够时被改的是第一处而结果仍是 `[OK]`（BUG-2026-09-16-11）。
pub(crate) const DESCRIPTION: &str = "Apply Codex-format patch (*** Begin Patch). Content-matched hunks; use dry_run to preview. \
     WARNING: matching takes the FIRST hit — if the same context appears more than once in the file, \
     add surrounding context lines or anchor the chunk with '@@ <context line>', otherwise the first \
     occurrence is edited (silently) and the result still reports [OK].";

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register(ToolHandler {
        key: "apply_patch".to_string(),
        description: DESCRIPTION,
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "patch": {"type": "string", "description": "Codex patch text. If a hunk's context is not unique in the file, extend it with surrounding lines or anchor the chunk with '@@ <context line>'; the engine edits the FIRST match."},
                "dry_run": {"type": "boolean", "description": "Preview only (also reports WOULD_OVERWRITE when '*** Add File:' targets an existing path)", "default": false}
            },
            "required": ["patch"],
            "additionalProperties": false
        }),
        handler: handle_apply_patch,
        risk: ToolRisk::Write,
        category: crate::permission::ToolCategory::Write,
        default_timeout: std::time::Duration::from_secs(60),
    });
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
        let signature =
            git2::Signature::now("QAQ-Harness Test", "qaqh-test@local").expect("signature");
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
        if let Some(err) = &result.error {
            return serde_json::json!({
                "status": "error",
                "code": err.code,
                "message": err.message,
                "hint": err.hint,
            });
        }
        let data = result.data.clone();
        if data.as_object().is_some_and(|o| !o.is_empty()) {
            data
        } else {
            let raw = result.model_text();
            match serde_json::from_str::<serde_json::Value>(raw) {
                Ok(v) if v.is_object() => v,
                _ => serde_json::json!({ "status": "error", "raw": raw }),
            }
        }
    }

    #[test]
    fn codex_format_routes_to_engine_and_writes() {
        // `*** Begin Patch` 格式走自研内容匹配引擎：无行号、空白容错。
        let (dir, ws) = repo_with_commit(&[("a.txt", "line1\nline2\nline3\n")]);
        let patch = "\
*** Begin Patch
*** Update File: a.txt
@@
-line2
+LINE2
*** End Patch
";
        let out = run_in(&ws, patch, serde_json::json!({}));
        assert_eq!(out["status"], "ok", "got: {out}");
        assert_eq!(out["format"], "codex");
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
    fn codex_format_dry_run_prechecks_without_writing() {
        let (dir, ws) = repo_with_commit(&[("a.txt", "line1\nline2\n")]);
        let patch = "\
*** Begin Patch
*** Update File: a.txt
@@
-line2
++LINE2
*** End Patch
";
        let out = run_in(&ws, patch, serde_json::json!({ "dry_run": true }));
        assert_eq!(out["status"], "ok", "got: {out}");
        assert_eq!(out["dry_run"], true);
        assert_eq!(out["files"], 1);
        assert!(out["touched"][0].as_str().unwrap().ends_with("a.txt"));
        // 文件未被修改
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "line1\nline2\n"
        );
    }

    #[test]
    fn codex_format_no_match_rejects_with_zero_changes() {
        let (dir, ws) = repo_with_commit(&[("a.txt", "line1\nline2\n")]);
        let patch = "\
*** Begin Patch
*** Update File: a.txt
@@
-never-exists
++NOPE
*** End Patch
";
        let out = run_in(&ws, patch, serde_json::json!({}));
        assert_eq!(out["status"], "error", "got: {out}");
        assert_eq!(out["code"], "NO_MATCH");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "line1\nline2\n"
        );
    }

    // ── T-3-1 / BUG-2026-09-16-10：Add File 覆盖已有文件不再静默 ──

    /// dry_run 打到已存在路径必须显式 `WOULD_OVERWRITE`，而不是普通
    /// `[DRY RUN] … ok`（旧行为：零警示 + 发 pending_id）。
    #[test]
    fn add_file_refuses_existing_path() {
        let (dir, ws) = repo_with_commit(&[("a.txt", "one\ntwo\nthree\nfour\nfive\n")]);
        let patch = "\
*** Begin Patch
*** Add File: a.txt
+clobbered
*** End Patch
";
        let out = run_in(&ws, patch, serde_json::json!({ "dry_run": true }));
        assert_eq!(out["status"], "error", "got: {out}");
        assert_eq!(out["code"], "WOULD_OVERWRITE", "got: {out}");
        let hint = out["hint"].as_str().unwrap_or_default();
        assert!(
            hint.contains("overwrite"),
            "hint must state the overwrite: {out}"
        );
        // dry_run 不落盘，也不发 pending_id
        assert!(out.get("pending_id").is_none(), "got: {out}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one\ntwo\nthree\nfour\nfive\n"
        );
    }

    /// 真 apply 保持上游覆盖语义（fixture `011_add_overwrites_existing_file`
    /// 依赖它），但旧内容必须记入 `FileDelta.old` 并在响应里显式列出
    /// `overwritten`——不再是一句 `applied: 1 file(s)` 了事。
    #[test]
    fn add_file_overwrite_is_reported_with_old_contents() {
        let (dir, ws) = repo_with_commit(&[("a.txt", "one\ntwo\nthree\nfour\nfive\n")]);
        let patch = "\
*** Begin Patch
*** Add File: a.txt
+clobbered
*** End Patch
";
        let out = run_in(&ws, patch, serde_json::json!({}));
        assert_eq!(out["status"], "ok", "got: {out}");
        assert_eq!(out["overwritten"][0], "a.txt", "got: {out}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "clobbered\n"
        );
    }

    // ── N-2②/N-2③（PR #87 评审建议项）：多文件覆盖逐条上报 + 账本同步 ──

    /// N-2③：同批多个 `*** Add File:` 命中已存在路径时，`overwritten` 必须逐个列出。
    /// （评审当时按 `ADD_FILE_MARKER` 全局匹配推断「只报第一个」；HEAD 已按 delta
    /// 判定，本用例把多文件行为锁住。）
    #[test]
    fn multiple_add_file_overwrites_are_all_reported() {
        let (dir, ws) = repo_with_commit(&[("a.txt", "old a\n"), ("b.txt", "old b\n")]);
        let patch = "\
*** Begin Patch
*** Add File: a.txt
+new a
*** Add File: b.txt
+new b
*** End Patch
";
        let out = run_in(&ws, patch, serde_json::json!({}));
        assert_eq!(out["status"], "ok", "got: {out}");
        assert_eq!(
            out["overwritten"],
            serde_json::json!(["a.txt", "b.txt"]),
            "every overwritten file must be listed: {out}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "new a\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "new b\n"
        );
    }

    /// N-2②：覆盖后账本（`file_state`）必须同步到新内容——否则紧随其后的 `edit`
    /// 会拿旧指纹把本次写入判成漂移（STALE_FILE/HASH_MISMATCH）。
    #[test]
    fn overwrite_refreshes_file_state_for_followup_edit() {
        let (dir, ws) = repo_with_commit(&[("a.txt", "one\ntwo\n")]);
        let abs = dir.path().canonicalize().unwrap().join("a.txt");
        let before = crate::file_shared::content_hash("one\ntwo\n");
        let patch = "*** Begin Patch\n*** Add File: a.txt\n+clobbered\n*** End Patch\n";
        let out = run_in(&ws, patch, serde_json::json!({}));
        assert_eq!(out["status"], "ok", "got: {out}");

        let abs = abs.to_string_lossy().to_string();
        assert_eq!(
            crate::file_state::last_hash(&abs),
            Some(crate::file_shared::content_hash("clobbered\n")),
            "apply_patch must refresh the ledger key for the overwritten file"
        );

        // 紧随其后的 edit：传覆盖前的旧指纹必须被拒……
        let stale = crate::edit::exec_edit(&serde_json::json!({
            "path": abs,
            "expected_hash": before,
            "hunks": [{"kind": "replace", "old": "clobbered", "new": "CLOBBERED"}],
        }));
        assert!(
            !stale.is_success(),
            "the pre-overwrite hash must be rejected: {}",
            stale.model_text()
        );
        // ……不传指纹（内容定位）必须成功，且看得见 apply_patch 写进去的新内容。
        let ok = crate::edit::exec_edit(&serde_json::json!({
            "path": abs,
            "hunks": [{"kind": "replace", "old": "clobbered", "new": "CLOBBERED"}],
        }));
        assert!(
            ok.is_success(),
            "follow-up edit must see the new content: {}",
            ok.model_text()
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "CLOBBERED\n"
        );
    }

    // ── T-3-2 / BUG-2026-09-16-09：失败结果陈述部分应用事实 ──
    //
    // 通道事实：模型读 `render_xml_envelope()`（body = `model.text` = `message`），
    // `error.hint` 只服务展示面且被 512 截断。断言因此打在 envelope 上。

    /// 1 号 hunk 合法、2 号上下文不存在 → **模型可见文本**必须列出已生效文件，
    /// 且不得再出现「no partial application happened」这类与事实相反的断言。
    #[test]
    fn failed_hunk_reports_partial_application_to_the_model() {
        let (dir, ws) = repo_with_commit(&[("a.txt", "alpha\n"), ("b.txt", "beta\n")]);
        let patch = "\
*** Begin Patch
*** Update File: a.txt
@@
-alpha
+ALPHA
*** Update File: b.txt
@@
-THIS LINE IS ABSENT
+whatever
*** End Patch
";
        crate::CURRENT_WORKSPACE
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clone_from(&ws.to_string());
        let result = exec_apply_patch(&serde_json::json!({ "patch": patch }));
        let envelope = result.render_xml_envelope();
        assert!(
            envelope.contains("error_code=\"NO_MATCH\""),
            "got: {envelope}"
        );
        assert!(
            envelope.contains("already applied: a.txt"),
            "model must see the already-applied file: {envelope}"
        );
        assert!(
            envelope.contains("not applied: b.txt"),
            "model must see the not-applied file: {envelope}"
        );
        assert!(
            !envelope.contains("no partial application"),
            "stale claim must be gone: {envelope}"
        );
        // 展示面 hint 不再塞清单，必须留在 512 预算内（否则尾部被截断）
        let hint = result
            .error
            .as_ref()
            .and_then(|e| e.hint.clone())
            .unwrap_or_default();
        assert!(
            hint.chars().count() <= qaqh_types::TOOL_SUMMARY_MAX_CHARS,
            "hint must stay within budget, got {} chars: {hint}",
            hint.chars().count()
        );
        // 1 号 hunk 确实已落盘（证明文案说的「非原子」为真）
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "ALPHA\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "beta\n"
        );
    }

    /// 批量补丁的**尾部**必须活下来：20 个文件已生效、第 21 个失败时，
    /// 模型可见文本里要同时出现首个、最后一个已生效路径与失败路径。
    /// （展示面 hint 有 512 硬上限，清单放那里必然被截断——本用例即该回归锁。）
    #[test]
    fn partial_failure_lists_survive_for_many_files() {
        let mut files: Vec<(String, String)> = (0..20)
            .map(|i| (format!("d{i}.txt"), format!("v{i}\n")))
            .collect();
        files.push(("zz.txt".to_string(), "keep\n".to_string()));
        let owned: Vec<(&str, &str)> = files
            .iter()
            .map(|(n, c)| (n.as_str(), c.as_str()))
            .collect();
        let (_dir, ws) = repo_with_commit(&owned);
        let mut patch = String::from("*** Begin Patch\n");
        for i in 0..20 {
            patch.push_str(&format!("*** Update File: d{i}.txt\n@@\n-v{i}\n+V{i}\n"));
        }
        patch.push_str("*** Update File: zz.txt\n@@\n-ABSENT LINE\n+whatever\n*** End Patch\n");
        crate::CURRENT_WORKSPACE
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clone_from(&ws.to_string());
        let result = exec_apply_patch(&serde_json::json!({ "patch": patch }));
        let envelope = result.render_xml_envelope();
        assert!(
            envelope.contains("already applied: d0.txt"),
            "head of the list must survive: {envelope}"
        );
        assert!(
            envelope.contains("d19.txt"),
            "tail of the list must survive: {envelope}"
        );
        assert!(
            envelope.contains("not applied: zz.txt"),
            "the actually-failed path must survive: {envelope}"
        );
    }

    // ── T-3-3 / BUG-2026-09-16-11：描述里补「上下文充分性」警示 ──

    #[test]
    fn tool_description_warns_about_ambiguous_context() {
        let mut mgr = crate::ToolManager::new();
        register(&mut mgr);
        let desc = mgr
            .handlers
            .get("apply_patch")
            .expect("apply_patch registered")
            .description;
        assert!(
            desc.contains("FIRST hit"),
            "description must warn that the first match wins: {desc}"
        );
        assert!(
            desc.contains("@@ <context line>"),
            "description must tell the model to anchor with @@: {desc}"
        );
    }
}
