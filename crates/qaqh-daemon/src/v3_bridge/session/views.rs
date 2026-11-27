use std::collections::BTreeMap;
use std::sync::Arc;

use qaqh_ringing_runtime::RingingHub;
use qaqh_ringing_v3::{
    self as v3, BlockKind, BlockView, CompactionSummary, ErrorCode, RpcError, SessionSnapshot,
    TranscriptPage,
};
use rusqlite::OptionalExtension;

use super::super::{RealBackend, internal, projection};
use super::commands::summary;
use super::mappings::{activity, goal_view, turn_terminal};
use super::rpc;
use crate::v3_server::session_runtime::SessionRuntime;
use crate::v3_server::state::ServerState;

pub(super) async fn snapshot(
    backend: &RealBackend,
    runtime: &Arc<SessionRuntime>,
    tail_blocks: u32,
    tail_turns: u32,
) -> Result<SessionSnapshot, RpcError> {
    let id = runtime.id.clone();
    let service = backend.service.clone();
    let raw = id.as_str().to_string();
    let tail = tail_blocks.clamp(1, 500);
    // Capture the replay watermark before reads so no published event can be absent from the snapshot.
    let seq = runtime.bus.last_seq();
    let owner = service.owners().get_or_create(&raw);
    let state = owner
        .snapshot_tailed(tail)
        .await
        .map_err(|error| internal(error.to_string()))?;
    let (rows, started, compaction) = service
        .owners()
        .db()
        .read({
            let raw = raw.clone();
            move |conn| {
                let rows = read_v3_timeline_page(conn, &raw, None, tail)?;
                let started = {
                    let mut statement = conn
                        .prepare("SELECT turn_id, started_at_ms FROM turns WHERE session_id = ?1")?;
                    statement
                        .query_map([&raw], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                        })?
                        .collect::<Result<Vec<_>, _>>()?
                        .into_iter()
                        .collect::<BTreeMap<_, _>>()
                };
                let compaction = conn
                    .query_row(
                        "SELECT created_at_ms, ord, (SELECT COUNT(*) FROM blocks b2 WHERE b2.session_id = blocks.session_id AND b2.ord < blocks.ord) FROM blocks WHERE session_id = ?1 AND kind = 'compaction' ORDER BY ord DESC LIMIT 1",
                        [&raw],
                        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)),
                    )
                    .optional()?;
                Ok::<_, qaqh_session::db::DbError>((rows, started, compaction))
            }
        })
        .await
        .map_err(|error| internal(error.to_string()))?;
    let mut blocks = Vec::with_capacity(rows.len());
    for row in rows {
        blocks.push(block_view(&backend.hub, &backend.state, &raw, row).await?);
    }
    let call_ids = blocks
        .iter()
        .filter_map(|block| block.tool_call_id.as_ref())
        .map(|id| id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let mut tool_calls = Vec::new();
    for tool in &state.tools {
        if call_ids.contains(tool.call_id.as_str()) {
            tool_calls.push(tool_view(backend, &raw, tool).await?);
        }
    }
    let mut pending_interactions = Vec::with_capacity(state.interactions.len());
    for interaction in &state.interactions {
        pending_interactions.push(
            projection::interaction_view(&backend.hub, &backend.state, &raw, interaction)
                .await
                .map_err(internal)?,
        );
    }
    let mut exec_sessions = Vec::with_capacity(state.exec_sessions.len());
    for exec in &state.exec_sessions {
        exec_sessions.push(
            projection::exec_view(&backend.hub, &backend.state, exec)
                .await
                .map_err(internal)?,
        );
    }
    let turns = state
        .turns
        .iter()
        .enumerate()
        .rev()
        .take(tail_turns.clamp(1, 256) as usize)
        .rev()
        .map(|(index, turn)| v3::TurnSummary {
            ordinal: index as u64 + 1,
            terminal: turn.outcome.as_ref().map(turn_terminal),
            started_at_ms: started
                .get(turn.turn_id.as_str())
                .copied()
                .unwrap_or_default()
                .max(0) as u64,
        })
        .collect();
    let pinned_compaction = match state.timeline.iter().find_map(|entry| match entry {
        qaqh_ringing_contract::state::TimelineEntry::Compaction { summary } => Some(summary),
        _ => None,
    }) {
        Some(qaqh_ringing_contract::fact::ContentValue::Inline { text }) => {
            let (pinned_at_ms, _, blocks_compacted) =
                compaction.ok_or_else(|| internal("compaction block metadata missing"))?;
            Some(CompactionSummary {
                pinned_at_ms: pinned_at_ms.max(0) as u64,
                blocks_compacted: blocks_compacted.max(0) as u64,
                summary: text.clone(),
            })
        }
        Some(_) => return Err(internal("compaction summary must be inline")),
        None => None,
    };
    let meta = summary(backend, &id).await?;
    let mut subagents = Vec::with_capacity(state.subagents.len());
    for subagent in &state.subagents {
        let service = backend.service.clone();
        let child = subagent.child_session_id.as_str().to_string();
        let loaded = RealBackend::blocking(move || {
            service
                .agent_residency_loaded(&child)
                .map_err(|error| error.to_string())
        })
        .await?;
        subagents.push(v3::SubagentSummary {
            session_id: qaqh_core_vocab::SessionId::new(subagent.child_session_id.as_str())
                .map_err(|error| internal(error.to_string()))?,
            nickname: subagent.role.clone(),
            status: match subagent.status {
                Some(qaqh_ringing_contract::fact::SubagentTerminalStatus::Completed) => {
                    v3::TeamAgentStatus::Completed
                }
                Some(qaqh_ringing_contract::fact::SubagentTerminalStatus::Failed) => {
                    v3::TeamAgentStatus::Errored
                }
                Some(_) => v3::TeamAgentStatus::Interrupted,
                None => v3::TeamAgentStatus::Running,
            },
            residency: if loaded {
                v3::TeamAgentResidency::Loaded
            } else {
                v3::TeamAgentResidency::Unloaded
            },
        });
    }
    runtime.set_activity(activity(state.activity));
    runtime.set_tool_mode(meta.tool_mode);
    let timeline_epoch = projection::epoch(&id, runtime.epoch());
    let (driver, driver_ordinal) = runtime.driver_with_ordinal();
    Ok(SessionSnapshot {
        session_id: id,
        title: meta.title,
        workspace_id: meta.workspace_id,
        cwd: meta.cwd,
        archived: meta.archived,
        settings_revision: meta.settings_revision,
        model_selection: meta.model_selection,
        activity: activity(state.activity),
        timeline_epoch,
        seq,
        turns,
        tail_blocks: blocks,
        tool_calls,
        pending_interactions,
        exec_sessions,
        subagents,
        goal: state.goal.as_ref().map(goal_view),
        driver_client_instance_id: driver.map(|driver| driver.instance),
        driver_ordinal,
        pinned_compaction,
        tool_mode: meta.tool_mode,
    })
}

pub(super) async fn page(
    backend: &RealBackend,
    runtime: &Arc<SessionRuntime>,
    before_ord: u64,
    limit: u32,
    timeline_epoch: u64,
) -> Result<TranscriptPage, RpcError> {
    let current_epoch = projection::epoch(&runtime.id, runtime.epoch());
    if timeline_epoch != current_epoch {
        return Err(rpc(
            ErrorCode::StaleCursor,
            "时间线代际已变化，必须重取快照",
        ));
    }
    let service = backend.service.clone();
    let raw = runtime.id.as_str().to_string();
    let before = if before_ord == 0 {
        None
    } else {
        Some(i64::try_from(before_ord).map_err(|_| super::super::bad_request("before_ord 溢出"))?)
    };
    let limit = limit.clamp(1, 500);
    let (rows, older) = service
        .owners()
        .db()
        .read({
            let raw = raw.clone();
            move |conn| {
                let rows = read_v3_timeline_page(conn, &raw, before, limit)?;
                let older = match rows.first() {
                    Some(row) if row.ord > 1 => {
                        !read_v3_timeline_page(conn, &raw, Some(row.ord - 1), 1)?.is_empty()
                    }
                    _ => false,
                };
                Ok::<_, qaqh_session::db::DbError>((rows, older))
            }
        })
        .await
        .map_err(|error| internal(error.to_string()))?;
    let mut blocks = Vec::with_capacity(rows.len());
    for row in rows {
        blocks.push(block_view(&backend.hub, &backend.state, &raw, row).await?);
    }
    let next_before_ord = if older {
        blocks.first().map(|block| block.ord)
    } else {
        None
    };
    let call_ids = blocks
        .iter()
        .filter_map(|block| block.tool_call_id.as_ref())
        .map(|id| id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let mut tool_calls = Vec::new();
    if !call_ids.is_empty() {
        let owner = backend.service.owners().get_or_create(&raw);
        let state = owner
            .snapshot_tailed(500)
            .await
            .map_err(|error| internal(error.to_string()))?;
        for tool in &state.tools {
            if call_ids.contains(tool.call_id.as_str()) {
                tool_calls.push(tool_view(backend, &raw, tool).await?);
            }
        }
    }
    Ok(TranscriptPage {
        blocks,
        tool_calls,
        next_before_ord,
        timeline_epoch: current_epoch,
    })
}

pub(crate) async fn block_view(
    hub: &RingingHub,
    state: &ServerState,
    session_id: &str,
    row: qaqh_session::db::store::BlockRow,
) -> Result<BlockView, RpcError> {
    let payload = row
        .payload_json
        .ok_or_else(|| internal(format!("block {} 缺 payload_json", row.block_id)))?;
    let entry = serde_json::from_str::<qaqh_ringing_contract::state::TimelineEntry>(&payload)
        .map_err(|error| internal(format!("解析 block {}: {error}", row.block_id)))?;
    let (kind, content, tool_call_id) = match entry {
        qaqh_ringing_contract::state::TimelineEntry::Input { content, .. } => {
            (BlockKind::User, content, None)
        }
        qaqh_ringing_contract::state::TimelineEntry::AssistantBlock {
            block_kind,
            content,
            ..
        } => (
            match block_kind {
                qaqh_ringing_contract::fact::AssistantBlockKind::Answer => BlockKind::Answer,
                qaqh_ringing_contract::fact::AssistantBlockKind::Reasoning => BlockKind::Reasoning,
                qaqh_ringing_contract::fact::AssistantBlockKind::ToolCall => BlockKind::ToolCall,
            },
            content,
            None,
        ),
        qaqh_ringing_contract::state::TimelineEntry::ToolCall { call_id, .. } => (
            BlockKind::ToolCall,
            qaqh_ringing_contract::fact::ContentValue::Inline {
                text: String::new(),
            },
            Some(tool_call_id(call_id.as_str())?),
        ),
        qaqh_ringing_contract::state::TimelineEntry::ToolResult {
            call_id, content, ..
        } => (
            BlockKind::ToolResult,
            content,
            Some(tool_call_id(call_id.as_str())?),
        ),
        qaqh_ringing_contract::state::TimelineEntry::Compaction { summary } => {
            (BlockKind::System, summary, None)
        }
    };
    Ok(BlockView {
        ord: u64::try_from(row.ord).map_err(|_| internal("negative block ord"))?,
        kind,
        content: projection::content_ref(hub, state, session_id, content)
            .await
            .map_err(internal)?,
        tool_call_id,
        created_at_ms: row.created_at_ms.max(0) as u64,
    })
}

async fn tool_view(
    backend: &RealBackend,
    session_id: &str,
    tool: &qaqh_ringing_contract::state::ToolCallView,
) -> Result<v3::ToolCallView, RpcError> {
    use v3::ToolCallStatus;
    let (status, finished) = match &tool.state {
        qaqh_ringing_contract::state::ToolCallState::Declared
        | qaqh_ringing_contract::state::ToolCallState::AwaitingInteraction { .. } => {
            (ToolCallStatus::Pending, None)
        }
        qaqh_ringing_contract::state::ToolCallState::Running => (ToolCallStatus::Running, None),
        qaqh_ringing_contract::state::ToolCallState::Cancelling => {
            (ToolCallStatus::Cancelled, None)
        }
        qaqh_ringing_contract::state::ToolCallState::Finished { outcome } => (
            match outcome {
                qaqh_ringing_contract::fact::ToolCallOutcome::Completed
                | qaqh_ringing_contract::fact::ToolCallOutcome::Yielded { .. } => {
                    ToolCallStatus::Succeeded
                }
                qaqh_ringing_contract::fact::ToolCallOutcome::Failed { .. } => {
                    ToolCallStatus::Failed
                }
                qaqh_ringing_contract::fact::ToolCallOutcome::Cancelled { .. } => {
                    ToolCallStatus::Cancelled
                }
                qaqh_ringing_contract::fact::ToolCallOutcome::Interrupted { .. } => {
                    ToolCallStatus::Indeterminate
                }
            },
            tool.metrics
                .as_ref()
                .map(|metrics| metrics.finished_at_ms.max(0) as u64),
        ),
    };
    Ok(v3::ToolCallView {
        tool_call_id: tool_call_id(tool.call_id.as_str())?,
        tool_name: tool.tool_name.clone().unwrap_or_default(),
        status,
        started_at_ms: tool
            .metrics
            .as_ref()
            .map(|metrics| metrics.started_at_ms.max(0) as u64)
            .unwrap_or_default(),
        finished_at_ms: finished,
        summary: match &tool.output {
            Some(output) => Some(
                projection::content_ref(&backend.hub, &backend.state, session_id, output.clone())
                    .await
                    .map_err(internal)?,
            ),
            None => None,
        },
    })
}

fn tool_call_id(raw: &str) -> Result<qaqh_core_vocab::ToolCallId, RpcError> {
    qaqh_core_vocab::ToolCallId::new(raw).map_err(|error| internal(error.to_string()))
}

fn read_v3_timeline_page(
    conn: &rusqlite::Connection,
    session_id: &str,
    before_ord: Option<i64>,
    limit: u32,
) -> rusqlite::Result<Vec<qaqh_session::db::store::BlockRow>> {
    let mut stmt = conn.prepare(
        "SELECT block_id, turn_id, ord, kind, text, call_id, payload_json, created_at_ms
         FROM blocks
         WHERE session_id = ?1 AND kind != 'compaction' AND (?2 IS NULL OR ord < ?2)
         ORDER BY ord DESC
         LIMIT ?3",
    )?;
    let mut rows: Vec<qaqh_session::db::store::BlockRow> = stmt
        .query_map(rusqlite::params![session_id, before_ord, limit], |row| {
            Ok(qaqh_session::db::store::BlockRow {
                block_id: row.get(0)?,
                turn_id: row.get(1)?,
                ord: row.get(2)?,
                kind: row.get(3)?,
                text: row.get(4)?,
                call_id: row.get(5)?,
                payload_json: row.get(6)?,
                created_at_ms: row.get(7)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    rows.reverse();
    Ok(rows)
}
