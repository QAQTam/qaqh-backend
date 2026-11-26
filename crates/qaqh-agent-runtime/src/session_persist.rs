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
    AssistantBlockKind, BlockId, ContentValue, TurnError, TurnId, TurnTerminal,
};
use qaqh_ringing_contract::state::{TimelineEntry, TurnOutcome};
use qaqh_session::db::DbError;
use qaqh_session::owner::SessionOwner;
use qaqh_types::{ContentBlock, Message, UsageInfo};

use crate::progress::AttemptBlocks;
use crate::provider_job::ProviderOutcome;

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
    outcome: TurnOutcome,
    usage: Option<UsageInfo>,
    original_failure: Option<(&str, &str)>,
) -> Result<(), String> {
    if let Err(error) = owner.finish_turn(turn.as_ref(), "finished", outcome).await {
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
    match persist_turn_or_emit_failure(owner, turn, outcome, usage, None).await {
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
    match persist_turn_or_emit_failure(owner, turn, outcome, usage, None).await {
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
