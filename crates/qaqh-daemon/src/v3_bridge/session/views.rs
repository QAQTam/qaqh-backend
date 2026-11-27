use std::collections::BTreeMap;
use std::sync::Arc;

use qaqh_core_vocab::{BlockId, ContentRef, InputId, SessionId, TurnId, TurnPhase};
use qaqh_ringing_contract::commands::SubagentTerminalKind as OldSubagentTerminalKind;
use qaqh_ringing_contract::fact::{ActorKind, ContentValue, InputPurpose as OldInputPurpose};
use qaqh_ringing_runtime::RingingHub;
use qaqh_ringing_v3::{
    self as v3, BlockActor, BlockKind, BlockView, CompactionSummary, ErrorCode, InputPurpose,
    InterAgentView, RpcError, SessionSnapshot, SubagentTerminalKind, SubagentTerminalView,
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
            tool_calls.push(
                projection::tool_call_view(&backend.hub, &backend.state, &raw, tool)
                    .await
                    .map_err(internal)?,
            );
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
    let mut turns = Vec::new();
    for (index, turn) in state
        .turns
        .iter()
        .enumerate()
        .rev()
        .take(tail_turns.clamp(1, 256) as usize)
        .rev()
    {
        let turn_id = TurnId::new(turn.turn_id.as_str()).map_err(|error| internal(error.to_string()))?;
        let input_id = turn
            .input_id
            .as_ref()
            .map(|id| InputId::new(id.as_str()).map_err(|error| internal(error.to_string())))
            .transpose()?;
        let phase = turn.phase.map(|phase| match phase {
            qaqh_ringing_contract::TurnPhase::Running => TurnPhase::Running,
            qaqh_ringing_contract::TurnPhase::WaitingUser => TurnPhase::WaitingUser,
            qaqh_ringing_contract::TurnPhase::Cancelling => TurnPhase::Cancelling,
        });
        let usage = match &turn.outcome {
            Some(qaqh_ringing_contract::state::TurnOutcome::Finished {
                usage: Some(usage),
                ..
            }) => Some(projection::map_usage(usage.clone())),
            _ => None,
        };
        turns.push(v3::TurnSummary {
            turn_id,
            input_id,
            ordinal: index as u64 + 1,
            phase,
            terminal: turn.outcome.as_ref().map(turn_terminal),
            usage,
            started_at_ms: started
                .get(turn.turn_id.as_str())
                .copied()
                .unwrap_or_default()
                .max(0) as u64,
        });
    }
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
        custom_tools: meta.custom_tools,
        handoff_from: meta.handoff_from,
        created_at_ms: meta.created_at_ms,
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
                tool_calls.push(
                    projection::tool_call_view(&backend.hub, &backend.state, &raw, tool)
                        .await
                        .map_err(internal)?,
                );
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
    let row_block_id = BlockId::new(row.block_id.clone())
        .map_err(|error| internal(format!("block_id `{}`: {error}", row.block_id)))?;
    let row_turn_id = row
        .turn_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(|id| TurnId::new(id).map_err(|error| internal(error.to_string())))
        .transpose()?;
    match entry {
        qaqh_ringing_contract::state::TimelineEntry::Input {
            block_id,
            turn_id,
            purpose,
            content,
            attachments: raw_attachments,
            images: raw_images,
            inter_agent,
            subagent_terminal,
            actor,
        } => {
            let mut images = Vec::with_capacity(raw_images.len());
            for image in raw_images {
                images.push(
                    projection::image_from_b64(hub, session_id, &image.data, image.mime_type)
                        .await
                        .map_err(internal)?,
                );
            }
            let mut attachments = Vec::with_capacity(raw_attachments.len());
            for content_ref in raw_attachments {
                attachments.push(
                    projection::content_ref(
                        hub,
                        state,
                        session_id,
                        ContentValue::Ref { content_ref },
                    )
                    .await
                    .map_err(internal)?,
                );
            }
            let inter_agent = inter_agent
                .map(|envelope| -> Result<InterAgentView, RpcError> {
                    Ok(InterAgentView {
                        message_id: envelope.message_id,
                        root_session_id: SessionId::new(envelope.root_session_id)
                            .map_err(|error| internal(error.to_string()))?,
                        author: envelope.author,
                        recipient: envelope.recipient,
                        other_recipients: envelope.other_recipients,
                        task_id: envelope.task_id,
                        reply_to: envelope.reply_to,
                        created_at_ms: envelope.created_at_ms.max(0) as u64,
                    })
                })
                .transpose()?;
            let subagent_terminal = subagent_terminal
                .map(|notice| -> Result<SubagentTerminalView, RpcError> {
                    Ok(SubagentTerminalView {
                        child_session_id: SessionId::new(notice.child_session_id)
                            .map_err(|error| internal(error.to_string()))?,
                        parent_call_id: tool_call_id(&notice.parent_call_id)?,
                        terminal: match notice.terminal {
                            OldSubagentTerminalKind::Completed => SubagentTerminalKind::Completed,
                            OldSubagentTerminalKind::Failed => SubagentTerminalKind::Failed,
                            OldSubagentTerminalKind::Cancelled => SubagentTerminalKind::Cancelled,
                            OldSubagentTerminalKind::TimedOut => SubagentTerminalKind::TimedOut,
                        },
                    })
                })
                .transpose()?;
            let turn_id = turn_id
                .map(|id| TurnId::new(id.as_str()).map_err(|error| internal(error.to_string())))
                .transpose()?
                .or(row_turn_id);
            Ok(BlockView {
                block_id: BlockId::new(block_id.as_str())
                    .map_err(|error| internal(error.to_string()))?,
                turn_id,
                ord: u64::try_from(row.ord).map_err(|_| internal("negative block ord"))?,
                kind: BlockKind::User,
                content: projection::content_ref(hub, state, session_id, content)
                    .await
                    .map_err(internal)?,
                tool_call_id: None,
                created_at_ms: row.created_at_ms.max(0) as u64,
                actor: Some(match actor.kind {
                    ActorKind::User => BlockActor::User,
                    ActorKind::Api => BlockActor::Api,
                    ActorKind::System => BlockActor::System,
                    ActorKind::Agent => BlockActor::Agent,
                    ActorKind::Subagent => BlockActor::Subagent,
                }),
                purpose: Some(match purpose {
                    OldInputPurpose::TriggerTurn => InputPurpose::TriggerTurn,
                    OldInputPurpose::QueueOnly => InputPurpose::QueueOnly,
                    OldInputPurpose::Steer => InputPurpose::Steer,
                    OldInputPurpose::Interject => InputPurpose::Interject,
                }),
                model: None,
                usage: None,
                generated_ms: None,
                images,
                attachments,
                inter_agent,
                subagent_terminal,
            })
        }
        qaqh_ringing_contract::state::TimelineEntry::AssistantBlock {
            block_id,
            turn_id,
            block_kind,
            content,
            model,
            usage,
            generated_ms,
            ..
        } => Ok(BlockView {
            block_id: BlockId::new(block_id.as_str())
                .map_err(|error| internal(error.to_string()))?,
            turn_id: Some(
                TurnId::new(turn_id.as_str()).map_err(|error| internal(error.to_string()))?,
            ),
            ord: u64::try_from(row.ord).map_err(|_| internal("negative block ord"))?,
            kind: match block_kind {
                qaqh_ringing_contract::fact::AssistantBlockKind::Answer => BlockKind::Answer,
                qaqh_ringing_contract::fact::AssistantBlockKind::Reasoning => BlockKind::Reasoning,
                qaqh_ringing_contract::fact::AssistantBlockKind::ToolCall => BlockKind::ToolCall,
            },
            content: projection::content_ref(hub, state, session_id, content)
                .await
                .map_err(internal)?,
            tool_call_id: None,
            created_at_ms: row.created_at_ms.max(0) as u64,
            actor: None,
            purpose: None,
            model: (!model.is_empty()).then_some(model),
            usage: usage.map(projection::map_usage),
            generated_ms,
            images: Vec::new(),
            attachments: Vec::new(),
            inter_agent: None,
            subagent_terminal: None,
        }),
        qaqh_ringing_contract::state::TimelineEntry::ToolCall { call_id, turn_id } => {
            let turn_id = turn_id
                .map(|id| TurnId::new(id.as_str()).map_err(|error| internal(error.to_string())))
                .transpose()?
                .or(row_turn_id);
            Ok(BlockView {
                block_id: row_block_id,
                turn_id,
                ord: u64::try_from(row.ord).map_err(|_| internal("negative block ord"))?,
                kind: BlockKind::ToolCall,
                content: ContentRef::Inline {
                    text: String::new(),
                },
                tool_call_id: Some(tool_call_id(call_id.as_str())?),
                created_at_ms: row.created_at_ms.max(0) as u64,
                actor: None,
                purpose: None,
                model: None,
                usage: None,
                generated_ms: None,
                images: Vec::new(),
                attachments: Vec::new(),
                inter_agent: None,
                subagent_terminal: None,
            })
        }
        qaqh_ringing_contract::state::TimelineEntry::ToolResult {
            call_id,
            turn_id,
            content,
            images: raw_images,
        } => {
            let mut images = Vec::with_capacity(raw_images.len());
            for image in raw_images {
                let b64 = qaqh_types::image_store::load_image_b64(&image.sha256, &image.mime_type)
                    .map_err(internal)?;
                images.push(
                    projection::image_from_b64(hub, session_id, &b64, image.mime_type)
                        .await
                        .map_err(internal)?,
                );
            }
            let turn_id = turn_id
                .map(|id| TurnId::new(id.as_str()).map_err(|error| internal(error.to_string())))
                .transpose()?
                .or(row_turn_id);
            Ok(BlockView {
                block_id: row_block_id,
                turn_id,
                ord: u64::try_from(row.ord).map_err(|_| internal("negative block ord"))?,
                kind: BlockKind::ToolResult,
                content: projection::content_ref(hub, state, session_id, content)
                    .await
                    .map_err(internal)?,
                tool_call_id: Some(tool_call_id(call_id.as_str())?),
                created_at_ms: row.created_at_ms.max(0) as u64,
                actor: None,
                purpose: None,
                model: None,
                usage: None,
                generated_ms: None,
                images,
                attachments: Vec::new(),
                inter_agent: None,
                subagent_terminal: None,
            })
        }
        qaqh_ringing_contract::state::TimelineEntry::Compaction { summary } => Ok(BlockView {
            block_id: row_block_id,
            turn_id: row_turn_id,
            ord: u64::try_from(row.ord).map_err(|_| internal("negative block ord"))?,
            kind: BlockKind::System,
            content: projection::content_ref(hub, state, session_id, summary)
                .await
                .map_err(internal)?,
            tool_call_id: None,
            created_at_ms: row.created_at_ms.max(0) as u64,
            actor: None,
            purpose: None,
            model: None,
            usage: None,
            generated_ms: None,
            images: Vec::new(),
            attachments: Vec::new(),
            inter_agent: None,
            subagent_terminal: None,
        }),
    }
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
