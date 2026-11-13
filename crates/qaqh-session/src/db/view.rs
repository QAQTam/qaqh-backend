//! SessionState 组装:业务表直查 → 契约读模型。
//!
//! owner 未加载时提供 DB-only 快照(bootstrap 兜底);owner 运行时由 owner
//! 在内存视图上追加进度尾窗与实时状态,并填写 `seq`。所有 typed 字段在
//! 写入侧保存 serde wire 形式,此处反序列化;坏数据返回 `Corrupt` 报错,
//! 不静默降级。

use rusqlite::{Connection, OptionalExtension};
use serde::de::DeserializeOwned;

use qaqh_ringing_contract::fact::{
    ActivityState, InteractionDecision, SessionId, ToolCallId, ToolMetrics, TurnId,
};
use qaqh_ringing_contract::state::{
    ExecSessionState, ExecSessionView, InteractionResolution, InteractionView, SessionState,
    SubagentView, TimelineEntry, ToolCallState, ToolCallView, TurnOutcome, TurnPhase, TurnView,
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
    let exec_sessions = store::exec_sessions_for_bootstrap(conn, session_id)?
        .into_iter()
        .map(exec_session_view)
        .collect::<Result<Vec<_>, DbError>>()?;

    // activity 由活动回合 phase 直接派生(spec §4.4);后台执行会话数量不参与。
    // 无 pending phase 的活动回合按 running 处理;未决交互优先于 running,
    // 避免 waiting_user 被 active turn 遮蔽。
    let active_phase = store::active_turn_phase(conn, session_id)?;
    let has_pending_interaction = !interactions.is_empty();
    let activity = if has_pending_interaction {
        ActivityState::WaitingUser
    } else {
        match active_phase.as_deref() {
            Some("waiting_user") => ActivityState::WaitingUser,
            Some("cancelling") => ActivityState::Cancelling,
            Some(_) => ActivityState::Running,
            None => ActivityState::Idle,
        }
    };
    let current_turn_id = turns
        .iter()
        .filter(|turn| turn.outcome.is_none())
        .map(|turn| turn.turn_id.clone())
        .next_back();
    // 权威来源:sessions.title(rename 唯一写口是 owner::set_title)。
    let title = session_title(conn, session_id)?;
    let subagents = load_subagents(conn, session_id)?;
    let goal = super::goals::resolve_goal(conn, session_id)?
        .as_ref()
        .map(super::goals::goal_view);

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
        exec_sessions,
        driver: None,
        goal,
    })
}

/// Load exactly one turn's authoritative metadata, timeline and tool views.
pub fn load_session_turn(
    conn: &Connection,
    session_id: &str,
    turn_id: &str,
) -> Result<Option<qaqh_ringing_contract::SessionTurnSnapshot>, DbError> {
    if !store::session_exists(conn, session_id)? {
        return Err(DbError::SessionNotFound {
            session_id: session_id.to_string(),
        });
    }
    let turn = load_turn(conn, session_id, turn_id)?;
    let Some(turn) = turn else { return Ok(None) };
    let timeline = store::read_transcript_for_turn(conn, session_id, turn_id)?
        .into_iter()
        .map(|block| {
            let text = block.payload_json.ok_or_else(|| {
                DbError::Corrupt(format!("block {} has no payload_json", block.block_id))
            })?;
            serde_json::from_str::<TimelineEntry>(&text)
                .map_err(|error| DbError::Corrupt(format!("block {}: {error}", block.block_id)))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let tools = store::tool_calls_for_turn(conn, session_id, turn_id)?
        .into_iter()
        .map(tool_call_view)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(qaqh_ringing_contract::SessionTurnSnapshot {
        turn,
        timeline,
        tools,
    }))
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
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
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
            // `ToolEffectInfo` 当前只有 SubagentSpawned 一个变体(let-else 的 else
            // 不可达,cargo 警告);直接解构,将来加变体时编译器会逼着补处理分支。
            let qaqh_ringing_contract::state::ToolEffectInfo::SubagentSpawned {
                child_session_id,
                name,
                ..
            } = effect;
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

/// 子会话终态:读最后一回合的 **outcome 权威**(ADR-0018)。
///
/// `status='finished'` 只表示回合已结算——Completed/Cancelled/Failed 三种
/// terminal 都落 'finished'。旧投影把 'finished' 一律当 Completed,provider
/// 失败与超时取消的子代理在管理面显示"已完成",失败被静默吞掉。
/// 仍在跑 / 从未开回合 → None(运行中,不是终态)。
fn child_terminal_status(
    conn: &Connection,
    child_session_id: &str,
) -> Result<Option<qaqh_ringing_contract::fact::SubagentTerminalStatus>, DbError> {
    use qaqh_ringing_contract::fact::{SubagentTerminalStatus, TurnTerminal};

    let row: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT status, outcome_json FROM turns WHERE session_id = ?1
             AND status IN ('active', 'finished', 'interrupted')
             ORDER BY seq DESC LIMIT 1",
            [child_session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(rusqlite_error)?;
    let Some((status, outcome_json)) = row else {
        return Ok(None);
    };
    match status.as_str() {
        "active" => Ok(None),
        "finished" | "interrupted" => {
            // 与 turn_view 同律:结算行必须带 outcome,缺失即损坏,不猜终态。
            let text = outcome_json.ok_or_else(|| {
                DbError::Corrupt(format!(
                    "subagent {child_session_id}: turn is {status} without outcome_json"
                ))
            })?;
            let outcome = parse_typed::<TurnOutcome>(
                &format!("subagent {child_session_id}"),
                "turn outcome",
                &text,
            )?;
            Ok(Some(match outcome {
                TurnOutcome::Finished { terminal, .. } => match terminal {
                    TurnTerminal::Completed => SubagentTerminalStatus::Completed,
                    TurnTerminal::Failed => SubagentTerminalStatus::Failed,
                    TurnTerminal::Cancelled => SubagentTerminalStatus::Cancelled,
                },
                TurnOutcome::Interrupted { .. } => SubagentTerminalStatus::Cancelled,
            }))
        }
        other => Err(DbError::Corrupt(format!(
            "subagent {child_session_id}: unknown turn status {other:?}"
        ))),
    }
}

fn rusqlite_error(error: rusqlite::Error) -> DbError {
    DbError::Corrupt(format!("query failed: {error}"))
}

/// ADR-0014：回读指定回合的答复文本（assistant 块按 payload 的
/// block_kind=Answer 过滤，ord 序拼接）。新 loop 的答复恒为 inline；
/// 外置大内容属旧框架，不在此列。
pub fn turn_answer_text(
    conn: &Connection,
    session_id: &str,
    turn_id: &str,
) -> Result<String, DbError> {
    let mut stmt = conn.prepare(
        "SELECT payload_json FROM blocks
         WHERE session_id = ?1 AND turn_id = ?2 AND kind = 'assistant'
         ORDER BY ord",
    )?;
    let rows = stmt
        .query_map([session_id, turn_id], |row| row.get::<_, String>(0))
        .map_err(rusqlite_error)?;
    let mut answer = String::new();
    for row in rows {
        let payload = row.map_err(rusqlite_error)?;
        if let Ok(TimelineEntry::AssistantBlock {
            block_kind: qaqh_ringing_contract::fact::AssistantBlockKind::Answer,
            content: qaqh_ringing_contract::fact::ContentValue::Inline { text },
            ..
        }) = serde_json::from_str::<TimelineEntry>(&payload)
        {
            answer.push_str(&text);
        }
    }
    Ok(answer)
}

/// ADR-0015:触发回合的输入块正文(首块)。goal 续跑引擎用它识别自动续跑轮
/// (续跑消息携带 `<qaqh_goal_continuation>` 标记);无输入块返回空串。
pub fn turn_input_text(
    conn: &Connection,
    session_id: &str,
    turn_id: &str,
) -> Result<String, DbError> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT payload_json FROM blocks
             WHERE session_id = ?1 AND turn_id = ?2 AND kind = 'input'
             ORDER BY ord LIMIT 1",
            rusqlite::params![session_id, turn_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(rusqlite_error)?;
    let Some(payload) = payload else {
        return Ok(String::new());
    };
    if let Ok(TimelineEntry::Input {
        content: qaqh_ringing_contract::fact::ContentValue::Inline { text },
        ..
    }) = serde_json::from_str::<TimelineEntry>(&payload)
    {
        return Ok(text);
    }
    Ok(String::new())
}

/// ADR-0014：子会话 id → spawn 名（`SubagentSpawned` effect 是血缘命名权威）。
/// 懒投影一次性全扫；无 effect 的会话（wire spawn）回落 id 派生名。
pub fn spawn_names(
    conn: &Connection,
) -> Result<std::collections::HashMap<String, String>, DbError> {
    let mut stmt = conn.prepare(
        "SELECT call_id, effects_json FROM tool_calls
         WHERE effects_json LIKE '%subagent_spawned%'",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut names = std::collections::HashMap::new();
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
            } = effect;
            names.insert(child_session_id, name);
        }
    }
    Ok(names)
}

fn load_turns(conn: &Connection, session_id: &str) -> Result<Vec<TurnView>, DbError> {
    let mut stmt = conn.prepare(
        "SELECT turn_id, input_id, phase, status, outcome_json FROM turns
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
    rows.map(|row| {
        let (turn_id, input_id, phase, status, outcome_json) = row?;
        turn_view(turn_id, input_id, phase, status, outcome_json)
    })
    .collect()
}

fn load_turn(
    conn: &Connection,
    session_id: &str,
    turn_id: &str,
) -> Result<Option<TurnView>, DbError> {
    let row = conn
        .query_row(
            "SELECT turn_id, input_id, phase, status, outcome_json
             FROM turns WHERE session_id = ?1 AND turn_id = ?2",
            rusqlite::params![session_id, turn_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()?;
    row.map(|(turn_id, input_id, phase, status, outcome_json)| {
        turn_view(turn_id, input_id, phase, status, outcome_json)
    })
    .transpose()
}

fn turn_view(
    turn_id: String,
    input_id: Option<String>,
    phase: Option<String>,
    status: String,
    outcome_json: Option<String>,
) -> Result<TurnView, DbError> {
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
    let phase = match outcome {
        Some(_) => None,
        None => match phase.as_deref() {
            Some("running") => Some(TurnPhase::Running),
            Some("waiting_user") => Some(TurnPhase::WaitingUser),
            Some("cancelling") => Some(TurnPhase::Cancelling),
            Some(other) => {
                return Err(DbError::Corrupt(format!(
                    "turn {turn_id}: unknown phase {other:?}"
                )));
            }
            None => Some(TurnPhase::Running),
        },
    };
    Ok(TurnView {
        turn_id: TurnId::new(turn_id),
        input_id: input_id.map(qaqh_ringing_contract::fact::InputId::new),
        phase,
        outcome,
    })
}

fn load_timeline(conn: &Connection, session_id: &str) -> Result<Vec<TimelineEntry>, DbError> {
    let blocks = store::read_transcript_page(conn, session_id, None, 10_000)?;
    blocks
        .into_iter()
        .map(|block| {
            let text = block.payload_json.ok_or_else(|| {
                DbError::Corrupt(format!("block {} has no payload_json", block.block_id))
            })?;
            let mut entry: TimelineEntry = serde_json::from_str(&text)
                .map_err(|error| DbError::Corrupt(format!("block {}: {error}", block.block_id)))?;
            // ord 列是权威落库序;payload_json 里的 ord 只是占位,读取时一律回填。
            if let TimelineEntry::AssistantBlock { ord, .. } = &mut entry {
                *ord = block.ord;
            }
            Ok(entry)
        })
        .collect()
}

fn tool_call_view(call: ToolCallRow) -> Result<ToolCallView, DbError> {
    let who = format!("tool call {}", call.call_id);
    // state_json 是唯一权威;state_kind 冗余列只用于查询,不参与投影判定。
    let state = parse_typed::<ToolCallState>(&who, "state", &call.state_json)?;
    let args = match call.args_json {
        Some(text) => Some(parse_typed::<serde_json::Value>(&who, "args", &text)?),
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
    // yielded 的调用视图带显式 exec_session_id;其他来源的列值(迁移历史)
    // 也如实透出,客户端按 call_id/exec_session_id 关联。
    let exec_session_id = call
        .exec_session_id
        .map(qaqh_ringing_contract::fact::ExecSessionId::new);
    Ok(ToolCallView {
        call_id: ToolCallId::new(call.call_id),
        turn_id: call.turn_id.map(TurnId::new),
        tool_name: Some(call.tool_name),
        args,
        state,
        exec_session_id,
        output: call
            .output_text
            .map(|text| qaqh_ringing_contract::fact::ContentValue::Inline { text })
            .or_else(|| {
                call.output_ref.map(
                    |content_id| qaqh_ringing_contract::fact::ContentValue::Ref {
                        content_ref: qaqh_ringing_contract::fact::ContentRef::new(
                            qaqh_ringing_contract::fact::ContentHash::new(content_id),
                        ),
                    },
                )
            }),
        output_truncated: call.output_truncated,
        metrics,
        code_delta,
        effects,
    })
}

pub fn exec_session_view(row: store::ExecSessionRow) -> Result<ExecSessionView, DbError> {
    let who = format!("exec session {}", row.exec_session_id);
    let state = parse_typed::<ExecSessionState>(&who, "state", &row.state_json)?;
    // 库内 content_id 为裸 hex(schema 6→7 契约);wire ref 在此边界组装 `sha256:` 前缀。
    let output =
        row.output_content_id.map(|content_id| {
            qaqh_ringing_contract::fact::ContentValue::Ref {
                content_ref: qaqh_ringing_contract::fact::ContentRef::new(
                    qaqh_ringing_contract::fact::ContentHash::new(store::canonical_content_ref(
                        &content_id,
                    )),
                ),
            }
        });
    Ok(ExecSessionView {
        id: qaqh_ringing_contract::fact::ExecSessionId::new(row.exec_session_id),
        owner_session_id: SessionId::new(row.session_id),
        origin_turn_id: row
            .origin_turn_id
            .map(TurnId::new)
            .unwrap_or_else(|| TurnId::new(String::new())),
        origin_call_id: row
            .origin_call_id
            .map(ToolCallId::new)
            .unwrap_or_else(|| ToolCallId::new(String::new())),
        state,
        tty: row.tty,
        command: row.command,
        cwd: row.cwd,
        started_at_ms: row.started_at_ms,
        finished_at_ms: row.finished_at_ms,
        next_offset: row.next_offset,
        output_preview: row.output_preview,
        preview_start_offset: row.preview_start_offset,
        output_complete: row.output_complete,
        capture_omitted_bytes: row.capture_omitted_bytes,
        stream_dropped_bytes: row.stream_dropped_bytes,
        output,
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
