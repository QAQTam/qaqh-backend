//! resource_publish — canonical `WorkspaceResourceChanged` fact publication.
//!
//! Todo mutation entry points (typed tools, UI-direct tool runs, service
//! writes, goal-mode transitions) all land here after their file write
//! succeeded: the actor thread re-reads the resource, persists a summary blob,
//! and only then appends the fact (I2/I5). There is deliberately no live-frame
//! or dashboard-broadcast shortcut on these paths.

use qaqh_session::{
    canonical::{ToolLedger, generate_ulid, stable_workspace_resource_id},
    session_fact_v2::{EventId, ResourceKind, ToolCallId, TurnId},
};

use crate::agent::{state::agent::unix_ms, types::RingContext};

/// Todo resources live once per session; their ids are session-stable.
pub(crate) fn todo_resource_id(session_id: &str) -> qaqh_session::session_fact_v2::ResourceId {
    stable_workspace_resource_id(session_id, ResourceKind::Todo)
}

/// Append a Todo resource fact from the current on-disk state.
///
/// Callers must have persisted the todo file first. Failures are returned, not
/// swallowed: a lost fact would leave other clients stale until the next
/// tool/turn terminal.
pub(crate) fn append_todo_resource_fact(
    ledger: &mut ToolLedger,
    session_id: &str,
    turn_id: Option<TurnId>,
    call_id: Option<ToolCallId>,
) -> Result<(), String> {
    let store = qaqh_workspace::todo::load_todo_for(session_id)?;
    let summary = serde_json::to_vec_pretty(&store)
        .map_err(|error| format!("serialize todo summary: {error}"))?;
    ledger
        .append_workspace_resource_changed(
            EventId::new(generate_ulid()),
            turn_id,
            call_id,
            ResourceKind::Todo,
            todo_resource_id(session_id),
            &summary,
            None,
            false,
            unix_ms(),
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Todo mutation entry point used by [`ToolRuntime::collect`]: only tools that
/// persist todo state publish a resource fact; reads do not.
pub(crate) fn is_todo_mutation_tool(tool_name: &str) -> bool {
    matches!(tool_name, "todo_write" | "todo_update")
}

/// Best-effort publication on the actor thread. Errors are logged: the tool
/// already persisted its file, and the tool result must not be rewritten into
/// a failure because the fact append failed.
pub(crate) fn publish_todo_resource_fact_for_ctx(
    ctx: &mut RingContext,
    turn_id: Option<TurnId>,
    call_id: Option<ToolCallId>,
) {
    let session_id = ctx.agent.session.session_id.clone();
    let ledger = match ctx.agent.tool_ledger_mut() {
        Ok(Some(ledger)) => ledger,
        Ok(None) => return,
        Err(error) => {
            log::warn!("[todo] resource fact skipped (ledger unavailable): {error}");
            return;
        }
    };
    if let Err(error) = append_todo_resource_fact(ledger, &session_id, turn_id, call_id) {
        log::warn!("[todo] resource fact append failed: {error}");
    }
}
