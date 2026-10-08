//! dashboard.rs — dashboard 快照装配。
//!
//! Pure assembly only: `qaqh_workspace::dashboard` produces `qaqh-domain`
//! models directly（proto 侧投影层已删）。Engine consumers call
//! [`build_snapshot`] directly. Todo 变更通知不在这里：即时刷新的
//! DashboardUpdated 双发已退役，canonical `WorkspaceResourceChanged` fact
//! 由 `agent/resource_publish.rs` 统一发布。

/// Builds the native replaceable dashboard record without exposing the legacy
/// `Agent2Ui::Dashboard` schema to new consumers.
pub fn build_snapshot(session_id: String) -> qaqh_domain::DashboardSnapshot {
    qaqh_domain::DashboardSnapshot {
        session_id: session_id.clone(),
        documents: qaqh_workspace::dashboard::build_documents(),
        recent_edits: qaqh_workspace::dashboard::build_recent_edits(),
        tasks: qaqh_workspace::dashboard::build_tasks_for(&session_id),
        current_todo_id: qaqh_workspace::dashboard::build_current_todo_id_for(&session_id),
    }
}
