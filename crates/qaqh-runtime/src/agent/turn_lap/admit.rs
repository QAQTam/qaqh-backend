//! Admit/dispatch 阶段：权限/ask/plan 审查 + 工具执行分发 (knife-7 A2 step5)
//!
//! 从 `engine_turn.rs` 原样搬运，行为不变，仅可见性 `pub(crate)` 化。

use std::collections::HashSet;

use qaqh_types::UsageInfo;

use crate::agent::engine_tool::ToolEngine;
use crate::agent::tool_runtime::ToolRuntime;
use crate::agent::turn_lap::gate::{abort_running_turn, seal_timeline_terminal_round};
use crate::agent::types::*;

// ── helpers (from engine_turn.rs, duplicated for phase decoupling) ──

fn domain_failure(
    code: &str,
    message: String,
    dedupe_key: Option<&str>,
) -> qaqh_domain::DomainError {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    qaqh_domain::DomainError {
        error_id: format!("err-{code}-{ts}"),
        code: code.to_string(),
        message,
        retryable: false,
        dedupe_key: dedupe_key.map(|s| s.to_string()),
    }
}

fn occurrence_id() -> String {
    format!(
        "occ-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
}

/// 取消终态收尾：seal 本轮为 Cancelled 并 abort 当前 turn。
#[allow(clippy::too_many_arguments)]
fn finish_cancelled_round(
    ctx: &mut RingContext,
    turn_id: &str,
    round_num: u32,
    active_stream_block: Option<&(qaqh_domain::TimelineBlockKind, String)>,
    timeline_tools_open: &HashSet<String>,
    last_usage: Option<UsageInfo>,
) -> Outcome {
    seal_timeline_terminal_round(
        ctx,
        turn_id,
        round_num,
        active_stream_block,
        timeline_tools_open,
        qaqh_domain::TimelineTurnState::Cancelled,
        None,
    );
    abort_running_turn(ctx, turn_id.to_string(), last_usage)
}

fn emit_active_ask(ctx: &mut RingContext, state: &TurnState) {
    if let Some(ask) = state.pending_asks.front() {
        // Ringing 双发：InteractionRequested（ask 交互请求）
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
            qaqh_domain::ControlEvent::InteractionRequested {
                interaction_id: ask.call_id.clone(),
                turn_id: state.turn_id.clone(),
                mode: ask.mode,
                questions: ask
                    .questions
                    .iter()
                    .map(|q| qaqh_domain::AskQuestion {
                        id: q.id.clone(),
                        question: q.question.clone(),
                        options: q.options.clone(),
                        allow_custom: q.allow_custom,
                    })
                    .collect(),
            },
        ));
    }
}

// ── execute_admitted_batch ──

/// Thin compatibility entry: scheduling/execution lives in [`ToolRuntime`].
pub fn execute_admitted_batch(
    ctx: &mut RingContext,
    tool: &ToolEngine,
    admitted: Vec<AdmittedTool>,
    tool_call_order: &[String],
    serial_call_ids: &HashSet<String>,
    turn_id: &str,
    round_num: u32,
) -> bool {
    ToolRuntime::execute_batch(
        ctx,
        tool,
        admitted,
        tool_call_order,
        serial_call_ids,
        turn_id,
        round_num,
    )
}

// ── Admit/dispatch for run_lap's !turn_completed first batch ──

/// Handle the full `!turn_completed` tool cycle for one gate lap.
///
/// Covers verbatim the inline block from `run_lap` after `parse_and_ingest`:
/// - `LoopPhase::ToolsRunning`
/// - duplicate ID check (→ `Handled`)
/// - `MAX_TOOL_CALLS_PER_ROUND = 16` truncate
/// - `tool_call_order` + `serial_call_ids` via workspace write-serialization
/// - `tool.admit_batch` + pre-execution suspend (permission / plan / todo)
/// - bounded parallel + serial execution (with `_with_diff`, `CodeChanged`, cancel)
/// - post-execution suspend (permission / ask / plan / todo)
///
/// Returns `Some(Outcome)` if the turn must yield/abort/handled immediately;
/// `None` means the caller should continue to backfill / `ContinueTurn`.
///
/// `suspended` is set in-place when yielding, matching `TurnEngine.suspended`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn admit_and_dispatch(
    ctx: &mut RingContext,
    tool: &mut ToolEngine,
    turn_context: &crate::agent::context::TurnContext,
    turn_id: &str,
    round_num: u32,
    last_usage: Option<UsageInfo>,
    active_stream_block: &Option<(qaqh_domain::TimelineBlockKind, String)>,
    timeline_tools_open: &HashSet<String>,
    suspended: &mut Option<TurnState>,
) -> Option<Outcome> {
    // ── Execute tools ──
    *ctx.phase = LoopPhase::ToolsRunning;

    let mut pending = ctx.agent.msg.get_last_step_pending();
    if pending.is_empty() {
        return None;
    }

    // Duplicate tool-call ID check → terminal outcome (M4)：
    // 裸 Handled 会把 phase 永久卡在 ToolsRunning 且 timeline 不 seal；
    // 这里先发 OperationFailed，再 seal 本轮为 Cancelled 并返回
    // TurnAborted，保证 turn 必有终态事件、phase 回到 Idle。
    {
        let mut seen = HashSet::new();
        if pending.iter().any(|t| !seen.insert(t.id.clone())) {
            ctx.agent.msg.remove_last_step_if_incomplete();
            // Ringing 双发：OperationFailed（结构化错误）
            ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
                qaqh_domain::ControlEvent::OperationFailed {
                    occurrence_id: occurrence_id(),
                    scope: qaqh_domain::ErrorScope::Tool,
                    error: domain_failure(
                        "duplicate_tool_call",
                        "Duplicate tool-call ID from model".into(),
                        Some("duplicate_tool_call"),
                    ),
                    operation_id: None,
                },
            ));
            let tool_ids: HashSet<String> = pending.iter().map(|t| t.id.clone()).collect();
            super::gate::seal_timeline_terminal_round(
                ctx,
                turn_id,
                round_num,
                None,
                &tool_ids,
                qaqh_domain::TimelineTurnState::Cancelled,
                Some(qaqh_domain::TimelineFailure {
                    code: "duplicate_tool_call".into(),
                    message: "Model emitted duplicate tool-call IDs; the round was aborted".into(),
                }),
            );
            return Some(Outcome::TurnAborted {
                turn_id: turn_id.to_string(),
                usage: last_usage.clone(),
            });
        }
    }

    // A model response must not create unbounded work. Rejected
    // calls still receive a result so the next round can recover.
    const MAX_TOOL_CALLS_PER_ROUND: usize = 16;
    if pending.len() > MAX_TOOL_CALLS_PER_ROUND {
        let rejected = pending.split_off(MAX_TOOL_CALLS_PER_ROUND);
        for call in rejected {
            ctx.agent.msg.push_tool_result_direct(
                &call.id,
                "[ERROR] Tool-call limit exceeded for this round (max 16). Retry the remaining calls in a later round.",
                false,
            );
        }
    }

    // Admit the complete model batch before executing any member.
    let tool_call_order = pending
        .iter()
        .map(|call| call.id.clone())
        .collect::<Vec<_>>();
    log::info!(
        "[TURN] run_lap turn_id={} round_num={} admit_batch {} pending tools",
        turn_id,
        round_num,
        pending.len()
    );
    let serial_call_ids = ToolRuntime::serial_call_ids(&pending);
    let admission = tool.admit_batch(ctx, &pending, turn_id, round_num, turn_context);
    if !admission.pending_permission_ids.is_empty()
        || !admission.pending_plans.is_empty()
        || admission.pending_todo_activation.is_some()
    {
        let reason = if !admission.pending_permission_ids.is_empty() {
            YieldReason::PermissionPending
        } else {
            // pending_plans / pending_todo_activation 同一评审 UI，review_type 区分
            YieldReason::PlanReview
        };
        // Capture plan info before moving into TurnState
        let plan_submitted = if reason == YieldReason::PlanReview {
            if let Some(ref todo_act) = admission.pending_todo_activation {
                Some((
                    todo_act.call_id.clone(),
                    String::new(),
                    "todo_activation".to_string(),
                    Some(todo_act.items.clone()),
                ))
            } else {
                admission.pending_plans.front().map(|plan| {
                    (
                        plan.call_id.clone(),
                        plan.content.clone(),
                        "plan".to_string(),
                        None,
                    )
                })
            }
        } else {
            None
        };
        *suspended = Some(TurnState {
            session_id: ctx.agent.session.seed.clone(),
            turn_id: turn_id.to_string(),
            round_num,
            pending_permission_ids: admission.pending_permission_ids,
            deferred_authorized: admission.authorized,
            tool_call_order,
            serial_call_ids,
            pending_asks: admission.pending_asks,
            pending_plans: admission.pending_plans,
            pending_todo_activation: admission.pending_todo_activation,
            usage: last_usage.clone(),
            reason,
        });
        if let Some((call_id, plan_content, review_type, todo_items)) = plan_submitted {
            // Ringing 双发：PlanReviewRequested（plan 评审请求）
            ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
                qaqh_domain::ControlEvent::PlanReviewRequested {
                    interaction_id: call_id.clone(),
                    turn_id: turn_id.to_string(),
                    plan_content,
                    review_type,
                    todo_items: todo_items.map(|items| {
                        items
                            .into_iter()
                            .map(|t| qaqh_domain::TodoItem {
                                id: t.id,
                                title: t.title,
                                description: t.description,
                                complexity: t.complexity,
                            })
                            .collect()
                    }),
                },
            ));
        }
        return Some(Outcome::YieldToUser {
            turn_id: turn_id.to_string(),
            reason,
        });
    }

    // Execute the authorized batch through the same runtime path as
    // approved-resume batches. The helper owns ordering, progress, outbox,
    // cancellation sealing, and skill-effect application.
    if !execute_admitted_batch(
        ctx,
        tool,
        admission.authorized,
        &tool_call_order,
        &serial_call_ids,
        turn_id,
        round_num,
    ) {
        return Some(finish_cancelled_round(
            ctx,
            turn_id,
            round_num,
            active_stream_block.as_ref(),
            timeline_tools_open,
            last_usage,
        ));
    }

    // Suspend before the next gate lap while any approval,
    // ask_user call, or plan review from this assistant round
    // is unresolved.
    if !admission.pending_permission_ids.is_empty()
        || !admission.pending_asks.is_empty()
        || !admission.pending_plans.is_empty()
        || admission.pending_todo_activation.is_some()
    {
        let reason = if !admission.pending_permission_ids.is_empty() {
            YieldReason::PermissionPending
        } else if admission.pending_todo_activation.is_some() || !admission.pending_plans.is_empty()
        {
            YieldReason::PlanReview
        } else {
            YieldReason::AskUser
        };
        // Capture plan info before moving into TurnState
        let plan_submitted = if reason == YieldReason::PlanReview {
            if let Some(ref todo_act) = admission.pending_todo_activation {
                Some((
                    todo_act.call_id.clone(),
                    String::new(),
                    "todo_activation".to_string(),
                    Some(todo_act.items.clone()),
                ))
            } else {
                admission.pending_plans.front().map(|plan| {
                    (
                        plan.call_id.clone(),
                        plan.content.clone(),
                        "plan".to_string(),
                        None,
                    )
                })
            }
        } else {
            None
        };
        *suspended = Some(TurnState {
            session_id: ctx.agent.session.seed.clone(),
            turn_id: turn_id.to_string(),
            round_num,
            pending_permission_ids: admission.pending_permission_ids,
            deferred_authorized: Vec::new(),
            tool_call_order,
            serial_call_ids,
            pending_asks: admission.pending_asks,
            pending_plans: admission.pending_plans,
            pending_todo_activation: None,
            usage: last_usage.clone(),
            reason,
        });
        if reason == YieldReason::AskUser {
            emit_active_ask(ctx, suspended.as_ref().expect("suspended ask state"));
        }
        if let Some((call_id, plan_content, review_type, todo_items)) = plan_submitted {
            // Ringing 双发：PlanReviewRequested（plan 评审请求）
            ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
                qaqh_domain::ControlEvent::PlanReviewRequested {
                    interaction_id: call_id.clone(),
                    turn_id: turn_id.to_string(),
                    plan_content,
                    review_type,
                    todo_items: todo_items.map(|items| {
                        items
                            .into_iter()
                            .map(|t| qaqh_domain::TodoItem {
                                id: t.id,
                                title: t.title,
                                description: t.description,
                                complexity: t.complexity,
                            })
                            .collect()
                    }),
                },
            ));
        }
        return Some(Outcome::YieldToUser {
            turn_id: turn_id.to_string(),
            reason,
        });
    }

    None
}
