//! Native replaceable dashboard snapshot assembly (PR-1-4 / B4).
//!
//! Pure assembly only: `qaqh_workspace::dashboard` now produces `qaqh-domain`
//! models directly (PR-3-3 单源化，proto 侧投影层已删）。All
//! four engine consumers call [`build_snapshot`] directly.

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
