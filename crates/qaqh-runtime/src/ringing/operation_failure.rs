//! Worker `ControlEvent::OperationFailed` → v2 control 频道（ephemeral）。
//!
//! 背景：Ringing ack 只代表命令进了 worker 队列，业务失败原本由 v1 领域事件
//! `ControlEvent::OperationFailed` 回传。hub-fact-bus 阶段 3d 删掉 v1 广播面之后
//! 这条通道断了：`cancel_target_mismatch`、`turn_busy` 之类的拒绝对客户端完全
//! 不可见，命令收据只能靠 TTL 过期（`pending_store.rs` 的降级说明）。
//!
//! 镜像放在 worker 事件桥而不是产生侧：引擎只有 `RingContext`（拿不到 v2 hub），
//! 而失败有十余个产生点（dispatch / outcome / input / injection / compact …）。
//! 在这一处折叠，所有产生点自动获得同一个 wire 形状。

use qaqh_domain::{ControlEvent, DomainEvent, ErrorScope};
use qaqh_session::SessionManager;
use qaqh_session::session_fact_v2::{OperationError, OperationFailure, OperationScope};

use super::v2::V2ProjectionHub;

/// 领域事件 → wire 通知。非 `OperationFailed` 一律 `None`（纯函数，便于测试）。
pub(crate) fn to_wire(event: &DomainEvent, failed_at_ms: i64) -> Option<OperationFailure> {
    let DomainEvent::Control(ControlEvent::OperationFailed {
        occurrence_id,
        scope,
        error,
        operation_id,
    }) = event
    else {
        return None;
    };
    Some(OperationFailure {
        occurrence_id: occurrence_id.clone(),
        scope: scope_of(*scope),
        error: OperationError {
            error_id: error.error_id.clone(),
            code: error.code.clone(),
            message: error.message.clone(),
            retryable: error.retryable,
            dedupe_key: error.dedupe_key.clone(),
        },
        operation_id: operation_id.clone(),
        failed_at_ms,
    })
}

fn scope_of(scope: ErrorScope) -> OperationScope {
    match scope {
        ErrorScope::Control => OperationScope::Control,
        ErrorScope::Conversation => OperationScope::Conversation,
        ErrorScope::Tool => OperationScope::Tool,
        ErrorScope::System => OperationScope::System,
    }
}

/// 把一条 worker 领域事件发布到 v2（非失败事件静默跳过）。
///
/// 没有 hub（未启用 Ringing 的宿主、单测）时静默返回——与压缩镜像同语义：
/// 通知面的缺失不值得让 worker 事件桥报错。
pub(crate) fn publish_operation_failure(
    hub: Option<&V2ProjectionHub>,
    sessions: &SessionManager,
    session_id: &str,
    event: &DomainEvent,
) {
    let Some(hub) = hub else { return };
    let Some(failure) = to_wire(event, unix_ms()) else {
        return;
    };
    let session_dir = sessions.session_path_dir(session_id);
    if let Err(error) = hub.publish_operation_failed(&session_dir, session_id, failure) {
        log::warn!("[operation-failure] publish failed for {session_id}: {error}");
    }
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::{ControlEvent, DomainError, DomainEvent};

    fn failure_event(scope: ErrorScope) -> DomainEvent {
        DomainEvent::Control(ControlEvent::OperationFailed {
            occurrence_id: "cmd-1".into(),
            scope,
            error: DomainError {
                error_id: "cmd-1".into(),
                code: "cancel_target_mismatch".into(),
                message: "turn t9 is not the active turn (current: t1)".into(),
                retryable: false,
                dedupe_key: Some("cmd-1".into()),
            },
            operation_id: Some("cmd-1".into()),
        })
    }

    #[test]
    fn operation_failed_maps_every_field() {
        let wire = to_wire(&failure_event(ErrorScope::Conversation), 1_700).expect("maps");
        assert_eq!(wire.occurrence_id, "cmd-1");
        assert_eq!(wire.scope, OperationScope::Conversation);
        assert_eq!(wire.error.code, "cancel_target_mismatch");
        assert_eq!(wire.error.dedupe_key.as_deref(), Some("cmd-1"));
        assert_eq!(wire.operation_id.as_deref(), Some("cmd-1"));
        assert_eq!(wire.failed_at_ms, 1_700);
    }

    #[test]
    fn scope_vocabulary_is_exhaustive() {
        assert_eq!(scope_of(ErrorScope::Control), OperationScope::Control);
        assert_eq!(scope_of(ErrorScope::Tool), OperationScope::Tool);
        assert_eq!(scope_of(ErrorScope::System), OperationScope::System);
    }

    #[test]
    fn non_failure_events_do_not_map() {
        let other = DomainEvent::Control(ControlEvent::OperationCompleted {
            occurrence_id: "cmd-1".into(),
            scope: ErrorScope::Conversation,
            operation_id: Some("cmd-1".into()),
        });
        assert!(to_wire(&other, 1).is_none());
    }
}
