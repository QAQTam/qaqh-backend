//! TurnEngine: drives the gate→tools→repeat cycle.
//!
//! Owns: suspended TurnState.
//! Receives: RingContext + ToolEngine (for tool execution).
//! Returns: Outcome (ContinueTurn, YieldToUser, TurnComplete, Error).

use std::collections::{HashMap, HashSet, VecDeque};

use qaqh_domain::AskAnswer;
use qaqh_session::canonical::{
    ToolLedgerError, causation_for_command, generate_ulid, sha256_content_hash,
};
use qaqh_session::session_fact_v2::{
    ActorKind, ActorRef, ContentHash, ContentRef, EventId, InteractionDecision, InteractionExpired,
    InteractionExpiryReason, InteractionKind, InteractionRequested, InteractionResolved,
};
use qaqh_types::UsageInfo;

use super::engine_tool::ToolEngine;
use super::turn_actor::{
    InteractionAdmission, InteractionState, TurnActor, TurnActorError, TurnCancellation,
};
use super::types::*;
use crate::agent::state::agent::{AgentState, tool_ledger_lease_ms, unix_ms};
use crate::agent::tool_runtime::{canonical_call_id, canonical_interaction_id, canonical_turn_id};
use crate::agent::turn_lap::admit as turn_admit;
use crate::agent::turn_lap::backfill as turn_backfill;
use crate::agent::turn_lap::gate::{
    GateRequestResult, abort_running_turn, gate_request, provider_for, seal_timeline_terminal_round,
};

/// Map the runtime's decision string onto the canonical structured verdict.
///
/// Unknown strings stay `None` so a future decision kind does not silently
/// masquerade as an existing one.
fn structured_interaction_decision(decision: &str) -> Option<InteractionDecision> {
    match decision {
        "approved" => Some(InteractionDecision::Approved),
        "rejected" => Some(InteractionDecision::Rejected),
        "answered" => Some(InteractionDecision::Answered),
        "dismissed" => Some(InteractionDecision::Dismissed),
        _ => None,
    }
}

/// Why the turn is being resumed.
pub enum ResumeReason {
    /// User answered permission dialogs — all approvals resolved.
    PermissionResolved,
}

/// 传输层请求快照 dump（诊断工具）：`QAQH_REQUEST_LOG=1` 时，每次 gate
/// 请求构建后把**真实传输的消息列表**写入 `<data>/sessions/<seed>/request-log.jsonl`
/// （每请求一行 JSON），与 `messages.jsonl`（store 理论落盘）对比，二分定位
/// "落盘 ≠ 传输"分叉点（例如 trailing 注入已持久化但模型请求未携带）。
/// 默认关闭，无任何正常路径开销。
fn dump_request_log(
    session_id: &str,
    rev: u64,
    turns: usize,
    trailing: usize,
    messages: &[qaqh_types::Message],
) {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ENABLED.get_or_init(|| {
        std::env::var("QAQH_REQUEST_LOG")
            .map(|v| v == "1")
            .unwrap_or(false)
    }) {
        return;
    }
    if session_id.is_empty() {
        return;
    }
    let dir = qaqh_types::platform::data_dir()
        .join("sessions")
        .join(session_id);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("request-log.jsonl"))
    else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let rec = serde_json::json!({
        "ts": now,
        "rev": rev,
        "turns": turns,
        "trailing": trailing,
        "n": messages.len(),
        "messages": messages.iter().map(|m| {
            let text = m.content.iter()
                .filter_map(|b| match b {
                    qaqh_types::ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            serde_json::json!({ "role": m.role, "text": text })
        }).collect::<Vec<_>>(),
    });
    // Keep diagnostics metadata-only while exposing whether the opt-in full
    // request logger was building its additional in-memory JSON tree.
    qaqh_memwatch::global().record_phase(
        "request_log.value_built",
        Some(session_id),
        None,
        None,
        None,
    );
    use std::io::Write;
    let _ = writeln!(f, "{rec}");
}

/// Upper bound on mid-stream recovery requests within one turn. Exhaustion
/// fails the turn rather than reporting an unconfirmed response as complete.
const MAX_STREAM_CONTINUATIONS: u32 = 3;

/// 自动压缩已禁用（2026-10-12）：本地 token 估算与上游 usage 口径存在系统性
/// 偏差（图片密集会话尤其严重），软阈值/硬窗口预检会反复误触发压缩；压缩
/// 失败后又按 revision 冷却失效，形成每 lap 重试并烧穿额度的循环。压缩现在
/// 只保留手动路径（loop_outcome `start_compact`），后续由 handoff 文档机制
/// 取代（届时连同本注释一并清理）。

/// TurnEngine manages a single LLM turn lifecycle.
pub struct TurnEngine {
    /// Canonical lifecycle mirror for the existing `run_lap` execution path.
    actor: TurnActor,
    /// If Some, a turn is suspended waiting for permission or ask_user.
    pub(crate) suspended: Option<TurnState>,
    /// Recovery requests consumed for the current turn.
    pub(crate) continuation_count: u32,
}

impl Default for TurnEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl TurnEngine {
    pub fn new() -> Self {
        Self {
            actor: TurnActor::new(),
            suspended: None,
            continuation_count: 0,
        }
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

    /// Wire turn id of the turn the actor considers current (running or
    /// suspended); `None` when idle/terminal. Cancel target validation reads
    /// this BEFORE any side effect (2026-10-23 审查 §7).
    pub(crate) fn current_turn_id(&self) -> Option<&str> {
        match self.actor.state() {
            qaqh_session::actor::TurnCoreState::Active { turn_id, .. } => Some(turn_id.as_str()),
            _ => None,
        }
    }

    pub(crate) fn observe_outcome(
        &mut self,
        agent: &mut AgentState,
        outcome: &Outcome,
    ) -> Result<(), TurnActorError> {
        let yielding = matches!(outcome, Outcome::YieldToUser { .. });
        let pending_interactions = if yielding {
            self.suspended
                .as_ref()
                .map(Self::pending_interaction_ids)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        self.actor
            .observe_outcome_with_interactions(outcome, &pending_interactions)?;
        if yielding {
            self.persist_interaction_requests(agent)?;
        }
        Ok(())
    }

    fn persist_interaction_requests(&self, agent: &mut AgentState) -> Result<(), TurnActorError> {
        let Some(state) = self.suspended.as_ref() else {
            return Ok(());
        };
        let Some(ledger) = agent
            .tool_ledger_mut()
            .map_err(|error| TurnActorError::ToolLedger(error.to_string()))?
        else {
            return Ok(());
        };
        let now = unix_ms();
        ledger
            .ensure_lease(now, tool_ledger_lease_ms())
            .map_err(|error| TurnActorError::ToolLedger(error.to_string()))?;

        let turn_id = canonical_turn_id(&state.turn_id);
        // (wire 交互 id, kind, 正文 content_id)。
        //
        // 正文 content_id = `sha256(正文 bytes)`，由 hub 在发布同一条域事件时写进
        // content store（`qaqh_domain::interaction_body` 是两处唯一的序列化来源）。
        // ask / plan / permission 都带正文；permission 的详情正文由
        // `engine_tool` 在请求授权时构造（纯 v2 的 wire 上没有 tool 频道快照）。
        let mut requests: Vec<(&str, InteractionKind, Option<String>)> = Vec::new();
        // permission 的详情正文也走 canonical ref（#345 之后新增）：纯 v2 的 wire
        // 上没有 tool 频道快照，壳层只能从 content store 取正文渲染授权面板。
        let permission_bodies: std::collections::HashMap<&str, &Vec<u8>> = state
            .pending_permission_bodies
            .iter()
            .map(|(call_id, body)| (call_id.as_str(), body))
            .collect();
        requests.extend(state.pending_permission_ids.iter().map(|call_id| {
            let content_id = permission_bodies
                .get(call_id.as_str())
                .map(|body| qaqh_types::sha256_hex(body));
            (call_id.as_str(), InteractionKind::Permission, content_id)
        }));
        requests.extend(state.pending_asks.iter().map(|ask| {
            let body = qaqh_domain::interaction_body::ask_body(ask.mode, &ask.questions);
            (
                ask.call_id.as_str(),
                InteractionKind::Ask,
                Some(qaqh_types::sha256_hex(&body)),
            )
        }));
        requests.extend(state.pending_plans.iter().map(|plan| {
            let body = qaqh_domain::interaction_body::plan_body(&plan.content, "plan", None);
            (
                plan.call_id.as_str(),
                InteractionKind::Plan,
                Some(qaqh_types::sha256_hex(&body)),
            )
        }));
        if let Some(todo) = &state.pending_todo_activation {
            let body =
                qaqh_domain::interaction_body::plan_body("", "todo_activation", Some(&todo.items));
            requests.push((
                todo.call_id.as_str(),
                InteractionKind::Plan,
                Some(qaqh_types::sha256_hex(&body)),
            ));
        }

        for (wire_interaction_id, kind, content_id) in requests {
            let interaction_id = canonical_interaction_id(wire_interaction_id);
            let call_id = canonical_call_id(wire_interaction_id);
            let request_ref = match content_id {
                // canonical ref 的格式校验要求 `sha256:<64 hex>`；content store 的
                // id 是裸 hex，v2 content 端点负责归一（strip 前缀后查 store）。
                Some(content_id) => {
                    ContentRef::new(ContentHash::new(format!("sha256:{content_id}")))
                }
                None => {
                    let request_bytes = serde_json::to_vec(&serde_json::json!({
                        "interaction_id": interaction_id.as_str(),
                        "call_id": call_id.as_str(),
                        "kind": format!("{kind:?}"),
                    }))
                    .unwrap_or_default();
                    ContentRef::new(sha256_content_hash(&request_bytes))
                }
            };
            let payload = InteractionRequested {
                interaction_id,
                call_id: Some(call_id.clone()),
                turn_id: turn_id.clone(),
                kind,
                request_ref,
                expires_at_ms: None,
                requested_at_ms: now,
            };
            ledger
                .append_interaction_requested(
                    EventId::new(generate_ulid()),
                    Some(call_id),
                    payload,
                    now,
                )
                .map_err(|error| TurnActorError::ToolLedger(error.to_string()))?;
        }
        Ok(())
    }

    pub(crate) fn record_interaction_resolution(
        agent: &mut AgentState,
        interaction_id: &str,
        decision: &str,
        causation_id: Option<&str>,
        actor: Option<ActorRef>,
    ) -> Result<(), TurnActorError> {
        // See `structured_interaction_decision`: the wire decision strings map
        // onto the canonical enum so replays can return a typed verdict.
        let Some(ledger) = agent
            .tool_ledger_mut()
            .map_err(|error| TurnActorError::ToolLedger(error.to_string()))?
        else {
            return Ok(());
        };
        let now = unix_ms();
        ledger
            .ensure_lease(now, tool_ledger_lease_ms())
            .map_err(|error| TurnActorError::ToolLedger(error.to_string()))?;
        let decision_bytes =
            serde_json::to_vec(&serde_json::json!({ "decision": decision })).unwrap_or_default();
        let payload = InteractionResolved {
            interaction_id: canonical_interaction_id(interaction_id),
            decision_ref: ContentRef::new(sha256_content_hash(&decision_bytes)),
            decision: structured_interaction_decision(decision),
            // 归因（S6）：daemon 依鉴权身份填 actor；缺省回退本地 user（现状语义）。
            resolved_by: actor.unwrap_or(ActorRef {
                kind: ActorKind::User,
                id: "user".into(),
                display_name: None,
            }),
            resolution_seq: 1,
            resolved_at_ms: now,
        };
        let causation_id = causation_id.and_then(causation_for_command);
        ledger
            .append_interaction_resolved_with_causation(
                EventId::new(generate_ulid()),
                causation_id,
                None,
                None,
                payload,
                now,
            )
            .map_err(|error| TurnActorError::ToolLedger(error.to_string()))?;
        Ok(())
    }

    pub(crate) fn admit_interaction_resolution(
        &mut self,
        interaction_id: &str,
    ) -> InteractionAdmission {
        self.actor.admit_interaction_resolution(interaction_id)
    }

    pub(crate) fn interaction_state(&self, interaction_id: &str) -> InteractionState {
        self.actor.interaction_state(interaction_id)
    }

    pub(crate) fn begin_input(
        &mut self,
        turn_id: &str,
        input_id: &str,
    ) -> Result<(), TurnActorError> {
        self.actor.begin_input(turn_id, input_id)?;
        self.continuation_count = 0;
        Ok(())
    }

    /// Cancel through the canonical ledger when this session is durable.
    ///
    /// `call_ids` are wire IDs collected by the caller before it drops the
    /// suspended/runtime state. Calls that already have an intent are left
    /// open by the actor; calls without one are sealed as executionless
    /// `Cancelled`.
    pub(crate) fn cancel_with_ledger(
        &mut self,
        agent: &mut AgentState,
        turn_id: Option<&str>,
        call_ids: Vec<String>,
    ) -> Result<TurnCancellation, TurnActorError> {
        let before = self.actor.state().clone();
        let wire_turn = turn_id.map(str::to_string).or_else(|| match &before {
            qaqh_session::actor::TurnCoreState::Active { turn_id, .. } => {
                Some(turn_id.as_str().to_string())
            }
            _ => None,
        });
        let Some(wire_turn) = wire_turn else {
            return Ok(match before {
                qaqh_session::actor::TurnCoreState::Terminal { .. } => {
                    TurnCancellation::AlreadyTerminal
                }
                _ => TurnCancellation::Idle,
            });
        };

        let ledger = match agent.tool_ledger_mut() {
            Ok(Some(ledger)) => ledger,
            Ok(None) => return self.actor.cancel(&wire_turn),
            Err(error) => return Err(TurnActorError::ToolLedger(error.to_string())),
        };
        let actor_turn = qaqh_session::session_fact_v2::TurnId::new(wire_turn.clone());
        let ledger_turn = canonical_turn_id(&wire_turn);
        let canonical_calls = call_ids
            .into_iter()
            .map(|call_id| canonical_call_id(&call_id))
            .collect();
        self.actor
            .cancel_tool_batch(
                ledger,
                &actor_turn,
                &ledger_turn,
                canonical_calls,
                unix_ms(),
            )
            .map_err(|error| TurnActorError::ToolLedger(error.to_string()))?;
        if matches!(
            before,
            qaqh_session::actor::TurnCoreState::Active { .. }
                | qaqh_session::actor::TurnCoreState::Terminal {
                    terminal: qaqh_session::session_fact_v2::TurnTerminal::Cancelled,
                    ..
                }
        ) {
            let now = unix_ms();
            ledger
                .ensure_lease(now, tool_ledger_lease_ms())
                .map_err(|error| TurnActorError::ToolLedger(error.to_string()))?;
            ledger
                .append_turn_finished(
                    EventId::new(generate_ulid()),
                    qaqh_session::session_fact_v2::TurnFinished {
                        turn_id: ledger_turn,
                        terminal: qaqh_session::session_fact_v2::TurnTerminal::Cancelled,
                        usage: None,
                        error: None,
                        finished_at_ms: now,
                    },
                    now,
                )
                .map_err(|error| TurnActorError::ToolLedger(error.to_string()))?;
        }
        Ok(match before {
            qaqh_session::actor::TurnCoreState::Active { .. } => TurnCancellation::Interrupted {
                reason: qaqh_session::session_fact_v2::InterruptReason::CancelBeforeSeal,
            },
            qaqh_session::actor::TurnCoreState::Terminal { .. } => {
                TurnCancellation::AlreadyTerminal
            }
            qaqh_session::actor::TurnCoreState::Idle => TurnCancellation::Idle,
        })
    }

    fn pending_interaction_ids(state: &TurnState) -> Vec<String> {
        let mut ids = Vec::new();
        ids.extend(state.pending_permission_ids.iter().cloned());
        ids.extend(state.pending_asks.iter().map(|ask| ask.call_id.clone()));
        ids.extend(state.pending_plans.iter().map(|plan| plan.call_id.clone()));
        if let Some(todo) = &state.pending_todo_activation {
            ids.push(todo.call_id.clone());
        }
        ids.sort();
        ids.dedup();
        ids
    }

    /// 中止/丢弃悬空回合时，把挂起表登记的全部交互按 `TurnCancelled` 落 expiry
    /// fact（2026-10-07 幽灵审批回归）。`InteractionRequested` 已在 fact 链上，
    /// 只清内存态会让投影把这些交互永远投影成待审批，壳层反复拉起审批卡且
    /// 点击永远被拒。与先到的 resolution 冲突时按 first-answer-wins 忽略。
    fn expire_suspended_interactions(ctx: &mut RingContext, saved: &TurnState) {
        let ledger = match ctx.agent.tool_ledger_mut() {
            Ok(Some(ledger)) => ledger,
            Ok(None) => return,
            Err(error) => {
                log::warn!("[TURN] tool ledger unavailable for interaction expiry: {error}");
                return;
            }
        };
        let turn_id = canonical_turn_id(&saved.turn_id);
        for wire_id in Self::pending_interaction_ids(saved) {
            let payload = InteractionExpired {
                interaction_id: canonical_interaction_id(&wire_id),
                reason: InteractionExpiryReason::TurnCancelled,
                recovery_ref: None,
                expired_at_ms: unix_ms(),
            };
            if let Err(error) = ledger.append_interaction_expired(
                EventId::new(generate_ulid()),
                Some(turn_id.clone()),
                Some(canonical_call_id(&wire_id)),
                payload,
                unix_ms(),
            ) {
                Self::warn_expiry_failure(&wire_id, error);
            }
        }
    }

    /// 幽灵交互收敛：worker 已不认识该 interaction（崩溃重启、会话切换、回合
    /// 被取代），它永远不可再被应答——给请求 fact 的 id 补 `TurnCancelled`
    /// expiry 终态，投影随之停止把它投影成待审批。仍登记在挂起表里的（排队
    /// 中的后续 ask / plan）保持 pending，轮到它时照常应答。
    ///
    /// wire 与 canonical 两种形态各落一条：请求 fact 的 id 可能是任一形态
    /// （v2 壳层透传 canonical，TUI 透传 wire），对不存在 id 的 expiry 追加
    /// 是无害的孤儿 fact。
    fn expire_unanswerable_interaction(&self, ctx: &mut RingContext, interaction_id: &str) {
        if self.actor.interaction_state_canonical(interaction_id) != InteractionState::Unknown {
            return;
        }
        let ledger = match ctx.agent.tool_ledger_mut() {
            Ok(Some(ledger)) => ledger,
            Ok(None) => return,
            Err(error) => {
                log::warn!("[TURN] tool ledger unavailable for interaction expiry: {error}");
                return;
            }
        };
        let mut target_ids = vec![canonical_interaction_id(interaction_id)];
        let direct = qaqh_session::session_fact_v2::InteractionId::new(interaction_id.to_string());
        if !target_ids.contains(&direct) {
            target_ids.push(direct);
        }
        for target_id in target_ids {
            let payload = InteractionExpired {
                interaction_id: target_id.clone(),
                reason: InteractionExpiryReason::TurnCancelled,
                recovery_ref: None,
                expired_at_ms: unix_ms(),
            };
            if let Err(error) = ledger.append_interaction_expired(
                EventId::new(generate_ulid()),
                None,
                None,
                payload,
                unix_ms(),
            ) {
                Self::warn_expiry_failure(target_id.as_str(), error);
            }
        }
    }

    fn warn_expiry_failure(interaction_id: &str, error: ToolLedgerError) {
        match error {
            ToolLedgerError::InteractionTerminalConflict { .. } => {}
            error => log::warn!("[TURN] failed to expire interaction {interaction_id}: {error}"),
        }
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
        if saved.session_id != ctx.agent.session.session_id {
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

    /// H1/H2：悬空 turn 属于已替换的会话时，收敛 timeline 并丢弃。
    /// 返回 true 表示检测到陈旧悬空且已处理（调用方应立即返回 Handled）。
    fn drop_stale_suspension(&mut self, ctx: &mut RingContext) -> bool {
        let stale = self
            .suspended
            .as_ref()
            .is_some_and(|s| s.session_id != ctx.agent.session.session_id);
        if !stale {
            return false;
        }
        log::warn!("[TURN] dropping suspension belonging to a replaced session");
        if let Some(saved) = self.suspended.take() {
            // 注意：此处 ctx.agent 已是被替换后的新会话，ledger 不能再写——
            // 旧会话的交互 expiry 由该会话自身的应答/中止路径收敛。
            if let Err(error) = self.actor.cancel(&saved.turn_id) {
                log::error!(
                    "[TURN] SessionActor rejected stale turn {}: {error}",
                    saved.turn_id
                );
            }
            let tool_ids: HashSet<String> = saved.tool_call_order.iter().cloned().collect();
            seal_timeline_terminal_round(
                ctx,
                &saved.turn_id,
                saved.round_num,
                None,
                &tool_ids,
                qaqh_domain::TimelineTurnState::Cancelled,
                Some(qaqh_domain::TimelineFailure {
                    code: "session_replaced".into(),
                    message: "The active session was replaced while this turn was suspended."
                        .into(),
                }),
            );
        }
        true
    }

    /// H1：显式中止并分离当前悬空的 turn（被新用户输入取代）。
    /// 返回被中止的 turn_id；timeline 与领域事件双发 Cancelled 终态，
    /// 悬空 tool_use 由调用方用 remove_last_step_if_incomplete 清理。
    pub fn abort_suspended(&mut self, ctx: &mut RingContext) -> Option<String> {
        let saved = self.suspended.take()?;
        Self::expire_suspended_interactions(ctx, &saved);
        if let Err(error) = self.actor.cancel(&saved.turn_id) {
            log::error!(
                "[TURN] SessionActor rejected superseded turn {}: {error}",
                saved.turn_id
            );
        }
        log::warn!(
            "[TURN] aborting suspended turn {} (superseded by newer input)",
            saved.turn_id
        );
        let tool_ids: HashSet<String> = saved.tool_call_order.iter().cloned().collect();
        seal_timeline_terminal_round(
            ctx,
            &saved.turn_id,
            saved.round_num,
            None,
            &tool_ids,
            qaqh_domain::TimelineTurnState::Cancelled,
            Some(qaqh_domain::TimelineFailure {
                code: "superseded_by_new_input".into(),
                message: "A newer user message replaced this suspended turn.".into(),
            }),
        );
        ctx.emitter
            .emit_domain(qaqh_domain::DomainEvent::Conversation(
                qaqh_domain::ConversationEvent::ConversationCancelled {
                    turn_id: Some(saved.turn_id.clone()),
                },
            ));
        Some(saved.turn_id)
    }

    /// Resolve one LLM permission by call ID. The turn only advances after
    /// every permission from the assistant round has been accounted for.
    pub fn handle_permission_resolved(
        &mut self,
        ctx: &mut RingContext,
        tool: &mut ToolEngine,
        call_id: &str,
        command_id: &str,
        admitted: Option<AdmittedTool>,
        actor: Option<ActorRef>,
    ) -> Outcome {
        // H1/H2：悬空状态属于其它会话时直接丢弃。
        if self.drop_stale_suspension(ctx) {
            return Outcome::Handled;
        }
        let Some(saved) = self.suspended.as_mut() else {
            log::warn!("[TURN] permission resolved without a suspended turn: {call_id}");
            return Outcome::Handled;
        };
        if saved.reason != YieldReason::PermissionPending
            || !saved
                .pending_permission_ids
                .iter()
                .any(|id| crate::agent::tool_runtime::permission_id_matches(id, call_id))
        {
            log::warn!("[TURN] stale permission resolution ignored: {call_id}");
            return Outcome::Handled;
        }
        let decision = if admitted.is_some() {
            "approved"
        } else {
            "rejected"
        };
        if let Err(error) = Self::record_interaction_resolution(
            ctx.agent,
            call_id,
            decision,
            Some(command_id),
            actor,
        ) {
            log::error!("[TURN] failed to persist permission resolution {call_id}: {error}");
        }

        if let Some(admitted) = admitted {
            saved.deferred_authorized.push(admitted);
        }
        saved
            .pending_permission_ids
            .retain(|id| !crate::agent::tool_runtime::permission_id_matches(id, call_id));
        if !saved.pending_permission_ids.is_empty() {
            return Outcome::YieldToUser {
                turn_id: saved.turn_id.clone(),
                reason: YieldReason::PermissionPending,
            };
        }

        let mut saved = self.suspended.take().expect("permission suspension exists");
        let deferred_authorized = std::mem::take(&mut saved.deferred_authorized);
        if !turn_admit::execute_admitted_batch(
            ctx,
            tool,
            Some(&mut self.actor),
            crate::agent::tool_runtime::ToolBatchOrigin::Resume,
            deferred_authorized,
            &saved.tool_call_order,
            &saved.serial_call_ids,
            &saved.turn_id,
            saved.round_num,
        ) {
            let tool_ids = saved.tool_call_order.iter().cloned().collect();
            seal_timeline_terminal_round(
                ctx,
                &saved.turn_id,
                saved.round_num,
                None,
                &tool_ids,
                qaqh_domain::TimelineTurnState::Cancelled,
                None,
            );
            return abort_running_turn(ctx, saved.turn_id, saved.usage);
        }

        if !saved.pending_plans.is_empty() {
            saved.reason = YieldReason::PlanReview;
            let turn_id = saved.turn_id.clone();
            if let Some(plan) = saved.pending_plans.front() {
                // Ringing 双发：PlanReviewRequested（resume 重放）
                ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
                    qaqh_domain::ControlEvent::PlanReviewRequested {
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
        command_id: &str,
        answers: &[AskAnswer],
        actor: Option<ActorRef>,
    ) -> Outcome {
        // H1/H2：悬空状态属于其它会话时直接丢弃。
        if self.drop_stale_suspension(ctx) {
            return Outcome::Handled;
        }
        let active = match self.suspended.as_ref() {
            Some(state) if state.reason == YieldReason::AskUser => match state.pending_asks.front()
            {
                Some(active) => active,
                None => {
                    Self::expire_unanswerable_interaction(self, ctx, ask_id);
                    Self::emit_ask_rejected(ctx, ask_id, "No active ask_user prompt");
                    return Outcome::Handled;
                }
            },
            _ => {
                Self::expire_unanswerable_interaction(self, ctx, ask_id);
                Self::emit_ask_rejected(ctx, ask_id, "No active ask_user prompt");
                return Outcome::Handled;
            }
        };

        let active_wire_id = active.call_id.clone();
        if !crate::agent::tool_runtime::interaction_id_matches(&active_wire_id, ask_id) {
            Self::expire_unanswerable_interaction(self, ctx, ask_id);
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
        // resolution fact 必须落在请求 fact 的 id 上（canonical_interaction_id
        // (wire)）。入站 id 已是 canonical 形态时再派生一次会得到错位 id，投影
        // 永远折叠不了 resolution，壳层反复拉起审批卡（2026-10-07 回归）。
        if let Err(error) = Self::record_interaction_resolution(
            ctx.agent,
            &active_wire_id,
            "answered",
            Some(command_id),
            actor,
        ) {
            log::error!("[TURN] failed to persist ask resolution {ask_id}: {error}");
        }

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
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
            qaqh_domain::ControlEvent::InteractionResolved {
                interaction_id: active.call_id,
                resolution: qaqh_domain::AskResolution::Answered,
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
    #[allow(clippy::too_many_arguments)]
    pub fn handle_plan_response(
        &mut self,
        ctx: &mut RingContext,
        tool: &mut ToolEngine,
        call_id: &str,
        command_id: &str,
        approved: bool,
        message: &str,
        autonomous: bool,
        actor: Option<ActorRef>,
    ) -> Outcome {
        // H1/H2：悬空状态属于其它会话时直接丢弃。
        if self.drop_stale_suspension(ctx) {
            return Outcome::Handled;
        }
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
        let Some(active_id) =
            active_id.filter(|id| crate::agent::tool_runtime::interaction_id_matches(id, call_id))
        else {
            log::warn!("[TURN] plan response without a suspended review: {call_id}");
            Self::expire_unanswerable_interaction(self, ctx, call_id);
            return Outcome::Handled;
        };
        // 与 ask 同理：resolution fact 落在请求 fact 的 id（wire 形态）上，
        // canonical 入站 id 不得再派生一次。
        if let Err(error) = Self::record_interaction_resolution(
            ctx.agent,
            active_id,
            if approved { "approved" } else { "rejected" },
            Some(command_id),
            actor,
        ) {
            log::error!("[TURN] failed to persist plan resolution {call_id}: {error}");
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
            ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
                qaqh_domain::ControlEvent::PlanReviewResolved {
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
                "Plan approved. Goal automation is currently frozen. Track execution with todo(action=\"create\") and todo(action=\"set\", id=\"T…\", status=\"…\"); mark unnecessary work with status=\"cancelled\".\n\n{}",
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
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
            qaqh_domain::ControlEvent::PlanReviewResolved {
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
        command_id: &str,
        actor: Option<ActorRef>,
    ) -> Outcome {
        // H1/H2：悬空状态属于其它会话时直接丢弃。
        if self.drop_stale_suspension(ctx) {
            return Outcome::Handled;
        }
        let active_id = self
            .suspended
            .as_ref()
            .filter(|state| state.reason == YieldReason::AskUser)
            .and_then(|state| state.pending_asks.front())
            .map(|ask| ask.call_id.as_str());
        if !active_id
            .is_some_and(|id| crate::agent::tool_runtime::interaction_id_matches(id, ask_id))
        {
            Self::expire_unanswerable_interaction(self, ctx, ask_id);
            Self::emit_ask_rejected(ctx, ask_id, "ask_id does not match the active prompt");
            return Outcome::Handled;
        }
        if let Err(error) = Self::record_interaction_resolution(
            ctx.agent,
            active_id.expect("dismiss matched the active ask"),
            "dismissed",
            Some(command_id),
            actor,
        ) {
            log::error!("[TURN] failed to persist ask dismissal {ask_id}: {error}");
        }

        let saved = self.suspended.take().expect("active ask suspension exists");
        let tool_ids = saved.tool_call_order.iter().cloned().collect();
        seal_timeline_terminal_round(
            ctx,
            &saved.turn_id,
            saved.round_num,
            None,
            &tool_ids,
            qaqh_domain::TimelineTurnState::Cancelled,
            None,
        );
        tool.clear_pending();
        ctx.agent.msg.remove_last_step_if_incomplete();
        ctx.agent
            .msg
            .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
        // Ringing 双发：InteractionResolved（ask 交互终结）
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
            qaqh_domain::ControlEvent::InteractionResolved {
                interaction_id: ask_id.to_string(),
                resolution: qaqh_domain::AskResolution::Dismissed,
            },
        ));
        Outcome::TurnAborted {
            turn_id: saved.turn_id,
            usage: saved.usage,
        }
    }

    fn emit_ask_rejected(ctx: &mut RingContext, ask_id: &str, message: &str) {
        // legacy AskRejected 退役：无 Ringing 专用事件，按 §4.2 登记
        // "由 OperationFailed（ErrorScope::Control, code=ask_rejected）覆盖"。
        ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
            qaqh_domain::ControlEvent::OperationFailed {
                occurrence_id: format!("occ-ask-rejected-{ask_id}"),
                scope: qaqh_domain::ErrorScope::Control,
                error: qaqh_domain::DomainError {
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

    /// 构造本轮 gate 的传输快照（原 prepare_gate_snapshot 的 auto-compact
    /// 预检分支已随自动压缩禁用移除，见文件头注释）。
    ///
    /// 覆盖原 `run_lap` 中“Build and measure”整段：
    /// `build_context` → `dump_request_log` → `estimate_prepared_request`
    /// → `token_calibration_fingerprint`/`prepared_request_key`。
    /// 返回 `(messages, request_estimate, fingerprint, key)` 供 `gate_request` 与
    /// 事后 `observe_prepared_request` 消费。
    fn prepare_gate_snapshot(
        ctx: &mut RingContext,
    ) -> (
        Vec<qaqh_types::Message>,
        crate::agent::state::token_calibration::RequestTokenEstimate,
        String,
        String,
    ) {
        let messages = ctx.agent.build_context();
        dump_request_log(
            &ctx.agent.session.session_id,
            ctx.agent.msg.context_revision(),
            ctx.agent.msg.turn_count(),
            ctx.agent.msg.trailing_messages().len(),
            &messages,
        );
        let request_fingerprint = ctx.agent.token_calibration_fingerprint();
        let (request_estimate, request_key) = ctx.agent.estimate_prepared_request_with_key(
            &messages,
            Some(&ctx.agent.tool_defs),
            &request_fingerprint,
        );
        (messages, request_estimate, request_fingerprint, request_key)
    }

    /// ChatGPT 套餐直连的 401 恢复（对标 codex `ProviderUnauthorizedRecovery::Recovered`，
    /// codex-rs/model-provider/src/provider.rs:48-53）：Authentication 类失败 →
    /// 强制刷新（旋转式 refresh token：单飞 + 先落盘）→ 重建 provider 整请求
    /// 重试一次。刷新或重建失败一律返回原始结果——原 401 的信息量高于恢复失败。
    ///
    /// 只有 `credential == ChatgptPlan` 的会话才有可刷新凭据；其余来源的 401
    /// 走原有失败路径（零克隆）。
    fn gate_request_with_plan_auth_retry(
        ctx: &mut RingContext,
        provider: &qaqh_gate::ProviderConfig,
        messages: Vec<qaqh_types::Message>,
        turn_id: &str,
        round_num: u32,
        last_usage: Option<UsageInfo>,
    ) -> GateRequestResult {
        let retry_messages = (ctx.agent.config.credential
            == qaqh_types::CredentialSource::ChatgptPlan)
            .then(|| messages.clone());
        let result = gate_request(
            ctx,
            provider,
            messages,
            Some(ctx.agent.tool_defs.clone()),
            turn_id,
            round_num,
            last_usage.clone(),
        );
        let needs_retry = retry_messages.is_some()
            && result
                .gate_error
                .as_ref()
                .is_some_and(|e| e.kind == qaqh_gate::ErrorKind::Authentication);
        if !needs_retry {
            return result;
        }
        log::warn!("[TURN] plan route got 401: force-refreshing ChatGPT token and retrying once");
        let refreshed = qaqh_siwc::manager()
            .map(|manager| manager.force_refresh(None))
            .is_some_and(|refresh| refresh.is_ok());
        if !refreshed {
            return result;
        }
        let Ok(rebuilt) = provider_for(ctx, turn_id) else {
            return result;
        };
        gate_request(
            ctx,
            &rebuilt,
            retry_messages.unwrap_or_default(),
            Some(ctx.agent.tool_defs.clone()),
            turn_id,
            round_num,
            last_usage,
        )
    }

    fn run_lap(
        &mut self,
        ctx: &mut RingContext,
        tool: &mut ToolEngine,
        turn_id: String,
        round_num: u32,
        last_usage: Option<UsageInfo>,
    ) -> Outcome {
        let turn_context =
            crate::agent::context::TurnContext::from_legacy(ctx, turn_id.clone(), round_num);
        log::info!("[TURN] run_lap turn_id={} round_num={}", turn_id, round_num);
        // L1 round-boundary durability: everything the previous round (or the
        // turn's user message at round 0) enqueued reaches messages.jsonl
        // here, before the next LLM request. The archive append path already
        // fsyncs, so a kill between rounds loses at most the round in flight
        // — and the L2 WAL covers even that.
        ctx.agent.drain_persist_ops();

        // MCP 回合边界 refresh（设计 §5.3 / PR-M1-5）：全局工具缓存脏标记命中
        // 才重建（连接后首次拉取 / crash 清缓存 / 后续 list 变化）。批次为
        // 全量重建语义：clear_dynamic + 逐条 register_dynamic（碰撞拒绝计数
        // 告警）。必须在本 actor 线程调用（thread-local manager）。
        if let Some(batch) = qaqh_mcp::take_projection_batch() {
            let rejected = qaqh_workspace::runtime::replace_dynamic_tools(batch);
            if rejected > 0 {
                log::warn!("[TURN] MCP dynamic refresh: {rejected} tool(s) rejected (collision)");
            }
            ctx.agent.refresh_tool_defs();
        }
        // LSP 投影（`lsp` 聚合工具 enabled 即在场；增量合并，不清 MCP 层）。
        if let Some(batch) = qaqh_lsp::take_projection_batch() {
            let rejected = qaqh_workspace::runtime::merge_dynamic_tools(batch);
            if rejected > 0 {
                log::warn!("[TURN] LSP dynamic refresh: {rejected} tool(s) rejected (collision)");
            }
            ctx.agent.refresh_tool_defs();
        }
        // PR-M2-2：MCP 资源清单注入块（回合边界刷新；内容变化才物化，
        // prefix cache 友好——与 skills envelope 同管线）。
        ctx.agent.sync_mcp_resource_injection(ctx.flow);
        // Rebuild provider from current config（gate_lap 准备逻辑，见 turn_lap::gate）
        // 构造失败（如套餐用量未登录/未授权）必须**显式失败**这个回合：
        // 回退成空 key 只会打出一个上游 401，把真正的原因埋掉。
        let provider = match provider_for(ctx, &turn_id) {
            Ok(provider) => provider,
            Err(message) => return Outcome::Error(message),
        };

        // 单回合执行块：所有路径均 return（clippy::never_loop），无需循环。
        {
            // ── Interrupt check ──
            if turn_context.runtime().cancellation().is_set() || qaqh_workspace::is_cancel() {
                ctx.emitter
                    .emit_timeline(qaqh_domain::TimelineIntent::TurnSealed {
                        turn_id: turn_id.clone(),
                        state: qaqh_domain::TimelineTurnState::Cancelled,
                        failure: None,
                    });
                return abort_running_turn(ctx, turn_id, last_usage);
            }
            if !ctx.pending.is_empty() {
                ctx.agent.msg.remove_last_step_if_incomplete();
                ctx.agent
                    .msg
                    .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
                return Outcome::Handled;
            }

            if round_num == 0 && crate::test_hooks::plan_review_enabled() {
                let call_id = format!("test-plan-review-{turn_id}");
                let content = crate::test_hooks::plan_review_content();
                let tool_call = qaqh_types::ToolCall {
                    id: call_id.clone(),
                    call_type: "function".into(),
                    function: qaqh_types::FunctionCall {
                        name: "plan_submit".into(),
                        arguments: serde_json::json!({ "plan": content }).to_string(),
                    },
                };
                let assistant_msg = crate::agent::util::build_assistant_message(
                    "",
                    "",
                    None,
                    std::slice::from_ref(&tool_call),
                );
                // Keep the test hook on the production transcript path: open
                // the tool block before the assistant step is admitted, then
                // let the normal plan-response handler update and seal it.
                ctx.emitter
                    .emit_timeline(qaqh_domain::TimelineIntent::BlockOpened {
                        turn_id: turn_id.clone(),
                        round_num,
                        block_id: format!("tool:{call_id}"),
                        kind: qaqh_domain::TimelineBlockKind::Tool,
                        reasoning_kind: None,
                        tool: Some(qaqh_domain::TimelineTool {
                            exit_code: None,
                            completed_at_ms: None,
                            tool_call_id: call_id.clone(),
                            name: tool_call.function.name.clone(),
                            state: qaqh_domain::TimelineToolState::Prepared,
                            summary: Some("plan_submit · prepared".into()),
                            args_json: Some(tool_call.function.arguments.clone()),
                            output: None,
                            diff: None,
                            progress: String::new(),
                            progress_truncated: false,
                            progress_stream: None,
                            progress_bytes_total: 0,
                            display: None,
                            failure: None,
                            permission: None,
                        }),
                    });
                let receipt = ctx.flow.ingest(
                    &mut ctx.agent.msg,
                    qaqh_message::builtin::MODEL,
                    assistant_msg.clone(),
                );
                if !receipt.stored {
                    log::warn!(
                        "[TURN] plan-review hook assistant step was not stored: {:?}",
                        ctx.flow.trace().back()
                    );
                }
                crate::agent::util::emit_round_complete_via_emitter(
                    ctx.emitter,
                    &turn_id,
                    round_num,
                    &assistant_msg,
                    "",
                    "",
                    std::slice::from_ref(&tool_call),
                );
                ctx.agent
                    .msg
                    .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);

                let mut pending_plans = VecDeque::new();
                pending_plans.push_back(PendingPlan {
                    call_id: call_id.clone(),
                    content: content.clone(),
                });
                self.suspended = Some(TurnState {
                    session_id: ctx.agent.session.session_id.clone(),
                    turn_id: turn_id.clone(),
                    round_num,
                    usage: last_usage.clone(),
                    pending_permission_ids: Vec::new(),
                    pending_permission_bodies: Vec::new(),
                    deferred_authorized: Vec::new(),
                    tool_call_order: vec![call_id.clone()],
                    serial_call_ids: HashSet::new(),
                    pending_asks: VecDeque::new(),
                    pending_plans,
                    pending_todo_activation: None,
                    reason: YieldReason::PlanReview,
                });
                ctx.emitter.emit_domain(qaqh_domain::DomainEvent::Control(
                    qaqh_domain::ControlEvent::PlanReviewRequested {
                        interaction_id: call_id,
                        turn_id: turn_id.clone(),
                        plan_content: content,
                        review_type: "plan".into(),
                        todo_items: None,
                    },
                ));
                return Outcome::YieldToUser {
                    turn_id,
                    reason: YieldReason::PlanReview,
                };
            }

            // ── 传输快照 + 估计（auto-compact 预检已随自动压缩禁用移除）──
            let (messages, request_estimate, request_fingerprint, request_key) =
                Self::prepare_gate_snapshot(ctx);

            let GateRequestResult {
                content,
                reasoning,
                reasoning_kind,
                tool_calls_raw,
                response_output_items,
                mut active_stream_block,
                mut timeline_tools_open,
                had_error,
                done_seen,
                gate_error,
                current_request_usage,
                request_error,
                stop_reason,
                last_usage,
            } = Self::gate_request_with_plan_auth_retry(
                ctx, &provider, messages, &turn_id, round_num, last_usage,
            );

            if turn_context.runtime().cancellation().is_set() {
                seal_timeline_terminal_round(
                    ctx,
                    &turn_id,
                    round_num,
                    active_stream_block.as_ref(),
                    &timeline_tools_open,
                    qaqh_domain::TimelineTurnState::Cancelled,
                    None,
                );
                return abort_running_turn(ctx, turn_id, last_usage);
            }

            if (had_error || request_error.is_some()) && !done_seen {
                // 失败槽 code 结构化：有 gate 结构化错误时用统一 ErrorKind 的
                // snake_case 码（timeline 消费方可以按码分支，不再对着无差别的
                // "model_request_failed" 猜）；仅本地构造失败（request_error，
                // 请求未到 gate）时保留历史字面量兜底。
                let failure_code = gate_error
                    .as_ref()
                    .map(|e| e.kind.as_str())
                    .unwrap_or("model_request_failed")
                    .to_string();
                let message = gate_error
                    .map(|e| e.message)
                    .or_else(|| request_error.clone())
                    .unwrap_or_else(|| "Model request failed".into());
                // 自动压缩禁用后（文件头注释），端点 context-overflow 400 不再
                // 触发本地强制压缩回收——直接整轮失败，由用户手动压缩后重试。
                log::info!(
                    "[TURN] run_lap turn_id={} round_num={} gate error or had_error={}",
                    turn_id,
                    round_num,
                    had_error
                );
                ctx.agent
                    .msg
                    .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);
                seal_timeline_terminal_round(
                    ctx,
                    &turn_id,
                    round_num,
                    active_stream_block.as_ref(),
                    &timeline_tools_open,
                    qaqh_domain::TimelineTurnState::Failed,
                    Some(qaqh_domain::TimelineFailure {
                        code: failure_code,
                        message: message.clone(),
                    }),
                );
                return Outcome::TurnFailed {
                    turn_id,
                    usage: last_usage,
                    message,
                };
            }
            if done_seen && (had_error || request_error.is_some()) {
                // 内容已完整流式输出后才报错（[DONE] 后断连/杂散错误）：
                // 不否定已完成的作答——"作答完成却 TurnFailed"会造成前端
                // 状态残留（markdown 不集中渲染、下一条消息带上一条）。
                log::warn!(
                    "[TURN] run_lap turn_id={} round_num={} gate error after Done (ignored): {}",
                    turn_id,
                    round_num,
                    gate_error
                        .map(|e| e.message)
                        .or(request_error.clone())
                        .unwrap_or_default()
                );
            }

            if let Some(usage) = current_request_usage {
                // 词元钱包读数：本回合 API 自己回报的输入量记进线程本地，
                // 随后每个工具结果落盘时盖章（`qaqh_types::context_usage`）。
                // 这是模型唯一能"读"到的上下文占用口径——此前它没有任何
                // 测量通道，只能拿"会话看起来很大"去估，并在 responses 端点
                // 的思考摘要回放下把自估变成"事实"。
                qaqh_types::context_usage::note_api_context_usage(
                    usage.prompt_tokens,
                    ctx.agent.config.context_length,
                );
                let accepted = ctx.agent.observe_prepared_request(
                    &request_fingerprint,
                    &request_key,
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

            // Persist replayable assistant context before scheduling recovery.
            // A recovery is a fresh request, not a provider token-cursor resume.
            let incomplete_stream = !had_error && request_error.is_none() && stop_reason.is_none();
            if incomplete_stream {
                let partial = super::stream_continuation::partial_message(
                    &content,
                    &reasoning,
                    reasoning_kind,
                    &tool_calls_raw,
                    response_output_items,
                );
                let failure = match partial {
                    Ok(message) => {
                        let receipt = ctx.flow.ingest(
                            &mut ctx.agent.msg,
                            qaqh_message::builtin::MODEL,
                            message,
                        );
                        ctx.agent.drain_persist_ops();
                        if !receipt.stored {
                            Some("Partial assistant response could not be stored")
                        } else if self.continuation_count >= MAX_STREAM_CONTINUATIONS {
                            Some("Stream recovery budget exhausted")
                        } else {
                            None
                        }
                    }
                    Err(message) => Some(message),
                };
                if let Some(message) = failure {
                    seal_timeline_terminal_round(
                        ctx,
                        &turn_id,
                        round_num,
                        active_stream_block.as_ref(),
                        &timeline_tools_open,
                        qaqh_domain::TimelineTurnState::Failed,
                        Some(qaqh_domain::TimelineFailure {
                            code: "stream_recovery_failed".into(),
                            message: message.into(),
                        }),
                    );
                    return Outcome::TurnFailed {
                        turn_id,
                        usage: last_usage,
                        message: message.into(),
                    };
                }
                self.continuation_count += 1;
                crate::agent::turn_lap::gate::seal_active_stream_block(
                    ctx,
                    &turn_id,
                    round_num,
                    &mut active_stream_block,
                );
                return Outcome::ContinueTurn {
                    turn_id,
                    round_num,
                    usage: last_usage,
                };
            } else {
                self.continuation_count = 0;
            }

            // ── Parse + push assistant message (turn_lap::parse) ──
            let crate::agent::turn_lap::parse::ParseOutput {
                parsed,
                assistant_msg,
                turn_completed: effect,
            } = crate::agent::turn_lap::parse::parse_and_ingest(
                ctx,
                &turn_id,
                round_num,
                &content,
                &reasoning,
                reasoning_kind,
                &tool_calls_raw,
                response_output_items,
                &mut active_stream_block,
                &mut timeline_tools_open,
            );
            // BUG-015 run_lap 级断言：parse 后的块级隔离在 parse::debug_assert 已验证；
            // 此处额外确保 lap 级“text↔tool 交替不重复回放” — timeline_tools_open 与 parsed 必须一致
            debug_assert!(
                timeline_tools_open.len() >= parsed.len(),
                "BUG-015 lap: timeline_tools_open {} < parsed {}",
                timeline_tools_open.len(),
                parsed.len()
            );
            let _ = (&parsed, &assistant_msg);

            if !effect {
                // ── Admit/dispatch 段已迁入 turn_lap::admit (knife-7 A2 step5) ──
                if let Some(outcome) = turn_admit::admit_and_dispatch(
                    ctx,
                    tool,
                    &mut self.actor,
                    &turn_context,
                    &turn_id,
                    round_num,
                    last_usage.clone(),
                    &active_stream_block,
                    &timeline_tools_open,
                    &mut self.suspended,
                ) {
                    return outcome;
                }

                // All tools from this round are now resolved → backfill/skills/ContinueTurn (knife-7 A2 step6)
                return turn_backfill::handle_tools_done(ctx, turn_id, round_num, last_usage);
            }

            // TurnComplete / fall-through → backfill/skills/ContinueTurn (knife-7 A2 step6)
            turn_backfill::handle_turn_complete(ctx, turn_id, round_num, last_usage)
        }
    }

    fn emit_completed_tool_round(
        &self,
        ctx: &mut RingContext,
        turn_id: &str,
        round_num: u32,
    ) -> Vec<qaqh_message::StepToolResult> {
        // Delegates to turn_lap::backfill (knife-7 A2 step6) — keeps handle_* sites thin
        // while sharing one verbatim implementation.
        turn_backfill::emit_completed_tool_round(ctx, turn_id, round_num)
    }

    /// Reset all turn state (called on Cancel / new session).
    pub fn reset(&mut self) {
        self.actor.reset();
        self.reset_runtime_state();
    }

    /// Clear the active turn runtime without erasing the actor's terminal
    /// result. Cancellation handlers use this so a later duplicate command can
    /// observe `AlreadyTerminal` instead of republishing a terminal.
    pub(crate) fn reset_runtime_state(&mut self) {
        self.suspended = None;
        self.continuation_count = 0;
    }

    pub fn take_suspended_for_abort(&mut self) -> Option<(String, Option<UsageInfo>)> {
        self.suspended.take().map(|state| {
            if let Err(error) = self.actor.cancel(&state.turn_id) {
                log::error!(
                    "[TURN] SessionActor rejected aborted turn {}: {error}",
                    state.turn_id
                );
            }
            (state.turn_id, state.usage)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    #[derive(Default)]
    struct RecordingEmitter {
        conversation: RefCell<Vec<(qaqh_domain::RoundDeltaKind, String)>>,
        timeline: RefCell<Vec<(String, String)>>,
    }

    impl crate::agent::types::Emitter for RecordingEmitter {
        fn emit_domain(&self, event: qaqh_domain::DomainEvent) {
            if let qaqh_domain::DomainEvent::Conversation(
                qaqh_domain::ConversationEvent::BlockCheckpoint { kind, text, .. },
            ) = event
            {
                self.conversation.borrow_mut().push((kind, text));
            }
        }

        fn emit_timeline(&self, intent: qaqh_domain::TimelineIntent) {
            if let qaqh_domain::TimelineIntent::BlockCheckpoint { block_id, text, .. } = intent {
                self.timeline.borrow_mut().push((block_id, text));
            }
        }
    }

    fn assert_block_scope_after_tool(
        kind: qaqh_domain::RoundDeltaKind,
        first_block_id: &str,
        second_block_id: &str,
        first_delta: &str,
        second_delta: &str,
    ) {
        use crate::agent::turn_lap::gate::{
            CHECKPOINT_TOKEN_INTERVAL, emit_stream_block_checkpoint, reset_stream_block_checkpoint,
        };
        let emitter = RecordingEmitter::default();
        let mut stream_block_id = None;
        let mut stream_block_text = String::new();
        let mut checkpoint_tokens = CHECKPOINT_TOKEN_INTERVAL - 1;
        let mut last_checkpoint_at = std::time::Instant::now();

        let first_round = first_delta.to_string();
        emit_stream_block_checkpoint(
            &emitter,
            "turn-1",
            0,
            first_block_id,
            kind,
            &first_round,
            first_delta,
            &mut stream_block_id,
            &mut stream_block_text,
            &mut checkpoint_tokens,
            &mut last_checkpoint_at,
        );

        // The tool block seals the first stream block before the next one opens.
        reset_stream_block_checkpoint(&mut stream_block_id, &mut stream_block_text);

        checkpoint_tokens = CHECKPOINT_TOKEN_INTERVAL - 1;
        let second_round = format!("{first_delta}{second_delta}");
        emit_stream_block_checkpoint(
            &emitter,
            "turn-1",
            0,
            second_block_id,
            kind,
            &second_round,
            second_delta,
            &mut stream_block_id,
            &mut stream_block_text,
            &mut checkpoint_tokens,
            &mut last_checkpoint_at,
        );

        checkpoint_tokens = CHECKPOINT_TOKEN_INTERVAL - 1;
        let final_round = format!("{second_round}-tail");
        emit_stream_block_checkpoint(
            &emitter,
            "turn-1",
            0,
            second_block_id,
            kind,
            &final_round,
            "-tail",
            &mut stream_block_id,
            &mut stream_block_text,
            &mut checkpoint_tokens,
            &mut last_checkpoint_at,
        );

        assert!(
            emitter.conversation.into_inner().is_empty(),
            "compatibility checkpoints must not duplicate the round body"
        );

        assert_eq!(
            emitter.timeline.into_inner(),
            vec![
                (first_block_id.to_string(), first_delta.to_string()),
                (second_block_id.to_string(), second_delta.to_string()),
                (second_block_id.to_string(), format!("{second_delta}-tail")),
            ]
        );
    }

    #[test]
    fn checkpoints_keep_round_text_but_replace_each_stream_block_locally() {
        assert_block_scope_after_tool(
            qaqh_domain::RoundDeltaKind::Answering,
            "round-0:text:0",
            "round-0:text:1",
            "answer-before-tool",
            "answer-after-tool",
        );
        assert_block_scope_after_tool(
            qaqh_domain::RoundDeltaKind::Thinking,
            "round-0:reasoning:0",
            "round-0:reasoning:1",
            "reasoning-before-tool",
            "reasoning-after-tool",
        );
    }
}
