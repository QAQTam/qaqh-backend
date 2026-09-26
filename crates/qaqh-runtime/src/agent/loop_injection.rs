//! agent::loop::injection — 注入通道（as_system 注入/injection_bus/compact 协同）。
//!
//! 由 `loop_core.rs` 拆分（Phase 2-5）：`impl Loop` 跨文件块，对外 API 不变。

use super::injection::{
    EnqueueResult, Injection, InjectionPriority, InjectionSemantics, MAX_INTERJECT_PER_SAFE_POINT,
    MAX_STEER_PER_SAFE_POINT, SUBAGENT_SOURCE,
};
use super::loop_core::Loop;
use super::loop_core::parse_subagent_status_tag;
use super::types::*;

impl Loop {
    // ═══════════════════════════════════════════════════
    // Interrupt polling (called by engines during long ops)
    // ═══════════════════════════════════════════════════

    /// 见缝插针注入消费者（回合 lap 边界调用）：从 cmd_rx 吸收排队的
    /// `as_system` 注入（子代理报告等）到 InjectionBus，由调用方随后在
    /// lap 边界交给 ContextFlow 落盘 trailing。
    ///
    /// 时机保证（PLAN-FIX-INJECTION-CACHE ②）：只在工具回合完成后的
    /// lap 边界被调用——此时本轮 tool_call 与其 tool_result 均已提交
    /// （工具执行是同步阻塞的），注入取号必然排在本轮全部结果之后，绝不
    /// 夹在 assistant(toolcall) 与其 tool_result 之间。注入以 user +
    /// name=subagent 角色落盘（chat/responses 两协议的对话流主体，可见性
    /// 保证）。
    pub(super) fn is_injection_command(cmd: &super::types::WorkerCommand) -> bool {
        matches!(
            &cmd.frame,
            env
                if matches!(
                    &env.command,
                    qaqh_ringing::RingingCommand::Conversation(
                        qaqh_domain::ConversationCommand::ConversationSendMessage {
                            as_system: true,
                            ..
                        }
                    )
                )
        )
    }

    /// Drop all pending injections when the active session is replaced.
    /// Compact's legacy deferred queue is filtered here as well; otherwise a
    /// report accepted before the switch could be dispatched into the new
    /// session after compact finishes.
    pub(super) fn clear_injections(&mut self) {
        self.injection_bus.clear();
        let before = self.deferred_ringing.len();
        self.deferred_ringing
            .retain(|cmd| !Self::is_injection_command(cmd));
        let dropped = before.saturating_sub(self.deferred_ringing.len());
        if dropped > 0 {
            log::info!("[INJECT] dropped {dropped} deferred injection(s) on session switch");
        }
    }

    pub(super) fn injection_session_matches(&self, command_session_id: &str) -> bool {
        let current_session_id = &self.session.agent.session.seed;
        current_session_id.is_empty()
            || command_session_id.is_empty()
            || command_session_id == current_session_id
    }

    /// Emit the subagent terminal-state tag event after a successful enqueue.
    /// Only `SUBAGENT_SOURCE` injections carry the tag contract; future
    /// sources may reuse this hook with their own event vocabulary.
    pub(super) fn emit_subagent_status(&self, session_id: &str, source: &str, text: &str) {
        if source != SUBAGENT_SOURCE {
            return;
        }
        if let Some((name, state)) = parse_subagent_status_tag(text) {
            self.paced_emitter
                .emit_domain(qaqh_domain::DomainEvent::Control(
                    qaqh_domain::ControlEvent::SubagentStatus {
                        seed: session_id.to_string(),
                        name,
                        state,
                    },
                ));
        }
    }

    /// 吸收注入到总线（busy 路径）：session 作用域校验 + command_id 幂等。
    /// 入队成功后立即发射 subagent 状态标签事件（保持现有时机）。
    pub(super) fn absorb_injection(&mut self, mut injection: Injection) {
        if !self.injection_session_matches(&injection.session_id) {
            log::warn!(
                "[INJECT] rejected injection for stale session (command={}, current={})",
                injection.session_id,
                self.session.agent.session.seed
            );
            return;
        }
        let session_id = self.session.agent.session.seed.clone();
        if session_id.is_empty() {
            log::warn!(
                "[INJECT] rejected injection without an active session (command_id={})",
                injection.command_id
            );
            return;
        }
        let text_len = injection.text.len();
        let command_id = injection.command_id.clone();
        let text = injection.text.clone();
        let source = injection.source;
        // 总线以当前 session 为作用域（command 携带的 session 仅用于陈旧性校验）。
        injection.session_id = session_id.clone();
        self.injection_bus.switch_session(&session_id);
        match self.injection_bus.enqueue(injection) {
            EnqueueResult::Queued => {
                log::info!(
                    "[INJECT] injection queued via InjectionBus (seed={}, text_len={}, pending={})",
                    session_id,
                    text_len,
                    self.injection_bus.pending_len()
                );
                // Absorbed injections do not create a turn of their own, so
                // keep the existing lightweight tracker convergence signal.
                self.emit_subagent_status(&session_id, source, &text);
            }
            EnqueueResult::DuplicateCommandId | EnqueueResult::DuplicateInputId => {
                log::info!(
                    "[INJECT] duplicate injection ignored (seed={}, command_id={})",
                    session_id,
                    command_id
                );
            }
            EnqueueResult::StaleSession => {
                log::warn!(
                    "[INJECT] rejected injection for stale/empty session (seed={}, command_id={})",
                    session_id,
                    command_id
                );
            }
        }
    }

    /// Keep the idle path's existing immediate turn semantics while using the
    /// bus to claim the command id exactly once for this session.
    pub(super) fn claim_injection(&mut self, injection: &Injection, command_id: &str) -> bool {
        if self.session.agent.session.seed.is_empty() {
            return true;
        }
        if !self.injection_session_matches(&injection.session_id) {
            log::warn!(
                "[INJECT] ignored idle injection for stale session (command={}, current={})",
                injection.session_id,
                self.session.agent.session.seed
            );
            return false;
        }
        let session_id = self.session.agent.session.seed.clone();
        self.injection_bus.switch_session(&session_id);
        let mut claimed = injection.clone();
        claimed.session_id = session_id.clone();
        match self.injection_bus.enqueue(claimed) {
            EnqueueResult::Queued => {
                self.emit_subagent_status(&session_id, injection.source, &injection.text);
                let _ = self.injection_bus.drain();
                true
            }
            EnqueueResult::DuplicateCommandId | EnqueueResult::DuplicateInputId => {
                log::info!(
                    "[INJECT] duplicate idle injection ignored: command={command_id}, input={}",
                    injection.input_id
                );
                false
            }
            EnqueueResult::StaleSession => false,
        }
    }

    /// 统一注入入口（刀 7 第一阶段）：所有非用户命令的消息注入
    /// （subagent 报告，未来 system/MCP）都经此进入 Loop。
    ///
    /// 时机决策：
    /// - compact 进行中 → 入总线（priority 记为 Deferred），compact 完成后
    ///   idle 再逐条开新 turn（`dispatch_injections_after_compact`）；
    /// - turn 运行中（phase != Idle）→ 入总线，lap 边界由 `drain_injections`
    ///   落盘进当前 turn；
    /// - idle → 占 command_id 后立即经 `handle_system_input` 开新 turn。
    ///
    /// 会话作用域校验（stale/空 session → 拒绝 + 日志）与 command_id 幂等
    /// 均由总线承担。返回 Some(outcome) 表示已开 turn（由调用方
    /// apply_outcome），None 表示入队等待或被拒绝。
    pub fn inject(&mut self, injection: Injection) -> Option<Outcome> {
        let command_id = injection.command_id.clone();
        let text = injection.text.clone();
        let queue_only = matches!(
            injection.input_purpose,
            qaqh_domain::ConversationInputPurpose::QueueOnly
                | qaqh_domain::ConversationInputPurpose::Steer
                | qaqh_domain::ConversationInputPurpose::Interject
        );

        if queue_only {
            self.absorb_injection(injection);
            return None;
        }

        if self.session.agent.manual_compact_running() {
            let mut deferred = injection;
            deferred.priority = InjectionPriority::Deferred;
            self.absorb_injection(deferred);
            return None;
        }

        match self.phase {
            LoopPhase::Idle => {
                // 取消门置位期间，系统注入不得清取消标记、不得开新回合——
                // 那会把已取消的会话复活（新的 `TurnStart`）。注入改为入总线
                // 排队，等用户动作（用户输入/ToolInvoke/会话切换）复位 token
                // 后由 lap 边界落盘，既不丢子代理结果，也不复活会话。
                if self.cancel.is_set() {
                    log::warn!(
                        "[INJECT] session is user-cancelled; system injection queued without opening a turn (command_id={command_id})"
                    );
                    self.absorb_injection(injection);
                    return None;
                }
                if !self.claim_injection(&injection, &command_id) {
                    return None;
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
                // 进入该注入命令的 causation 作用域：开 turn 期间发射的事件
                // 必须归属到注入者的 command_id（与 dispatch_deferred_ringing /
                // 其它单命令派发路径一致）。
                let _scope = self
                    .paced_emitter
                    .enter_causation(Some(command_id.as_str()));
                let outcome = self.input.handle_system_input(
                    &mut ctx,
                    &mut self.session.turn,
                    &injection.input_id,
                    &text,
                    Some(command_id.as_str()),
                );
                let _ = ctx;
                Some(outcome)
            }
            _ => {
                self.absorb_injection(injection);
                None
            }
        }
    }

    /// Hand bus records to ContextFlow only at a lap boundary. ContextFlow
    /// remains responsible for the actual store write and write ordering.
    pub(super) fn drain_injections(&mut self) {
        let session_id = self.session.agent.session.seed.clone();
        self.injection_bus.switch_session(&session_id);
        let records = self
            .injection_bus
            .drain_limited(MAX_STEER_PER_SAFE_POINT, MAX_INTERJECT_PER_SAFE_POINT);
        if records.is_empty() {
            return;
        }

        let mut submitted = 0;
        for record in records {
            if record.session_id != session_id {
                log::warn!(
                    "[INJECT] skipped injection from stale session (record={}, current={})",
                    record.session_id,
                    session_id
                );
                continue;
            }
            let message = record.message();
            let input_id = record.input_id;
            match self
                .flow
                .submit(qaqh_message::builtin::SUBAGENT, message, Some(input_id))
            {
                Ok(()) => submitted += 1,
                Err(e) => log::error!("[INJECT] ContextFlow submit failed: {e}"),
            }
        }
        if submitted == 0 {
            return;
        }

        let model = self.session.agent.config.model.clone();
        let effort = self.session.agent.config.reasoning_effort.clone();
        let (drained, _) =
            self.flow
                .drain_turn_boundary(&mut self.session.agent.msg, &model, &effort);
        if drained > 0 {
            log::info!("[INJECT] lap boundary drained {drained} injection(s) via ContextFlow");
        }
    }

    pub fn drain_pending_injections(&mut self) {
        use qaqh_domain::ConversationCommand;
        use qaqh_ringing::RingingCommand;
        self.injection_bus
            .switch_session(&self.session.agent.session.seed);
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            let env = cmd.frame;
            match &env.command {
                // ── 注入命令：as_system 消息（子代理报告等）──────────
                // 统一走 Loop::inject()（turn 运行中 → 入总线，lap 边界
                // 再由 drain_injections 交给 ContextFlow）。
                RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
                    text,
                    message_id,
                    input_purpose,
                    as_system: true,
                    subagent_terminal,
                    ..
                }) => {
                    if let Some(notification) = subagent_terminal
                        && let Err(error) = self.record_subagent_terminal(notification.clone())
                    {
                        self.emit_operation_failed(
                            &env.command_id,
                            qaqh_domain::ErrorScope::Conversation,
                            "subagent_finish_append_failed",
                            &error,
                        );
                        continue;
                    }
                    if text.is_empty() {
                        continue;
                    }
                    let input_id = message_id.clone().unwrap_or_else(|| env.command_id.clone());
                    let injection = Injection {
                        session_id: env.seed.clone(),
                        command_id: env.command_id.clone(),
                        input_id,
                        input_purpose: *input_purpose,
                        source: SUBAGENT_SOURCE,
                        role: qaqh_types::Message::ROLE_USER,
                        text: text.clone(),
                        priority: injection_priority(*input_purpose),
                        semantics: InjectionSemantics::NextTurn,
                    };
                    let _ = self.inject(injection);
                }
                // ── 其它命令（用户消息/非注入）：绝不丢弃 ────────────────
                // daemon 侧已 ACK（accepted），静默丢弃会让调用方永久悬挂。
                // 放入 deferred_ringing，主循环 idle 时按 FIFO 派发（复用
                // 既有 session 切换保留队列，语义一致）。
                _ => {
                    self.deferred_ringing
                        .push_back(super::types::WorkerCommand {
                            frame: env,
                            causation: cmd.causation,
                        });
                }
            }
        }
    }

    // ═══════════════════════════════════════════════════
}

pub(super) fn injection_priority(
    purpose: qaqh_domain::ConversationInputPurpose,
) -> InjectionPriority {
    match purpose {
        qaqh_domain::ConversationInputPurpose::TriggerTurn
        | qaqh_domain::ConversationInputPurpose::QueueOnly => InjectionPriority::Normal,
        qaqh_domain::ConversationInputPurpose::Steer => InjectionPriority::Steer,
        qaqh_domain::ConversationInputPurpose::Interject => InjectionPriority::Interject,
    }
}

#[cfg(test)]
mod tests {
    //! T-1-3：取消门（`CancelToken`）的语义回归。
    //!
    //! 未修复代码上 `system_injection_is_rejected_after_user_cancel` 红：
    //! 取消后的系统注入仍走 idle 分支 `handle_system_input`（清取消标记 +
    //! `TurnStarted`），把用户已取消的会话复活。

    use super::*;
    use crate::agent::loop_core::LoopChannels;
    use crate::agent::types::{CancelToken, WriterEvent};
    use std::sync::Arc;

    const SESSION: &str = "t13-parent-seed";

    /// 测试线程上构造 Loop（与 `loop_core.rs` 的 drain/dispatch 测试同构）。
    /// 事件端由调用方持有：paced emitter 同步写入，故可直接断言「哪些事件被
    /// 发出」——本用例要锁定的正是「取消后不再出现 TurnStart」。
    fn test_loop() -> (Loop, std::sync::mpsc::Receiver<WriterEvent>) {
        let channels = LoopChannels::new();
        let event_rx = channels.event_rx;
        let mut agent = crate::agent::state::agent::AgentState::new(qaqh_config::Config::default());
        agent.ephemeral = true;
        agent.session_manager = None;
        agent.session.seed = SESSION.to_string();
        let lp = Loop::from_channels(
            agent,
            channels.cmd_rx,
            channels.event_tx,
            CancelToken::new(),
            channels.writer_dead,
            Arc::new(crate::agent::liveness::WorkerLiveness::new()),
            None,
        );
        (lp, event_rx)
    }

    /// 排空事件通道并返回（paced emitter 同步写入，无异步窗口）。
    fn drain_events(rx: &std::sync::mpsc::Receiver<WriterEvent>) -> Vec<WriterEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    fn turn_starts(events: &[WriterEvent]) -> usize {
        events
            .iter()
            .filter(|event| match event {
                WriterEvent::Ringing(env) => matches!(
                    env.event,
                    qaqh_ringing::RingingEvent::Conversation(
                        qaqh_domain::ConversationEvent::TurnStarted { .. }
                    )
                ),
                WriterEvent::Timeline(_) => false,
            })
            .count()
    }

    fn cancel(lp: &mut Loop) {
        lp.on_conversation(
            qaqh_domain::ConversationCommand::ConversationCancel { turn_id: None },
            "cmd-cancel",
            SESSION,
        );
    }

    /// 用户取消后，系统注入（子代理报告）被拒：不开新回合，且注入不丢。
    #[test]
    fn system_injection_is_rejected_after_user_cancel() {
        // 会话键控取消表：让 `set_cancel(true)` 落在会话表而非进程级 flag，
        // 避免污染同进程内并行运行的其它单元测试。
        qaqh_workspace::runtime::set_context(SESSION, 4);
        let (mut lp, event_rx) = test_loop();
        cancel(&mut lp);
        assert!(lp.cancel.is_set(), "ConversationCancel 必须置位取消 token");
        // 取消自身会 emit ConversationCancelled；先确认事件通道确实可观测
        // （否则「没有 TurnStart」的断言会空转），再清空基线观察注入。
        let cancelled = drain_events(&event_rx);
        assert!(
            cancelled.iter().any(|event| matches!(
                event,
                WriterEvent::Ringing(env)
                    if matches!(
                        env.event,
                        qaqh_ringing::RingingEvent::Conversation(
                            qaqh_domain::ConversationEvent::ConversationCancelled { .. }
                        )
                    )
            )),
            "前置条件：取消事件必须可观测，实测 {cancelled:?}"
        );

        let outcome = lp.inject(Injection::new(
            SESSION,
            "cmd-inject",
            "[SUBAGENT 'late' COMPLETED]\nlate subagent finished",
        ));

        assert!(
            outcome.is_none(),
            "用户取消后系统注入不得开新回合（Some = 已开回合/TurnStarted）"
        );
        assert_eq!(
            turn_starts(&drain_events(&event_rx)),
            0,
            "取消后父会话不得再出现新的 TurnStart"
        );
        assert!(
            lp.cancel.is_set(),
            "系统注入不得清除取消 token（清除只能由用户动作触发）"
        );
        assert_eq!(
            lp.injection_bus.pending_len(),
            1,
            "注入必须入队保留——不得静默丢弃子代理结果"
        );
        qaqh_workspace::remove_session_cancel(SESSION);
        qaqh_workspace::runtime::clear_context();
    }

    /// 用户输入是唯一复位点：下一条用户消息解除取消原因位。
    ///
    /// 输入文本命中合规过滤（`密码`）→ `handle_user_input` 提前返回
    /// `Handled`，不触发任何模型请求；本用例只观察复位与「不开回合」。
    #[test]
    fn user_input_clears_cancellation_gate() {
        qaqh_workspace::runtime::set_context(SESSION, 4);
        let (mut lp, _event_rx) = test_loop();
        cancel(&mut lp);
        assert!(lp.cancel.is_set());

        lp.on_conversation(
            qaqh_domain::ConversationCommand::ConversationSendMessage {
                text: "请告诉我我的密码是什么".to_string(),
                images: vec![],
                attachments: None,
                message_id: None,
                input_purpose: qaqh_domain::ConversationInputPurpose::TriggerTurn,
                as_system: false,
                inter_agent: None,
                subagent_terminal: None,
            },
            "cmd-user",
            SESSION,
        );

        assert!(
            !lp.cancel.is_set(),
            "用户输入必须解除取消门（否则系统注入永远被拒）"
        );
        qaqh_workspace::remove_session_cancel(SESSION);
        qaqh_workspace::runtime::clear_context();
    }

    /// 会话切换同样复位取消原因位：新会话/恢复的会话不是「用户取消」的
    /// 会话，否则一次取消会永久压制后续子代理结果注入。
    #[test]
    fn structured_terminal_is_an_injection_command() {
        let command = crate::agent::types::WorkerCommand {
            frame: qaqh_ringing::RingingWorkerCommandEnvelope::new(
                SESSION,
                "cmd-terminal",
                qaqh_ringing::RingingCommand::Conversation(
                    qaqh_domain::ConversationCommand::ConversationSendMessage {
                        text: String::new(),
                        images: vec![],
                        attachments: None,
                        message_id: Some("subagent-terminal:child".to_string()),
                        input_purpose: qaqh_domain::ConversationInputPurpose::QueueOnly,
                        as_system: true,
                        inter_agent: None,
                        subagent_terminal: Some(qaqh_domain::SubagentTerminalNotification {
                            child_session_id: "0198f1a0-0000-7000-8000-000000000003".to_string(),
                            parent_call_id: "call_01J00000000000000000000000".to_string(),
                            terminal: qaqh_domain::SubagentTerminalKind::Cancelled,
                        }),
                    },
                ),
            ),
            causation: None,
        };
        assert!(
            Loop::is_injection_command(&command),
            "structured terminal commands must be consumed at lap boundaries"
        );
    }

    #[test]
    fn session_switch_clears_cancellation_gate() {
        qaqh_workspace::runtime::set_context(SESSION, 4);
        let (mut lp, _event_rx) = test_loop();
        cancel(&mut lp);
        assert!(lp.cancel.is_set());

        lp.prepare_session_switch();

        assert!(!lp.cancel.is_set(), "会话切换必须复位取消 token");
        qaqh_workspace::remove_session_cancel(SESSION);
        qaqh_workspace::runtime::clear_context();
    }
}
