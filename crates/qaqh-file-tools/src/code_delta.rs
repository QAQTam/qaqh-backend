//! Code-delta calculation for successful file mutations.
//!
//! R-4 豁免点：本文件与 `execution.rs` 使用的 `qaqh_types::CodeDeltaRecord`
//! 是编辑效果的被动纯数据记录，属 R-4 允许的「被动投影记录」（口径与完整例外
//! 清单见 `dashboard.rs:7`）——除此以外禁止引入 domain 事件/行为类型。
//!
//! 本模块**零子进程**。`files_created` 由 `execution.rs` 派发前调用的
//! [`probe_pre_existed`]（一次 `fs::metadata`）判定。此前这里是 git2 → git CLI
//! 两步探测（`rev-parse HEAD` + `cat-file -e HEAD:<path>`，以「是否在 HEAD」
//! 近似「是否新建」），已整体移除：
//! - 统计口径不值得每次成功的 write/edit/delete 付两次进程 spawn（Windows 上
//!   尤其贵）；
//! - HEAD 口径本身不准——未 commit 的既有文件会被报成新建；
//! - 旧探测还把 delete 档位算好的 `files_deleted=1` 覆写成 0（当时的注释自认
//!   「既有口径」），随之一起修正。

/// 派发前存在性探针：args 里的 `path`/`file_path` 在本次调用动手之前是否
/// 已是磁盘上的文件。一次 `fs::metadata`，零 spawn。绝对路径原样、相对路径
/// 锚定 workspace_root（与 `file_mutate::resolve_mutation_path` 同口径）。
/// 只有 `write` 的结果被消费（判 `files_created`）；其余档位忽略该值。
pub fn probe_pre_existed(args: &serde_json::Value, workspace_root: &std::path::Path) -> bool {
    let Some(raw) = crate::support::arg_path(args) else {
        return false;
    };
    let path = std::path::Path::new(raw);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace_root.join(path)
    };
    resolved.is_file()
}

pub fn compute(
    tool_name: &str,
    args: &serde_json::Value,
    file_existed_before: bool,
) -> Option<qaqh_types::CodeDeltaRecord> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let file_path = crate::support::arg_path(args);

    // 行数一律从 args 文本数出来（廉价，无子进程）。
    // 档位键是 execution.rs 的 resolved tool name：`write` 不是 `file`
    // （旧拼法 `("file", "write")` 在这里永远匹配不上，实测返回 None）。
    match tool_name {
        "write" => {
            let content = args
                .get("content")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            Some(qaqh_types::CodeDeltaRecord {
                timestamp: now,
                lines_added: content.lines().count(),
                lines_removed: 0,
                // 覆盖既有文件不算创建；派发前探针是唯一的判定源。
                files_created: usize::from(!file_existed_before),
                files_deleted: 0,
                file: file_path.map(String::from),
            })
        }
        "apply_patch" => {
            // 行数与增删文件数都从补丁文本数出来。此前这一档落进
            // `_ => None`，CodeChanged 与 code_stats.jsonl 对 apply_patch 整体缺席。
            let patch = args.get("patch").and_then(|value| value.as_str())?;
            let stats = crate::apply_patch_engine::patch_stats(patch).ok()?;
            Some(qaqh_types::CodeDeltaRecord {
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
        "delete" => Some(qaqh_types::CodeDeltaRecord {
            timestamp: now,
            lines_added: 0,
            lines_removed: 0,
            files_created: 0,
            // 成功即删除了一个文件（旧 git 探测会把这里覆写成 0，已随探测移除）。
            files_deleted: 1,
            file: file_path.map(String::from),
        }),
        "edit" => {
            // str_replace：old_string 删除行数、new_string 新增行数（旧拼写同权）。
            let added = args
                .get("new_string")
                .or_else(|| args.get("new_str"))
                .and_then(|x| x.as_str())
                .map(|s| s.lines().count())
                .unwrap_or(0);
            let removed = args
                .get("old_string")
                .or_else(|| args.get("old_str"))
                .and_then(|x| x.as_str())
                .map(|s| s.lines().count())
                .unwrap_or(0);
            Some(qaqh_types::CodeDeltaRecord {
                timestamp: now,
                lines_added: added,
                lines_removed: removed,
                // edit 读取既有文件是成功前提，本次调用不可能是创建者。
                files_created: 0,
                files_deleted: 0,
                file: file_path.map(String::from),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{compute, probe_pre_existed};
    use serde_json::json;

    /// 探针只读磁盘，不碰任何子进程；绝对/相对/缺 path 三种拼写都要给对。
    #[test]
    fn probe_pre_existed_reads_the_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let existing = dir.path().join("has.txt");
        std::fs::write(&existing, "x").expect("write seed");
        assert!(
            probe_pre_existed(&json!({ "path": existing.to_string_lossy() }), dir.path()),
            "绝对路径已存在 → true"
        );
        assert!(
            probe_pre_existed(&json!({ "file_path": "has.txt" }), dir.path()),
            "相对路径锚定 workspace_root"
        );
        assert!(
            !probe_pre_existed(&json!({ "path": "nope.txt" }), dir.path()),
            "不存在 → false"
        );
        assert!(
            !probe_pre_existed(&json!({}), dir.path()),
            "无 path 参数 → false"
        );
    }

    /// write 的 files_created 完全由派发前探针决定：新建=1，覆盖=0。
    /// （旧 git 探测把「未 commit 的既有文件」报成新建，此处口径改为磁盘。）
    #[test]
    fn write_files_created_follows_the_pre_dispatch_probe() {
        let args = json!({ "path": "a.txt", "content": "x\ny\n" });
        let created = compute("write", &args, false).expect("write delta");
        assert_eq!((created.files_created, created.files_deleted), (1, 0));
        let overwrite = compute("write", &args, true).expect("write delta");
        assert_eq!((overwrite.files_created, overwrite.files_deleted), (0, 0));
    }

    /// edit 成功前提就是文件存在，永远不报创建（旧档位恒 1 全靠 git 覆写回 0，
    /// 无 git 的仓库外路径因此虚报——现在结构性不可能）。
    #[test]
    fn edit_never_claims_creation() {
        let delta = compute(
            "edit",
            &json!({"file_path": "a.rs", "old_string": "x", "new_string": "y"}),
            true,
        )
        .expect("edit delta");
        assert_eq!((delta.lines_added, delta.lines_removed), (1, 1));
        assert_eq!((delta.files_created, delta.files_deleted), (0, 0));
    }

    /// 回归锁：delete 的 files_deleted=1 不许再被任何覆写归零。
    #[test]
    fn delete_keeps_its_deletion_count() {
        let delta = compute("delete", &json!({ "path": "a.txt" }), true).expect("delete delta");
        assert_eq!((delta.files_created, delta.files_deleted), (0, 1));
    }

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
        let delta =
            compute("apply_patch", &json!({ "patch": patch }), false).expect("apply_patch delta");
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
        let delta =
            compute("apply_patch", &json!({ "patch": patch }), false).expect("apply_patch delta");
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
                false,
            )
            .is_none(),
            "缺 End Patch 的补丁不该出数"
        );
        assert!(compute("apply_patch", &json!({}), false).is_none());
    }

    /// 档位键必须是 resolved tool name。旧写法 `("file", "write")` 实测永远
    /// 匹配不上（注册表里这个工具就叫 `write`），write 因此从不出 CodeChanged。
    #[test]
    fn write_tool_reports_content_line_count() {
        let delta = compute(
            "write",
            &json!({ "path": "a.txt", "content": "x\ny\n" }),
            false,
        )
        .expect("write 必须出 CodeChanged");
        assert_eq!((delta.lines_added, delta.lines_removed), (2, 0));
        assert_eq!(delta.file.as_deref(), Some("a.txt"));
    }
}
