//! Native replaceable dashboard snapshot assembly (PR-1-4 / B4).
//!
//! Pure assembly only: `qaqh_workspace::dashboard` now produces `qaqh-domain`
//! models directly (PR-3-3 单源化，proto 侧投影层已删）。All
//! four engine consumers call [`build_snapshot`] directly.

/// 注册名意义上的 todo 工具（todo/split.rs：todo_write / todo_update / todo_list）。
///
/// legacy 名 `"todo"` 已退役（todo_contract 锁定）。这是 dashboard 即时刷新的
/// 触发判定，流式（engine_tool）与结果回填（tool_runtime）路径共用——
/// tool_runtime 侧曾各自手写 `matches!(name, "todo")` 而漂移成死分支
/// （2026-10-05 注释审计 §3.4），故收敛到本函数。
pub(crate) fn is_todo_tool(tool_name: &str) -> bool {
    matches!(tool_name, "todo_write" | "todo_update" | "todo_list")
}

#[cfg(test)]
mod tests {
    use super::is_todo_tool;

    /// 锁定 dashboard 即时刷新的触发名单：现役三名，legacy 名不得再混入。
    #[test]
    fn is_todo_tool_matches_only_active_trio() {
        for name in ["todo_write", "todo_update", "todo_list"] {
            assert!(is_todo_tool(name), "{name} 应命中");
        }
        for retired in ["todo", "todo_create", "todo_insert", "todo_set"] {
            assert!(!is_todo_tool(retired), "legacy 名 {retired} 不得命中");
        }
        assert!(!is_todo_tool("exec"));
    }
}

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
