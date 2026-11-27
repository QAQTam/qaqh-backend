use qaqh_ringing_contract::state as old_state;
use qaqh_ringing_runtime::RingingHub;
use qaqh_ringing_v3 as v3;
use qaqh_runtime::QaqhService;

use crate::v3_server::state::ServerState;

use super::super::session;

pub(super) async fn timeline_block(
    service: &QaqhService,
    hub: &RingingHub,
    state: &ServerState,
    session_id: &str,
    entry: old_state::TimelineEntry,
) -> Result<v3::BlockView, String> {
    let expected = match &entry {
        old_state::TimelineEntry::Input { block_id, .. }
        | old_state::TimelineEntry::AssistantBlock { block_id, .. } => {
            block_id.as_str().to_string()
        }
        old_state::TimelineEntry::ToolCall { call_id, .. } => {
            format!("tool_call:{session_id}:{}", call_id.as_str())
        }
        old_state::TimelineEntry::ToolResult { call_id, .. } => {
            format!("tool_result:{session_id}:{}", call_id.as_str())
        }
        old_state::TimelineEntry::Compaction { .. } => String::new(),
    };
    let raw = session_id.to_string();
    let expected_for_query = expected.clone();
    let db = service.owners().db().clone();
    let row = tokio::task::spawn_blocking(move || {
        let map = |row: &rusqlite::Row<'_>| Ok(qaqh_session::db::store::BlockRow {
            block_id: row.get(0)?, turn_id: row.get(1)?, ord: row.get(2)?, kind: row.get(3)?,
            text: row.get(4)?, call_id: row.get(5)?, payload_json: row.get(6)?, created_at_ms: row.get(7)?,
        });
        db.read_blocking(move |conn| {
            let sql = "SELECT block_id, turn_id, ord, kind, text, call_id, payload_json, created_at_ms FROM blocks WHERE session_id = ?1 AND block_id = ?2";
            if !expected_for_query.is_empty() {
                return conn.query_row(sql, [&raw, &expected_for_query], map);
            }
            conn.query_row("SELECT block_id, turn_id, ord, kind, text, call_id, payload_json, created_at_ms FROM blocks WHERE session_id = ?1 AND kind = 'compaction' ORDER BY ord DESC LIMIT 1", [&raw], map)
        })
    }).await.map_err(|error| error.to_string())?.map_err(|error| format!("read appended block {expected}: {error}"))?;
    session::block_view(hub, state, session_id, row)
        .await
        .map_err(|error| error.message)
}
