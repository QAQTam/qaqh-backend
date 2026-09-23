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

use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::ToolRisk;
use crate::apply_patch_engine::EngineError;
use crate::file_mutate::{ambient_tool_context, mutation_display, mutation_error};
use crate::tool_api::{
    ErasedTool, OutputBudget, ToolCallContext, ToolContentBlock, ToolDescriptor, ToolDisplay,
    ToolExecutionError, ToolExposure, ToolName, ToolProjection, ToolSource, TypedTool,
    TypedToolAdapter,
};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyPatchArgs {
    pub patch: String,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ApplyPatchOutput {
    pub timeis: String,
    pub status: String,
    pub format: String,
    pub dry_run: bool,
    pub files: usize,
    pub insertions: usize,
    pub deletions: usize,
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub overwritten: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub touched: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_id: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    model_text: String,
    #[serde(skip)]
    #[schemars(skip)]
    first_path: Option<String>,
}

impl ToolProjection for ApplyPatchOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: self.model_text.clone(),
        }]
    }

    fn summary(&self) -> Option<String> {
        self.model_text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(|line| line.chars().take(160).collect())
    }

    fn display(&self, _args: &Value) -> ToolDisplay {
        mutation_display(
            self.first_path.as_deref(),
            "",
            crate::tool_api::PathOp::Patch,
            "apply_patch",
            &self.model_text,
            None,
        )
    }
}

pub struct ApplyPatchTool;

impl TypedTool for ApplyPatchTool {
    type Args = ApplyPatchArgs;
    type Output = ApplyPatchOutput;

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::new("apply_patch").expect("valid apply_patch tool name"),
            display_name: None,
            description: DESCRIPTION.to_string(),
            input_schema: apply_patch_schema(),
            output_schema: serde_json::to_value(schemars::schema_for!(ApplyPatchOutput))
                .expect("apply_patch output schema"),
            category: crate::permission::ToolCategory::Write,
            risk: ToolRisk::Write,
            default_timeout: Duration::from_secs(60),
            exposure: ToolExposure::Direct,
            source: ToolSource::Builtin,
            output_budget: OutputBudget::default(),
            capabilities: crate::tool_capabilities::builtin_capabilities("apply_patch")
                .unwrap_or_default(),
        }
    }

    #[allow(clippy::result_large_err)] // ToolExecutionError is the frozen typed boundary.
    fn run(
        &self,
        ctx: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        use crate::apply_patch_engine::{apply_patch_engine, dry_run_patch_engine};

        if args.patch.is_empty() {
            return Err(mutation_error(
                "PARSE_ERROR",
                "apply_patch: missing 'patch'",
                Some(
                    "Provide a Codex-format patch: '*** Begin Patch' ... '*** End Patch' (see the tool description for the format).",
                ),
                json!({}),
            ));
        }

        let patch = args.patch;
        let dry_run = args.dry_run;
        let outcome = if dry_run {
            dry_run_patch_engine(&patch, &ctx.workspace_root)
        } else {
            apply_patch_engine(&patch, &ctx.workspace_root, Default::default())
        };
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                let (code, hint) = error_code_and_hint(&error);
                return Err(mutation_error(
                    code,
                    error.to_string(),
                    Some(&hint),
                    json!({}),
                ));
            }
        };

        let mut insertions = 0usize;
        let mut deletions = 0usize;
        for delta in &outcome.deltas {
            match (&delta.old, &delta.new) {
                (None, Some(new)) => insertions += new.lines().count(),
                (Some(old), None) => deletions += old.lines().count(),
                (Some(old), Some(new)) => {
                    let (added, removed, _) = crate::file_shared::diff_stats_between(old, new);
                    insertions += added as usize;
                    deletions += removed as usize;
                }
                (None, None) => {}
            }
            // 账本同步：引擎直接写盘，touched 文件的最新内容登记进 file_state，
            // 否则后续 edit 盲定位防漂移会误报。
            if !dry_run {
                if let Some(new) = &delta.new {
                    // 账本键用解析后的绝对路径（与 read/edit/write 同键）。
                    crate::file_state::record_write(&delta.resolved_path, new);
                }
                let operation = if delta.old.is_none() {
                    "add"
                } else if delta.new.is_none() {
                    "delete"
                } else {
                    "update"
                };
                crate::journal::record_change(
                    &ctx.session_id,
                    "",
                    "apply_patch",
                    &delta.path,
                    operation,
                    delta.old.as_deref(),
                    delta.new.as_deref(),
                    "ok",
                );
            }
        }

        let files = outcome.affected.added.len()
            + outcome.affected.modified.len()
            + outcome.affected.deleted.len();
        let overwritten: Vec<String> = outcome
            .deltas
            .iter()
            .filter(|delta| {
                delta.old.is_some()
                    && delta.new.is_some()
                    && outcome
                        .affected
                        .added
                        .iter()
                        .any(|path| path == &delta.path)
            })
            .map(|delta| delta.path.clone())
            .collect();
        let touched: Vec<String> = if dry_run {
            outcome
                .deltas
                .iter()
                .map(|delta| delta.path.clone())
                .collect()
        } else {
            Vec::new()
        };
        let first_path = outcome.deltas.first().map(|delta| delta.path.clone());

        let mut model_text = if dry_run {
            format!(
                "[DRY RUN] apply_patch — patch parses: {files} file(s), +{insertions} -{deletions}; engine pre-checked every hunk against current file contents (a real apply may still differ)\n"
            )
        } else if overwritten.is_empty() {
            format!("[OK] apply_patch — applied: {files} file(s), +{insertions} -{deletions}\n")
        } else {
            format!(
                "[OK] apply_patch — applied: {files} file(s), +{insertions} -{deletions}; OVERWROTE existing: {} (previous contents recorded for rollback)\n",
                overwritten.join(", ")
            )
        };
        let pending_id = if dry_run {
            let pending_id = crate::pending::store("apply_patch", &json!({"patch": patch}));
            model_text.push_str(&format!(
                "\npending_id={pending_id} — confirm with confirm_apply {{\"pending_id\":\"{pending_id}\",\"action\":\"apply\"}}"
            ));
            Some(pending_id)
        } else {
            None
        };

        Ok(ApplyPatchOutput {
            timeis: crate::now_utc8(),
            status: "ok".to_string(),
            format: "codex".to_string(),
            dry_run,
            files,
            insertions,
            deletions,
            added: outcome.affected.added,
            modified: outcome.affected.modified,
            deleted: outcome.affected.deleted,
            overwritten,
            touched,
            pending_id,
            model_text,
            first_path,
        })
    }
}

fn apply_patch_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "patch": {"type": "string", "description": "Codex patch text. If a hunk's context is not unique in the file, extend it with surrounding lines or anchor the chunk with '@@ <context line>'; the engine edits the FIRST match."},
            "dry_run": {"type": "boolean", "description": "Preview only (also reports WOULD_OVERWRITE when '*** Add File:' targets an existing path)", "default": false}
        },
        "required": ["patch"],
        "additionalProperties": false
    })
}

/// Map an engine error to its `(code, hint)` pair for the display plane.
fn error_code_and_hint(error: &EngineError) -> (&'static str, String) {
    match error {
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
        EngineError::SymlinkTarget { .. } => (
            "SYMLINK_TARGET",
            "Patch paths must not be symbolic links: resolve the link and retry with the real target path (links are never replaced or written through).".to_string(),
        ),
        EngineError::WouldOverwrite { .. } => (
            "WOULD_OVERWRITE",
            "dry-run finding: '*** Add File:' would replace an existing file wholesale. Use '*** Update File:' to edit it in place (or '*** Delete File:' first). To overwrite deliberately, re-send the same patch without dry_run — a real apply keeps the upstream overwrite semantics and records the replaced contents in the delta/journal for rollback.".to_string(),
        ),
    }
}

/// 工具描述。**必须**保留「重复上下文取首个命中」的警示：匹配器不做歧义
/// 拒绝（`seek_sequence` exact 循环直接返回首个 `i`，与上游 codex 一致），
/// 上下文不够时被改的是第一处而结果仍是 `[OK]`（BUG-2026-09-16-11）。
pub(crate) const DESCRIPTION: &str = "Apply a Codex-format patch (*** Begin Patch). Matching takes the FIRST hit; \
     disambiguate repeated context with extra lines or '@@ <context line>'. dry_run previews.";

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_typed(ApplyPatchTool);
}

/// Compatibility entry retained until `confirm_apply` is typed (Wave 6).
///
/// Production registration uses [`ApplyPatchTool`] directly; this bridge keeps
/// the existing in-process call shape for confirm-apply and older tests.
pub(super) fn exec_apply_patch(args: &Value) -> crate::ToolResult {
    if args
        .get("patch")
        .and_then(Value::as_str)
        .filter(|patch| !patch.is_empty())
        .is_none()
    {
        return crate::ToolResult::error(
            json!({
                "timeis": crate::now_utc8(),
                "status": "error",
                "code": "PARSE_ERROR",
                "message": "apply_patch: missing 'patch'",
                "hint": "Provide a Codex-format patch: '*** Begin Patch' ... '*** End Patch' (see the tool description for the format).",
            })
            .to_string(),
        );
    }
    let ctx = ambient_tool_context("apply-patch-compat", Duration::from_secs(60));
    TypedToolAdapter::new(ApplyPatchTool)
        .execute(ctx, args.clone())
        .unwrap_or_else(|fatal| panic!("apply_patch tool fatal: {}", fatal.message))
        .to_tool_result()
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
        let patch = "*** Begin Patch\n*** Add File: a.txt\n+clobbered\n*** End Patch\n";
        let out = run_in(&ws, patch, serde_json::json!({}));
        assert_eq!(out["status"], "ok", "got: {out}");

        let abs = abs.to_string_lossy().to_string();
        assert_eq!(
            crate::file_state::last_hash(&abs),
            Some(crate::file_shared::content_hash("clobbered\n")),
            "apply_patch must refresh the ledger key for the overwritten file"
        );

        // 紧随其后的 edit：精确匹配必须看得见 apply_patch 写进去的新内容。
        let ok = crate::edit::exec_edit(&serde_json::json!({
            "path": abs,
            "old_str": "clobbered",
            "new_str": "CLOBBERED",
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
            .lookup("apply_patch")
            .expect("apply_patch registered")
            .description
            .as_str();
        assert!(
            desc.contains("FIRST hit"),
            "description must warn that the first match wins: {desc}"
        );
        assert!(
            desc.contains("@@ <context line>"),
            "description must tell the model to anchor with @@: {desc}"
        );
    }

    #[test]
    fn typed_apply_patch_registration_and_display_are_same_source() {
        let mut manager = crate::ToolManager::new();
        register(&mut manager);
        assert!(
            manager.builtins["apply_patch"].legacy.is_none(),
            "apply_patch still has legacy executor"
        );

        let (_dir, workspace) = repo_with_commit(&[("a.txt", "old\n")]);
        let patch = "*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** End Patch\n";
        crate::CURRENT_WORKSPACE
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .clone_from(&workspace);
        let result = exec_apply_patch(&serde_json::json!({"patch": patch}));
        assert!(result.is_success(), "{}", result.model_text());
        assert_eq!(result.data["files"], serde_json::json!(1));
        let display = result.display().expect("typed display");
        let display_text = match &display.body {
            Some(qaqh_types::ToolResultDisplayBody::Text { text, .. }) => text,
            other => panic!("unexpected apply_patch display body: {other:?}"),
        };
        assert_eq!(display_text, result.model_text());
    }

    #[cfg(unix)]
    #[test]
    fn apply_patch_rejects_symlink_target() {
        let (dir, ws) = repo_with_commit(&[("target.txt", "hello\n")]);
        let target = dir.path().join("target.txt");
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let patch = format!(
            "*** Begin Patch\n*** Update File: {}\n@@\n-hello\n+HELLO\n*** End Patch\n",
            link.display()
        );
        let out = run_in(&ws, &patch, serde_json::json!({}));
        assert_eq!(out["code"], "SYMLINK_TARGET", "got: {out}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello\n");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
