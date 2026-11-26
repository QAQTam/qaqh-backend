//! v4.5.1 任务1:回合收口持久化失败的 Fail-Closed 支撑件
//! (修复单 §二.1,宪法 #8 独立子模块——session_loop.rs 只做薄调用接线)。

use qaqh_session::db::DbError;

/// 持久化失败统一错误码(修复单 §二.1.2 / §二.1.4)。
pub(crate) const RESPONSE_PERSIST_FAILED: &str = "response_persist_failed";

/// 盘满时给出用户可读的明确提示(修复单 §二.1.4);其余持久化错误原样透传。
pub(crate) fn persist_failure_message(error: &DbError) -> String {
    if error.is_disk_full() {
        format!("磁盘空间不足，上下文未保存: {error}")
    } else {
        error.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OS 层盘满(StorageFull)必须翻译成明确的中文提示,供
    /// TurnTerminal::Failed 携带(修复单 §二.1.4)。SQLITE_FULL → DiskFull 的
    /// 翻译由 session_loop_tests 的盘满端到端反证真实覆盖。
    #[test]
    fn disk_full_maps_to_explicit_user_facing_hint() {
        let error = DbError::Io(std::io::Error::from(std::io::ErrorKind::StorageFull));
        assert!(error.is_disk_full());
        let message = persist_failure_message(&error);
        assert!(
            message.contains("磁盘空间不足，上下文未保存"),
            "盘满提示必须显式: {message}"
        );
    }

    /// 非盘满错误原样透传,不冒充磁盘满。
    #[test]
    fn other_errors_pass_through_verbatim() {
        let error = DbError::Corrupt("bad row".to_string());
        assert!(!error.is_disk_full());
        assert_eq!(
            persist_failure_message(&error),
            "corrupt business row: bad row"
        );
    }
}
