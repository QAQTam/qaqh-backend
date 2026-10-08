//! exec 调用级 spy 双扫（CLEAN-3 / T9）。
//!
//! 批级审计（runtime `workspace_audit`）只能把整批变更按批归因——并行工具
//! 交错时无法区分谁改了什么，多调用批只能如实标 `scan`。exec 是归因价值
//! 最高的盲区（模型跑脚本批量改文件），本模块在 **exec 工具的实际执行点**
//! 前后各补一次扫描，把该调用的净变更精确归因：
//!
//! 1. SMJ 记账：`journal::record_change(tool=exec, tool_use_id=call_id)`，
//!    模型用既有 journal 工具 query/replay 就能找回脚本改动；
//! 2. file_state 账本刷新：消除脚本改动后的 `stale_file` 假阳性；
//! 3. 返回涉及路径的账本键，供批级 `finish` 去重——同一变更不得既记
//!    exec 又记 scan（审计流水翻倍）。
//!
//! 失败绝不阻断工具调用：扫描/存储任何错误只 warn，退化为无归因
//! （批级审计兜底）。与批级扫描并存的双份开销只落在 exec 上——它本身
//! 就是重型调用，可忽略。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use qaqh_spy::{Change, ChangeStatus, ScanId, ScanOpts, Session, Trigger};

/// 争锁等待上界：与 runtime 批级审计同口径。并行 exec 各自开 session 扫描
/// 时会争 store 锁，超时即放弃本次归因（批级兜底），不阻塞工具线程。
const LOCK_WAIT: Duration = Duration::from_millis(500);

/// 一次 exec 调用的审计边界：`begin` 打点，`finish` 收口并归因。
pub struct ExecAudit {
    session: Session,
    mark: ScanId,
    root: PathBuf,
    call_id: String,
    session_id: String,
}

/// exec 执行**之前**打点。失败返回 `None`（静默退化）。
pub fn begin(root: &Path, call_id: &str, session_id: &str) -> Option<ExecAudit> {
    if matches!(
        std::env::var("QAQH_SPY_DISABLED").as_deref(),
        Ok("1") | Ok("true")
    ) {
        return None;
    }
    let session = Session::open(root, None)
        .map(|s| s.with_opts(ScanOpts {
            lock_wait: LOCK_WAIT,
            ..ScanOpts::default()
        }))
        .map_err(|e| log::warn!("[spy] exec 归因：打开会话失败，本调用跳过: {e}"))
        .ok()?;
    let outcome = session
        .scan(Trigger::ToolStart)
        .map_err(|e| log::warn!("[spy] exec 归因：边界扫描失败，本调用跳过: {e}"))
        .ok()?;
    Some(ExecAudit {
        session,
        mark: ScanId(outcome.id),
        root: root.to_path_buf(),
        call_id: call_id.to_string(),
        session_id: session_id.to_string(),
    })
}

impl ExecAudit {
    /// exec 执行**之后**收口：补扫 → SMJ 记账（tool=exec）→ 刷新账本。
    /// 返回涉及路径的 file_state 账本键（供批级审计去重）。
    pub fn finish(self) -> Vec<String> {
        let changes = match self.session.changes_after_tool_end(&self.mark) {
            Ok(changes) => changes,
            Err(error) => {
                log::warn!("[spy] exec 归因：批尾扫描失败（不影响工具结果）: {error}");
                return Vec::new();
            }
        };
        let mut attributed: HashSet<String> = HashSet::new();
        for change in &changes {
            let abs = absolutize(&self.root, &change.path);
            attributed.insert(crate::file_state::ledger_key(&abs));
            let before = change
                .before
                .as_deref()
                .and_then(|sha| self.session.cat(sha).ok());
            let after = change
                .after
                .as_deref()
                .and_then(|sha| self.session.cat(sha).ok());
            let before_text = before.as_deref().and_then(|b| std::str::from_utf8(b).ok());
            let after_text = after.as_deref().and_then(|b| std::str::from_utf8(b).ok());
            // 任一侧解不出 UTF-8 才算二进制（删除/新增的单侧 None 是正常形态）。
            let binary =
                before.is_some() && before_text.is_none() || after.is_some() && after_text.is_none();
            if binary {
                continue; // SMJ 只存文本；二进制文件由批级报告的快照路径覆盖。
            }
            let _ = crate::journal::record_change(
                &self.session_id,
                &self.call_id,
                "exec",
                &change.path,
                op_name(change.status),
                before_text,
                after_text,
                "ok",
            );
            match change.status {
                ChangeStatus::Deleted => crate::file_state::record_delete(&abs),
                _ => {
                    if let Some(text) = after_text {
                        crate::file_state::record_write(&abs, text);
                    }
                }
            }
        }
        attributed.into_iter().collect()
    }
}

/// SMJ 的 op 字段：与批级扫描的 `scan_*` 区分，归因口径为 exec。
fn op_name(status: ChangeStatus) -> &'static str {
    match status {
        ChangeStatus::Added => "exec_added",
        ChangeStatus::Modified => "exec_modified",
        ChangeStatus::Deleted => "exec_deleted",
    }
}

/// spy 的 Change.path 是工作区相对路径（'/' 分隔）；账本需要绝对路径。
fn absolutize(root: &Path, rel: &str) -> String {
    let candidate = Path::new(rel);
    if candidate.is_absolute() {
        return rel.to_string();
    }
    root.join(rel).to_string_lossy().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CLEAN-3/T9：exec 期间脚本写入的文件被精确归因——SMJ 记 tool=exec
    /// + call_id，file_state 账本刷新，返回的账本键指向被改文件。
    #[test]
    fn exec_audit_attributes_script_changes_to_call() {
        let tmp = std::env::temp_dir().join(format!("qaqh-exec-audit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let audit = begin(&tmp, "call-exec-1", "sess-exec-1").expect("exec audit begin");
        let target = tmp.join("script-output.txt");
        std::fs::write(&target, b"generated").unwrap();

        let keys = audit.finish();
        assert_eq!(keys.len(), 1, "恰好一条净变更被归因");
        let abs = target.to_string_lossy().to_string();
        assert!(
            crate::file_state::last_hash(&abs).is_some(),
            "file_state 账本应已刷新，消除 stale_file 假阳性"
        );
        let steps = crate::journal::query(Some("sess-exec-1"), Some("script-output.txt"), None);
        assert!(
            steps
                .iter()
                .any(|s| s.tool == "exec" && s.tool_use_id == "call-exec-1"),
            "SMJ 应有 exec 归因记录（批级 finish 据此跳过）: {steps:?}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 幂等收口：exec 无净变更时返回空归因集，不产生 SMJ 记录。
    #[test]
    fn exec_audit_without_changes_attributes_nothing() {
        let tmp = std::env::temp_dir().join(format!("qaqh-exec-audit-clean-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let audit = begin(&tmp, "call-exec-2", "sess-exec-2").expect("exec audit begin");
        let keys = audit.finish();
        assert!(keys.is_empty());
        let steps = crate::journal::query(Some("sess-exec-2"), None, None);
        assert!(steps.is_empty(), "零变更不得记账: {steps:?}");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
