//! Code-delta 统计：终态展示投影的被动再投影。
//!
//! R-4 豁免点：本文件与 `execution.rs` 使用的 `qaqh_types::CodeDeltaRecord`
//! 只是「这次改动长什么样」的被动纯数据记录，不携带任何行为；此处不得引入
//! domain 事件/行为类型。
//!
//! **只有一个事实源：`ToolResultDisplay`。** 工具在落盘那一刻已经把行差算成
//! 真 diff（`file_shared::diff_stats_between` 逐块比对盘上正文，apply_patch 用
//! 补丁引擎的逐文件计数），这里不再从参数文本反推第二套数字。
//!
//! 被废弃的旧口径及其病灶：
//! - `edit` 从 `old_string`/`new_string` 数行 → 改 10 行块里的 1 行报 `+10 −10`
//!   （展示面同一调用报 `+1 −1`），`replace_all` 命中 N 次只算 1 次；
//! - `write` 从 `content` 数行且 `lines_removed` 恒 0 → 覆盖既有文件时声称「没删
//!   任何行」；
//! - `delete` 恒 `+0 −0` → 明明整份正文没了；
//! - `files_created` 靠派发前一次 `fs::metadata` 探针猜——而工具自己已经知道
//!   这次是新建还是覆盖（`files[].op`）。
//!
//! 现在拿不出权威行差就不出记录，绝不拿参数文本凑一个数。

/// 把一次成功调用的展示投影折算成 code-delta 记录。
///
/// `None` = 这个工具没有行数概念（read/grep/exec…），或它自己声明没算
/// （`dry_run` 预览不声称改动）。`Some(0)/Some(0)` = 真的零行差，与「不知道」
/// 在类型上就是两件事，这里如实透传。
pub fn from_display(
    display: &qaqh_types::ToolResultDisplay,
) -> Option<qaqh_types::CodeDeltaRecord> {
    let lines_added = display.lines_added?;
    let lines_removed = display.lines_removed?;
    // 单文件调用归属到那一个路径；多文件补丁不归属到任何单一路径（旧口径保持）。
    let file = match display.files.as_slice() {
        [only] => Some(only.path.clone()),
        _ => None,
    };
    Some(qaqh_types::CodeDeltaRecord {
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        lines_added: lines_added as usize,
        lines_removed: lines_removed as usize,
        files_created: count_op(display, "add"),
        files_deleted: count_op(display, "delete"),
        file,
    })
}

fn count_op(display: &qaqh_types::ToolResultDisplay, op: &str) -> usize {
    display
        .files
        .iter()
        .filter(|file| file.op == op)
        .count()
}

#[cfg(test)]
mod tests {
    use super::from_display;
    use qaqh_types::{ToolResultDisplay, ToolResultDisplayFile};

    fn display(
        added: Option<u32>,
        removed: Option<u32>,
        files: Vec<(&str, &str)>,
    ) -> ToolResultDisplay {
        ToolResultDisplay {
            summary: None,
            diff: None,
            files: files
                .into_iter()
                .map(|(path, op)| ToolResultDisplayFile {
                    path: path.to_string(),
                    op: op.to_string(),
                    lines_added: added,
                    lines_removed: removed,
                })
                .collect(),
            lines_added: added,
            lines_removed: removed,
            header: None,
            body: None,
            outcome: None,
        }
    }

    /// 编辑既有文件：真 diff 说了算，新建/删除计数都由 `files[].op` 决定，
    /// 不再需要任何派发前探针。
    #[test]
    fn update_reports_the_authored_line_delta() {
        let record = from_display(&display(Some(1), Some(1), vec![("src/a.rs", "update")]))
            .expect("display 出数即出记录");
        assert_eq!(
            (
                record.lines_added,
                record.lines_removed,
                record.files_created,
                record.files_deleted
            ),
            (1, 1, 0, 0)
        );
        assert_eq!(record.file.as_deref(), Some("src/a.rs"));
    }

    /// `write` 新建 vs 覆盖由工具自己的 `op` 区分——旧实现靠 `fs::metadata` 猜。
    #[test]
    fn created_and_deleted_files_come_from_file_ops() {
        let created = from_display(&display(Some(5), Some(0), vec![("new.txt", "add")])).unwrap();
        assert_eq!((created.files_created, created.files_deleted), (1, 0));
        let deleted = from_display(&display(Some(0), Some(5), vec![("gone.txt", "delete")])).unwrap();
        assert_eq!((deleted.files_created, deleted.files_deleted), (0, 1));
        assert_eq!(deleted.lines_removed, 5, "delete 不再是恒 0");
    }

    /// 多文件补丁不归属到单一路径，行差取终态合计。
    #[test]
    fn multi_file_promise_has_no_single_path() {
        let record = from_display(&display(
            Some(3),
            Some(1),
            vec![("a.txt", "add"), ("b.txt", "delete"), ("c.txt", "update")],
        ))
        .unwrap();
        assert_eq!(record.file, None);
        assert_eq!(
            (record.files_created, record.files_deleted, record.lines_added),
            (1, 1, 3)
        );
    }

    /// 只读工具（以及只出预览的 dry_run）不出行差：没有记录，而不是记录一堆 0。
    #[test]
    fn no_line_authority_means_no_record() {
        assert!(from_display(&display(None, None, vec![("src/a.rs", "update")])).is_none());
        assert!(from_display(&display(Some(2), None, vec![])).is_none());
        assert!(from_display(&display(Some(0), Some(0), vec![])).is_some());
    }
}
