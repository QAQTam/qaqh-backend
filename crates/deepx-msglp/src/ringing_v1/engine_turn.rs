//! TurnEngine: drives the gate→tools→repeat cycle.
//!
//! Owns: suspended TurnState.
//! Receives: RingContext + ToolEngine (for tool execution).
//! Returns: Outcome (ContinueTurn, YieldToUser, TurnComplete, Error).

use std::collections::{HashMap, HashSet};

use deepx_message::Effect;
use deepx_domain::AskAnswer;
use deepx_types::UsageInfo;

use super::engine_tool::ToolEngine;
use super::types::*;
use crate::services::conflict;
use crate::services::dashboard;
use crate::util;

/// A1：`block_checkpoint` 发射间隔（delta 次数或时长，先到为准）。
const CHECKPOINT_TOKEN_INTERVAL: u32 = 64;
const CHECKPOINT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
/// A3：`usage_updated` 节流窗口（流式期间 ~1s 一条；终值由 lap 结束补发）。
const USAGE_EMIT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Why the turn is being resumed.
pub enum ResumeReason {
    /// User answered permission dialogs — all approvals resolved.
    PermissionResolved,
}

/// TurnEngine manages a single LLM turn lifecycle.
pub struct TurnEngine {
    /// If Some, a turn is suspended waiting for permission or ask_user.
    pub(crate) suspended: Option<TurnState>,
}

impl TurnEngine {
    /// A1：按 delta 次数 / 时间窗发射 `block_checkpoint`（该 round 当前完整文本）。
    fn maybe_emit_block_checkpoint(
        ctx: &mut RingContext,
        turn_id: &str,
        round_num: u32,
        kind: deepx_domain::RoundDeltaKind,
        text: &str,
        tokens_since_checkpoint: &mut u32,
        last_checkpoint_at: &mut std::time::Instant,
    ) {
        *tokens_since_checkpoint += 1;
        if *tokens_since_checkpoint < CHECKPOINT_TOKEN_INTERVAL
            && last_checkpoint_at.elapsed() < CHECKPOINT_INTERVAL
        {
            return;
        }
        *tokens_since_checkpoint = 0;
        *last_checkpoint_at = std::time::Instant::now();
        ctx.emitter.emit_domain(deepx_domain::DomainEvent::Conversation(
            deepx_domain::ConversationEvent::BlockCheckpoint {
                turn_id: turn_id.to_string(),
                round_num,
                kind,
                text: text.to_string(),
                char_count: text.chars().count() as u32,
            },
        ));
    }

    pub fn new() -> Self {
        Self { suspended: None }
    }

    pub fn is_suspended(&self) -> bool {
        self.suspended.is_some()
    }

    /// Returns the reason the turn was suspended, or None if not suspended.
    pub fn suspended_reason(&self) -> Option<YieldReason> {
        self.suspended.as_ref().map(|s| s.reason)
    }

    pub fn suspended_turn_id(&self) -> Option<&str> {
        self.suspended.as_ref().map(|state| state.turn_id.as_str())
    }

    // ── Public API ──

    /// Run one full lap around the gate→tools ring.
    /// Called initially by InputEngine after user input, and recursively
    /// by Loop::apply_outcome for ContinueTurn.
    pub fn run(
        &mut self,
        ctx: &mut RingContext,
        tool: &mut ToolEngine,
        turn_id: String,
        round_num: u32,
        last_usage: Option<UsageInfo>,
    ) -> Outcome {
        self.run_lap(ctx, tool, turn_id, round_num, last_usage)
    }

    /// Resume a suspended turn.
    pub fn resume(
        &mut self,
        ctx: &mut RingContext,
        tool: &mut ToolEngine,
        reason: ResumeReason,
    ) -> Outcome {
        let saved = match self.suspended.take() {
            Some(s) => s,
            None => return Outcome::Error("No suspended turn to resume".into()),
        };
        if saved.session_id != ctx.agent.session.seed {
            log::warn!("[TURN] refusing to resume stale turn {}", saved.turn_id);
            return Outcome::Handled;
        }

        match reason {
            ResumeReason::PermissionResolved => {
                log::info!(
                    "[TURN] resuming turn {} round {}",
                    saved.turn_id,
                    saved.round_num
                );
                self.emit_completed_tool_round(ctx, &saved.turn_id, saved.round_num);
                self.run_lap(ctx, tool, saved.turn_id, saved.round_num + 1, saved.usage)
            }
        }
    }

    /// Resolve one LLM permission by call ID. The turn only advances after
    /// every permission from the assistant round has been accounted for.
    pub fn handle_permission_resolved(
        &mut self,
        ctx: &mut RingContext,
        tool: &mut ToolEngine,
        call_id: &str,
        admitted: Option<AdmittedTool>,
    ) -> Outcome {
        let Some(saved) = self.suspended.as_mut() else {
            log::warn!("[TURN] permission resolved without a suspended turn: {call_id}");
            return Outcome::Handled;
        };
        if saved.reason != YieldReason::PermissionPending
            || !saved.pending_permission_ids.iter().any(|id| id == call_id)
        {
            log::warn!("[TURN] stale permission resolution ignored: {call_id}");
            return Outcome::Handled;
        }

        if let Some(admitted) = admitted {
            saved.deferred_authorized.push(admitted);
        }
        saved.pending_permission_ids.retain(|id| id != call_id);
        if !saved.pending_permission_ids.is_empty() {
            return Outcome::YieldToUser {
                turn_id: saved.turn_id.clone(),
                reason: YieldReason::PermissionPending,
            };
        }

        let mut saved = self.suspended.take().expect("permission suspension exists");
        let deferred_authorized = std::mem::take(&mut saved.deferred_authorized);
        if !Self::execute_admitted_batch(
            ctx,
            tool,
            deferred_authorized,
            &saved.tool_call_order,
            &saved.serial_call_ids,
            &saved.turn_id,
            saved.round_num,
        ) {
            let tool_ids = saved.tool_call_order.iter().cloned().collect();
            Self::seal_timeline_terminal_round(
                ctx,
                &saved.turn_id,
                saved.round_num,
                None,
                &tool_ids,
                deepx_domain::TimelineTurnState::Cancelled,
                None,
            );
            return Self::abort_running_turn(ctx, saved.turn_id, saved.usage);
        }

        if !saved.pending_plans.is_empty() {
            saved.reason = YieldReason::PlanReview;
            let turn_id = saved.turn_id.clone();
            if let Some(plan) = saved.pending_plans.front() {
                // Ringing 双发：PlanReviewRequested（resume 重放）
                ctx.emitter.emit_domain(deepx_domain::DomainEvent::Control(
                    deepx_domain::ControlEvent::PlanReviewRequested {
                        interaction_id: plan.call_id.clone(),
                        turn_id: turn_id.clone(),
                        plan_content: plan.content.clone(),
                        review_type: "plan".to_string(),
                        todo_items: None,
                    },
                ));
            }
            self.suspended = Some(saved);
            return Outcome::YieldToUser {
                turn_id,
                reason: YieldReason::PlanReview,
            };
        }

        if !saved.pending_asks.is_empty() {
            saved.reason = YieldReason::AskUser;
            let turn_id = saved.turn_id.clone();
            Self::emit_active_ask(ctx, &saved);
            self.suspended = Some(saved);
            return Outcome::YieldToUser {
                turn_id,
                reason: YieldReason::AskUser,
            };
        }

        self.emit_completed_tool_round(ctx, &saved.turn_id, saved.round_num);
        self.run_lap(ctx, tool, saved.turn_id, saved.round_num + 1, saved.usage)
    }

    /// Validate and apply an answer to the front ask without consuming state
    /// on identity or payload errors.
    pub fn handle_ask_response(
        &mut self,
        ctx: &mut RingContext,
        tool: &mut ToolEngine,
        ask_id: &str,
        answers: &[AskAnswer],
    ) -> Outcome {
        let active = match self.suspended.as_ref() {
            Some(state) if state.reason == YieldReason::AskUser => {
                match state.pending_asks.front() {
                    Some(active) => active,
                    None => {
                        Self::emit_ask_rejected(ctx, ask_id, "No active ask_user prompt");
                        return Outcome::Handled;
                    }
                }
            }
            _ => {
                Self::emit_ask_rejected(ctx, ask_id, "No active ask_user prompt");
                return Outcome::Handled;
            }
        };

        if active.call_id != ask_id {
            Self::emit_ask_rejected(ctx, ask_id, "ask_id does not match the active prompt");
            return Outcome::Handled;
        }
        let ordered = match Self::validate_answers(active, answers) {
            Ok(ordered) => ordered,
            Err(message) => {
                Self::emit_ask_rejected(ctx, ask_id, &message);
                return Outcome::Handled;
            }
        };

        let mut saved = self.suspended.take().expect("active ask suspension exists");
        let active = saved.pending_asks.pop_front().expect("active ask exists");
        let content = serde_json::json!({
            "status": "answered",
            "answers": ordered,
        })
        .to_string();
        ctx.agent
            .msg
            .push_tool_result_direct(&active.call_id, &content, true);
        ctx.agent
            .msg
            .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
        // Ringing 双发：InteractionResolved（ask 已回答）
        ctx.emitter.emit_domain(deepx_domain::DomainEvent::Control(
            deepx_domain::ControlEvent::InteractionResolved {
                interaction_id: active.call_id,
                resolution: deepx_domain::AskResolution::Answered,
            },
        ));

        if !saved.pending_asks.is_empty() {
            saved.reason = YieldReason::AskUser;
            let turn_id = saved.turn_id.clone();
            Self::emit_active_ask(ctx, &saved);
            self.suspended = Some(saved);
            return Outcome::YieldToUser {
                turn_id,
                reason: YieldReason::AskUser,
            };
        }

        self.emit_completed_tool_round(ctx, &saved.turn_id, saved.round_num);
        self.run_lap(ctx, tool, saved.turn_id, saved.round_num + 1, saved.usage)
    }

    /// Validate and apply a plan review decision without consuming state
    /// on identity or payload errors.
    pub fn handle_plan_response(
        &mut self,
        ctx: &mut RingContext,
        tool: &mut ToolEngine,
        call_id: &str,
        approved: bool,
        message: &str,
        autonomous: bool,
    ) -> Outcome {
        let active_id = self
            .suspended
            .as_ref()
            .filter(|state| state.reason == YieldReason::PlanReview)
            .and_then(|state| {
                state
                    .pending_plans
                    .front()
                    .map(|p| p.call_id.as_str())
                    .or_else(|| {
                        state
                            .pending_todo_activation
                            .as_ref()
                            .map(|t| t.call_id.as_str())
                    })
            });
        if active_id != Some(call_id) {
            log::warn!("[TURN] plan response without a suspended review: {call_id}");
            return Outcome::Handled;
        }

        let mut saved = self
            .suspended
            .take()
            .expect("plan review suspension exists");

        // ── Todo activation path (Goal mode frozen) ──
        if let Some(todo_act) = saved.pending_todo_activation.take() {
            if approved {
                let content =
                    "Goal automation is temporarily unavailable. Use manual todo tools instead."
                        .to_string();
                ctx.agent
                    .msg
                    .push_tool_result_direct(&todo_act.call_id, &content, false);
                log::warn!(
                    "[TURN] approved todo activation rejected — Goal mode frozen: {}",
                    content
                );
            } else {
                ctx.agent.msg.push_tool_result_direct(
                    &todo_act.call_id,
                    &format!(
                        "Todo activation rejected: {}",
                        if message.is_empty() {
                            "no reason given"
                        } else {
                            message
                        }
                    ),
                    false,
                );
            }
            ctx.agent
                .msg
                .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
            // Ringing 双发：PlanReviewResolved（todo 激活裁决）
            ctx.emitter.emit_domain(deepx_domain::DomainEvent::Control(
                deepx_domain::ControlEvent::PlanReviewResolved {
                    interaction_id: todo_act.call_id,
                    approved,
                },
            ));
            self.emit_completed_tool_round(ctx, &saved.turn_id, saved.round_num);
            return self.run_lap(ctx, tool, saved.turn_id, saved.round_num + 1, saved.usage);
        }

        // ── Plan review path ──
        let plan = saved
            .pending_plans
            .pop_front()
            .expect("pending plan exists");

        let content = if approved && autonomous {
            format!(
                "Plan approved. Goal automation is currently frozen. Track execution with todo(action=\"create|update\"); use todo(action=\"cancel\") for work that is no longer needed.\n\n{}",
                plan.content
            )
        } else if approved {
            format!("Plan approved.\n\n{}", plan.content)
        } else {
            format!(
                "Plan rejected: {}\n\n{}",
                if message.is_empty() {
                    "no reason given"
                } else {
                    message
                },
                plan.content
            )
        };
        ctx.agent
            .msg
            .push_tool_result_direct(&plan.call_id, &content, approved);
        ctx.agent
            .msg
            .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
        // Ringing 双发：PlanReviewResolved（plan 裁决）
        ctx.emitter.emit_domain(deepx_domain::DomainEvent::Control(
            deepx_domain::ControlEvent::PlanReviewResolved {
                interaction_id: plan.call_id,
                approved,
            },
        ));

        self.emit_completed_tool_round(ctx, &saved.turn_id, saved.round_num);
        self.run_lap(ctx, tool, saved.turn_id, saved.round_num + 1, saved.usage)
    }

    /// Abort the active suspended ask. A stale dismiss leaves state untouched.
    pub fn handle_ask_dismiss(
        &mut self,
        ctx: &mut RingContext,
        tool: &mut ToolEngine,
        ask_id: &str,
    ) -> Outcome {
        let active_id = self
            .suspended
            .as_ref()
            .filter(|state| state.reason == YieldReason::AskUser)
            .and_then(|state| state.pending_asks.front())
            .map(|ask| ask.call_id.as_str());
        if active_id != Some(ask_id) {
            Self::emit_ask_rejected(ctx, ask_id, "ask_id does not match the active prompt");
            return Outcome::Handled;
        }

        let saved = self.suspended.take().expect("active ask suspension exists");
        let tool_ids = saved.tool_call_order.iter().cloned().collect();
        Self::seal_timeline_terminal_round(
            ctx,
            &saved.turn_id,
            saved.round_num,
            None,
            &tool_ids,
            deepx_domain::TimelineTurnState::Cancelled,
            None,
        );
        tool.clear_pending();
        ctx.agent.msg.remove_last_step_if_incomplete();
        ctx.agent
            .msg
            .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
        // Ringing 双发：InteractionResolved��ask 交互终结）
        ctx.emitter.emit_domain(deepx_domain::DomainEvent::Control(
            deepx_domain::ControlEvent::InteractionResolved {
                interaction_id: ask_id.to_string(),
                resolution: deepx_domain::AskResolution::Dismissed,
            },
        ));
        Outcome::TurnAborted {
            turn_id: saved.turn_id,
            usage: saved.usage,
            consume_queued_interrupt: false,
        }
    }

    fn emit_ask_rejected(ctx: &mut RingContext, ask_id: &str, message: &str) {
        // legacy AskRejected 退役：无 Ringing 专用事件，按 §4.2 登记
        // "由 OperationFailed（ErrorScope::Control, code=ask_rejected）覆盖"。
        ctx.emitter.emit_domain(deepx_domain::DomainEvent::Control(
            deepx_domain::ControlEvent::OperationFailed {
                occurrence_id: format!("occ-ask-rejected-{ask_id}"),
                scope: deepx_domain::ErrorScope::Control,
                error: deepx_domain::DomainError {
                    error_id: format!("ask-rejected-{ask_id}"),
                    code: "ask_rejected".into(),
                    message: message.to_string(),
                    retryable: false,
                    dedupe_key: Some(format!("ask_rejected:{ask_id}")),
                },
                operation_id: None,
            },
        ));
    }

    /// 构造结构化领域错误（error_id = 时间戳，dedupe 可选）。
    fn domain_failure(
        code: &str,
        message: String,
        dedupe_key: Option<&str>,
    ) -> deepx_domain::DomainError {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        deepx_domain::DomainError {
            error_id: format!("err-{code}-{ts}"),
            code: code.to_string(),
            message,
            retryable: false,
            dedupe_key: dedupe_key.map(|s| s.to_string()),
        }
    }

    /// OperationFailed 的 occurrence_id（时间戳）。
    fn occurrence_id() -> String {
        format!(
            "occ-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
        )
    }

    fn emit_active_ask(ctx: &mut RingContext, state: &TurnState) {
        if let Some(ask) = state.pending_asks.front() {
            // Ringing 双发：InteractionRequested（ask 交互请求）
            ctx.emitter.emit_domain(deepx_domain::DomainEvent::Control(
                deepx_domain::ControlEvent::InteractionRequested {
                    interaction_id: ask.call_id.clone(),
                    turn_id: state.turn_id.clone(),
                    mode: ask.mode,
                    questions: ask
                        .questions
                        .iter()
                        .map(|q| deepx_domain::AskQuestion {
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

    fn validate_answers(ask: &PendingAsk, answers: &[AskAnswer]) -> Result<Vec<AskAnswer>, String> {
        let mut supplied = HashMap::new();
        for answer in answers {
            if supplied
                .insert(answer.question_id.as_str(), answer.answer.as_str())
                .is_some()
            {
                return Err(format!("duplicate answer for {}", answer.question_id));
            }
        }

        let mut ordered = Vec::with_capacity(ask.questions.len());
        for question in &ask.questions {
            let answer = supplied
                .remove(question.id.as_str())
                .ok_or_else(|| format!("missing answer for {}", question.id))?;
            if answer.trim().is_empty() {
                return Err(format!("empty answer for {}", question.id));
            }
            if !question.options.iter().any(|option| option == answer) && !question.allow_custom {
                return Err(format!("invalid answer for {}", question.id));
            }
            ordered.push(AskAnswer {
                question_id: question.id.clone(),
                answer: answer.to_string(),
            });
        }
        if !supplied.is_empty() {
            return Err("response contains unknown question ids".into());
        }
        Ok(ordered)
    }

    // ── Internal lap execution ──

    fn abort_running_turn(
        ctx: &mut RingContext,
        turn_id: String,
        usage: Option<UsageInfo>,
    ) -> Outcome {
        ctx.agent.msg.remove_last_step_if_incomplete();
        ctx.agent
            .msg
            .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
        Outcome::TurnAborted {
            turn_id,
            usage,
            consume_queued_interrupt: true,
        }
    }

    fn seal_timeline_terminal_round(
        ctx: &mut RingContext,
        turn_id: &str,
        round_num: u32,
        active_stream_block: Option<&(deepx_domain::TimelineBlockKind, String)>,
        tool_ids: &HashSet<String>,
        state: deepx_domain::TimelineTurnState,
        failure: Option<deepx_domain::TimelineFailure>,
    ) {
        if let Some((_, block_id)) = active_stream_block {
            ctx.emitter
                .emit_timeline(deepx_domain::TimelineIntent::BlockSealed {
                    turn_id: turn_id.to_string(),
                    round_num,
                    block_id: block_id.clone(),
                });
        }
        for tool_id in tool_ids {
            ctx.emitter
                .emit_timeline(deepx_domain::TimelineIntent::BlockSealed {
                    turn_id: turn_id.to_string(),
                    round_num,
                    block_id: format!("tool:{tool_id}"),
                });
        }
        ctx.emitter
            .emit_timeline(deepx_domain::TimelineIntent::RoundSealed {
                turn_id: turn_id.to_string(),
                round_num,
                is_final: true,
            });
        ctx.emitter
            .emit_timeline(deepx_domain::TimelineIntent::TurnSealed {
                turn_id: turn_id.to_string(),
                state,
                failure,
            });
    }

    fn seal_active_stream_block(
        ctx: &mut RingContext,
        turn_id: &str,
        round_num: u32,
        active: &mut Option<(deepx_domain::TimelineBlockKind, String)>,
    ) {
        if let Some((_, block_id)) = active.take() {
            ctx.emitter
                .emit_timeline(deepx_domain::TimelineIntent::BlockSealed {
                    turn_id: turn_id.to_string(),
                    round_num,
                    block_id,
                });
        }
    }

    fn ensure_stream_block(
        ctx: &mut RingContext,
        turn_id: &str,
        round_num: u32,
        active: &mut Option<(deepx_domain::TimelineBlockKind, String)>,
        segment: &mut u32,
        kind: deepx_domain::TimelineBlockKind,
    ) -> String {
        if let Some((active_kind, block_id)) = active {
            if *active_kind == kind {
                return block_id.clone();
            }
        }
        Self::seal_active_stream_block(ctx, turn_id, round_num, active);
        let label = match kind {
            deepx_domain::TimelineBlockKind::Reasoning => "reasoning",
            deepx_domain::TimelineBlockKind::Text => "text",
            _ => "stream",
        };
        let block_id = format!("round-{round_num}:{label}:{}", *segment);
        *segment = (*segment).saturating_add(1);
        ctx.emitter
            .emit_timeline(deepx_domain::TimelineIntent::BlockOpened {
                turn_id: turn_id.to_string(),
                round_num,
                block_id: block_id.clone(),
                kind,
                tool: None,
            });
        *active = Some((kind, block_id.clone()));
        block_id
    }

    fn execute_admitted_batch(
        ctx: &mut RingContext,
        tool: &ToolEngine,
        mut admitted: Vec<AdmittedTool>,
        tool_call_order: &[String],
        serial_call_ids: &HashSet<String>,
        turn_id: &str,
        round_num: u32,
    ) -> bool {
        const MAX_PARALLEL_TOOL_WORKERS: usize = 4;
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
            let batch_len = parallel.len().min(MAX_PARALLEL_TOOL_WORKERS);
            let batch: Vec<_> = parallel.drain(..batch_len).collect();
            let (progress_tx, progress_rx) = deepx_workspace::bounded_exec_progress_channel();
            let mut handles = Vec::new();
            for admitted in batch {
                let tx = progress_tx.clone();
                let call_id = admitted.call_id.clone();
                let tool_name = admitted.auth.tool_name().to_string();
                let tool_args = admitted.auth.args().clone();
                ToolEngine::emit_timeline_tool_running(
                    ctx, turn_id, round_num, &call_id, &tool_name, &tool_args,
                );
                let handle = std::thread::Builder::new()
                    .stack_size(4 * 1024 * 1024)
                    .spawn({
                        let auth = admitted.auth;
                        let id = call_id.clone();
                        move || {
                            let result =
                                deepx_workspace::execution::execute_authorized(*auth, Some(tx));
                            (
                                id,
                                result.content,
                                result.success,
                                result.code_delta,
                                result.skill_effects,
                            )
                        }
                    })
                    .expect("tool thread spawn");
                handles.push((call_id, tool_name, handle));
            }
            drop(progress_tx);
            tool.drain_progress_external(ctx, progress_rx, turn_id, round_num);

            let cancelled = ctx.cancel.is_set();
            for (call_id, tool_name, handle) in handles {
                if cancelled {
                    let _ = handle.join();
                    continue;
                }
                match handle.join() {
                    Ok((_id, content, success, code_delta, skill_effects)) => {
                        ctx.agent
                            .msg
                            .push_tool_result_direct(&call_id, &content, success);
                        ordered_skill_effects.push((call_id.clone(), skill_effects));
                        if let Some(ref delta) = code_delta {
                            ctx.stats.push_delta(delta.clone());
                            // Ringing 双发：CodeChanged（与 engine_tool 同载荷）
                            ctx.emitter.emit_domain(deepx_domain::DomainEvent::Tool(
                                deepx_domain::ToolEvent::CodeChanged {
                                    lines_added: delta.lines_added,
                                    lines_removed: delta.lines_removed,
                                    files_created: delta.files_created,
                                    files_deleted: delta.files_deleted,
                                    file: delta.file.clone(),
                                },
                            ));
                        }
                        // Instant refresh for todo tools
        if matches!(tool_name.as_str(), "todo") {
                            // Ringing 双发：DashboardUpdated（replaceable 覆盖）
                            ctx.emitter.emit_domain(deepx_domain::DomainEvent::Control(
                                deepx_domain::ControlEvent::DashboardUpdated {
                                    hp_connected: true,
                                    session_seed: ctx.agent.session.seed.clone(),
                                    tool_calls_total: 0,
                                    tool_failures: 0,
                                    current_phase: "single".into(),
                                    streaming: false,
                                },
                            ));
                            ctx.emitter.emit_domain(deepx_domain::DomainEvent::Control(
                                deepx_domain::ControlEvent::DashboardSnapshot {
                                    snapshot: dashboard::build_snapshot(
                                        ctx.agent.session.seed.clone(),
                                    ),
                                },
                            ));
                        }
                    }
                    Err(_) => ctx.agent.msg.push_tool_result_direct(
                        &call_id,
                        "[ERROR] tool thread panicked",
                        false,
                    ),
                }
            }
        }

        for admitted in serial {
            if ctx.cancel.is_set() {
                return false;
            }
            let call_id = admitted.call_id;
            let tool_name = admitted.auth.tool_name().to_string();
            let tool_args = admitted.auth.args().clone();
            ToolEngine::emit_timeline_tool_running(
                ctx, turn_id, round_num, &call_id, &tool_name, &tool_args,
            );
            let (progress_tx, progress_rx) = deepx_workspace::bounded_exec_progress_channel();
            let handle = std::thread::Builder::new()
                .stack_size(4 * 1024 * 1024)
                .spawn(move || {
                    let result = deepx_workspace::execution::execute_authorized(
                        *admitted.auth,
                        Some(progress_tx),
                    );
                    (
                        result.content,
                        result.success,
                        result.code_delta,
                        result.skill_effects,
                    )
                })
                .expect("tool thread spawn");
            tool.drain_progress_external(ctx, progress_rx, turn_id, round_num);
            match handle.join() {
                Ok((content, success, code_delta, skill_effects)) => {
                    ctx.agent
                        .msg
                        .push_tool_result_direct(&call_id, &content, success);
                    ordered_skill_effects.push((call_id.clone(), skill_effects));
                    if let Some(ref delta) = code_delta {
                        ctx.stats.push_delta(delta.clone());
                        // Ringing 双发：CodeChanged（与 engine_tool 同载荷）
                        ctx.emitter.emit_domain(deepx_domain::DomainEvent::Tool(
                            deepx_domain::ToolEvent::CodeChanged {
                                lines_added: delta.lines_added,
                                lines_removed: delta.lines_removed,
                                files_created: delta.files_created,
                                files_deleted: delta.files_deleted,
                                file: delta.file.clone(),
                            },
                        ));
                    }
                    // Instant refresh for todo tools
        if matches!(tool_name.as_str(), "todo") {
                        // Ringing 双发：DashboardUpdated（replaceable 覆盖）
                        ctx.emitter.emit_domain(deepx_domain::DomainEvent::Control(
                            deepx_domain::ControlEvent::DashboardUpdated {
                                hp_connected: true,
                                session_seed: ctx.agent.session.seed.clone(),
                                tool_calls_total: 0,
                                tool_failures: 0,
                                current_phase: "single".into(),
                                streaming: false,
                            },
                        ));
                        ctx.emitter.emit_domain(deepx_domain::DomainEvent::Control(
                            deepx_domain::ControlEvent::DashboardSnapshot {
                                snapshot: dashboard::build_snapshot(ctx.agent.session.seed.clone()),
                            },
                        ));
                    }
                }
                Err(_) => ctx.agent.msg.push_tool_result_direct(
                    &call_id,
                    "[ERROR] tool thread panicked",
                    false,
                ),
            }
        }

        if ctx.cancel.is_set() {
            return false;
        }
        ordered_skill_effects.sort_by_key(|(call_id, _)| {
            tool_call_order
                .iter()
                .position(|id| id == call_id)
                .unwrap_or(usize::MAX)
        });
        for (_, effects) in ordered_skill_effects {
            ctx.agent.apply_tool_effects(effects);
        }
        true
    }

    /// Run compact inline during a gate lap boundary.
    /// Uses CompactEngine to build prompt, calls LLM inline (blocking),
    /// applies result, and streams CompactDelta events to the frontend.
    /// After compact, the current turn continues normally.
    fn run_auto_compact(ctx: &mut RingContext) -> bool {
        let compact_eng = super::engine_compact::CompactEngine::new();
        let (prompt, kept, head, provider, compact_id) =
            match compact_eng.build_prompt_and_meta(ctx) {
                Some(v) => v,
                None => return false,
            };
        let turns_removed = ctx.agent.msg.turns().len().saturating_sub(kept);

        let emitter = ctx.emitter;
        let mut summary = String::new();
        let mut on_event = |ev: deepx_gate::StreamEvent| match ev {
            deepx_gate::StreamEvent::ContentDelta(d) => {
                summary.push_str(&d);
                // Ringing 双发：CompactProgress（replaceable 流式摘要）
                emitter.emit_domain(deepx_domain::DomainEvent::Conversation(
                    deepx_domain::ConversationEvent::CompactProgress {
                        compact_id: compact_id.clone(),
                        delta: d,
                    },
                ));
            }
            deepx_gate::StreamEvent::ReasoningDelta(d) => {
                // legacy CompactDelta reasoning 透传已退役（§4.2 登记：由 CompactProgress 覆盖）
                let _ = d;
            }
            _ => {}
        };

        let msgs = vec![deepx_types::Message::user(&prompt)];
        let result = deepx_gate::chat_stream(
            &provider,
            msgs,
            None,
            20480,
            None,
            None,
            None,
            &mut on_event,
        );

        match result {
            Ok(()) if !summary.trim().is_empty() => {
                let before = {
                    let (c, t, tc, tr, ts, sp, _, _) = ctx.agent.msg.compute_context_stats(None);
                    c + t + tc + tr + ts + sp
                };
                ctx.agent.msg.apply_compact(&summary, kept);
                ctx.agent
                    .msg
                    .snapshot_full(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
                let after = {
                    let (c, t, tc, tr, ts, sp, _, _) = ctx.agent.msg.compute_context_stats(None);
                    c + t + tc + tr + ts + sp
                };
                // Ringing 双发：CompactFinished（成功终态）
                ctx.emitter
                    .emit_domain(deepx_domain::DomainEvent::Conversation(
                        deepx_domain::ConversationEvent::CompactFinished {
                            compact_id,
                            status: deepx_domain::CompactStatus::Completed,
                            summary_chars: Some(summary.chars().count()),
                            turns_compacted: Some(head as u32),
                            turns_removed: Some(turns_removed as u32),
                        },
                    ));
                log::info!("[TURN] auto-compact done: {before} → {after} tokens");
                true
            }
            Ok(()) => {
                // Ringing 双发：CompactFinished（空摘要 → 失败终态）
                ctx.emitter
                    .emit_domain(deepx_domain::DomainEvent::Conversation(
                        deepx_domain::ConversationEvent::CompactFinished {
                            compact_id,
                            status: deepx_domain::CompactStatus::Failed,
                            summary_chars: Some(0),
                            turns_compacted: Some(0),
                            turns_removed: Some(0),
                        },
                    ));
                false
            }
            Err(e) => {
                // Ringing 双发：CompactFinished（失败终态）
                ctx.emitter
                    .emit_domain(deepx_domain::DomainEvent::Conversation(
                        deepx_domain::ConversationEvent::CompactFinished {
                            compact_id,
                            status: deepx_domain::CompactStatus::Failed,
                            summary_chars: Some(0),
                            turns_compacted: Some(0),
                            turns_removed: Some(0),
                        },
                    ));
                log::error!("[TURN] auto-compact failed: {e}");
                false
            }
        }
    }

    fn run_lap(
        &mut self,
        ctx: &mut RingContext,
        tool: &mut ToolEngine,
        turn_id: String,
        round_num: u32,
        mut last_usage: Option<UsageInfo>,
    ) -> Outcome {
        log::info!("[TURN] run_lap turn_id={} round_num={}", turn_id, round_num);
        // Rebuild provider from current config
        let ep = deepx_config::registry::find_endpoint(
            &ctx.agent.config.provider_id,
            &ctx.agent.config.endpoint,
        );
        let is_responses = ep.as_ref().map(|e| e.protocol.as_str()) == Some("responses");
        let provider = if is_responses {
            let mut p = deepx_gate::ProviderConfig::responses(
                &ctx.agent.config.base_url,
                &ctx.agent.config.api_key,
                &ctx.agent.config.model,
                ep.as_ref().and_then(|e| e.responses_path.clone()),
            );
            if let Some(endpoint) = ep.as_ref() {
                p.responses_compat = deepx_gate::ResponsesCompat {
                    web_search: endpoint.responses_web_search,
                    echo_web_search_call: endpoint.responses_echo_web_search_call,
                    send_include: endpoint.responses_send_include,
                    effort_max: endpoint.responses_effort_max.clone(),
                    supports_user: endpoint.responses_supports_user,
                    search_function_alias: endpoint.responses_search_function_alias.clone(),
                    echo_reasoning_content: endpoint.responses_echo_reasoning_content,
                };
            }
            p
        } else {
            let mut p = deepx_gate::ProviderConfig::openai(
                &ctx.agent.config.base_url,
                &ctx.agent.config.api_key,
                &ctx.agent.config.model,
                ep.as_ref().and_then(|e| e.user_id_mode.clone()),
                ep.as_ref().and_then(|e| e.chat_path.clone()),
                ep.as_ref()
                    .map(|e| e.thinking_mode.clone())
                    .unwrap_or_default(),
                ep.as_ref()
                    .map(|e| e.cache_field.clone())
                    .unwrap_or_default(),
                ep.as_ref().map(|e| e.supports_thinking).unwrap_or(true),
                ep.as_ref().and_then(|e| e.do_sample),
            )
            .with_stateful(ep.as_ref().map(|e| e.stateful).unwrap_or(false))
            .with_stream_usage(ep.as_ref().map(|e| e.include_stream_usage).unwrap_or(false));
            if let Some(endpoint) = ep.as_ref() {
                p.supports_reasoning_effort = endpoint.supports_reasoning_effort;
                p.tool_call_content_null = endpoint.tool_call_content_null;
                p.supports_reasoning_content = endpoint.supports_reasoning_content;
                p.require_provider_parameters = endpoint.require_provider_parameters;
            }
            p
        };

        loop {
            // ── Interrupt check ──
            if ctx.cancel.is_set()
                || deepx_workspace::CANCEL.load(std::sync::atomic::Ordering::SeqCst)
            {
                ctx.emitter
                    .emit_timeline(deepx_domain::TimelineIntent::TurnSealed {
                        turn_id: turn_id.clone(),
                        state: deepx_domain::TimelineTurnState::Cancelled,
                        failure: None,
                    });
                return Self::abort_running_turn(ctx, turn_id, last_usage);
            }
            if !ctx.pending.is_empty() {
                ctx.agent.msg.remove_last_step_if_incomplete();
                ctx.agent
                    .msg
                    .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
                return Outcome::Handled;
            }

            // ── Build and measure the exact request snapshot for this gate lap ──
            // Auto-compaction has one authoritative decision source: a local
            // preflight estimate of the messages + tool schema that are about to
            // be sent. Provider usage is terminal telemetry and only calibrates
            // future estimates; it never directly controls compaction.
            let mut messages = ctx.agent.build_context();
            let mut request_estimate = ctx
                .agent
                .estimate_prepared_request(&messages, Some(&ctx.agent.tool_defs));
            let threshold = ctx.agent.config.auto_compact_threshold;
            if threshold > 0.0 {
                let limit = ctx.agent.config.context_limit as u64;
                if request_estimate.upper_bound_tokens as f64 > limit as f64 * threshold {
                    log::info!(
                        "[TURN] auto-compact preflight: raw={}, predicted={}, upper={}/{limit} tokens ({} samples, {:.0}% threshold)",
                        request_estimate.raw_tokens,
                        request_estimate.predicted_tokens,
                        request_estimate.upper_bound_tokens,
                        request_estimate.sample_count,
                        threshold * 100.0
                    );
                    if Self::run_auto_compact(ctx) {
                        messages = ctx.agent.build_context();
                        request_estimate = ctx
                            .agent
                            .estimate_prepared_request(&messages, Some(&ctx.agent.tool_defs));
                        if request_estimate.upper_bound_tokens as f64 > limit as f64 * threshold {
                            // One compact attempt per gate boundary. Remaining
                            // pressure may be non-compactable system/tool overhead.
                            log::warn!(
                                "[TURN] post-compact preflight remains above threshold: upper={}/{limit}",
                                request_estimate.upper_bound_tokens
                            );
                        }
                    }
                }
            }

            // ── Emit pending cache diagnostic ──
            if let Some((hash, reasons)) = ctx.agent.take_cache_diagnostics() {
                // legacy CacheDiagnostics 退役：Ringing 无对应事件（§4.2 登记：诊断事件退役）
                let _ = (hash, reasons);
            }

            let tools = Some(ctx.agent.tool_defs.clone());
            let mut content = String::new();
            let mut reasoning = String::new();
            // A1：block_checkpoint 节流（delta 次数 + 时间窗，先到为准）。
            let mut checkpoint_tokens: u32 = 0;
            let mut last_checkpoint_at = std::time::Instant::now();
            // A3：usage 节流状态（最后发射时刻 + 已发 total，终值补发依据）。
            let mut last_usage_emit_at: Option<std::time::Instant> = None;
            let mut last_emitted_usage_total: u32 = 0;
            let mut tool_calls_raw = serde_json::Value::Null;
            let mut active_stream_block: Option<(deepx_domain::TimelineBlockKind, String)> = None;
            let mut timeline_segment = 0u32;
            let mut timeline_tools_open = HashSet::new();
            let mut had_error = false;
            let mut gate_error = None;
            let mut current_request_usage: Option<UsageInfo> = None;
            let request_fingerprint = ctx.agent.token_calibration_fingerprint();

            *ctx.phase = LoopPhase::GateRunning;
            let cancel_arc = ctx.cancel.arc();

            // ── SSE Gate Request ──
            log::info!(
                "[TURN] run_lap turn_id={} round_num={} calling chat_stream",
                turn_id,
                round_num
            );
            let result = deepx_gate::chat_stream(
                &provider,
                messages,
                tools,
                ctx.agent.config.max_tokens,
                Some(ctx.agent.config.reasoning_effort.clone()),
                Some(ctx.agent.session.seed.clone()),
                Some(&cancel_arc),
                &mut |event| match event {
                    deepx_gate::StreamEvent::ContentDelta(d) => {
                        if ctx.cancel.is_set() {
                            return;
                        }
                        content.push_str(&d);
                        let block_id = Self::ensure_stream_block(
                            ctx,
                            &turn_id,
                            round_num,
                            &mut active_stream_block,
                            &mut timeline_segment,
                            deepx_domain::TimelineBlockKind::Text,
                        );
                        ctx.emitter
                            .emit_timeline(deepx_domain::TimelineIntent::TextDelta {
                                turn_id: turn_id.clone(),
                                round_num,
                                block_id,
                                delta: d.clone(),
                            });
                        ctx.emitter
                            .emit_domain(deepx_domain::DomainEvent::Conversation(
                                deepx_domain::ConversationEvent::RoundDelta {
                                    turn_id: turn_id.clone(),
                                    round_num,
                                    kind: deepx_domain::RoundDeltaKind::Answering,
                                    delta: d,
                                },
                            ));
                        // A1：周期完整值（replaceable 覆盖，乱序/丢 delta 自愈）。
                        Self::maybe_emit_block_checkpoint(
                            ctx,
                            &turn_id,
                            round_num,
                            deepx_domain::RoundDeltaKind::Answering,
                            &content,
                            &mut checkpoint_tokens,
                            &mut last_checkpoint_at,
                        );
                    }
                    deepx_gate::StreamEvent::ReasoningDelta(r) => {
                        if ctx.cancel.is_set() {
                            return;
                        }
                        reasoning.push_str(&r);
                        let block_id = Self::ensure_stream_block(
                            ctx,
                            &turn_id,
                            round_num,
                            &mut active_stream_block,
                            &mut timeline_segment,
                            deepx_domain::TimelineBlockKind::Reasoning,
                        );
                        ctx.emitter
                            .emit_timeline(deepx_domain::TimelineIntent::TextDelta {
                                turn_id: turn_id.clone(),
                                round_num,
                                block_id,
                                delta: r.clone(),
                            });
                        ctx.emitter
                            .emit_domain(deepx_domain::DomainEvent::Conversation(
                                deepx_domain::ConversationEvent::RoundDelta {
                                    turn_id: turn_id.clone(),
                                    round_num,
                                    kind: deepx_domain::RoundDeltaKind::Thinking,
                                    delta: r,
                                },
                            ));
                        // A1：周期完整值（replaceable 覆盖，乱序/丢 delta 自愈）。
                        Self::maybe_emit_block_checkpoint(
                            ctx,
                            &turn_id,
                            round_num,
                            deepx_domain::RoundDeltaKind::Thinking,
                            &reasoning,
                            &mut checkpoint_tokens,
                            &mut last_checkpoint_at,
                        );
                    }
                    deepx_gate::StreamEvent::Done {
                        raw_message, usage, ..
                    } => {
                        if let Some(ref u) = usage {
                            ctx.agent.session.record_usage(u);
                            if !ctx.agent.ephemeral {
                                deepx_session::SessionManager::global().persist_usage(
                                    &ctx.agent.session.seed,
                                    ctx.agent.session.usage_totals.clone(),
                                    ctx.agent.session.last_usage.clone(),
                                    ctx.agent.session.usage_requests,
                                    ctx.agent.session.cache_reported_requests,
                                );
                            }
                            util::record_token_usage(u, &ctx.agent.config.model);
                            last_usage = usage.clone();
                            current_request_usage = usage.clone();
                        }
                        // A3：终值必发——节流窗口可能吞掉最后一条流式值，此处补发
                        // 请求权威终值（replaceable 覆盖；与 done 前的 record_usage 一致）。
                        if let Some(final_usage) = current_request_usage.clone() {
                            if final_usage.total_tokens != last_emitted_usage_total {
                                last_emitted_usage_total = final_usage.total_tokens;
                                ctx.emitter
                                    .emit_domain(deepx_domain::DomainEvent::Conversation(
                                        deepx_domain::ConversationEvent::UsageUpdated {
                                            turn_id: turn_id.clone(),
                                            round_num,
                                            usage: final_usage.clone(),
                                            context_limit: ctx.agent.config.context_limit,
                                            model: ctx.agent.config.model.clone(),
                                        },
                                    ));
                            }
                        }
                        content.clear();
                        reasoning.clear();
                        let mut blocks: Vec<serde_json::Value> = Vec::new();
                        for block in &raw_message.content {
                            match block {
                                deepx_types::ContentBlock::Text { text } => content.push_str(text),
                                deepx_types::ContentBlock::Reasoning { reasoning: r } => {
                                    reasoning.push_str(r)
                                }
                                deepx_types::ContentBlock::ToolUse { id, name, input } => {
                                    blocks.push(serde_json::json!({
                                        "id": id, "name": name, "arguments": input.to_string(),
                                    }));
                                }
                                _ => {}
                            }
                        }
                        if !blocks.is_empty() {
                            tool_calls_raw = serde_json::Value::Array(blocks);
                        }
                    }
                    deepx_gate::StreamEvent::ToolCallProgress {
                        index,
                        id,
                        name,
                        args_so_far,
                    } => {
                        log::info!(
                            "[TURN] emit_delta ToolCallPreview turn_id={turn_id} round={round_num} idx={index} id={id} name={name}"
                        );
                        let block_id = format!("tool:{id}");
                        Self::seal_active_stream_block(
                            ctx,
                            &turn_id,
                            round_num,
                            &mut active_stream_block,
                        );
                        if timeline_tools_open.insert(id.clone()) {
                            ctx.emitter
                                .emit_timeline(deepx_domain::TimelineIntent::BlockOpened {
                                    turn_id: turn_id.clone(),
                                    round_num,
                                    block_id: block_id.clone(),
                                    kind: deepx_domain::TimelineBlockKind::Tool,
                                    tool: Some(deepx_domain::TimelineTool {
                                        tool_call_id: id.clone(),
                                        name: name.clone(),
                                        state: deepx_domain::TimelineToolState::Prepared,
                                        summary: None,
                                        args_json: Some(args_so_far.clone()),
                                        output: None,
                                        progress: String::new(),
                                        failure: None,
                                        permission: None,
                                    }),
                                });
                        }
                        // Ringing 双发：ToolCallPrepared（replaceable 预览，可被 ToolStarted 覆盖）
                        ctx.emitter.emit_domain(deepx_domain::DomainEvent::Tool(
                            deepx_domain::ToolEvent::ToolCallPrepared {
                                tool_call_id: id.clone(),
                                turn_id: turn_id.clone(),
                                round_num,
                                name: name.clone(),
                                args_so_far: args_so_far.clone(),
                            },
                        ));
                    }
                    deepx_gate::StreamEvent::WebSearchStatus(status) => {
                        // Server-side search progress (Responses API built-in
                        // tool). Transient status line; RoundComplete blocks
                        // replace it on arrival.
                        // Ringing 双发：ProviderToolStatus（replaceable，按 call_id 合并）
                        let provider_state = match status.as_str() {
                            "completed" | "done" => deepx_domain::ProviderToolState::Completed,
                            "searching" | "running" | "in_progress" => {
                                deepx_domain::ProviderToolState::Searching
                            }
                            _ => deepx_domain::ProviderToolState::InProgress,
                        };
                        ctx.emitter
                            .emit_domain(deepx_domain::DomainEvent::Conversation(
                                deepx_domain::ConversationEvent::ProviderToolStatus {
                                    turn_id: turn_id.clone(),
                                    round_num,
                                    call_id: format!("ws-{turn_id}-{round_num}"),
                                    tool_kind: "web_search".into(),
                                    state: provider_state,
                                },
                            ));
                    }
                    deepx_gate::StreamEvent::UsageUpdate(u) => {
                        last_usage = Some(u.clone());
                        current_request_usage = Some(u.clone());
                        ctx.agent.session.tokens =
                            ctx.agent.session.tokens.max(u.total_tokens as u64);
                        // A3：节流 ~1s（replaceable 覆盖显示）；终值由 Done 分支补发。
                        let due = last_usage_emit_at
                            .map_or(true, |at| at.elapsed() >= USAGE_EMIT_INTERVAL);
                        if due {
                            last_usage_emit_at = Some(std::time::Instant::now());
                            last_emitted_usage_total = u.total_tokens;
                            ctx.emitter
                                .emit_domain(deepx_domain::DomainEvent::Conversation(
                                    deepx_domain::ConversationEvent::UsageUpdated {
                                        turn_id: turn_id.clone(),
                                        round_num,
                                        usage: u.clone(),
                                        context_limit: ctx.agent.config.context_limit,
                                        model: ctx.agent.config.model.clone(),
                                    },
                                ));
                        }
                    }
                    deepx_gate::StreamEvent::Retrying {
                        attempt,
                        max_retries,
                        delay_secs,
                        error,
                    } => {
                        // Ringing 双发：ProviderRetrying（重试可见性）
                        ctx.emitter
                            .emit_domain(deepx_domain::DomainEvent::Conversation(
                                deepx_domain::ConversationEvent::ProviderRetrying {
                                    turn_id: turn_id.clone(),
                                    round_num,
                                    attempt,
                                    max_retries,
                                    delay_secs,
                                    error_message: error,
                                },
                            ));
                    }
                    deepx_gate::StreamEvent::Error(msg) => {
                        log::error!(
                            "[TURN] gate error turn_id={turn_id} round_num={round_num}: {msg}"
                        );
                        gate_error = Some(msg);
                        had_error = true;
                    }
                },
            );

            if ctx.cancel.is_set() {
                Self::seal_timeline_terminal_round(
                    ctx,
                    &turn_id,
                    round_num,
                    active_stream_block.as_ref(),
                    &timeline_tools_open,
                    deepx_domain::TimelineTurnState::Cancelled,
                    None,
                );
                return Self::abort_running_turn(ctx, turn_id, last_usage);
            }

            if had_error || result.is_err() {
                log::info!(
                    "[TURN] run_lap turn_id={} round_num={} gate error or had_error={}",
                    turn_id,
                    round_num,
                    had_error
                );
                ctx.agent
                    .msg
                    .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
                let message = gate_error
                    .or_else(|| result.err().map(|error| error.to_string()))
                    .unwrap_or_else(|| "Model request failed".into());
                Self::seal_timeline_terminal_round(
                    ctx,
                    &turn_id,
                    round_num,
                    active_stream_block.as_ref(),
                    &timeline_tools_open,
                    deepx_domain::TimelineTurnState::Failed,
                    Some(deepx_domain::TimelineFailure {
                        code: "model_request_failed".into(),
                        message: message.clone(),
                    }),
                );
                return Outcome::TurnFailed {
                    turn_id,
                    usage: last_usage,
                    message,
                };
            }

            if let Some(usage) = current_request_usage {
                let accepted = ctx.agent.observe_prepared_request(
                    &request_fingerprint,
                    request_estimate.raw_tokens,
                    u64::from(usage.prompt_tokens),
                );
                log::debug!(
                    "[TURN] token calibration sample: raw={}, observed={}, accepted={accepted}",
                    request_estimate.raw_tokens,
                    usage.prompt_tokens
                );
            }

            log::info!(
                "[TURN] run_lap turn_id={} round_num={} gate succeeded, parsing response",
                turn_id,
                round_num
            );

            // ── Parse + push assistant message ──
            let parsed = util::parse_tool_calls_from_response(
                &content,
                &reasoning,
                &tool_calls_raw,
                &ctx.agent,
            );
            // Structured/non-streamed tool calls can arrive without a prior
            // ToolCallProgress event. Open their native blocks here so every
            // later lifecycle patch has one stable target.
            for tool_call in &parsed {
                if timeline_tools_open.insert(tool_call.id.clone()) {
                    let args_json = tool_call.function.arguments.clone();
                    ctx.emitter
                        .emit_timeline(deepx_domain::TimelineIntent::BlockOpened {
                            turn_id: turn_id.clone(),
                            round_num,
                            block_id: format!("tool:{}", tool_call.id),
                            kind: deepx_domain::TimelineBlockKind::Tool,
                            tool: Some(deepx_domain::TimelineTool {
                                tool_call_id: tool_call.id.clone(),
                                name: tool_call.function.name.clone(),
                                state: deepx_domain::TimelineToolState::Prepared,
                                summary: None,
                                args_json: Some(args_json),
                                output: None,
                                progress: String::new(),
                                failure: None,
                                permission: None,
                            }),
                        });
                }
            }
            let assistant_msg = util::build_assistant_message(&content, &reasoning, &parsed);
            let effect = ctx.agent.msg.push_assistant(assistant_msg.clone());
            ctx.agent
                .msg
                .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);

            util::emit_round_complete_via_emitter(
                ctx.emitter,
                &turn_id,
                round_num,
                &assistant_msg,
                &content,
                &reasoning,
                &parsed,
            );

            // Native transcript sealing is independent from the legacy
            // RoundComplete projection. A Markdown consumer only sees these
            // blocks as final after the explicit seal.
            Self::seal_active_stream_block(ctx, &turn_id, round_num, &mut active_stream_block);
            if parsed.is_empty() {
                ctx.emitter
                    .emit_timeline(deepx_domain::TimelineIntent::RoundSealed {
                        turn_id: turn_id.clone(),
                        round_num,
                        is_final: true,
                    });
            }

            match effect {
                Effect::None => {
                    // ── Execute tools ──
                    *ctx.phase = LoopPhase::ToolsRunning;

                    let mut pending = ctx.agent.msg.get_last_step_pending();
                    if !pending.is_empty() {
                        // Duplicate tool-call ID check
                        let mut seen = HashSet::new();
                        if pending.iter().any(|t| !seen.insert(t.id.clone())) {
                            ctx.agent.msg.remove_last_step_if_incomplete();
                            // Ringing 双发：OperationFailed（结构化错误）
                            ctx.emitter.emit_domain(deepx_domain::DomainEvent::Control(
                                deepx_domain::ControlEvent::OperationFailed {
                                    occurrence_id: Self::occurrence_id(),
                                    scope: deepx_domain::ErrorScope::Tool,
                                    error: Self::domain_failure(
                                        "duplicate_tool_call",
                                        "Duplicate tool-call ID from model".into(),
                                        Some("duplicate_tool_call"),
                                    ),
                                    operation_id: None,
                                },
                            ));
                            return Outcome::Handled;
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
                        const MAX_PARALLEL_TOOL_WORKERS: usize = 4;
                        let (_serial_groups, serial_after) =
                            conflict::resolve_write_conflicts(&pending);
                        let serial_call_ids: HashSet<String> = serial_after
                            .iter()
                            .map(|index| pending[*index].id.clone())
                            .collect();
                        let admission = tool.admit_batch(ctx, &pending, &turn_id, round_num);
                        if !admission.pending_permission_ids.is_empty()
                            || !admission.pending_plans.is_empty()
                            || admission.pending_todo_activation.is_some()
                        {
                            let reason = if !admission.pending_permission_ids.is_empty() {
                                YieldReason::PermissionPending
                            } else if admission.pending_todo_activation.is_some() {
                                YieldReason::PlanReview // same UI interaction, review_type distinguishes
                            } else {
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
                            self.suspended = Some(TurnState {
                                session_id: ctx.agent.session.seed.clone(),
                                turn_id: turn_id.clone(),
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
                            if let Some((call_id, plan_content, review_type, todo_items)) =
                                plan_submitted
                            {
                                // Ringing 双发：PlanReviewRequested（plan 评审请求）
                                ctx.emitter.emit_domain(deepx_domain::DomainEvent::Control(
                                    deepx_domain::ControlEvent::PlanReviewRequested {
                                        interaction_id: call_id.clone(),
                                        turn_id: turn_id.clone(),
                                        plan_content,
                                        review_type,
                                        todo_items: todo_items.map(|items| {
                                            items
                                                .into_iter()
                                                .map(|t| deepx_domain::TodoItem {
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
                            return Outcome::YieldToUser { turn_id, reason };
                        }
                        let mut ordered_skill_effects = Vec::new();
                        let (mut parallel_authorized, serial_authorized): (Vec<_>, Vec<_>) =
                            admission
                                .authorized
                                .into_iter()
                                .partition(|admitted| !serial_call_ids.contains(&admitted.call_id));

                        // Execute independent tools in bounded parallel batches.
                        while !parallel_authorized.is_empty() {
                            let batch_len =
                                parallel_authorized.len().min(MAX_PARALLEL_TOOL_WORKERS);
                            let batch: Vec<_> = parallel_authorized.drain(..batch_len).collect();
                            let (progress_tx, progress_rx) =
                                deepx_workspace::bounded_exec_progress_channel();
                            let mut handles: Vec<(String, std::thread::JoinHandle<_>)> = Vec::new();

                            for admitted in batch {
                                let tx = progress_tx.clone();
                                let call_id = admitted.call_id.clone();
                                let tool_name = admitted.auth.tool_name().to_string();
                                let tool_args = admitted.auth.args().clone();
                                ToolEngine::emit_timeline_tool_running(
                                    ctx, &turn_id, round_num, &call_id, &tool_name, &tool_args,
                                );
                                let handle = std::thread::Builder::new()
                                    .stack_size(4 * 1024 * 1024)
                                    .spawn({
                                        let auth = admitted.auth;
                                        let cid = call_id.clone();
                                        move || {
                                            let result =
                                                deepx_workspace::execution::execute_authorized(
                                                    *auth,
                                                    Some(tx),
                                                );
                                            (
                                                cid,
                                                result.content,
                                                result.success,
                                                result.code_delta,
                                                result.skill_effects,
                                            )
                                        }
                                    })
                                    .expect("tool thread spawn");
                                handles.push((call_id, handle));
                            }
                            drop(progress_tx);

                            // Drain progress
                            tool.drain_progress_external(ctx, progress_rx, &turn_id, round_num);

                            // Collect results
                            let cancelled = ctx.cancel.is_set();
                            for (call_id, h) in handles {
                                if cancelled {
                                    let _ = h.join(); // reap
                                } else {
                                    match h.join() {
                                        Ok((_cid, content, success, code_delta, skill_effects)) => {
                                            ctx.agent.msg.push_tool_result_direct(
                                                &call_id, &content, success,
                                            );
                                            ordered_skill_effects
                                                .push((call_id.clone(), skill_effects));
                                            if let Some(ref delta) = code_delta {
                                                ctx.stats.push_delta(delta.clone());
                                                // Ringing 双发：CodeChanged（与 engine_tool 同载荷）
                                                ctx.emitter.emit_domain(
                                                    deepx_domain::DomainEvent::Tool(
                                                        deepx_domain::ToolEvent::CodeChanged {
                                                            lines_added: delta.lines_added,
                                                            lines_removed: delta.lines_removed,
                                                            files_created: delta.files_created,
                                                            files_deleted: delta.files_deleted,
                                                            file: delta.file.clone(),
                                                        },
                                                    ),
                                                );
                                            }
                                        }
                                        Err(_) => {
                                            ctx.agent.msg.push_tool_result_direct(
                                                &call_id,
                                                "[ERROR] tool thread panicked",
                                                false,
                                            );
                                        }
                                    }
                                }
                            }
                        }

                        // Execute later same-file writers exactly once, after the
                        // first writer from their conflict group has completed.
                        for admitted in serial_authorized {
                            if ctx.cancel.is_set() {
                                break;
                            }
                            let call_id = admitted.call_id;
                            let tool_name = admitted.auth.tool_name().to_string();
                            let tool_args = admitted.auth.args().clone();
                            ToolEngine::emit_timeline_tool_running(
                                ctx, &turn_id, round_num, &call_id, &tool_name, &tool_args,
                            );
                            let (progress_tx, progress_rx) =
                                deepx_workspace::bounded_exec_progress_channel();
                            let handle = std::thread::Builder::new()
                                .stack_size(4 * 1024 * 1024)
                                .spawn({
                                    let auth = admitted.auth;
                                    move || {
                                        let result = deepx_workspace::execution::execute_authorized(
                                            *auth,
                                            Some(progress_tx),
                                        );
                                        (
                                            result.content,
                                            result.success,
                                            result.code_delta,
                                            result.skill_effects,
                                        )
                                    }
                                })
                                .expect("tool thread spawn");
                            tool.drain_progress_external(ctx, progress_rx, &turn_id, round_num);
                            match handle.join() {
                                Ok((content, success, code_delta, skill_effects)) => {
                                    ctx.agent
                                        .msg
                                        .push_tool_result_direct(&call_id, &content, success);
                                    ordered_skill_effects.push((call_id.clone(), skill_effects));
                                    if let Some(ref delta) = code_delta {
                                        ctx.stats.push_delta(delta.clone());
                                        // Ringing 双发：CodeChanged（与 engine_tool 同载荷）
                                        ctx.emitter.emit_domain(
                                            deepx_domain::DomainEvent::Tool(
                                                deepx_domain::ToolEvent::CodeChanged {
                                                    lines_added: delta.lines_added,
                                                    lines_removed: delta.lines_removed,
                                                    files_created: delta.files_created,
                                                    files_deleted: delta.files_deleted,
                                                    file: delta.file.clone(),
                                                },
                                            ),
                                        );
                                    }
                                }
                                Err(_) => ctx.agent.msg.push_tool_result_direct(
                                    &call_id,
                                    "[ERROR] tool thread panicked",
                                    false,
                                ),
                            }
                        }

                        if ctx.cancel.is_set() {
                            Self::seal_timeline_terminal_round(
                                ctx,
                                &turn_id,
                                round_num,
                                active_stream_block.as_ref(),
                                &timeline_tools_open,
                                deepx_domain::TimelineTurnState::Cancelled,
                                None,
                            );
                            return Self::abort_running_turn(ctx, turn_id, last_usage);
                        }
                        ordered_skill_effects.sort_by_key(|(call_id, _)| {
                            tool_call_order
                                .iter()
                                .position(|id| id == call_id)
                                .unwrap_or(usize::MAX)
                        });
                        for (_, effects) in ordered_skill_effects {
                            ctx.agent.apply_tool_effects(effects);
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
                            } else if admission.pending_todo_activation.is_some() {
                                YieldReason::PlanReview
                            } else if !admission.pending_plans.is_empty() {
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
                            self.suspended = Some(TurnState {
                                session_id: ctx.agent.session.seed.clone(),
                                turn_id: turn_id.clone(),
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
                                Self::emit_active_ask(
                                    ctx,
                                    self.suspended.as_ref().expect("suspended ask state"),
                                );
                            }
                            if let Some((call_id, plan_content, review_type, todo_items)) =
                                plan_submitted
                            {
                                // Ringing 双发：PlanReviewRequested（plan 评审请求）
                                ctx.emitter.emit_domain(deepx_domain::DomainEvent::Control(
                                    deepx_domain::ControlEvent::PlanReviewRequested {
                                        interaction_id: call_id.clone(),
                                        turn_id: turn_id.clone(),
                                        plan_content,
                                        review_type,
                                        todo_items: todo_items.map(|items| {
                                            items
                                                .into_iter()
                                                .map(|t| deepx_domain::TodoItem {
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
                            return Outcome::YieldToUser { turn_id, reason };
                        }
                    }

                    // All tools from this round are now resolved.
                    self.emit_completed_tool_round(ctx, &turn_id, round_num);

                    if let Err(error) = ctx.agent.skills.complete_model_lap() {
                        // Ringing 双发：OperationFailed（skill lap 失败）
                        ctx.emitter.emit_domain(deepx_domain::DomainEvent::Control(
                            deepx_domain::ControlEvent::OperationFailed {
                                occurrence_id: Self::occurrence_id(),
                                scope: deepx_domain::ErrorScope::Tool,
                                error: Self::domain_failure("skill_lap", error, Some("skill_lap")),
                                operation_id: None,
                            },
                        ));
                    }

                    // Another lap: tools executed, back to Gate
                    return Outcome::ContinueTurn {
                        turn_id,
                        round_num: round_num + 1,
                        usage: last_usage,
                    };
                }
                Effect::TurnComplete => {}
                _ => {}
            }

            ctx.agent
                .msg
                .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
            let forced = match ctx.agent.skills.complete_model_lap() {
                Ok(forced) => forced,
                Err(error) => {
                    // Ringing 双发：OperationFailed（skill lap 失败）
                    ctx.emitter.emit_domain(deepx_domain::DomainEvent::Control(
                        deepx_domain::ControlEvent::OperationFailed {
                            occurrence_id: Self::occurrence_id(),
                            scope: deepx_domain::ErrorScope::Tool,
                            error: Self::domain_failure("skill_lap", error, Some("skill_lap")),
                            operation_id: None,
                        },
                    ));
                    Vec::new()
                }
            };
            if ctx.agent.skills.has_requested() || !forced.is_empty() {
                return Outcome::ContinueTurn {
                    turn_id,
                    round_num: round_num + 1,
                    usage: last_usage,
                };
            }
            ctx.emitter
                .emit_timeline(deepx_domain::TimelineIntent::TurnSealed {
                    turn_id: turn_id.clone(),
                    state: deepx_domain::TimelineTurnState::Completed,
                    failure: None,
                });
            return Outcome::TurnComplete {
                turn_id,
                usage: last_usage,
            };
        }
    }

    fn emit_completed_tool_round(
        &self,
        ctx: &mut RingContext,
        turn_id: &str,
        round_num: u32,
    ) -> Vec<(String, String, String, bool)> {
        let results = ctx.agent.msg.last_step_tool_results();
        let ts = util::chrono_local_datetime();
        for (tc_id, name, content, success) in &results {
            let args = ctx
                .agent
                .msg
                .tool_call_args(tc_id)
                .map(|a| a.to_string())
                .unwrap_or_default();
            let summary: String = content
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(120)
                .collect();
            ToolEngine::emit_timeline_tool_result(
                ctx, turn_id, round_num, tc_id, name, &args, content, *success,
            );
            // Ringing 双发：AuditRecorded（args 只进 content store，事件仅携带引用）
            ctx.emitter.emit_domain(deepx_domain::DomainEvent::Tool(
                deepx_domain::ToolEvent::AuditRecorded {
                    tool_name: name.clone(),
                    result_summary: summary.clone(),
                    success: *success,
                    time: ts.clone(),
                    args_ref: None,
                },
            ));
            // Ringing 终态：ToolFinished（legacy 汇总 ToolResults 退役后的替代——
            // 批量执行路径每个工具单独发终态，与 UI 主动调用路径一致）。
            ctx.emitter.emit_domain(deepx_domain::DomainEvent::Tool(
                deepx_domain::ToolEvent::ToolFinished {
                    tool_call_id: tc_id.clone(),
                    turn_id: turn_id.to_string(),
                    round_num,
                    result: if *success {
                        deepx_types::ToolResult::ok(content.clone())
                    } else {
                        deepx_types::ToolResult::error(content.clone())
                    },
                },
            ));
        }

        for (tool_call_id, _, _, _) in &results {
            ctx.emitter
                .emit_timeline(deepx_domain::TimelineIntent::BlockSealed {
                    turn_id: turn_id.to_string(),
                    round_num,
                    block_id: format!("tool:{tool_call_id}"),
                });
        }
        ctx.emitter
            .emit_timeline(deepx_domain::TimelineIntent::RoundSealed {
                turn_id: turn_id.to_string(),
                round_num,
                is_final: false,
            });
        // Refresh status bar tasks after every tool round
        if !results.is_empty() {
            // Ringing 双发：DashboardUpdated（replaceable 覆盖）
            ctx.emitter.emit_domain(deepx_domain::DomainEvent::Control(
                deepx_domain::ControlEvent::DashboardUpdated {
                    hp_connected: true,
                    session_seed: ctx.agent.session.seed.clone(),
                    tool_calls_total: 0,
                    tool_failures: 0,
                    current_phase: "single".into(),
                    streaming: false,
                },
            ));
            ctx.emitter.emit_domain(deepx_domain::DomainEvent::Control(
                deepx_domain::ControlEvent::DashboardSnapshot {
                    snapshot: dashboard::build_snapshot(ctx.agent.session.seed.clone()),
                },
            ));
        }
        ctx.agent
            .msg
            .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
        results
    }

    /// Reset all turn state (called on Cancel / new session).
    pub fn reset(&mut self) {
        self.suspended = None;
    }

    pub fn take_suspended_for_abort(&mut self) -> Option<(String, Option<UsageInfo>)> {
        self.suspended
            .take()
            .map(|state| (state.turn_id, state.usage))
    }
}
