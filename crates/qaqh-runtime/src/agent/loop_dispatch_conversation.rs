//! agent::loop::dispatch_conversation — Conversation 命令分派（消息/取消/undo/compact）。
//!
//! 由 `loop_core.rs` 拆分（Phase 2-5）：`impl Loop` 跨文件块，对外 API 不变。

use super::loop_core::Loop;
use super::types::*;

use super::injection::{Injection, InjectionPriority, InjectionSemantics, SUBAGENT_SOURCE};
use super::turn_actor::TurnCancellation;
use qaqh_domain::{ConversationCommand, DomainEvent};

impl Loop {
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
            } => {
                let input_id = message_id.as_deref().unwrap_or(command_id).to_string();
                if as_system {
                    // 统一注入入口：时序决策（compact 进行中 / turn 运行
                    // 中 / idle）全部由 inject() 负责；compact 窗口不拒绝
                    // 不丢弃（入 Deferred 队列，compact 完成后开新 turn），
                    // SubagentStatus 标签事件由 inject() 在入队成功后发射。
                    let injection = Injection {
                        session_id: session_id.to_string(),
                        command_id: command_id.to_string(),
                        input_id,
                        input_purpose,
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
                let actor_cancel = match turn_id.as_deref() {
                    Some(turn_id) => self.session.turn.cancel_turn(turn_id),
                    None => self.session.turn.cancel_active_turn(),
                };
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
