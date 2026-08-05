//! Git utility functions — thin wrappers around git2 for backend use.
//!
//! These are NOT LLM-invocable tools. They are called directly by the
//! Git service functions exposed to clients through the daemon control protocol.

use git2::{DiffOptions, Repository};
use std::path::Path;

/// Open a git repository at the given path.
fn open_repo(path: &str) -> Result<Repository, String> {
    Repository::open(Path::new(path)).map_err(|e| format!("open repo: {e}"))
}

/// Get working-tree status as a JSON array of `{path, change, lines_added, lines_removed}`.
pub fn status_json(workspace: &str) -> Result<String, String> {
    let repo = open_repo(workspace)?;
    let mut files: Vec<serde_json::Value> = Vec::new();

    let statuses = repo.statuses(None).map_err(|e| format!("status: {e}"))?;
    for entry in statuses.iter() {
        let path = entry.path().unwrap_or("").to_string();
        let status = entry.status();
        let change = if status.is_index_new() || status.is_wt_new() {
            "added"
        } else if status.is_index_deleted() || status.is_wt_deleted() {
            "deleted"
        } else if status.is_index_modified() || status.is_wt_modified() {
            "modified"
        } else if status.is_index_renamed() || status.is_wt_renamed() {
            "renamed"
        } else {
            continue;
        };

        let (lines_added, lines_removed) = (0, 0);
        // FIXME(git2-isolation): diff_tree_to_workdir_with_index 的 pathspec 过滤可能失效，
        // 导致全量 diff 计算拖垮 tokio 运行时(code=1006)。注释掉待验证。
        // 复用前：if matches!(change, "modified" | "added") {
        //     let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());
        //     let mut opts = DiffOptions::new();
        //     opts.pathspec(&path);
        //     head_tree
        //         .and_then(|tree| {
        //             repo.diff_tree_to_workdir_with_index(Some(&tree), Some(&mut opts))
        //                 .ok()
        //         })
        //         .and_then(|d| d.stats().ok())
        //         .map(|s| (s.insertions() as u32, s.deletions() as u32))
        //         .unwrap_or((0, 0))
        // } else {
        //     (0, 0)
        // };

        files.push(serde_json::json!({
            "path": path,
            "change": change,
            "lines_added": lines_added,
            "lines_removed": lines_removed,
        }));
    }

    serde_json::to_string(&files).map_err(|e| format!("serialize: {e}"))
}

/// Get the current branch name (shorthand). Returns empty string if detached HEAD.
pub fn current_branch(workspace: &str) -> Result<String, String> {
    let repo = open_repo(workspace)?;
    let head = repo.head().map_err(|_| "no HEAD")?;
    if head.is_branch() {
        Ok(head.shorthand().unwrap_or("HEAD").to_string())
    } else {
        Ok("HEAD".into())
    }
}

/// List all local branches as a JSON array of `{name, current}`.
pub fn list_branches(workspace: &str) -> Result<String, String> {
    let repo = open_repo(workspace)?;
    let head_name = repo
        .head()
        .ok()
        .and_then(|h| Some(h.shorthand().ok().unwrap_or("HEAD").to_string()));

    let mut branches: Vec<serde_json::Value> = Vec::new();
    if let Ok(iter) = repo.branches(Some(git2::BranchType::Local)) {
        for b in iter.flatten() {
            let name = b.0.name().ok().flatten().unwrap_or("").to_string();
            if name.is_empty() {
                continue;
            }
            branches.push(serde_json::json!({
                "name": name,
                "current": head_name.as_deref() == Some(&name),
            }));
        }
    }
    serde_json::to_string(&branches).map_err(|e| format!("serialize: {e}"))
}

/// Switch to a branch. If `stash` is true, stash uncommitted changes first and
/// pop them after switching. Returns the new branch name.
pub fn switch_branch(workspace: &str, branch: &str, stash: bool) -> Result<String, String> {
    let mut repo = open_repo(workspace)?;

    let has_changes = repo.statuses(None).map(|s| !s.is_empty()).unwrap_or(false);
    let mut stashed = false;

    if stash && has_changes {
        let sig =
            git2::Signature::now("DeepX", "deepx@local").map_err(|e| format!("signature: {e}"))?;
        repo.stash_save(&sig, "deepx-auto-stash", None)
            .map_err(|e| format!("stash: {e}"))?;
        stashed = true;
    }

    {
        let branch_ref = repo
            .find_branch(branch, git2::BranchType::Local)
            .map_err(|e| format!("find branch '{}': {}", branch, e))?;
        let obj = branch_ref
            .get()
            .peel(git2::ObjectType::Tree)
            .map_err(|e| format!("peel: {e}"))?;

        let mut checkout_opts = git2::build::CheckoutBuilder::new();
        checkout_opts.safe();
        repo.checkout_tree(&obj, Some(&mut checkout_opts))
            .map_err(|e| format!("checkout tree: {e}"))?;
        repo.set_head(branch_ref.get().name().ok().unwrap_or(""))
            .map_err(|e| format!("set HEAD: {e}"))?;
    }

    if stashed {
        if let Err(e) = repo.stash_pop(0, None) {
            log::warn!("stash pop failed (likely conflict, stash kept): {e}");
        }
    }

    let new_head = repo
        .head()
        .ok()
        .and_then(|h| Some(h.shorthand().unwrap_or("HEAD").to_string()))
        .unwrap_or_default();
    Ok(new_head)
}

/// Stage all changes and commit with the given message. Returns the commit OID.
pub fn commit_all(workspace: &str, message: &str) -> Result<String, String> {
    let repo = open_repo(workspace)?;

    let mut index = repo.index().map_err(|e| format!("index: {e}"))?;
    index
        .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
        .map_err(|e| format!("add_all: {e}"))?;
    index.write().map_err(|e| format!("index write: {e}"))?;

    let tree_oid = index.write_tree().map_err(|e| format!("write_tree: {e}"))?;
    let tree = repo
        .find_tree(tree_oid)
        .map_err(|e| format!("find_tree: {e}"))?;

    let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let parents: Vec<&git2::Commit> = parent.iter().collect();

    let sig =
        git2::Signature::now("DeepX", "deepx@local").map_err(|e| format!("signature: {e}"))?;

    let oid = repo
        .commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
        .map_err(|e| format!("commit: {e}"))?;

    Ok(oid.to_string())
}

/// Get the diff for a single file (HEAD vs index + working tree) as a unified patch.
/// `*_with_index` includes both staged and unstaged edits, matching the status list.
pub fn file_diff(workspace: &str, file_path: &str) -> Result<String, String> {
    let repo = open_repo(workspace)?;
    let head = repo.head().map_err(|e| format!("head: {e}"))?;
    let head_tree = head.peel_to_tree().map_err(|e| format!("tree: {e}"))?;

    let mut diff_opts = DiffOptions::new();
    diff_opts.pathspec(file_path);

    let diff = repo
        .diff_tree_to_workdir_with_index(Some(&head_tree), Some(&mut diff_opts))
        .map_err(|e| format!("diff: {e}"))?;

    let mut patch_text = String::new();
    diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
        let origin = line.origin();
        let content = std::str::from_utf8(line.content()).unwrap_or("");
        // FileHeader ('F') / HunkHeader ('H') 行的 content 已是完整行
        // （"diff --git ...\n" / "@@ ...\n"），拼 origin 会把 'F'/'H' 混入
        // 输出，破坏 unified diff 格式（from_buffer 将无法解析）。
        match origin {
            'F' | 'H' => patch_text.push_str(content),
            _ => {
                patch_text.push(origin);
                patch_text.push_str(content);
            }
        }
        true
    })
    .map_err(|e| format!("print diff: {e}"))?;

    Ok(patch_text)
}

// ─────────────────────────────────────────────────────────────
// Patch 规范化（LLM 友好层）：行数修正 + 行号偏移重试 + 失败诊断
// ─────────────────────────────────────────────────────────────

/// 裸格式自动补 `diff --git` 头（libgit2 的 from_buffer 要求该头，
/// git CLI 则两种都接受）——宽容输入，标准语义。
fn auto_header(patch_text: &str) -> Result<String, String> {
    if patch_text.contains("diff --git") {
        return Ok(patch_text.to_string());
    }
    let old_path = patch_text
        .lines()
        .find(|l| l.starts_with("--- "))
        .map(|l| {
            l.trim_start_matches("--- ")
                .split('\t')
                .next()
                .unwrap_or("")
                .trim_start_matches("a/")
                .to_string()
        })
        .unwrap_or_default();
    if old_path.is_empty() {
        return Err("parse patch: no '--- a/<path>' header found in patch text".into());
    }
    Ok(format!("diff --git a/{old_path} b/{old_path}\n{patch_text}"))
}

/// 手写解析 `@@ -a[,b] +c[,d] @@ [section]` hunk 头。
/// 返回 ((old_start, old_count, new_start, new_count), section 后缀)。
fn parse_hunk_header(line: &str) -> Option<((usize, usize, usize, usize), String)> {
    let rest = line.trim_start().strip_prefix("@@")?;
    let (mid, suffix) = match rest.split_once(" @@") {
        Some((m, s)) => (m.trim(), s.trim().to_string()),
        None => (rest.trim(), String::new()),
    };
    let mut parts = mid.split_whitespace();
    let old = parts.next()?;
    let new = parts.next()?;
    let parse_side = |s: &str| -> Option<(usize, usize)> {
        let s = s.strip_prefix('-').or_else(|| s.strip_prefix('+'))?;
        let mut it = s.split(',');
        let start: usize = it.next()?.parse().ok()?;
        let count: usize = it.next().map(|c| c.parse().ok()).unwrap_or(Some(1))?;
        Some((start, count))
    };
    let (os, oc) = parse_side(old)?;
    let (ns, nc) = parse_side(new)?;
    Some(((os, oc, ns, nc), suffix))
}

/// 修正 hunk 头行数声明：LLM 手写 patch 时常算错 `@@ -a,b +c,d @@` 里的 b/d，
/// libgit2 会因此直接报 parse 错误。这里统计 hunk 体实际 context/removed/added
/// 行数，不符则重写声明（内容行原样保留，仅动头部）。
fn normalize_hunk_counts(patch_text: &str) -> Result<String, String> {
    let lines: Vec<&str> = patch_text.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim_start().starts_with("@@") {
            let mut body: Vec<&str> = Vec::new();
            let mut j = i + 1;
            while j < lines.len() {
                let l = lines[j];
                // hunk 体到下一个 @@ 或下一个文件头为止（diff --git / --- / +++
                // 是文件分隔行，不是 hunk 内容；内容行的 "-"/"+" 前缀后不会跟空格路径）
                if l.trim_start().starts_with("@@")
                    || l.starts_with("diff --git ")
                    || l.starts_with("--- ")
                    || l.starts_with("+++ ")
                {
                    break;
                }
                body.push(l);
                j += 1;
            }
            let (mut ctx, mut rem, mut add) = (0usize, 0usize, 0usize);
            for b in &body {
                if b.starts_with(' ') {
                    ctx += 1;
                } else if b.starts_with('-') {
                    rem += 1;
                } else if b.starts_with('+') {
                    add += 1;
                } // '\'（No newline）行忽略
            }
            match parse_hunk_header(line) {
                Some(((os, oc, ns, nc), suffix)) => {
                    let need_old = ctx + rem;
                    let need_new = ctx + add;
                    if need_old != oc || need_new != nc {
                        let head = format!("@@ -{os},{need_old} +{ns},{need_new} @@");
                        if suffix.is_empty() {
                            out.push(head);
                        } else {
                            out.push(format!("{head} {suffix}"));
                        }
                    } else {
                        out.push(line.to_string());
                    }
                }
                None => out.push(line.to_string()), // 无法解析的头原样保留（libgit2 会报错）
            }
            // hunk 体行原样输出（只动头部，不动内容）
            for b in &body {
                out.push(b.to_string());
            }
            i = j;
        } else {
            out.push(line.to_string());
            i += 1;
        }
    }
    // lines()+join 会丢掉尾随换行，而 libgit2 要求 patch 以换行结尾
    let mut text = out.join("\n");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

/// 对 patch 中所有 hunk 头做行号偏移 delta（offset 重试用）。
/// 偏移后行号 < 1 的版本跳过（无效）。
fn offset_hunks(patch_text: &str, delta: i64) -> Option<String> {
    let mut out = String::with_capacity(patch_text.len() + 32);
    let mut valid = true;
    for line in patch_text.lines() {
        if line.trim_start().starts_with("@@")
            && let Some(((os, oc, ns, nc), suffix)) = parse_hunk_header(line)
        {
            let (no, nn) = (os as i64 + delta, ns as i64 + delta);
            if no < 1 || nn < 1 {
                valid = false;
                break;
            }
            let head = format!("@@ -{no},{oc} +{nn},{nc} @@");
            if suffix.is_empty() {
                out.push_str(&head);
            } else {
                out.push_str(&format!("{head} {suffix}"));
            }
            out.push('\n');
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if valid { Some(out) } else { None }
}

/// 从 patch 文本提取 (文件路径, [(声明 old_start, 首条锚行)]) 列表。
/// 供失败诊断与 dry-run 预检共用。
fn extract_hunks(patch_text: &str) -> Vec<(String, Vec<(usize, String)>)> {
    let mut blocks: Vec<(String, Vec<(usize, String)>)> = Vec::new();
    let mut cur_path: Option<String> = None;
    let mut cur_hunks: Vec<(usize, String)> = Vec::new();
    let mut pending_start: Option<usize> = None;

    for line in patch_text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(p) = cur_path.take() {
                blocks.push((p, std::mem::take(&mut cur_hunks)));
            }
            let mut parts = rest.split_whitespace();
            let a = parts.next().unwrap_or("").trim_start_matches("a/");
            let b = parts.next().unwrap_or("").trim_start_matches("b/");
            cur_path = Some(if a == b { a.to_string() } else { b.to_string() });
            pending_start = None;
        } else if line.starts_with("--- ") && cur_path.is_none() {
            cur_path = Some(line.trim_start_matches("--- ").trim_start_matches("a/").to_string());
        } else if let Some(h) = parse_hunk_header(line) {
            pending_start = Some(h.0 .0);
        } else if let Some(start) = pending_start.take() {
            let is_anchor = (line.starts_with(' ') || line.starts_with('-'))
                && !line.starts_with("--- ")
                && !line.starts_with("+++ ")
                && !line.starts_with("diff --git ");
            if is_anchor && cur_path.is_some() {
                let anchor = line
                    .trim_start_matches([' ', '-'])
                    .trim_end_matches('\r')
                    .to_string();
                if !anchor.is_empty() {
                    cur_hunks.push((start, anchor));
                }
            }
        }
    }
    if let Some(p) = cur_path.take() {
        blocks.push((p, std::mem::take(&mut cur_hunks)));
    }
    blocks
}

/// 失败诊断：文件存在性 + 每个 hunk 锚行在文件中的实际位置（偏移量）。
fn diagnose_apply_failure(workspace: &str, patch_text: &str) -> String {
    let mut msgs: Vec<String> = Vec::new();
    for (path, hunks) in extract_hunks(patch_text) {
        let full = std::path::Path::new(workspace).join(&path);
        if !full.exists() {
            msgs.push(format!("{path}: file does not exist in the repo (check the patch path)"));
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&full) else {
            msgs.push(format!("{path}: cannot read file"));
            continue;
        };
        let file_lines: Vec<&str> = content.lines().collect();
        for (declared, anchor) in &hunks {
            let actual = file_lines
                .iter()
                .position(|l| l.trim() == anchor.trim())
                .map(|i| i + 1);
            match actual {
                Some(n) if n as i64 != *declared as i64 => {
                    let d = n as i64 - *declared as i64;
                    msgs.push(format!(
                        "{path}: hunk declares L{declared}, anchor {anchor:?} is actually at L{n} (offset {d:+#})"
                    ));
                }
                Some(_) => {}
                None => msgs.push(format!("{path}: hunk anchor {anchor:?} not found in file")),
            }
        }
    }
    if msgs.is_empty() {
        "context mismatch within the declared ranges (read the file and regenerate the patch)".to_string()
    } else {
        msgs.join("; ")
    }
}

// ─────────────────────────────────────────────────────────────
// Apply 主流程
// ─────────────────────────────────────────────────────────────

/// 单次尝试：from_buffer + stats + apply，返回 (files, insertions, deletions, touched)。
fn try_apply(
    repo: &git2::Repository,
    text: &str,
    loc: git2::ApplyLocation,
) -> Result<(usize, usize, usize, Vec<String>), String> {
    let diff = git2::Diff::from_buffer(text.as_bytes()).map_err(|e| format!("parse patch: {e}"))?;
    let stats = diff.stats().map_err(|e| format!("diff stats: {e}"))?;
    let (f, i, d) = (stats.files_changed(), stats.insertions(), stats.deletions());
    let mut touched: Vec<String> = Vec::new();
    for delta in diff.deltas() {
        if let Some(p) = delta.new_file().path() {
            touched.push(p.to_string_lossy().to_string());
        }
    }
    // stats/touched 必须在 apply 之前取：git_apply 会消费 diff 内部状态
    repo.apply(&diff, loc, None).map_err(|e| format!("{e}"))?;
    Ok((f, i, d, touched))
}

/// 行号偏移自动修正的最大尝试范围（对齐 git CLI 的 offset 语义；
/// libgit2 本身无 offset 搜索，这里在文本层重写 hunk 头后重试）。
const MAX_OFFSET: i64 = 5;

/// Apply a unified diff (patch text) to the repository — **standard git apply
/// semantics** via libgit2's apply engine, with LLM-friendly auto-correction:
///
/// - **事务：整包**——任一 hunk 上下文不符则整个 patch 拒绝，磁盘零改动；
/// - **行号自动修正**——hunk 头行数声明错误自动重算；行号偏移 ±MAX_OFFSET
///   内自动重试（上下文仍须精确匹配，不会错误应用）；
/// - 输入：标准 git 格式（`git diff`/`format-patch` 输出）；裸 `---/+++`
///   格式自动补头；
/// - 失败时附诊断：文件存在性 + 每个 hunk 锚行的实际位置（偏移量）。
///
/// `location`: `workdir` (default) | `index` | `both`.
/// Returns `{files, insertions, deletions, location, touched, offset_used}`.
pub fn apply_patch(workspace: &str, patch_text: &str, location: &str) -> Result<String, String> {
    let repo = open_repo(workspace)?;

    let loc = match location {
        "workdir" => git2::ApplyLocation::WorkDir,
        "index" => git2::ApplyLocation::Index,
        "both" => git2::ApplyLocation::Both,
        other => {
            return Err(format!(
                "invalid apply location {other:?} — use \"workdir\" | \"index\" | \"both\""
            ))
        }
    };

    let normalized = normalize_hunk_counts(&auto_header(patch_text)?)?;
    let mut last_err: Option<String> = None;
    let mut applied: Option<(i64, usize, usize, usize, Vec<String>)> = None;

    'retry: for delta in 0i64..=MAX_OFFSET {
        for sign in [1i64, -1i64] {
            if delta == 0 && sign == -1 {
                continue;
            }
            let candidate = if delta == 0 {
                normalized.clone()
            } else {
                match offset_hunks(&normalized, sign * delta) {
                    Some(c) => c,
                    None => continue,
                }
            };
            match try_apply(&repo, &candidate, loc) {
                Ok(meta) => {
                    applied = Some((sign * delta, meta.0, meta.1, meta.2, meta.3));
                    break 'retry;
                }
                Err(e) => last_err = Some(e),
            }
        }
    }

    match applied {
        Some((offset, files, insertions, deletions, touched)) => Ok(serde_json::json!({
            "files": files,
            "insertions": insertions,
            "deletions": deletions,
            "location": location,
            "touched": touched,
            "offset_used": offset,
        })
        .to_string()),
        None => Err(format!(
            "apply patch: {}",
            last_err.unwrap_or_else(|| "unknown error".into())
        ) + "\ndiagnostics: "
            + &diagnose_apply_failure(workspace, &normalized)),
    }
}

/// 预检 patch 而不应用（`git apply --check` 的库级近似）：
/// 格式解析 + 行数统计 + 每个 hunk 的上下文预检（锚行实际位置/偏移量）。
/// 注意 libgit2 无纯 check API，最终上下文匹配以真实 apply 为准。
pub fn check_patch(workspace: &str, patch_text: &str) -> Result<String, String> {
    // 校验 workspace 是 git 仓库（失败即报错，不应用）
    open_repo(workspace)?;
    let normalized = normalize_hunk_counts(&auto_header(patch_text)?)?;
    let diff = git2::Diff::from_buffer(normalized.as_bytes())
        .map_err(|e| format!("parse patch: {e}"))?;
    let stats = diff.stats().map_err(|e| format!("diff stats: {e}"))?;

    let mut hunks: Vec<serde_json::Value> = Vec::new();
    for (path, hs) in extract_hunks(&normalized) {
        let full = std::path::Path::new(workspace).join(&path);
        let content = if full.exists() {
            std::fs::read_to_string(&full).ok()
        } else {
            None
        };
        let file_lines: Option<Vec<String>> = content
            .as_ref()
            .map(|c| c.lines().map(|l| l.to_string()).collect());
        for (declared, anchor) in hs {
            let (actual, match_ok) = match &file_lines {
                Some(lines) => lines
                    .iter()
                    .position(|l| l.trim() == anchor.trim())
                    .map(|i| (Some(i + 1), true))
                    .unwrap_or((None, false)),
                None => (None, false),
            };
            let offset = actual.map(|n| n as i64 - declared as i64);
            hunks.push(serde_json::json!({
                "file": path,
                "declared_line": declared,
                "anchor_actual_line": actual,
                "offset": offset,
                "context_match": match_ok,
            }));
        }
    }

    Ok(serde_json::json!({
        "files": stats.files_changed(),
        "insertions": stats.insertions(),
        "deletions": stats.deletions(),
        "applied": false,
        "hunks": hunks,
    })
    .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn branch_switch_without_stash_preserves_dirty_worktree() {
        let dir = tempfile::tempdir().expect("temp repo");
        let repo = Repository::init(dir.path()).expect("init repo");
        fs::write(dir.path().join("tracked.txt"), "committed\n").expect("write fixture");

        let mut index = repo.index().expect("index");
        index
            .add_path(Path::new("tracked.txt"))
            .expect("stage fixture");
        let tree_id = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_id).expect("find tree");
        let signature = git2::Signature::now("DeepX Test", "deepx-test@local").expect("signature");
        let commit_id = repo
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .expect("initial commit");
        let commit = repo.find_commit(commit_id).expect("find commit");
        repo.branch("feature", &commit, false)
            .expect("create branch");
        let original_branch = current_branch(dir.path().to_str().unwrap()).expect("current branch");

        fs::write(dir.path().join("tracked.txt"), "uncommitted\n").expect("dirty fixture");

        let result = switch_branch(dir.path().to_str().unwrap(), "feature", false);

        assert_eq!(
            fs::read_to_string(dir.path().join("tracked.txt")).unwrap(),
            "uncommitted\n"
        );
        let branch_after = current_branch(dir.path().to_str().unwrap()).unwrap();
        if result.is_err() {
            assert_eq!(branch_after, original_branch);
        }
        assert!(branch_after == original_branch || branch_after == "feature");
    }

    /// 建一个带初始 commit 的临时仓库，返回 (tempdir, workspace path)。
    fn repo_with_commit(content: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("temp repo");
        let repo = Repository::init(dir.path()).expect("init repo");
        fs::write(dir.path().join("tracked.txt"), content).expect("write fixture");
        let mut index = repo.index().expect("index");
        index
            .add_path(Path::new("tracked.txt"))
            .expect("stage fixture");
        // 必须 write() 落盘：libgit2 的 commit 不会像 git CLI 那样自动刷新 index，
        // 否则磁盘 index 为空，diff_tree_to_workdir_with_index 会误判 INDEX_DELETED。
        index.write().expect("write index");
        let tree_id = index.write_tree().expect("write tree");
        let tree = repo.find_tree(tree_id).expect("find tree");
        let signature = git2::Signature::now("DeepX Test", "deepx-test@local").expect("signature");
        repo.commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .expect("initial commit");
        let ws = dir.path().to_str().unwrap().to_string();
        (dir, ws)
    }

    #[test]
    fn apply_patch_closes_generate_apply_loop() {
        // 闭环：修改 → file_diff 生成 patch → 恢复原状 → apply_patch 合入 → 内容一致
        let (dir, ws) = repo_with_commit("line1\nline2\nline3\n");
        let file = dir.path().join("tracked.txt");

        // 1) 修改工作区（模拟模型编辑）
        fs::write(&file, "line1\nLINE2-CHANGED\nline3\nline3b\n").unwrap();
        // 2) 生成 patch（HEAD vs workdir）
        let patch = file_diff(&ws, "tracked.txt").expect("generate patch");
        assert!(patch.contains("@@"), "patch must contain hunks: {patch}");
        // 3) 恢复原状（模拟丢弃修改）
        fs::write(&file, "line1\nline2\nline3\n").unwrap();
        // 4) git apply 语义合入
        let result = apply_patch(&ws, &patch, "workdir").expect("apply patch");
        let meta: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(meta["files"], 1);
        assert_eq!(meta["insertions"], 2); // LINE2-CHANGED + line3b
        assert_eq!(meta["deletions"], 1); // line2
        // 5) 内容与生成 patch 时一致（LF 归一化对比：git_apply 按 core.autocrlf
        //    将 workdir 写为 CRLF，与 git CLI 行为一致，属正确 git 语义）
        let actual = fs::read_to_string(&file).unwrap();
        assert_eq!(
            actual.replace("\r\n", "\n"),
            "line1\nLINE2-CHANGED\nline3\nline3b\n",
            "actual: {actual:?}"
        );
    }

    #[test]
    fn apply_patch_rejects_bad_patch_text() {
        let (_dir, ws) = repo_with_commit("line1\nline2\n");
        let err = apply_patch(&ws, "this is not a diff", "workdir").unwrap_err();
        assert!(err.contains("parse patch"), "got: {err}");
    }

    #[test]
    fn apply_patch_rejects_context_mismatch() {
        // git apply 语义：上下文不符 → 拒绝且不部分应用
        let (dir, ws) = repo_with_commit("aaa\nbbb\nccc\n");
        let patch = "--- a/tracked.txt\n+++ b/tracked.txt\n@@ -1,3 +1,3 @@\n aaa\n-WRONG_CONTEXT\n+XXX\n ccc\n";
        let err = apply_patch(&ws, patch, "workdir").unwrap_err();
        assert!(err.contains("apply patch"), "got: {err}");
        // 工作区未被部分修改（libgit2 全量校验后才应用）
        assert_eq!(
            fs::read_to_string(dir.path().join("tracked.txt")).unwrap(),
            "aaa\nbbb\nccc\n"
        );
    }

    #[test]
    fn apply_patch_rejects_invalid_location() {
        let (_dir, ws) = repo_with_commit("line1\n");
        let err = apply_patch(&ws, "--- a/x\n+++ b/x\n@@ -1,1 +1,1 @@\n-line1\n+line2\n", "elsewhere").unwrap_err();
        assert!(err.contains("invalid apply location"), "got: {err}");
    }
}
