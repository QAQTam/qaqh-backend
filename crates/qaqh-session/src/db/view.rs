//! SessionState 组装:业务表直查 → 契约读模型。
//!
//! owner 未加载时提供 DB-only 快照(bootstrap 兜底);owner 运行时由 owner
//! 在内存视图上追加进度尾窗与实时状态,并填写 `seq`。所有 typed 字段在
//! 写入侧保存 serde wire 形式,此处反序列化;坏数据返回 `Corrupt` 报错,
//! 不静默降级。

use rusqlite::{Connection, OptionalExtension};
use serde::de::DeserializeOwned;

use qaqh_ringing_contract::fact::{
    ActivityState, InteractionDecision, SessionId, ToolCallId, ToolError, ToolMetrics, TurnId,
    TurnMode,
};
use qaqh_ringing_contract::state::{
    InteractionResolution, InteractionView, SessionState, SubagentView, TimelineEntry,
    ToolCallStatus, ToolCallView, TurnOutcome, TurnView,
};

use super::DbError;
use super::store::{self, InteractionRow, ToolCallRow};

fn parse_typed<T: DeserializeOwned>(who: &str, label: &str, text: &str) -> Result<T, DbError> {
    serde_json::from_str(text).map_err(|error| DbError::Corrupt(format!("{who} {label}: {error}")))
}

/// 从业务表组装 SessionState。
pub fn load_session_state(conn: &Connection, session_id: &str) -> Result<SessionState, DbError> {
    if !store::session_exists(conn, session_id)? {
        return Err(DbError::SessionNotFound {
            session_id: session_id.to_string(),
        });
    }

    let turns = load_turns(conn, session_id)?;
    let timeline = load_timeline(conn, session_id)?;
    let tools = store::recent_tool_calls(conn, session_id, 500)?
        .into_iter()
        .map(tool_call_view)
        .collect::<Result<Vec<_>, DbError>>()?;
    let interactions = store::pending_interactions(conn, session_id)?
        .into_iter()
        .map(interaction_view)
        .collect::<Result<Vec<_>, DbError>>()?;

    let pending_tools = store::unfinished_tool_calls(conn, session_id)?;
    let has_active_turn = {
        let mut stmt =
            conn.prepare("SELECT COUNT(*) FROM turns WHERE session_id = ?1 AND status = 'active'")?;
        let count: i64 = stmt.query_row([session_id], |row| row.get(0))?;
        count > 0
    };
    let activity = if has_active_turn || !pending_tools.is_empty() {
        ActivityState::Running
    } else {
        ActivityState::Idle
    };
    let current_turn_id = turns
        .iter()
        .filter(|turn| turn.outcome.is_none())
        .map(|turn| turn.turn_id.clone())
        .next_back();
    // 权威来源:sessions.title(rename 唯一写口是 owner::set_title)。
    let title = session_title(conn, session_id)?;
    let subagents = load_subagents(conn, session_id)?;

    Ok(SessionState {
        session_id: Some(SessionId::new(session_id)),
        seq: 0,
        activity,
        current_turn_id,
        title,
        turns,
        timeline,
        tools,
        interactions,
        subagents,
        driver: None,
    })
}

/// `sessions.title`;空串归一为 None(创建时默认 '')。
fn session_title(conn: &Connection, session_id: &str) -> Result<Option<String>, DbError> {
    let mut stmt = conn.prepare("SELECT title FROM sessions WHERE session_id = ?1")?;
    let title: Option<String> = stmt
        .query_row([session_id], |row| row.get(0))
        .optional()
        .map_err(rusqlite_error)?;
    Ok(title.filter(|title| !title.is_empty()))
}

/// 子代理花名册:spawn 工具调用的 `SubagentSpawned` effect 是血缘权威
/// (parent_call_id / role),子会话业务行推导终态 status。`result` 不在此
/// 填充——子代理正文属于子会话自身 bootstrap,不为其复制第二份状态。
fn load_subagents(conn: &Connection, session_id: &str) -> Result<Vec<SubagentView>, DbError> {
    let mut stmt = conn.prepare(
        "SELECT call_id, effects_json FROM tool_calls
         WHERE session_id = ?1 AND effects_json IS NOT NULL ORDER BY started_at_ms",
    )?;
    let rows = stmt.query_map([session_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
        ))
    })?;
    let mut subagents = Vec::new();
    for row in rows {
        let (call_id, effects_json) = row?;
        let effects = parse_typed::<Vec<qaqh_ringing_contract::state::ToolEffectInfo>>(
            &format!("tool call {call_id}"),
            "effects",
            &effects_json,
        )?;
        for effect in effects {
            let qaqh_ringing_contract::state::ToolEffectInfo::SubagentSpawned {
                child_session_id,
                name,
                ..
            } = effect
            else {
                continue;
            };
            let status = child_terminal_status(conn, &child_session_id)?;
            subagents.push(SubagentView {
                child_session_id: SessionId::new(child_session_id),
                parent_call_id: ToolCallId::new(call_id.clone()),
                role: Some(name),
                status,
                result: None,
            });
        }
    }
    Ok(subagents)
}

/// 子会话终态:最后一回合 finished → Completed;interrupted → Cancelled;
/// 仍在跑 / 从未开回合 → None(运行中,不是终态)。
fn child_terminal_status(
    conn: &Connection,
    child_session_id: &str,
) -> Result<Option<qaqh_ringing_contract::fact::SubagentTerminalStatus>, DbError> {
    let mut stmt = conn.prepare(
        "SELECT status FROM turns WHERE session_id = ?1
         AND status IN ('active', 'finished', 'interrupted')
         ORDER BY seq DESC LIMIT 1",
    )?;
    let status: Option<String> = stmt
        .query_row([child_session_id], |row| row.get(0))
        .optional()
        .map_err(rusqlite_error)?;
    Ok(match status.as_deref() {
        Some("active") | None => None,
        Some("finished") => Some(qaqh_ringing_contract::fact::SubagentTerminalStatus::Completed),
        Some("interrupted") => {
            Some(qaqh_ringing_contract::fact::SubagentTerminalStatus::Cancelled)
        }
        Some(other) => {
            return Err(DbError::Corrupt(format!(
                "subagent {child_session_id}: unknown turn status {other:?}"
            )));
        }
    })
}

fn rusqlite_error(error: rusqlite::Error) -> DbError {
    DbError::Corrupt(format!("query failed: {error}"))
}

fn load_turns(conn: &Connection, session_id: &str) -> Result<Vec<TurnView>, DbError> {
    let mut stmt = conn.prepare(
        "SELECT turn_id, input_id, mode, status, outcome_json FROM turns
         WHERE session_id = ?1 ORDER BY seq",
    )?;
    let rows = stmt.query_map([session_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })?;
    let mut turns = Vec::new();
    for row in rows {
        let (turn_id, input_id, mode, status, outcome_json) = row?;
        let outcome = match status.as_str() {
            "finished" | "interrupted" => {
                let text = outcome_json.ok_or_else(|| {
                    DbError::Corrupt(format!("turn {turn_id} is {status} without outcome_json"))
                })?;
                Some(serde_json::from_str::<TurnOutcome>(&text).map_err(|error| {
                    DbError::Corrupt(format!("turn {turn_id} outcome_json: {error}"))
                })?)
            }
            _ => None,
        };
        let mode = match mode {
            Some(text) => Some(
                serde_json::from_str::<TurnMode>(&text)
                    .map_err(|error| DbError::Corrupt(format!("turn {turn_id} mode: {error}")))?,
            ),
            None => None,
        };
        turns.push(TurnView {
            turn_id: TurnId::new(turn_id),
            input_id: input_id.map(qaqh_ringing_contract::fact::InputId::new),
            mode,
            outcome,
        });
    }
    Ok(turns)
}

fn load_timeline(conn: &Connection, session_id: &str) -> Result<Vec<TimelineEntry>, DbError> {
    let blocks = store::read_transcript_page(conn, session_id, None, 10_000)?;
    blocks
        .into_iter()
        .map(|block| {
            let text = block.payload_json.ok_or_else(|| {
                DbError::Corrupt(format!("block {} has no payload_json", block.block_id))
            })?;
            serde_json::from_str::<TimelineEntry>(&text)
                .map_err(|error| DbError::Corrupt(format!("block {}: {error}", block.block_id)))
        })
        .collect()
}

fn tool_call_view(call: ToolCallRow) -> Result<ToolCallView, DbError> {
    let status = match call.status.as_str() {
        "declared" => ToolCallStatus::Declared,
        "awaiting_permission" => ToolCallStatus::AwaitingPermission,
        "running" | "backgrounded" => ToolCallStatus::Running,
        "finished" => ToolCallStatus::Finished,
        "failed" => ToolCallStatus::Failed,
        "cancelled" => ToolCallStatus::Cancelled,
        other => {
            return Err(DbError::Corrupt(format!(
                "unknown tool call status {other:?}"
            )));
        }
    };
    let who = format!("tool call {}", call.call_id);
    let args = match call.args_json {
        Some(text) => Some(parse_typed::<serde_json::Value>(&who, "args", &text)?),
        None => None,
    };
    let error = match call.error_json {
        Some(text) => Some(parse_typed::<ToolError>(&who, "error", &text)?),
        None => None,
    };
    let metrics = match call.meta_json {
        Some(text) => Some(parse_typed::<ToolMetrics>(&who, "metrics", &text)?),
        None => None,
    };
    let code_delta = match call.code_delta_json {
        Some(text) => Some(parse_typed::<qaqh_ringing_contract::state::CodeDelta>(
            &who,
            "code_delta",
            &text,
        )?),
        None => None,
    };
    let effects = match call.effects_json {
        Some(text) => parse_typed::<Vec<qaqh_ringing_contract::state::ToolEffectInfo>>(
            &who, "effects", &text,
        )?,
        None => Vec::new(),
    };
    Ok(ToolCallView {
        call_id: ToolCallId::new(call.call_id),
        turn_id: call.turn_id.map(TurnId::new),
        tool_name: Some(call.tool_name),
        args,
        status,
        output: call
            .output_text
            .map(|text| qaqh_ringing_contract::fact::ContentValue::Inline { text }),
        output_truncated: call.output_truncated,
        error,
        metrics,
        code_delta,
        effects,
    })
}

fn interaction_view(row: InteractionRow) -> Result<InteractionView, DbError> {
    let who = format!("interaction {}", row.interaction_id);
    // payload_json 保存写侧的完整 InteractionView(request/choices 随行);
    // kind 列是查询冗余,以 payload 为准反序列化。
    let mut view = parse_typed::<InteractionView>(&who, "payload", &row.payload_json)?;
    let resolution =
        match row.decision_json {
            Some(text) => parse_typed::<Option<InteractionDecision>>(&who, "decision", &text)?.map(
                |decision| InteractionResolution {
                    decision,
                    detail: None,
                    resolved_at_ms: row.resolved_at_ms.unwrap_or(row.created_at_ms),
                },
            ),
            None => None,
        };
    view.resolution = resolution;
    Ok(view)
}
