//! TurnEngine: drives the gate→tools→repeat cycle.
//!
//! Owns: suspended TurnState.
//! Receives: RingContext + ToolEngine (for tool execution).
//! Returns: Outcome (ContinueTurn, YieldToUser, TurnComplete, Error).

use std::collections::{HashMap, HashSet};

use deepx_message::Effect;
use deepx_proto::{Agent2Ui, AskAnswer, AskResolution, RoundDeltaKind};
use deepx_types::UsageInfo;

use super::engine_tool::ToolEngine;
use super::types::*;
use crate::services::conflict;
use crate::services::dashboard;
use crate::util;

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
        ) {
            return Self::abort_running_turn(ctx, saved.turn_id, saved.usage);
        }

        if !saved.pending_plans.is_empty() {
            saved.reason = YieldReason::PlanReview;
            let turn_id = saved.turn_id.clone();
            if let Some(plan) = saved.pending_plans.front() {
                ctx.emitter.emit(Agent2Ui::PlanSubmitted {
                    call_id: plan.call_id.clone(),
                    plan_content: plan.content.clone(),
                    review_type: "plan".to_string(),
                    todo_items: None,
                });
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
        ctx.emitter.emit(Agent2Ui::AskResolved {
            ask_id: active.call_id,
            resolution: AskResolution::Answered,
        });

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

        // ── Todo activation path ──
        if let Some(todo_act) = saved.pending_todo_activation.take() {
            if approved {
                let (content, success) =
                    match deepx_tools::todo::exec_todo_activate(&serde_json::json!({})) {
                        Ok(content) => (content, true),
                        Err(error) => (error, false),
                    };
                ctx.agent
                    .msg
                    .push_tool_result_direct(&todo_act.call_id, &content, success);
                if !success {
                    log::warn!(
                        "[TURN] approved todo activation failed during commit: {}",
                        content
                    );
                };
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
            ctx.emitter.emit(Agent2Ui::PlanResolved {
                call_id: todo_act.call_id,
                approved,
            });
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
                "Plan approved. Import with todo_import_plan, then call todo_activate to start goal mode.\n\n{}",
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
        ctx.emitter.emit(Agent2Ui::PlanResolved {
            call_id: plan.call_id,
            approved,
        });

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
        tool.clear_pending();
        ctx.agent.msg.remove_last_step_if_incomplete();
        ctx.agent
            .msg
            .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
        ctx.emitter.emit(Agent2Ui::AskResolved {
            ask_id: ask_id.to_string(),
            resolution: AskResolution::Dismissed,
        });
        Outcome::TurnAborted {
            turn_id: saved.turn_id,
            usage: saved.usage,
            consume_queued_interrupt: false,
        }
    }

    fn emit_ask_rejected(ctx: &mut RingContext, ask_id: &str, message: &str) {
        ctx.emitter.emit(Agent2Ui::AskRejected {
            ask_id: ask_id.to_string(),
            message: message.to_string(),
        });
    }

    fn emit_active_ask(ctx: &mut RingContext, state: &TurnState) {
        if let Some(ask) = state.pending_asks.front() {
            ctx.emitter.emit(Agent2Ui::AskUser {
                turn_id: state.turn_id.clone(),
                round_num: state.round_num,
                ask_id: ask.call_id.clone(),
                mode: ask.mode,
                questions: ask.questions.clone(),
            });
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

    fn execute_admitted_batch(
        ctx: &mut RingContext,
        tool: &ToolEngine,
        mut admitted: Vec<AdmittedTool>,
        tool_call_order: &[String],
        serial_call_ids: &HashSet<String>,
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
            let (progress_tx, progress_rx) = deepx_tools::bounded_exec_progress_channel();
            let mut handles = Vec::new();
            for admitted in batch {
                let tx = progress_tx.clone();
                let call_id = admitted.call_id.clone();
                let handle = std::thread::Builder::new()
                    .stack_size(4 * 1024 * 1024)
                    .spawn({
                        let auth = admitted.auth;
                        let id = call_id.clone();
                        move || {
                            let result =
                                deepx_tools::execution::execute_authorized(*auth, Some(tx));
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
                handles.push((call_id, handle));
            }
            drop(progress_tx);
            tool.drain_progress_external(ctx, progress_rx, "llm_tool");

            let cancelled = ctx.cancel.is_set();
            for (call_id, handle) in handles {
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
                            ctx.emitter.emit_delta(Agent2Ui::CodeDelta {
                                lines_added: delta.lines_added,
                                lines_removed: delta.lines_removed,
                                files_created: delta.files_created,
                                files_deleted: delta.files_deleted,
                                file: delta.file.clone(),
                            });
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
            let (progress_tx, progress_rx) = deepx_tools::bounded_exec_progress_channel();
            let handle = std::thread::Builder::new()
                .stack_size(4 * 1024 * 1024)
                .spawn(move || {
                    let result = deepx_tools::execution::execute_authorized(
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
            tool.drain_progress_external(ctx, progress_rx, &call_id);
            match handle.join() {
                Ok((content, success, code_delta, skill_effects)) => {
                    ctx.agent
                        .msg
                        .push_tool_result_direct(&call_id, &content, success);
                    ordered_skill_effects.push((call_id.clone(), skill_effects));
                    if let Some(ref delta) = code_delta {
                        ctx.stats.push_delta(delta.clone());
                        ctx.emitter.emit_delta(Agent2Ui::CodeDelta {
                            lines_added: delta.lines_added,
                            lines_removed: delta.lines_removed,
                            files_created: delta.files_created,
                            files_deleted: delta.files_deleted,
                            file: delta.file.clone(),
                        });
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
    fn run_auto_compact(ctx: &mut RingContext) {
        let compact_eng = super::engine_compact::CompactEngine::new();
        let (prompt, kept, head, provider) = match compact_eng.build_prompt_and_meta(ctx) {
            Some(v) => v,
            None => return,
        };

        ctx.emitter.emit(deepx_proto::Agent2Ui::CompactStart {
            turns_total: ctx.agent.msg.turn_count() as u32,
            turns_keeping: kept as u32,
        });

        let emitter = ctx.emitter;
        let mut summary = String::new();
        let mut on_event = |ev: deepx_gate::StreamEvent| match ev {
            deepx_gate::StreamEvent::ContentDelta(d) => {
                summary.push_str(&d);
                emitter.emit_delta(deepx_proto::Agent2Ui::CompactDelta { delta: d });
            }
            deepx_gate::StreamEvent::ReasoningDelta(d) => {
                emitter.emit_delta(deepx_proto::Agent2Ui::CompactDelta { delta: d });
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
                ctx.emitter.emit(deepx_proto::Agent2Ui::CompactEnd {
                    summary_chars: summary.chars().count(),
                    turns_compacted: head as u32,
                    turns_removed: ctx.agent.msg.turns().len().saturating_sub(kept) as u32,
                });
                log::info!("[TURN] auto-compact done: {before} → {after} tokens");
            }
            Ok(()) => {
                ctx.emitter.emit(deepx_proto::Agent2Ui::CompactEnd {
                    summary_chars: 0,
                    turns_compacted: 0,
                    turns_removed: 0,
                });
            }
            Err(e) => {
                ctx.emitter.emit(deepx_proto::Agent2Ui::CompactEnd {
                    summary_chars: 0,
                    turns_compacted: 0,
                    turns_removed: 0,
                });
                log::error!("[TURN] auto-compact failed: {e}");
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
        // Rebuild provider from current config
        let ep = deepx_config::registry::find_endpoint(
            &ctx.agent.config.provider_id,
            &ctx.agent.config.endpoint,
        );
        let is_responses = ep.as_ref().map(|e| e.protocol.as_str()) == Some("responses");
        let provider = if is_responses {
            deepx_gate::ProviderConfig::responses(
                &ctx.agent.config.base_url,
                &ctx.agent.config.api_key,
                &ctx.agent.config.model,
                ep.as_ref().and_then(|e| e.responses_path.clone()),
            )
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
            if ctx.cancel.is_set() || deepx_tools::CANCEL.load(std::sync::atomic::Ordering::SeqCst)
            {
                return Self::abort_running_turn(ctx, turn_id, last_usage);
            }
            if !ctx.pending.is_empty() {
                ctx.agent.msg.remove_last_step_if_incomplete();
                ctx.agent
                    .msg
                    .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
                return Outcome::Handled;
            }

            // ── Auto-compact check (gate lap boundary) ──
            // Use API-reported prompt_tokens (input fill) when available, fallback to
            // heuristic.  prompt_tokens measures context-window pressure — completion
            // tokens are output and do not occupy the window.  Runs inline (blocking):
            // the compact frees context before the next gate request.
            let threshold = ctx.agent.config.auto_compact_threshold;
            if threshold > 0.0 {
                let total = last_usage
                    .as_ref()
                    .map(|u| u.prompt_tokens as u64)
                    .unwrap_or_else(|| {
                        let (c, t, tc, tr, ts, sp, _, _) =
                            ctx.agent.msg.compute_context_stats(None);
                        c + t + tc + tr + ts + sp
                    });
                let limit = ctx.agent.config.context_limit as u64;
                if total as f64 > limit as f64 * threshold {
                    log::info!(
                        "[TURN] auto-compact lap-boundary: {total}/{limit} tokens ({:.0}% threshold)",
                        threshold * 100.0
                    );
                    Self::run_auto_compact(ctx);
                }
            }

            let messages = ctx.agent.build_context();

            // ── Emit pending cache diagnostic ──
            if let Some((hash, reasons)) = ctx.agent.take_cache_diagnostics() {
                ctx.emitter.emit_delta(deepx_proto::Agent2Ui::CacheDiagnostics {
                    prefix_hash: hash,
                    prefix_changed: true,
                    change_reasons: reasons,
                });
            }

            let tools = Some(ctx.agent.tool_defs.clone());
            let mut content = String::new();
            let mut reasoning = String::new();
            let mut tool_calls_raw = serde_json::Value::Null;
            let mut had_error = false;

            *ctx.phase = LoopPhase::GateRunning;
            let cancel_arc = ctx.cancel.arc();

            // ── SSE Gate Request ──
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
                        ctx.emitter.emit_delta(Agent2Ui::RoundDelta {
                            turn_id: turn_id.clone(),
                            round_num,
                            kind: RoundDeltaKind::Answering,
                            delta: d,
                        });
                    }
                    deepx_gate::StreamEvent::ReasoningDelta(r) => {
                        if ctx.cancel.is_set() {
                            return;
                        }
                        reasoning.push_str(&r);
                        ctx.emitter.emit_delta(Agent2Ui::RoundDelta {
                            turn_id: turn_id.clone(),
                            round_num,
                            kind: RoundDeltaKind::Thinking,
                            delta: r,
                        });
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
                        ctx.emitter.emit_delta(Agent2Ui::ToolCallPreview {
                            turn_id: turn_id.clone(),
                            round_num,
                            index,
                            id,
                            name,
                            args_so_far,
                        });
                    }
                    deepx_gate::StreamEvent::UsageUpdate(u) => {
                        last_usage = Some(u.clone());
                        ctx.agent.session.tokens =
                            ctx.agent.session.tokens.max(u.total_tokens as u64);
                        ctx.emitter.emit_delta(Agent2Ui::UsageUpdated {
                            turn_id: turn_id.clone(),
                            round_num,
                            usage: u,
                            context_limit: ctx.agent.config.context_limit,
                            model: ctx.agent.config.model.clone(),
                        });
                    }
                    deepx_gate::StreamEvent::Retrying {
                        attempt,
                        max_retries,
                        delay_secs,
                        error,
                    } => {
                        ctx.emitter.emit(Agent2Ui::ProviderRetrying {
                            turn_id: turn_id.clone(),
                            round_num,
                            attempt,
                            max_retries,
                            delay_secs,
                            error,
                        });
                    }
                    deepx_gate::StreamEvent::Error(msg) => {
                        ctx.emitter.emit(Agent2Ui::Error { message: msg });
                        had_error = true;
                    }
                },
            );

            if ctx.cancel.is_set() {
                return Self::abort_running_turn(ctx, turn_id, last_usage);
            }

            if had_error || result.is_err() {
                ctx.agent
                    .msg
                    .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
                return Outcome::Handled;
            }

            // ── Parse + push assistant message ──
            let parsed = util::parse_tool_calls_from_response(
                &content,
                &reasoning,
                &tool_calls_raw,
                &ctx.agent,
            );
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
                            ctx.emitter.emit(Agent2Ui::Error {
                                message: "Duplicate tool-call ID from model".into(),
                            });
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
                        const MAX_PARALLEL_TOOL_WORKERS: usize = 4;
                        let (_serial_groups, serial_after) =
                            conflict::resolve_write_conflicts(&pending);
                        let serial_call_ids: HashSet<String> = serial_after
                            .iter()
                            .map(|index| pending[*index].id.clone())
                            .collect();
                        let admission = tool.admit_batch(ctx, &pending);
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
                                ctx.emitter.emit(Agent2Ui::PlanSubmitted {
                                    call_id,
                                    plan_content,
                                    review_type,
                                    todo_items,
                                });
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
                                deepx_tools::bounded_exec_progress_channel();
                            let mut handles: Vec<(String, std::thread::JoinHandle<_>)> = Vec::new();

                            for admitted in batch {
                                let tx = progress_tx.clone();
                                let call_id = admitted.call_id.clone();
                                let handle = std::thread::Builder::new()
                                    .stack_size(4 * 1024 * 1024)
                                    .spawn({
                                        let auth = admitted.auth;
                                        let cid = call_id.clone();
                                        move || {
                                            let result = deepx_tools::execution::execute_authorized(
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
                            tool.drain_progress_external(ctx, progress_rx, "llm_tool");

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
                                                ctx.emitter.emit_delta(Agent2Ui::CodeDelta {
                                                    lines_added: delta.lines_added,
                                                    lines_removed: delta.lines_removed,
                                                    files_created: delta.files_created,
                                                    files_deleted: delta.files_deleted,
                                                    file: delta.file.clone(),
                                                });
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
                            let (progress_tx, progress_rx) =
                                deepx_tools::bounded_exec_progress_channel();
                            let handle = std::thread::Builder::new()
                                .stack_size(4 * 1024 * 1024)
                                .spawn({
                                    let auth = admitted.auth;
                                    move || {
                                        let result = deepx_tools::execution::execute_authorized(
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
                            tool.drain_progress_external(ctx, progress_rx, &call_id);
                            match handle.join() {
                                Ok((content, success, code_delta, skill_effects)) => {
                                    ctx.agent
                                        .msg
                                        .push_tool_result_direct(&call_id, &content, success);
                                    ordered_skill_effects.push((call_id.clone(), skill_effects));
                                    if let Some(ref delta) = code_delta {
                                        ctx.stats.push_delta(delta.clone());
                                        ctx.emitter.emit_delta(Agent2Ui::CodeDelta {
                                            lines_added: delta.lines_added,
                                            lines_removed: delta.lines_removed,
                                            files_created: delta.files_created,
                                            files_deleted: delta.files_deleted,
                                            file: delta.file.clone(),
                                        });
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
                                ctx.emitter.emit(Agent2Ui::PlanSubmitted {
                                    call_id,
                                    plan_content,
                                    review_type,
                                    todo_items,
                                });
                            }
                            return Outcome::YieldToUser { turn_id, reason };
                        }
                    }

                    // All tools from this round are now resolved.
                    self.emit_completed_tool_round(ctx, &turn_id, round_num);

                    if let Err(error) = ctx.agent.skills.complete_model_lap() {
                        ctx.emitter.emit(Agent2Ui::Error { message: error });
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
                    ctx.emitter.emit(Agent2Ui::Error { message: error });
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
        let tool_defs: Vec<_> = results
            .iter()
            .map(|(tc_id, name, content, success)| {
                let args = ctx
                    .agent
                    .msg
                    .tool_call_args(tc_id)
                    .map(|a| a.to_string())
                    .unwrap_or_default();
                ctx.emitter.emit_delta(Agent2Ui::AuditRecord {
                    tool_name: name.clone(),
                    result_summary: content
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(120)
                        .collect(),
                    success: *success,
                    time: ts.clone(),
                    args,
                });
                deepx_proto::ToolResultDef {
                    tool_call_id: tc_id.clone(),
                    output: content.clone(),
                    success: *success,
                    file: None,
                }
            })
            .collect();

        if !tool_defs.is_empty() {
            ctx.emitter.emit(Agent2Ui::ToolResults {
                turn_id: turn_id.to_string(),
                round_num,
                results: tool_defs,
            });
        }
        // Refresh status bar tasks after every tool round
        if !results.is_empty() {
            ctx.emitter.emit(Agent2Ui::Dashboard {
                hp_connected: true,
                session_seed: ctx.agent.session.seed.clone(),
                context_limit: ctx.agent.config.context_limit,
                tool_calls_total: 0,
                tool_failures: 0,
                current_phase: "single".into(),
                streaming: false,
                dsml_compat_count: ctx.agent.dsml_compat_count,
                documents: dashboard::build_documents(),
                recent_edits: dashboard::build_recent_edits(),
                tasks: dashboard::build_tasks(),
                session_title: ctx.agent.session.title.clone(),
                usage: None,
                model: Some(ctx.agent.config.model.clone()),
            });
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
