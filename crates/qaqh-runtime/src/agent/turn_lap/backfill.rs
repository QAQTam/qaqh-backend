//! Backfill 阶段：工具结果回填 → skills/ContinueTurn 判定 (knife-7 A2 step6)
//!
//! 从 `engine_turn.rs` 原样搬运，行为不变，仅可见性 `pub(crate)` 化。

use qaqh_session::canonical::generate_ulid;
use qaqh_session::session_fact_v2::{EventId, TurnFinished, TurnTerminal};
use qaqh_types::UsageInfo;

use crate::agent::dashboard;
use crate::agent::engine_tool::ToolEngine;
use crate::agent::state::agent::{tool_ledger_lease_ms, unix_ms};
use crate::agent::tool_runtime::canonical_turn_id;
use crate::agent::types::{Outcome, RingContext};

/// Durably append the canonical `TurnFinished` fact for a terminal outcome.
///
/// This is the fact `projected_run_status` folds into `idle` (and its
/// `usage` payload is the durable tok/s / context% source). Best-effort:
/// a ledger failure must not swallow the turn result.
pub(crate) fn record_turn_finished(
    ctx: &mut RingContext,
    turn_id: &str,
    terminal: TurnTerminal,
    usage: Option<UsageInfo>,
    error_code: Option<&str>,
) {
    let ledger = match ctx.agent.tool_ledger_mut() {
        Ok(Some(ledger)) => ledger,
        Ok(None) => return,
        Err(error) => {
            log::warn!("[TURN] tool ledger unavailable for turn_finished: {error}");
            return;
        }
    };
    let now = unix_ms();
    if let Err(error) = ledger.ensure_lease(now, tool_ledger_lease_ms()) {
        log::warn!("[TURN] canonical lease failed for turn_finished: {error}");
        return;
    }
    // Canonical validation: `Failed` terminals require a structured error.
    let error = terminal_error(error_code);
    if terminal == TurnTerminal::Failed && error.is_none() {
        log::warn!("[TURN] turn_finished(Failed) without an error code; falling back to completed");
        return record_turn_finished(ctx, turn_id, TurnTerminal::Completed, usage, None);
    }
    let payload = TurnFinished {
        turn_id: canonical_turn_id(turn_id),
        terminal,
        usage,
        error,
        finished_at_ms: now,
    };
    if let Err(error) = ledger.append_turn_finished(EventId::new(generate_ulid()), payload, now) {
        log::warn!("[TURN] failed to append turn_finished: {error}");
    }
}

fn terminal_error(error_code: Option<&str>) -> Option<qaqh_session::session_fact_v2::TurnError> {
    error_code.map(|code| qaqh_session::session_fact_v2::TurnError {
        code: code.to_string(),
        message: String::new(),
        retryable: false,
        details_ref: None,
    })
}

// ── helpers (from engine_turn.rs, moved for phase decoupling) ──

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

// ── emit_completed_tool_round (moved verbatim from engine_turn.rs) ──

/// Publish one ledger-committed tool result immediately, at most once per lap.
/// Round backfill uses the same path for denied/interaction-only calls.
/// Model history may still be waiting for ordered backfill; notification
/// order must not be coupled to that history ordering.
pub(crate) fn emit_completed_tool(
    ctx: &mut RingContext,
    turn_id: &str,
    round_num: u32,
    tc_id: &str,
    name: &str,
    result: &qaqh_types::ToolResult,
) {
    let session_id = &ctx.agent.session.session_id;
    let scope = &mut ctx.agent.completed_tool_notifications;
    if !scope.as_ref().is_some_and(|(session, turn, round, _)| {
        session == session_id && turn == turn_id && *round == round_num
    }) {
        *scope = Some((
            session_id.clone(),
            turn_id.to_string(),
            round_num,
            std::collections::HashSet::new(),
        ));
    }
    if !scope
        .as_mut()
        .expect("notification scope")
        .3
        .insert(tc_id.to_string())
    {
        return;
    }
    let args = ctx
        .agent
        .msg
        .tool_call_args(tc_id)
        .map(|a| a.to_string())
        .unwrap_or_default();
    ToolEngine::emit_timeline_tool_result(ctx, turn_id, round_num, tc_id, name, &args, result);
    ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Tool(
        qaqh_domain::ToolEvent::ToolFinished {
            tool_call_id: tc_id.to_string(),
            turn_id: turn_id.to_string(),
            round_num,
            result: result.clone(),
        },
    ));
    ctx.emitter.emit_timeline(qaqh_domain::TimelineIntent::BlockSealed {
        turn_id: turn_id.to_string(),
        round_num,
        block_id: format!("tool:{tc_id}"),
    });
}

/// Finish the round; only publish results not already sent on worker completion.
///
/// `TurnEngine::emit_completed_tool_round` delegates here. Shared by
/// `handle_tools_done` (run_lap fast path) and by the suspend/resume
/// handlers that remain in `engine_turn.rs` (`handle_*`).
pub(crate) fn emit_completed_tool_round(
    ctx: &mut RingContext,
    turn_id: &str,
    round_num: u32,
) -> Vec<qaqh_message::StepToolResult> {
    let results = ctx.agent.msg.last_step_tool_results();
    for step_result in &results {
        let qaqh_message::StepToolResult {
            tool_call_id: tc_id,
            tool_name: name,
            result,
        } = step_result;
        emit_completed_tool(ctx, turn_id, round_num, tc_id, name, result);
    }
    ctx.emitter
        .emit_timeline(qaqh_domain::TimelineIntent::RoundSealed {
            turn_id: turn_id.to_string(),
            round_num,
            is_final: false,
        });
    // Refresh status bar tasks after every tool round
    if !results.is_empty() {
        // Ringing 双发：DashboardUpdated（replaceable 覆盖）
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
            qaqh_domain::ControlEvent::DashboardUpdated {
                hp_connected: true,
                session_id: ctx.agent.session.session_id.clone(),
                tool_calls_total: 0,
                tool_failures: 0,
                current_phase: "single".into(),
                streaming: false,
            },
        ));
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
            qaqh_domain::ControlEvent::DashboardSnapshot {
                snapshot: dashboard::build_snapshot(ctx.agent.session.session_id.clone()),
            },
        ));
    }
    ctx.agent
        .msg
        .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
    results
}

// ── skills / ContinueTurn backfill (verbatim tail of run_lap) ──

/// Tail for `!turn_completed` after tool execution: backfill + skills lap + next turn.
///
/// Verbatim from the `!turn_completed` branch of `run_lap` after
/// `admit_and_dispatch` returns `None` (all tools resolved).
pub(crate) fn handle_tools_done(
    ctx: &mut RingContext,
    turn_id: String,
    round_num: u32,
    last_usage: Option<UsageInfo>,
) -> Outcome {
    // All tools from this round are now resolved.
    emit_completed_tool_round(ctx, &turn_id, round_num);

    if let Err(error) = ctx.agent.skills.complete_model_lap() {
        // Ringing 双发：OperationFailed（skill lap 失败）
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
            qaqh_domain::ControlEvent::OperationFailed {
                occurrence_id: occurrence_id(),
                scope: qaqh_domain::ErrorScope::Tool,
                error: domain_failure("skill_lap", error, Some("skill_lap")),
                operation_id: None,
            },
        ));
    }
    // Host-forced activation (second ignored lap) changes the
    // active set: materialize the envelope at the latest
    // message position before the next model reply.
    ctx.agent.sync_skill_injection(ctx.flow);

    // Another lap: tools executed, back to Gate
    Outcome::ContinueTurn {
        turn_id,
        round_num: round_num + 1,
        usage: last_usage,
    }
}

/// Tail for `turn_completed` (and fall-through): flush, skill check, seal or continue.
///
/// Verbatim from the post-`match effect` tail of `run_lap`.
pub(crate) fn handle_turn_complete(
    ctx: &mut RingContext,
    turn_id: String,
    round_num: u32,
    last_usage: Option<UsageInfo>,
) -> Outcome {
    ctx.agent
        .msg
        .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
    let forced = match ctx.agent.skills.complete_model_lap() {
        Ok(forced) => forced,
        Err(error) => {
            // Ringing 双发：OperationFailed（skill lap 失败）
            ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
                qaqh_domain::ControlEvent::OperationFailed {
                    occurrence_id: occurrence_id(),
                    scope: qaqh_domain::ErrorScope::Tool,
                    error: domain_failure("skill_lap", error, Some("skill_lap")),
                    operation_id: None,
                },
            ));
            Vec::new()
        }
    };
    if ctx.agent.skills.has_requested() || !forced.is_empty() {
        // Host-forced activation changed the active set: materialize
        // the envelope before the next model lap.
        ctx.agent.sync_skill_injection(ctx.flow);
        return Outcome::ContinueTurn {
            turn_id,
            round_num: round_num + 1,
            usage: last_usage,
        };
    }
    // Canonical terminal fact (R3): flips projected status back to `idle`
    // and carries the durable usage payload for tok/s / context%.
    record_turn_finished(
        ctx,
        &turn_id,
        TurnTerminal::Completed,
        last_usage.clone(),
        None,
    );
    ctx.emitter
        .emit_timeline(qaqh_domain::TimelineIntent::TurnSealed {
            turn_id: turn_id.clone(),
            state: qaqh_domain::TimelineTurnState::Completed,
            failure: None,
        });
    Outcome::TurnComplete {
        turn_id,
        usage: last_usage,
    }
}
