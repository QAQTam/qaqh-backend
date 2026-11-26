//! 回合交互（权限 / ask）落库与阶段流转 Fail-Closed 支撑件（v4.6 P3-0）。
//!
//! 职责边界：
//! - `tool_awaiting_interaction` / `tool_back_to_declared` / `raise_interaction` /
//!   `complete_interaction` / `resolve_pending_interactions` 全部严格 Fail-Closed：
//!   取消 `.ok()` 吞错，写库失败响亮 `log::error!` 并向调用方传播 [`DbError`]；
//! - 在 `raise_interaction` 内部若 `interaction_requested` 已落库但随后
//!   `set_turn_phase(WaitingUser)` 失败，会尝试同步取消刚建的交互记录，防止留下
//!   幽灵审批卡；
//! - 将 `InteractionView` 构建与权限决定映射从 `session_loop.rs` 拆出（宪法 #8）。

use qaqh_ringing_contract::TurnPhase;
use qaqh_ringing_contract::events::InteractionEndStatus;
use qaqh_ringing_contract::fact::{
    ContentValue, InteractionChoice, InteractionChoiceTone, InteractionDecision, InteractionId,
    InteractionKind, ToolCallId, TurnId,
};
use qaqh_ringing_contract::state::{InteractionView, PermissionRequestDetails};
use qaqh_session::db::DbError;
use qaqh_session::owner::SessionOwner;

use crate::session_loop::PermissionAnswer;

pub(crate) async fn tool_awaiting_interaction(
    owner: &SessionOwner,
    call: &ToolCallId,
    interaction_id: &InteractionId,
) -> Result<(), DbError> {
    owner
        .tool_awaiting_interaction(call, interaction_id)
        .await
        .inspect_err(|error| {
            log::error!("[loop] tool_awaiting_interaction {call} failed: {error}");
        })
}

pub(crate) async fn tool_back_to_declared(
    owner: &SessionOwner,
    call_id: &str,
) -> Result<(), DbError> {
    owner
        .tool_back_to_declared(&ToolCallId::new(call_id.to_string()))
        .await
        .inspect_err(|error| {
            log::error!("[loop] tool_back_to_declared {call_id} failed: {error}");
        })
}

/// 升起交互并切换回合阶段为 `WaitingUser`。
///
/// 若 `interaction_requested` 已落库但随后 `set_turn_phase(WaitingUser)` 失败，
/// 尽力补发一次取消以回收库内刚建的交互行，再把真实 `DbError` 返回调用方。
pub(crate) async fn raise_interaction(
    owner: &SessionOwner,
    turn: &TurnId,
    view: InteractionView,
) -> Result<(), DbError> {
    let interaction_id = view.interaction_id.clone();
    owner
        .interaction_requested(view)
        .await
        .inspect_err(|error| {
            log::error!("[loop] interaction_requested {interaction_id} failed: {error}");
        })?;
    if let Err(error) = owner
        .set_turn_phase(turn.as_ref(), TurnPhase::WaitingUser)
        .await
    {
        log::error!(
            "[loop] set_turn_phase WaitingUser for {interaction_id} failed: {error}"
        );
        if let Err(cancel_error) = owner
            .interaction_resolved(&interaction_id, None, InteractionEndStatus::Cancelled)
            .await
        {
            log::error!(
                "[loop] rollback interaction_resolved {interaction_id} failed: {cancel_error}"
            );
        }
        return Err(error);
    }
    Ok(())
}

/// 结算单条交互并按需把回合阶段切回 `Running`（任一步落库失败均响亮报错上抛）。
pub(crate) async fn complete_interaction(
    owner: &SessionOwner,
    turn: &TurnId,
    interaction_id: &InteractionId,
    decision: Option<InteractionDecision>,
    status: InteractionEndStatus,
    restore_running: bool,
) -> Result<(), DbError> {
    owner
        .interaction_resolved(interaction_id, decision, status)
        .await
        .inspect_err(|error| {
            log::error!("[loop] interaction_resolved {interaction_id} failed: {error}");
        })?;
    if restore_running {
        owner
            .set_turn_phase(turn.as_ref(), TurnPhase::Running)
            .await
            .inspect_err(|error| {
                log::error!(
                    "[loop] set_turn_phase Running after {interaction_id} failed: {error}"
                );
            })?;
    }
    Ok(())
}

/// 回合取消/中断时批量取消挂起交互：逐条尽力写入并在任一失败时返回首个错误，
/// 禁止静默吞错制造幽灵审批卡。
pub(crate) async fn resolve_pending_interactions(
    owner: &SessionOwner,
    pending: Vec<InteractionId>,
) -> Result<(), DbError> {
    let mut first_error: Option<DbError> = None;
    for interaction_id in pending {
        if let Err(error) = owner
            .interaction_resolved(&interaction_id, None, InteractionEndStatus::Cancelled)
            .await
        {
            log::error!(
                "[loop] resolve_pending_interactions {interaction_id} failed: {error}"
            );
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

pub(crate) fn build_permission_view(
    interaction_id: &InteractionId,
    call: ToolCallId,
    turn: &TurnId,
    details: &PermissionRequestDetails,
) -> InteractionView {
    InteractionView {
        interaction_id: interaction_id.clone(),
        call_id: Some(call),
        turn_id: Some(turn.clone()),
        kind: InteractionKind::Permission,
        request: ContentValue::Inline {
            text: serde_json::to_string(details)
                .expect("permission request details are serializable"),
        },
        choices: vec![
            InteractionChoice {
                decision: InteractionDecision::Approved,
                label: "允许本次".into(),
                description: None,
                tone: InteractionChoiceTone::Primary,
            },
            InteractionChoice {
                decision: InteractionDecision::ApprovedForSession,
                label: "本对话默认允许".into(),
                description: None,
                tone: InteractionChoiceTone::Secondary,
            },
            InteractionChoice {
                decision: InteractionDecision::Rejected,
                label: "拒绝".into(),
                description: None,
                tone: InteractionChoiceTone::Destructive,
            },
        ],
        expires_at_ms: None,
        resolution: None,
    }
}

pub(crate) fn build_ask_view(
    interaction_id: &InteractionId,
    call_id: &str,
    turn: &TurnId,
    text: String,
) -> InteractionView {
    InteractionView {
        interaction_id: interaction_id.clone(),
        call_id: Some(ToolCallId::new(call_id.to_string())),
        turn_id: Some(turn.clone()),
        kind: InteractionKind::Ask,
        request: ContentValue::Inline { text },
        choices: vec![],
        expires_at_ms: None,
        resolution: None,
    }
}

pub(crate) fn permission_decision(answer: PermissionAnswer) -> InteractionDecision {
    if !answer.approved {
        InteractionDecision::Rejected
    } else if matches!(
        answer.scope,
        qaqh_ringing_contract::PermissionGrantScope::Session
            | qaqh_ringing_contract::PermissionGrantScope::Persistent
    ) || answer.trust_folder
    {
        InteractionDecision::ApprovedForSession
    } else {
        InteractionDecision::Approved
    }
}
