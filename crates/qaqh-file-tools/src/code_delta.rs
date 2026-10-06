//! Code-delta calculation for successful file mutations.
//!
//! R-4 豁免点：本文件与 `execution.rs` 使用的 `qaqh_domain::CodeDeltaRecord`
//! 是编辑效果的被动纯数据记录，属 R-4 允许的「被动投影记录」（口径与完整例外
//! 清单见 `dashboard.rs:7`）——除此以外禁止引入 domain 事件/行为类型。

pub fn compute(
    tool_name: &str,
    args: &serde_json::Value,
    workspace_root: &std::path::Path,
    session_id: &str,
) -> Option<qaqh_domain::CodeDeltaRecord> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let action = args
        .get("action")
        .and_then(|value| value.as_str())
        .unwrap_or(tool_name);
    let file_path = args.get("path").and_then(|value| value.as_str());

    // Compute text-based line counts from args (cheap, no git2 pathspec bug).
    // 档位键是 execution.rs 的 resolved tool name：`write` 不是 `file`
    // （旧拼法 `("file", "write")` 在这里永远匹配不上，实测返回 None）。
    let mut delta = match (tool_name, action) {
        ("write", _) => {
            let content = args
                .get("content")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            Some(qaqh_domain::CodeDeltaRecord {
                timestamp: now,
                lines_added: content.lines().count(),
                lines_removed: 0,
                files_created: 1,
                files_deleted: 0,
                file: file_path.map(String::from),
            })
        }
        ("apply_patch", _) => {
            // 行数从补丁文本数出来（不走 git：apply_patch 没有 `path` 参数，
            // 下面按单路径查 HEAD 的修正对它无意义）。此前这一档落进
            // `_ => None`，CodeChanged 与 code_stats.jsonl 对 apply_patch 整体缺席。
            let patch = args.get("patch").and_then(|value| value.as_str())?;
            let stats = crate::apply_patch_engine::patch_stats(patch).ok()?;
            Some(qaqh_domain::CodeDeltaRecord {
                timestamp: now,
                lines_added: stats.lines_added,
                lines_removed: stats.lines_removed,
                files_created: stats.files_created,
                files_deleted: stats.files_deleted,
                file: stats
                    .single_path
                    .map(|path| path.to_string_lossy().into_owned()),
            })
        }
        ("delete", _) => Some(qaqh_domain::CodeDeltaRecord {
            timestamp: now,
            lines_added: 0,
            lines_removed: 0,
            files_created: 0,
            files_deleted: 1,
            file: file_path.map(String::from),
        }),
        ("edit", _) => {
            // str_replace：old_str 删除行数、new_str 新增行数。
            let added = args
                .get("new_str")
                .and_then(|x| x.as_str())
                .map(|s| s.lines().count())
                .unwrap_or(0);
            let removed = args
                .get("old_str")
                .and_then(|x| x.as_str())
                .map(|s| s.lines().count())
                .unwrap_or(0);
            Some(qaqh_domain::CodeDeltaRecord {
                timestamp: now,
                lines_added: added,
                lines_removed: removed,
                files_created: 1,
                files_deleted: 0,
                file: file_path.map(String::from),
            })
        }
        _ => None,
    };

    // Override files_created from git when available (files_deleted is always
    // reset to 0 here — git_file_meta only checks HEAD tree existence, so it
    // never reports deletions and even clobbers the delete tool's files_deleted=1).
    // git2::Repository::open is a cheap metadata op — no diff, no
    // pathspec bug since we only check HEAD tree existence.
    if let (Some(path), Some(d)) = (file_path, &mut delta)
        && let Some(git) = git_file_meta(path, workspace_root, session_id)
    {
        d.files_created = git.files_created;
        d.files_deleted = git.files_deleted;
    }

    delta
}

/// Lightweight git file metadata — only checks HEAD tree existence, no diff.
/// Avoids the git2 pathspec bug that inflated lines_added / lines_removed.
pub(crate) struct GitFileMeta {
    files_created: usize,
    files_deleted: usize,
}

pub(crate) fn git_file_meta(
    file_path: &str,
    workspace_root: &std::path::Path,
    session_id: &str,
) -> Option<GitFileMeta> {
    if session_id.is_empty() {
        return None;
    }
    // PR-3-3：cwd 由宿主注入（daemon 会话初始化 / serve 请求体均已 set）。
    // 读注入值本身：空 / "." 视为无有效工作区（与旧只读磁盘解析的
    // “无 cwd → 无 git meta”语义对齐）。
    let workspace = workspace_root.to_string_lossy();
    if workspace.is_empty() || workspace == "." {
        return None;
    }
    // P2 去 git2：改用 git CLI 两步探测，语义与旧 libgit2 路径逐臂对齐——
    // ① `rev-parse --verify -q HEAD`：无仓库/无 HEAD → None（保留调用方
    //    档位算出的 files_created，与旧 Repository::open 失败回退一致）；
    // ② `cat-file -e HEAD:<path>`：exit 0 = 已在 HEAD（编辑既有文件 →
    //    files_created 归 0）；非 0 = HEAD 中不存在（新建 → files_created=1，
    //    与旧 head_tree.get_path 失败臂一致，含 delete 工具 files_deleted
    //    被覆写为 0 的既有口径）。
    // git 不可用（spawn 失败）→ None，行为同旧 open 失败。
    let head = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", "HEAD"])
        .current_dir(workspace.as_ref())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?;
    if !head.success() {
        return None;
    }
    let in_head = std::process::Command::new("git")
        .args(["cat-file", "-e", &format!("HEAD:{file_path}")])
        .current_dir(workspace.as_ref())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?;
    Some(match in_head.code() {
        Some(0) => GitFileMeta {
            files_created: 0,
            files_deleted: 0,
        },
        _ => GitFileMeta {
            files_created: 1,
            files_deleted: 0,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::compute;
    use serde_json::json;

    /// apply_patch 此前落进 `_ => None`：CodeChanged 与 code_stats.jsonl 对它
    /// 整体缺席。行数从补丁文本数，上下文行不计入。
    #[test]
    fn apply_patch_reports_line_counts_from_the_patch() {
        let patch = "\
*** Begin Patch
*** Update File: src/a.rs
@@
-old one
-old two
+new one
+new two
+new three
 context line
*** End Patch
";
        let delta = compute(
            "apply_patch",
            &json!({ "patch": patch }),
            std::path::Path::new("."),
            "s",
        )
        .expect("apply_patch delta");
        assert_eq!((delta.lines_added, delta.lines_removed), (3, 2));
        assert_eq!((delta.files_created, delta.files_deleted), (0, 0));
        assert_eq!(delta.file.as_deref(), Some("src/a.rs"));
    }

    #[test]
    fn multi_file_apply_patch_delta_sums_across_files() {
        let patch = "\
*** Begin Patch
*** Add File: a.txt
+one
+two
*** Delete File: b.txt
*** Update File: c.txt
@@
-x
+X
*** End Patch
";
        let delta = compute(
            "apply_patch",
            &json!({ "patch": patch }),
            std::path::Path::new("."),
            "s",
        )
        .expect("apply_patch delta");
        assert_eq!((delta.lines_added, delta.lines_removed), (3, 1));
        assert_eq!((delta.files_created, delta.files_deleted), (1, 1));
        assert_eq!(delta.file, None, "多文件补丁不归属到单一路径");
    }

    /// 数不出来就不报数字，而不是报一个假的零。
    #[test]
    fn apply_patch_without_a_parseable_patch_yields_no_delta() {
        assert!(
            compute(
                "apply_patch",
                &json!({ "patch": "*** Begin Patch\n+x\n" }),
                std::path::Path::new("."),
                "s",
            )
            .is_none(),
            "缺 End Patch 的补丁不该出数"
        );
        assert!(compute("apply_patch", &json!({}), std::path::Path::new("."), "s").is_none());
    }

    /// 档位键必须是 resolved tool name。旧写法 `("file", "write")` 实测永远
    /// 匹配不上（注册表里这个工具就叫 `write`），write 因此从不出 CodeChanged。
    #[test]
    fn write_tool_reports_content_line_count() {
        let delta = compute(
            "write",
            &json!({ "path": "a.txt", "content": "x\ny\n" }),
            std::path::Path::new("."),
            "s",
        )
        .expect("write 必须出 CodeChanged");
        assert_eq!((delta.lines_added, delta.lines_removed), (2, 0));
        assert_eq!(delta.file.as_deref(), Some("a.txt"));
    }
}
