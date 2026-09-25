//! agent::loop::dispatch_conversation — Conversation 命令分派（消息/取消/undo/compact）。
//!
//! 由 `loop_core.rs` 拆分（Phase 2-5）：`impl Loop` 跨文件块，对外 API 不变。

use super::loop_core::Loop;
use super::types::*;

use super::injection::{Injection, InjectionPriority, InjectionSemantics, SUBAGENT_SOURCE};
use super::turn_actor::TurnCancellation;
use qaqh_domain::{
    ConversationCommand, DomainEvent, SubagentTerminalKind, SubagentTerminalNotification,
};
use qaqh_session::canonical::{generate_ulid, ulid_from_text};
use qaqh_session::session_fact_v2::{
    ActorKind, ActorRef, AgentPath, EventId, InputAccepted, InputId, InputKind, InputPurpose,
    InterAgentCommunication, InterAgentContent, InterAgentDelivery, MessageId, SessionId,
    SubagentFinished, SubagentTerminalStatus, ToolCallId,
};

impl Loop {
    pub(super) fn record_subagent_terminal(
        &mut self,
        notification: SubagentTerminalNotification,
    ) -> Result<(), String> {
        let status = match notification.terminal {
            SubagentTerminalKind::Completed => SubagentTerminalStatus::Completed,
            SubagentTerminalKind::Failed => SubagentTerminalStatus::Failed,
            SubagentTerminalKind::Cancelled => SubagentTerminalStatus::Cancelled,
            SubagentTerminalKind::TimedOut => SubagentTerminalStatus::TimedOut,
        };
        let now = super::state::agent::unix_ms();
        let ledger = self
            .session
            .agent
            .tool_ledger_mut()
            .map_err(|error| format!("canonical ledger unavailable for finish: {error}"))?
            .ok_or_else(|| "canonical ledger is not initialized for finish".to_string())?;
        ledger
            .ensure_lease(now, super::state::agent::tool_ledger_lease_ms())
            .map_err(|error| format!("canonical ledger lease failed for finish: {error}"))?;
        let payload = SubagentFinished {
            child_session_id: SessionId::new(notification.child_session_id),
            parent_call_id: ToolCallId::new(notification.parent_call_id),
            status,
            result_ref: None,
            finished_at_ms: now,
            recovery_ref: None,
        };
        ledger
            .append_subagent_finished(EventId::new(generate_ulid()), payload, now)
            .map_err(|error| format!("canonical finish append failed: {error}"))?;
        Ok(())
    }

    fn record_input_accepted(
        &mut self,
        input_id: &str,
        text: &str,
        purpose: qaqh_domain::ConversationInputPurpose,
        input_kind: InputKind,
        actor: ActorRef,
        client_request_id: Option<String>,
    ) -> Result<(), String> {
        // Canonical inline content is capped at 8 KiB. Larger payloads must be
        // externalized before this boundary; until that producer exists, do
        // not fabricate a dangling content_ref.
        if text.len() > 8 * 1024 {
            log::warn!(
                "[INPUT] canonical InputAccepted skipped for oversized input {input_id} ({} bytes)",
                text.len()
            );
            return Ok(());
        }
        let now = super::state::agent::unix_ms();
        let ledger = self
            .session
            .agent
            .tool_ledger_mut()
            .map_err(|error| format!("canonical ledger unavailable for input: {error}"))?;
        let Some(ledger) = ledger else {
            return Ok(());
        };
        ledger
            .ensure_lease(now, super::state::agent::tool_ledger_lease_ms())
            .map_err(|error| format!("canonical ledger lease failed for input: {error}"))?;
        let payload = InputAccepted {
            input_id: InputId::new(format!("input_{}", ulid_from_text(input_id))),
            input_kind,
            input_purpose: match purpose {
                qaqh_domain::ConversationInputPurpose::TriggerTurn => InputPurpose::TriggerTurn,
                qaqh_domain::ConversationInputPurpose::QueueOnly => InputPurpose::QueueOnly,
            },
            content_ref: None,
            inline_text: Some(text.to_string()),
            attachments: vec![],
            actor,
            client_request_id,
        };
        ledger
            .append_input_accepted(EventId::new(generate_ulid()), payload, now)
            .map_err(|error| format!("canonical input accept append failed: {error}"))?;
        Ok(())
    }

    fn record_inter_agent_communication(
        &mut self,
        envelope: &qaqh_domain::InterAgentEnvelope,
        text: &str,
    ) -> Result<(), String> {
        if text.len() > 8 * 1024 {
            return Err(format!(
                "inter-agent inline content is {} bytes; maximum is {}",
                text.len(),
                8 * 1024
            ));
        }
        let author = AgentPath::parse_absolute(&envelope.author)
            .map_err(|error| format!("invalid author path {}: {error}", envelope.author))?;
        let recipient = AgentPath::parse_absolute(&envelope.recipient)
            .map_err(|error| format!("invalid recipient path {}: {error}", envelope.recipient))?;
        let other_recipients = envelope
            .other_recipients
            .iter()
            .map(|path| {
                AgentPath::parse_absolute(path)
                    .map_err(|error| format!("invalid other recipient {path}: {error}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let delivery = match envelope.delivery {
            qaqh_domain::InterAgentDelivery::Queue => InterAgentDelivery::Queue,
            qaqh_domain::InterAgentDelivery::Trigger => InterAgentDelivery::Trigger,
            qaqh_domain::InterAgentDelivery::Interrupt => InterAgentDelivery::Interrupt,
        };
        let payload = InterAgentCommunication {
            message_id: MessageId::new(envelope.message_id.clone()),
            root_session_id: SessionId::new(envelope.root_session_id.clone()),
            author,
            recipient,
            other_recipients,
            task_id: envelope.task_id.clone(),
            content: InterAgentContent::Inline {
                text: text.to_string(),
            },
            reply_to: envelope.reply_to.clone().map(MessageId::new),
            causation_id: envelope.causation_id.clone().map(EventId::new),
            delivery,
            created_at_ms: envelope.created_at_ms,
        };
        let now = super::state::agent::unix_ms();
        let ledger =
            self.session.agent.tool_ledger_mut().map_err(|error| {
                format!("canonical ledger unavailable for communication: {error}")
            })?;
        let Some(ledger) = ledger else {
            return Ok(());
        };
        ledger
            .ensure_lease(now, super::state::agent::tool_ledger_lease_ms())
            .map_err(|error| format!("canonical ledger lease failed for communication: {error}"))?;
        ledger
            .append_inter_agent_communication(EventId::new(generate_ulid()), payload, now)
            .map_err(|error| format!("canonical communication append failed: {error}"))?;
        Ok(())
    }

    pub(super) fn on_conversation(
        &mut self,
        command: ConversationCommand,
        command_id: &str,
        session_id: &str,
    ) {
        match command {
            ConversationCommand::ConversationSendMessage {
                text,
                images,
                attachments: _,
                message_id,
                input_purpose,
                as_system,
                inter_agent,
                subagent_terminal,
            } => {
                if let Some(terminal) = subagent_terminal
                    && let Err(error) = self.record_subagent_terminal(terminal)
                {
                    self.emit_operation_failed(
                        command_id,
                        qaqh_domain::ErrorScope::Conversation,
                        "subagent_finish_append_failed",
                        &error,
                    );
                    return;
                }
                if text.is_empty() {
                    return;
                }
                let inter_agent = inter_agent.as_ref();
                let input_id = inter_agent
                    .map(|envelope| envelope.message_id.clone())
                    .or(message_id)
                    .unwrap_or_else(|| command_id.to_string());
                if let Some(envelope) = inter_agent
                    && let Err(error) = self.record_inter_agent_communication(envelope, &text)
                {
                    self.emit_operation_failed(
                        command_id,
                        qaqh_domain::ErrorScope::Conversation,
                        "inter_agent_communication_append_failed",
                        &error,
                    );
                    return;
                }
                let effective_purpose = match inter_agent.map(|envelope| envelope.delivery) {
                    Some(qaqh_domain::InterAgentDelivery::Queue) => {
                        qaqh_domain::ConversationInputPurpose::QueueOnly
                    }
                    Some(qaqh_domain::InterAgentDelivery::Trigger) | None => input_purpose,
                    Some(qaqh_domain::InterAgentDelivery::Interrupt) => {
                        self.emit_operation_failed(
                            command_id,
                            qaqh_domain::ErrorScope::Conversation,
                            "inter_agent_interrupt_not_implemented",
                            "Interrupt delivery is not implemented yet",
                        );
                        return;
                    }
                };
                let (input_kind, actor, client_request_id) = if let Some(envelope) = inter_agent {
                    (
                        InputKind::UserText,
                        ActorRef {
                            kind: ActorKind::Subagent,
                            id: envelope.author.clone(),
                            display_name: None,
                        },
                        Some(envelope.message_id.clone()),
                    )
                } else if as_system {
                    (
                        InputKind::System,
                        ActorRef {
                            kind: ActorKind::System,
                            id: "system".to_string(),
                            display_name: None,
                        },
                        Some(input_id.clone()),
                    )
                } else {
                    (
                        InputKind::UserText,
                        ActorRef {
                            kind: ActorKind::User,
                            id: "user".to_string(),
                            display_name: None,
                        },
                        Some(input_id.clone()),
                    )
                };

                let injection_path = as_system
                    || inter_agent.is_some_and(|envelope| {
                        envelope.delivery == qaqh_domain::InterAgentDelivery::Queue
                    });
                if injection_path {
                    if let Err(error) = self.record_input_accepted(
                        &input_id,
                        &text,
                        effective_purpose,
                        input_kind,
                        actor,
                        client_request_id,
                    ) {
                        self.emit_operation_failed(
                            command_id,
                            qaqh_domain::ErrorScope::Conversation,
                            "input_accept_append_failed",
                            &error,
                        );
                        return;
                    }
                    // 统一注入入口：时序决策（compact 进行中 / turn 运行
                    // 中 / idle）全部由 inject() 负责；compact 窗口不拒绝
                    // 不丢弃（入 Deferred 队列，compact 完成后开新 turn），
                    // SubagentStatus 标签事件由 inject() 在入队成功后发射。
                    let injection = Injection {
                        session_id: session_id.to_string(),
                        command_id: command_id.to_string(),
                        input_id,
                        input_purpose: effective_purpose,
                        source: SUBAGENT_SOURCE,
                        role: qaqh_types::Message::ROLE_USER,
                        text,
                        priority: InjectionPriority::Normal,
                        semantics: InjectionSemantics::NextTurn,
                    };
                    if let Some(outcome) = self.inject(injection) {
                        self.apply_outcome(outcome);
                    }
                    return;
                }
                if self.session.agent.manual_compact_running() {
                    self.emit_operation_failed(
                        command_id,
                        qaqh_domain::ErrorScope::Conversation,
                        "compact_in_progress",
                        "Context compaction is running; wait for it to finish before sending a new message",
                    );
                    return;
                }
                // images 已是 domain ImageBlock（Ringing 命令直接携带）。
                // as_system=true 已在上方走注入通道；这里只处理用户输入。
                // H1：挂起 turn 被新用户输入取代——显式中止并分离旧悬空
                // 状态（镜像 undo_conflict 守卫），否则迟到 grant 会在新
                // turn 中间恢复旧 turn（幽灵 lap + 错位 backfill）。
                if self.session.turn.is_suspended() {
                    let mut abort_ctx = RingContext {
                        agent: &mut self.session.agent,
                        emitter: &self.paced_emitter,
                        cancel: &self.cancel,
                        phase: &mut self.phase,
                        pending: &mut self.pending,
                        writer_dead: &self.writer_dead,
                        stats: &mut self.session.stats,
                        flow: &mut self.flow,
                    };
                    if let Some(stale_turn_id) = self.session.turn.abort_suspended(&mut abort_ctx) {
                        log::warn!(
                            "[INPUT] suspended turn {stale_turn_id} superseded by newer user input"
                        );
                        self.session.agent.msg.remove_last_step_if_incomplete();
                    }
                }
                if let Err(error) = self.record_input_accepted(
                    &input_id,
                    &text,
                    effective_purpose,
                    input_kind,
                    actor,
                    client_request_id,
                ) {
                    self.emit_operation_failed(
                        command_id,
                        qaqh_domain::ErrorScope::Conversation,
                        "input_accept_append_failed",
                        &error,
                    );
                    return;
                }
                let mut ctx = RingContext {
                    agent: &mut self.session.agent,
                    emitter: &self.paced_emitter,
                    cancel: &self.cancel,
                    phase: &mut self.phase,
                    pending: &mut self.pending,
                    writer_dead: &self.writer_dead,
                    stats: &mut self.session.stats,
                    flow: &mut self.flow,
                };
                let outcome = self.input.handle_user_input(
                    &mut ctx,
                    &mut self.session.turn,
                    qaqh_message::builtin::USER,
                    &input_id,
                    &text,
                    images,
                );
                let _ = ctx;
                self.apply_outcome(outcome);
            }
            ConversationCommand::ConversationCancel { turn_id } => {
                self.cancel.set();
                qaqh_workspace::set_cancel(true);
                let cancel_call_ids = {
                    let mut call_ids = Vec::new();
                    if let Some(suspended) = self.session.turn.suspended.as_ref() {
                        call_ids.extend(suspended.pending_permission_ids.iter().cloned());
                        call_ids
                            .extend(suspended.pending_asks.iter().map(|ask| ask.call_id.clone()));
                        call_ids.extend(
                            suspended
                                .pending_plans
                                .iter()
                                .map(|plan| plan.call_id.clone()),
                        );
                        if let Some(todo) = &suspended.pending_todo_activation {
                            call_ids.push(todo.call_id.clone());
                        }
                        call_ids.extend(suspended.tool_call_order.iter().cloned());
                    }
                    call_ids.extend(
                        self.session
                            .agent
                            .msg
                            .get_last_step_pending()
                            .into_iter()
                            .map(|pending| pending.id),
                    );
                    call_ids.sort();
                    call_ids.dedup();
                    call_ids
                };
                let actor_cancel = self.session.turn.cancel_with_ledger(
                    &mut self.session.agent,
                    turn_id.as_deref(),
                    cancel_call_ids,
                );
                let actor_cancel_rejected = actor_cancel.is_err();
                let emit_terminal = match actor_cancel {
                    Ok(TurnCancellation::Interrupted { reason }) => {
                        log::debug!(
                            "[CANCEL] SessionActor interrupted active turn with {reason:?}"
                        );
                        true
                    }
                    Ok(TurnCancellation::Idle) => true,
                    Ok(TurnCancellation::AlreadyTerminal) => false,
                    Err(error) => {
                        log::error!("[CANCEL] SessionActor rejected cancellation: {error}");
                        true
                    }
                };
                // BUG-2026-09-13-08：取消不得留下「有 tool_use 无 tool_result」
                // 的孤儿 step —— 下轮模型会重发同一 tool_use，已执行过的工具
                // 被重复执行（挂起→批准→取消正是触发窗口）。
                //
                // 已在执行的批由 `execute_admitted_batch` 的取消收割路径回填
                // 真实结果（副作用已发生）；这里只兜底给「永远不会有结果」的
                // tool_use 补取消终态，再丢弃/保留 step 交给既有收尾。
                let sealed = self.session.agent.msg.seal_pending_tools_as_cancelled();
                eprintln!(
                    "DBG cancel-seal sealed={sealed} pending_save_after={}",
                    self.session.agent.msg.last_step_tool_results().len()
                );
                if sealed > 0 {
                    log::info!(
                        "[CANCEL] sealed {sealed} pending tool_use(s) as cancelled                          before dropping the incomplete step"
                    );
                }
                self.session.agent.msg.remove_last_step_if_incomplete();
                self.session.agent.msg.flush_meta(
                    &self.session.agent.config.model,
                    &self.session.agent.config.reasoning_effort,
                );
                if actor_cancel_rejected {
                    self.reset_all_engines();
                } else {
                    self.reset_all_engines_preserving_turn_terminal();
                }
                if emit_terminal {
                    self.paced_emitter.emit_domain(DomainEvent::Conversation(
                        qaqh_domain::ConversationEvent::ConversationCancelled { turn_id },
                    ));
                }
            }
            ConversationCommand::ConversationUndoTurn { turn_id } => {
                // 与 legacy UndoTurn 语义对齐：活动回合被挂起（ask/权限/plan
                // 未决）时拒绝 undo，避免跨引擎状态不一致。
                if self
                    .session
                    .turn
                    .suspended_turn_id()
                    .is_some_and(|active_turn_id| active_turn_id != turn_id)
                {
                    self.emit_operation_failed(
                        command_id,
                        qaqh_domain::ErrorScope::Conversation,
                        "undo_conflict",
                        &format!("Cannot undo {turn_id}: a different active turn is suspended"),
                    );
                    return;
                }
                self.session.turn.reset();
                self.session.tool.clear_pending();
                self.misc.handle_undo(&mut self.session.agent, &turn_id);
                self.emit_operation_completed(command_id, qaqh_domain::ErrorScope::Conversation);
            }
            ConversationCommand::ConversationSetMode { mode } => {
                let mode = match mode {
                    qaqh_domain::ConversationMode::Plan => "plan",
                    qaqh_domain::ConversationMode::Code => "code",
                };
                self.misc.set_mode(&mut self.session.agent, mode);
                self.emit_operation_completed(command_id, qaqh_domain::ErrorScope::Conversation);
            }
            ConversationCommand::ConversationCompact { .. } => {
                let outcome = self.start_compact(Some(command_id.to_string()));
                self.apply_outcome(outcome);
            }
            ConversationCommand::ConversationLoadMore { .. } => {
                self.emit_operation_failed(
                    command_id,
                    qaqh_domain::ErrorScope::Conversation,
                    "unsupported_command",
                    "Ringing v1 bootstrap already contains complete persisted history",
                );
            }
        }
    }
}
