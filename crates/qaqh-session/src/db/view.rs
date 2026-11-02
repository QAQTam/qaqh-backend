//! SessionState 组装:业务表直查 → 契约读模型。
//!
//! owner 未加载时提供 DB-only 快照(bootstrap 兜底);owner 运行时由 owner
//! 在内存视图上追加进度尾窗与实时状态,并填写 `seq`。所有 typed 字段在
//! 写入侧保存 serde wire 形式,此处反序列化;坏数据返回 `Corrupt` 报错,
//! 不静默降级。

use rusqlite::Connection;
use serde::de::DeserializeOwned;

use qaqh_ringing_contract::fact::{
    ActivityState, ContentValue, InteractionDecision, InteractionId, InteractionKind, SessionId,
    ToolCallId, ToolError, ToolMetrics, TurnId, TurnMode,
};
use qaqh_ringing_contract::state::{
    InteractionResolution, InteractionView, SessionState, TimelineEntry, ToolCallStatus,
    ToolCallView, TurnOutcome, TurnView,
};

use super::store::{self, InteractionRow, ToolCallRow};
use super::DbError;


fn parse_typed<T: DeserializeOwned>(who: &str, label: &str, text: &str) -> Result<T, DbError> {
    serde_json::from_str(text)
        .map_err(|error| DbError::Corrupt(format!("{who} {label}: {error}")))
}

/// 从业务表组装 SessionState。
pub fn load_session_state(
    conn: &Connection,
    session_id: &str,
) -> Result<SessionState, DbError> {
    if !store::session_exists(conn, session_id)? {
        return Err(DbError::SessionNotFound { session_id: session_id.to_string() });
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
        let mut stmt = conn.prepare(
            "SELECT COUNT(*) FROM turns WHERE session_id = ?1 AND status = 'active'",
        )?;
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

    Ok(SessionState {
        session_id: Some(SessionId::new(session_id)),
        seq: 0,
        activity,
        current_turn_id,
        title: None,
        turns,
        timeline,
        tools,
        interactions,
        subagents: Vec::new(),
        driver: None,
    })
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
            Some(text) => Some(serde_json::from_str::<TurnMode>(&text).map_err(|error| {
                DbError::Corrupt(format!("turn {turn_id} mode: {error}"))
            })?),
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
        other => return Err(DbError::Corrupt(format!("unknown tool call status {other:?}"))),
    };
    let who = format!("tool call {}", call.call_id);
    let args = match call.args_json {
        Some(text) => Some(parse_typed::<ContentValue>(&who, "args", &text)?),
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
    Ok(ToolCallView {
        call_id: ToolCallId::new(call.call_id),
        turn_id: Some(TurnId::new(call.turn_id)),
        tool_name: Some(call.tool_name),
        args,
        status,
        output: call
            .output_text
            .map(|text| qaqh_ringing_contract::fact::ContentValue::Inline { text }),
        output_truncated: call.output_truncated,
        error,
        metrics,
    })
}

fn interaction_view(row: InteractionRow) -> Result<InteractionView, DbError> {
    let who = format!("interaction {}", row.interaction_id);
    let resolution = match row.decision_json {
        Some(text) => Some(InteractionResolution {
            decision: parse_typed::<InteractionDecision>(&who, "decision", &text)?,
            detail: None,
            resolved_at_ms: row.resolved_at_ms.unwrap_or(row.created_at_ms),
        }),
        None => None,
    };
    Ok(InteractionView {
        interaction_id: InteractionId::new(row.interaction_id),
        call_id: row.call_id.map(ToolCallId::new),
        turn_id: row.turn_id.map(TurnId::new),
        kind: parse_typed::<InteractionKind>(&who, "kind", &row.kind)?,
        request: qaqh_ringing_contract::fact::ContentValue::Inline {
            text: row.payload_json,
        },
        choices: Vec::new(),
        expires_at_ms: None,
        resolution,
    })
}
