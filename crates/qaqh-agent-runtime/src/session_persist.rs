//! v4.5.1 任务1:回合收口持久化与 Fail-Closed 失败广播模块
//! (修复单 §二.1,宪法 #8 独立子模块——session_loop.rs 只做薄调用接线)。
//!
//! 职责边界:
//! - 负责 `seal_response` / `seal_partial` / `seal_assistant_blocks` 与
//!   `persist_completed` / `persist_cancelled` / `persist_failed` 全部落库收口;
//! - 当 `owner.finish_turn` 落库失败时,经 `owner.emit_turn_persist_failed`
//!   向前端事件流广播 `TurnEnded(Failed)` + `ActivityChanged(Idle)`,防止 UI 挂死;
//! - 本模块**严禁越权调用 `notify_terminal`**,只返回 [`TurnTerminalResolution`]
//!   交由调用方单点触发一次 `notify_terminal` (消除双重触发)。

use qaqh_hooks::HookEventName;
use qaqh_ringing_contract::fact::{
    AssistantBlockKind, BlockId, ContentValue, InterruptReason, ToolCallId, TurnError, TurnId,
    TurnTerminal,
};
use qaqh_ringing_contract::state::{TimelineEntry, TurnOutcome};
use qaqh_ringing_contract::ulid::generate_ulid;
use qaqh_session::db::DbError;
use qaqh_session::owner::SessionOwner;
use qaqh_types::{ContentBlock, Message, UsageInfo};

use crate::progress::AttemptBlocks;
use crate::provider_job::ProviderOutcome;
use crate::session_loop::tool_result_message;

/// 持久化失败统一错误码(修复单 §二.1.2 / §二.1.4)。
pub(crate) const RESPONSE_PERSIST_FAILED: &str = "response_persist_failed";

/// 持久化收口结果:交由 `SessionLoop` 单点观测 stop hook 与触发一次 `notify_terminal`。
pub(crate) struct TurnTerminalResolution {
    pub terminal: TurnTerminal,
    pub error: Option<String>,
    pub stop_hook: Option<(HookEventName, String)>,
}

/// 盘满时给出用户可读的明确提示(修复单 §二.1.4);其余持久化错误原样透传。
pub(crate) fn persist_failure_message(error: &DbError) -> String {
    if error.is_disk_full() {
        format!("磁盘空间不足，上下文未保存: {error}")
    } else {
        error.to_string()
    }
}

pub(crate) fn empty_assistant() -> Message {
    Message {
        msg_id: None,
        role: "assistant".into(),
        name: None,
        content: Vec::new(),
    }
}

/// 终态 seal:推理块 + 答案块,复用流式展示的 block 身份。任一 `append_block`
/// 失败立即向上传播(任务1 Fail-Closed),严禁 `.ok()` 吞错后假报完成。
pub(crate) async fn seal_response(
    owner: &SessionOwner,
    turn: &TurnId,
    blocks: &AttemptBlocks,
    outcome: &ProviderOutcome,
) -> Result<(), DbError> {
    let message = outcome.message.clone().unwrap_or_else(empty_assistant);
    let mut reasoning = String::new();
    let mut answer = String::new();
    for block in &message.content {
        match block {
            ContentBlock::Reasoning {
                reasoning: text, ..
            } => reasoning.push_str(text),
            ContentBlock::Text { text } => answer.push_str(text),
            _ => {}
        }
    }
    let reasoning = if reasoning.is_empty() {
        outcome.partial_reasoning.clone().unwrap_or_default()
    } else {
        reasoning
    };
    let answer = if answer.is_empty() {
        outcome.partial_text.clone().unwrap_or_default()
    } else {
        answer
    };
    seal_assistant_blocks(
        owner,
        turn,
        blocks,
        outcome,
        reasoning,
        answer,
        outcome.opaque_items.clone(),
    )
    .await
}

/// 中断路径的 seal:只保留已流出文本,无 opaque 锚点。
pub(crate) async fn seal_partial(
    owner: &SessionOwner,
    turn: &TurnId,
    blocks: &AttemptBlocks,
    outcome: &ProviderOutcome,
) -> Result<(), DbError> {
    seal_assistant_blocks(
        owner,
        turn,
        blocks,
        outcome,
        outcome.partial_reasoning.clone().unwrap_or_default(),
        outcome.partial_text.clone().unwrap_or_default(),
        Vec::new(),
    )
    .await
}

/// 两条 seal 的共同落库体(任务1 消重)。纯工具调用轮 answer 恒空:空文本且
/// 无 opaque 锚点时不 seal,否则每个工具轮都落库并广播一个空 assistant 块
/// (UI 冒空气泡,快照 bootstrap 也原样携带)。opaque 是 Responses 回放锚点,
/// 非空必须照常落库。
async fn seal_assistant_blocks(
    owner: &SessionOwner,
    turn: &TurnId,
    blocks: &AttemptBlocks,
    outcome: &ProviderOutcome,
    reasoning: String,
    answer: String,
    opaque: Vec<serde_json::Value>,
) -> Result<(), DbError> {
    if !reasoning.is_empty() {
        owner
            .append_block(TimelineEntry::AssistantBlock {
                block_id: BlockId::new(blocks.reasoning_block_id.clone()),
                turn_id: turn.clone(),
                ord: 0,
                block_kind: AssistantBlockKind::Reasoning,
                content: ContentValue::Inline { text: reasoning },
                model: blocks.model.clone(),
                usage: None,
                generated_ms: None,
                opaque: vec![],
            })
            .await?;
    }
    if !answer.is_empty() || !opaque.is_empty() {
        owner
            .append_block(TimelineEntry::AssistantBlock {
                block_id: BlockId::new(blocks.answer_block_id.clone()),
                turn_id: turn.clone(),
                ord: 0,
                block_kind: AssistantBlockKind::Answer,
                content: ContentValue::Inline { text: answer },
                model: blocks.model.clone(),
                usage: outcome.usage.clone(),
                generated_ms: outcome.generated_ms,
                opaque,
            })
            .await?;
    }
    Ok(())
}

fn finished_outcome(
    terminal: TurnTerminal,
    usage: Option<UsageInfo>,
    error: Option<TurnError>,
) -> TurnOutcome {
    TurnOutcome::Finished {
        terminal,
        usage,
        error,
    }
}

/// 统一回合终态落库与 Fail-Closed 内存广播:
/// 落库失败时响亮记日志、强制向前端事件流广播 `TurnEnded(Failed)` + `Idle`,
/// 并合并原失败原因与持久化失败提示供上层单次上报 `terminal_port`。
async fn persist_turn_or_emit_failure(
    owner: &SessionOwner,
    turn: &TurnId,
    status: &'static str,
    outcome: TurnOutcome,
    usage: Option<UsageInfo>,
    original_failure: Option<(&str, &str)>,
) -> Result<(), String> {
    if let Err(error) = owner.finish_turn(turn.as_ref(), status, outcome).await {
        log::error!("[session] finish_turn persist failed: {error}");
        let persist_msg = persist_failure_message(&error);
        let merged_msg = match original_failure {
            Some((orig_code, orig_msg)) if orig_code == RESPONSE_PERSIST_FAILED => {
                format!("{orig_msg}; 终态落库失败: {persist_msg}")
            }
            Some((orig_code, orig_msg)) => format!("{orig_code}: {orig_msg}; {persist_msg}"),
            None => persist_msg,
        };
        owner
            .emit_turn_persist_failed(
                turn.as_ref(),
                usage,
                RESPONSE_PERSIST_FAILED,
                merged_msg.clone(),
            )
            .await;
        return Err(merged_msg);
    }
    Ok(())
}

pub(crate) async fn persist_completed(
    owner: &SessionOwner,
    turn: &TurnId,
    usage: Option<UsageInfo>,
) -> TurnTerminalResolution {
    let outcome = finished_outcome(TurnTerminal::Completed, usage.clone(), None);
    match persist_turn_or_emit_failure(owner, turn, "finished", outcome, usage, None).await {
        Ok(()) => TurnTerminalResolution {
            terminal: TurnTerminal::Completed,
            error: None,
            stop_hook: None,
        },
        Err(merged_msg) => {
            let detail = format!("{RESPONSE_PERSIST_FAILED}: {merged_msg}");
            TurnTerminalResolution {
                terminal: TurnTerminal::Failed,
                error: Some(detail.clone()),
                stop_hook: Some((HookEventName::StopFailure, detail)),
            }
        }
    }
}

pub(crate) async fn persist_cancelled(
    owner: &SessionOwner,
    turn: &TurnId,
    usage: Option<UsageInfo>,
) -> TurnTerminalResolution {
    let outcome = finished_outcome(TurnTerminal::Cancelled, usage.clone(), None);
    match persist_turn_or_emit_failure(owner, turn, "finished", outcome, usage, None).await {
        Ok(()) => TurnTerminalResolution {
            terminal: TurnTerminal::Cancelled,
            error: None,
            stop_hook: Some((HookEventName::StopCancelled, "cancelled".to_string())),
        },
        Err(merged_msg) => {
            let detail = format!("{RESPONSE_PERSIST_FAILED}: {merged_msg}");
            TurnTerminalResolution {
                terminal: TurnTerminal::Failed,
                error: Some(detail.clone()),
                stop_hook: Some((HookEventName::StopFailure, detail)),
            }
        }
    }
}

pub(crate) async fn persist_failed(
    owner: &SessionOwner,
    turn: &TurnId,
    usage: Option<UsageInfo>,
    code: &str,
    message: &str,
) -> TurnTerminalResolution {
    let outcome = finished_outcome(
        TurnTerminal::Failed,
        usage.clone(),
        Some(TurnError {
            code: code.to_string(),
            message: message.to_string(),
            retryable: false,
            details_ref: None,
        }),
    );
    let detail = match persist_turn_or_emit_failure(
        owner,
        turn,
        "finished",
        outcome,
        usage,
        Some((code, message)),
    )
    .await
    {
        Ok(()) => format!("{code}: {message}"),
        Err(merged_msg) => format!("{RESPONSE_PERSIST_FAILED}: {merged_msg}"),
    };
    TurnTerminalResolution {
        terminal: TurnTerminal::Failed,
        error: Some(detail.clone()),
        stop_hook: Some((HookEventName::StopFailure, detail)),
    }
}

/// 中断路径 Fail-Closed 收口（v4.6 P3-0）：
/// 若此前清理挂起交互已失败则直接按 `response_persist_failed` 判死；否则写入
/// `Interrupted` 终态，写库失败时广播 `TurnEnded(Failed)` + `Idle` 并单点返回
/// 供调用方触发一次 `notify_terminal`。
pub(crate) async fn persist_interrupted(
    owner: &SessionOwner,
    turn: &TurnId,
    reason: InterruptReason,
    interaction_error: Option<DbError>,
) -> TurnTerminalResolution {
    let detail = format!("{reason:?}");
    if let Some(error) = interaction_error {
        let merged = format!("interrupted: {detail}; {}", persist_failure_message(&error));
        return persist_failed(owner, turn, None, RESPONSE_PERSIST_FAILED, &merged).await;
    }
    let outcome = TurnOutcome::Interrupted { reason };
    match persist_turn_or_emit_failure(
        owner,
        turn,
        "interrupted",
        outcome,
        None,
        Some(("interrupted", &detail)),
    )
    .await
    {
        Ok(()) => TurnTerminalResolution {
            terminal: TurnTerminal::Failed,
            error: Some(format!("interrupted: {detail}")),
            stop_hook: Some((HookEventName::StopCancelled, detail)),
        },
        Err(merged_msg) => {
            let detail = format!("{RESPONSE_PERSIST_FAILED}: {merged_msg}");
            TurnTerminalResolution {
                terminal: TurnTerminal::Failed,
                error: Some(detail.clone()),
                stop_hook: Some((HookEventName::StopFailure, detail)),
            }
        }
    }
}

/// 声明工具调用并落库 `ToolCall` 时间线块（v4.6 P3-0 Fail-Closed）：
/// 任一步写库失败立即报错中止派发/重试注入，严禁在未落库状态下继续修改 `messages`。
pub(crate) async fn declare_tool_call(
    owner: &SessionOwner,
    turn: &TurnId,
    call_id: &str,
    tool_name: &str,
    input: &serde_json::Value,
) -> Result<(), DbError> {
    let call = ToolCallId::new(call_id);
    owner
        .declare_tool_call(&call, turn, tool_name, Some(input.clone()))
        .await
        .inspect_err(|error| log::error!("[loop] declare {call_id} failed: {error}"))?;
    owner
        .append_block(TimelineEntry::ToolCall {
            call_id: call,
            turn_id: Some(turn.clone()),
        })
        .await
        .inspect_err(|error| {
            log::error!("[loop] append ToolCall block {call_id} failed: {error}");
        })
}

/// B 路重发的公共落盘/组消息逻辑（v4.6 P3-0 Fail-Closed）：
/// 截断的文本前缀、`declare_tool_call` 与失败 `ToolResult` 块全部落库成功后才
/// 并入 `messages`，任一写库失败立即返回 `Err(DbError)`，交由主循环统一收口为
/// `finish_persist_failed`。
pub(crate) async fn inject_failed_tool_results(
    owner: &SessionOwner,
    turn: &TurnId,
    outcome: &ProviderOutcome,
    blocks: &AttemptBlocks,
    messages: &mut Vec<Message>,
) -> Result<(), DbError> {
    let prefix = outcome
        .partial_text
        .clone()
        .filter(|t| !t.trim().is_empty());
    if let Some(prefix) = &prefix {
        // 前缀是被截断消息的已产出部分，下一 attempt 的新消息不含它。
        owner
            .append_block(TimelineEntry::AssistantBlock {
                block_id: BlockId::new(format!("block_{}", generate_ulid())),
                turn_id: turn.clone(),
                ord: 0,
                block_kind: AssistantBlockKind::Answer,
                content: ContentValue::Inline {
                    text: prefix.clone(),
                },
                model: blocks.model.clone(),
                usage: None,
                generated_ms: outcome.generated_ms,
                opaque: vec![],
            })
            .await
            .inspect_err(|error| {
                log::error!("[loop] inject partial prefix append_block failed: {error}");
            })?;
    }
    let mut reply = Vec::new();
    if let Some(prefix) = prefix {
        reply.push(ContentBlock::Text { text: prefix });
    }
    let mut tool_messages = Vec::new();
    for tool in &outcome.partial_tools {
        let input = serde_json::from_str::<serde_json::Value>(&tool.args_text)
            .unwrap_or(serde_json::Value::Object(Default::default()));
        reply.push(ContentBlock::ToolUse {
            id: tool.call_id.clone(),
            name: tool.name.clone(),
            input: input.clone(),
        });
        declare_tool_call(owner, turn, &tool.call_id, &tool.name, &input).await?;
        let failure = format!(
            "tool call was cut off mid-generation ({} bytes of arguments received); call the tool again with complete arguments",
            tool.args_text.len()
        );
        owner
            .append_block(TimelineEntry::ToolResult {
                call_id: ToolCallId::new(tool.call_id.clone()),
                turn_id: Some(turn.clone()),
                content: ContentValue::Inline {
                    text: failure.clone(),
                },
                images: Vec::new(),
            })
            .await
            .inspect_err(|error| {
                log::error!(
                    "[loop] inject failed tool_result {} append_block failed: {error}",
                    tool.call_id
                );
            })?;
        tool_messages.push(tool_result_message(&tool.call_id, &failure, false, &[]));
    }
    // 落库全部成功后才并入 messages，与正常 ToolUse 分支同形。
    messages.push(Message {
        msg_id: None,
        role: "assistant".into(),
        name: None,
        content: reply,
    });
    messages.extend(tool_messages);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OS 层盘满(StorageFull)必须翻译成明确的中文提示,供
    /// TurnTerminal::Failed 携带(修复单 §二.1.4)。SQLITE_FULL → DiskFull 的
    /// 翻译由 session_loop_tests 的盘满端到端反证真实覆盖。
    #[test]
    fn disk_full_maps_to_explicit_user_facing_hint() {
        let error = DbError::Io(std::io::Error::from(std::io::ErrorKind::StorageFull));
        assert!(error.is_disk_full());
        let message = persist_failure_message(&error);
        assert!(
            message.contains("磁盘空间不足，上下文未保存"),
            "盘满提示必须显式: {message}"
        );
    }

    /// 非盘满错误原样透传,不冒充磁盘满。
    #[test]
    fn other_errors_pass_through_verbatim() {
        let error = DbError::Corrupt("bad row".to_string());
        assert!(!error.is_disk_full());
        assert_eq!(
            persist_failure_message(&error),
            "corrupt business row: bad row"
        );
    }
}
