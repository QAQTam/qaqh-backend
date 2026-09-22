//! Admit/dispatch 阶段：权限/ask/plan 审查 + 工具执行分发 (knife-7 A2 step5)
//!
//! 从 `engine_turn.rs` 原样搬运，行为不变，仅可见性 `pub(crate)` 化。

use std::collections::HashSet;

use qaqh_types::UsageInfo;

use crate::agent::dashboard;
use crate::agent::engine_tool::ToolEngine;
use crate::agent::tool_runtime::{MAX_PARALLEL_TOOL_WORKERS, ToolRunOutcome, ToolRuntime};
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

/// 取消收尾：保证**每个 tool_use 恰有一条 tool_result**。
///
/// BUG-2026-09-13-08：`abort_running_turn` 会调用
/// `remove_last_step_if_incomplete()`，而该方法要求 step 全部 tool_use 都有
/// 结果。取消时若把「已执行但未回填」的项和「根本没跑」的项一视同仁，整个
/// step（含已执行结果）会被一起丢弃 → 下轮模型重发同一 tool_use，工具重复
/// 执行。
///
/// 因此本函数只处理**确实不会再有结果**的 call_id：
/// - 已在本轮回填过结果的（`msg.step_has_tool_result`）→ 跳过；
/// - outbox 里有执行记录的（工具真的跑过，只是结果没等到）→ 跳过，
///   由回填路径负责；
/// - 其余（从未执行 / 取消点在 spawn 之前）→ 补 `Cancelled` 终态。
///
/// 返回补了终态的项数。
fn seal_unexecuted_as_cancelled(
    ctx: &mut RingContext,
    call_ids: impl IntoIterator<Item = String>,
    reason: &str,
) -> usize {
    let executed = crate::agent::tool_outbox::executed_call_ids(&ctx.agent.session.seed);
    let mut sealed = 0;
    for call_id in call_ids {
        if ctx.agent.msg.step_has_tool_result(&call_id) || executed.contains(&call_id) {
            continue;
        }
        ctx.agent.msg.push_tool_result_canonical(
            &call_id,
            &qaqh_types::ToolResult::cancelled(reason),
            &[],
        );
        sealed += 1;
    }
    sealed
}

/// 回填一个已执行工具的结果，并发射其副作用（code delta / dashboard）。
///
/// 并行、串行与 `admit_and_dispatch` 三条路径的收尾完全同款——抽此一处
/// 收敛，避免其中一条再次漂移出「取消即丢弃结果」的缺陷。
fn backfill_executed_result(
    ctx: &mut RingContext,
    call_id: &str,
    tool_name: &str,
    turn_id: &str,
    round_num: u32,
    canonical_result: qaqh_types::ToolResult,
    code_delta: Option<qaqh_domain::CodeDeltaRecord>,
) {
    ctx.agent
        .msg
        .push_tool_result_canonical(call_id, &canonical_result, &canonical_result.images);
    if let Some(ref delta) = code_delta {
        ctx.stats.push_delta(delta.clone());
        // Ringing 双发：CodeChanged（与 engine_tool 同载荷）
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Tool(
            qaqh_domain::ToolEvent::CodeChanged {
                tool_call_id: call_id.to_string(),
                turn_id: turn_id.to_string(),
                round_num,
                lines_added: delta.lines_added,
                lines_removed: delta.lines_removed,
                files_created: delta.files_created,
                files_deleted: delta.files_deleted,
                file: delta.file.clone(),
            },
        ));
    }
    // Instant refresh for todo tools
    if matches!(tool_name, "todo") {
        // Ringing 双发：DashboardUpdated（replaceable 覆盖）
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
            qaqh_domain::ControlEvent::DashboardUpdated {
                hp_connected: true,
                session_seed: ctx.agent.session.seed.clone(),
                tool_calls_total: 0,
                tool_failures: 0,
                current_phase: "single".into(),
                streaming: false,
            },
        ));
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
            qaqh_domain::ControlEvent::DashboardSnapshot {
                snapshot: dashboard::build_snapshot(ctx.agent.session.seed.clone()),
            },
        ));
    }
}

/// 取消收尾的取消原因文案（进入模型上下文的 tool_result）。
const CANCELLED_TOOL_RESULT: &str = "[CANCELLED] Tool was not executed (user interrupted).";

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

// ── execute_admitted_batch (moved verbatim from engine_turn.rs) ──

/// Execute a batch of already-authorized tools, partitioned by write conflicts.
///
/// Shared helper for `handle_permission_resolved` (deferred batch) and for
/// future callers that need the same parallel/serial + progress + code_delta
/// + skill-effects + dashboard logic. Verbatim from `TurnEngine::execute_admitted_batch`.
pub fn execute_admitted_batch(
    ctx: &mut RingContext,
    tool: &ToolEngine,
    mut admitted: Vec<AdmittedTool>,
    tool_call_order: &[String],
    serial_call_ids: &HashSet<String>,
    turn_id: &str,
    round_num: u32,
) -> bool {
    // L3 outbox: execution facts are recorded inside the tool worker thread,
    // right after the tool returns, so a kill between "tool ran" and "result
    // persisted" is still distinguishable from "tool never ran".
    let outbox_seed = ctx.agent.session.seed.clone();
    admitted.sort_by_key(|item| {
        tool_call_order
            .iter()
            .position(|id| id == &item.call_id)
            .unwrap_or(usize::MAX)
    });
    let mut ordered_skill_effects = Vec::new();
    let (mut parallel, serial): (Vec<_>, Vec<_>) = admitted
        .into_iter()
        .partition(|item| !serial_call_ids.contains(&item.call_id));

    while !parallel.is_empty() {
        // 批间取消检查（对齐 `admit_and_dispatch` 的 C1）：取消已到达就不再
        // spawn 新工具线程、不再发 Running 事件；剩余批由下方补取消终态。
        if ctx.cancel.is_set() {
            let remaining = parallel
                .iter()
                .map(|item| item.call_id.clone())
                .chain(serial.iter().map(|item| item.call_id.clone()));
            seal_unexecuted_as_cancelled(ctx, remaining, CANCELLED_TOOL_RESULT);
            apply_ordered_skill_effects(ctx, ordered_skill_effects, tool_call_order);
            return false;
        }
        let batch_len = parallel.len().min(MAX_PARALLEL_TOOL_WORKERS);
        let batch: Vec<_> = parallel.drain(..batch_len).collect();
        for admitted in &batch {
            let call_id = admitted.call_id.clone();
            let tool_name = admitted.auth.tool_name().to_string();
            let tool_args = admitted.auth.args().clone();
            ToolEngine::emit_timeline_tool_running(
                ctx, turn_id, round_num, &call_id, &tool_name, &tool_args,
            );
        }
        let (progress_rx, runs) = ToolRuntime::spawn_batch(batch, outbox_seed.clone());
        let cancelled = ctx.cancel.is_set();
        let results = ToolRuntime::collect(ctx, tool, progress_rx, runs, turn_id, round_num);

        // BUG-2026-09-13-08：取消不再丢弃已执行结果。
        //
        // 工具线程一旦 spawn 就真实执行（副作用已发生、outbox 已记录），
        // 「取消」只意味着不再等剩余项——已 join 出来的 canonical 结果必须
        // 照常回填，否则 store 留下 open tool_use，下轮模型重发 → 重复执行。
        // 剩余批（含仍在执行的项）在下方统一补取消终态。
        for result in results {
            let call_id = result.call_id;
            let tool_name = result.tool_name;
            match result.outcome {
                ToolRunOutcome::Completed(result) => {
                    let result = *result;
                    backfill_executed_result(
                        ctx,
                        &call_id,
                        &tool_name,
                        turn_id,
                        round_num,
                        result.result,
                        result.code_delta,
                    );
                    ordered_skill_effects.push((call_id.clone(), result.skill_effects));
                }
                ToolRunOutcome::Panicked => ctx.agent.msg.push_tool_result_direct(
                    &call_id,
                    "[ERROR] tool thread panicked",
                    false,
                ),
            }
        }
        if cancelled {
            // 剩余并行批根本没起，或上面的项被回收时仍未落结果——统一补终态，
            // 保证「每个 tool_use 恰有一条 tool_result」（见 seal_unexecuted_as_cancelled）。
            let remaining = parallel.iter().map(|item| item.call_id.clone());
            seal_unexecuted_as_cancelled(ctx, remaining, CANCELLED_TOOL_RESULT);
            parallel.clear();
            break;
        }
    }

    let mut serial = serial.into_iter();
    while let Some(admitted) = serial.next() {
        if ctx.cancel.is_set() {
            // 串行路径取消：已收集的 skill_effects 照常应用，未执行的项（当前
            // 这一项及其后全部）补取消终态。旧实现 `return false` 会丢弃已
            // 收集的 skill_effects，并把未执行的 tool_use 留成 open。
            let remaining = std::iter::once(admitted.call_id)
                .chain(serial.by_ref().map(|admitted| admitted.call_id));
            seal_unexecuted_as_cancelled(ctx, remaining, CANCELLED_TOOL_RESULT);
            apply_ordered_skill_effects(ctx, ordered_skill_effects, tool_call_order);
            return false;
        }
        let call_id = admitted.call_id.clone();
        let tool_name = admitted.auth.tool_name().to_string();
        let tool_args = admitted.auth.args().clone();
        ToolEngine::emit_timeline_tool_running(
            ctx, turn_id, round_num, &call_id, &tool_name, &tool_args,
        );
        let (progress_rx, runs) = ToolRuntime::spawn_batch(vec![admitted], outbox_seed.clone());
        let results = ToolRuntime::collect(ctx, tool, progress_rx, runs, turn_id, round_num);
        for result in results {
            match result.outcome {
                ToolRunOutcome::Completed(result) => {
                    let result = *result;
                    backfill_executed_result(
                        ctx,
                        &call_id,
                        &tool_name,
                        turn_id,
                        round_num,
                        result.result,
                        result.code_delta,
                    );
                    ordered_skill_effects.push((call_id.clone(), result.skill_effects));
                }
                ToolRunOutcome::Panicked => ctx.agent.msg.push_tool_result_direct(
                    &call_id,
                    "[ERROR] tool thread panicked",
                    false,
                ),
            }
        }
    }

    if ctx.cancel.is_set() {
        // 串行循环收尾后仍可能落在取消态（最后一项 join 期间收到取消）：
        // 结果已回填，这里只做终态兜底，绝不丢弃。
        apply_ordered_skill_effects(ctx, ordered_skill_effects, tool_call_order);
        return false;
    }
    apply_ordered_skill_effects(ctx, ordered_skill_effects, tool_call_order);
    true
}

/// 按模型原始 tool_call_order 应用已收集的 skill effects。
fn apply_ordered_skill_effects(
    ctx: &mut RingContext,
    mut ordered: Vec<(String, Vec<qaqh_workspace::ToolEffect>)>,
    tool_call_order: &[String],
) {
    ordered.sort_by_key(|(call_id, _)| {
        tool_call_order
            .iter()
            .position(|id| id == call_id)
            .unwrap_or(usize::MAX)
    });
    for (_, effects) in ordered {
        ctx.agent.apply_tool_effects(effects, ctx.flow);
    }
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
    let write_pairs: Vec<(String, serde_json::Value)> = pending
        .iter()
        .map(|tool| (tool.name.clone(), tool.args.clone()))
        .collect();
    let (_serial_groups, serial_after) =
        qaqh_workspace::conflict::resolve_write_conflicts(&write_pairs);
    let serial_call_ids: HashSet<String> = serial_after
        .iter()
        .map(|index| pending[*index].id.clone())
        .collect();
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
