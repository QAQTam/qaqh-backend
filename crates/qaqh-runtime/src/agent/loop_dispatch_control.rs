//! agent::loop::dispatch_control — Control 命令分派（会话/技能/交互/模式）。
//!
//! 由 `loop_core.rs` 拆分（Phase 2-5）：`impl Loop` 跨文件块，对外 API 不变。

use super::loop_core::Loop;
use super::turn_actor::{InteractionAdmission, InteractionState};
use super::types::*;

use qaqh_domain::{ControlCommand, DomainEvent};
use qaqh_session::canonical::{causation_for_command, generate_ulid};
use qaqh_session::session_fact_v2::EventId;
use qaqh_session::session_fact_v2::ActorRef;

use crate::agent::state::agent::{tool_ledger_lease_ms, unix_ms};

impl Loop {
    fn reject_already_resolved_interaction(
        &mut self,
        command_id: &str,
        interaction_id: &str,
        kind: &str,
    ) -> bool {
        if self.session.turn.interaction_state(interaction_id) != InteractionState::AlreadyResolved
        {
            return false;
        }
        self.emit_operation_failed(
            command_id,
            qaqh_domain::ErrorScope::Control,
            "interaction_already_resolved",
            &format!("{kind} interaction was already resolved"),
        );
        true
    }

    fn admit_legacy_interaction_resolution(
        &mut self,
        command_id: &str,
        interaction_id: &str,
        outcome: &Outcome,
    ) -> bool {
        // Handled means the legacy validator rejected the command. In that
        // case the interaction must remain pending so a valid retry can win.
        if matches!(outcome, Outcome::Handled) {
            return true;
        }
        match self
            .session
            .turn
            .admit_interaction_resolution(interaction_id)
        {
            InteractionAdmission::Accepted { remaining } => {
                log::debug!(
                    "[INTERACTION] actor accepted {interaction_id}; {remaining} interaction(s) remain"
                );
                true
            }
            InteractionAdmission::Unknown => {
                // Process recovery or legacy state may predate the actor
                // registry; the legacy handler remains authoritative.
                true
            }
            InteractionAdmission::AlreadyResolved => {
                self.emit_operation_failed(
                    command_id,
                    qaqh_domain::ErrorScope::Control,
                    "interaction_already_resolved",
                    "interaction was already resolved",
                );
                false
            }
        }
    }

    pub(super) fn on_control(
        &mut self,
        command: ControlCommand,
        command_id: &str,
        expected_revision: u64,
        actor: Option<ActorRef>,
    ) {
        match command {
            ControlCommand::SessionCreate {
                close_current: _,
                cwd: _,
                tool_mode,
                custom_tools,
            } => {
                // 可选工具模式预置在 create 前应用，保证新会话的
                // system prompt 与工具集首轮就位（daemon 路径已先落盘）。
                if let Some(tool_mode) = tool_mode {
                    self.session
                        .agent
                        .apply_tool_mode(&tool_mode, &custom_tools);
                }
                // H2：无论 close_current 与否都必须走会话切换守卫——
                // 否则旧会话的挂起 turn/tool.pending 会指向被整包替换
                // 后的死 store，迟到 resume 直接打穿新会话。
                self.prepare_session_switch();
                self.lifecycle
                    .create_session(&mut self.session.agent, &self.cancel);
                self.sync_emitter_session();
                self.paced_emitter.emit_domain(DomainEvent::Control(
                    qaqh_domain::ControlEvent::SessionStateChanged {
                        session_id: self.session.agent.session.session_id.clone(),
                        state: qaqh_domain::SessionState::Created,
                    },
                ));
                self.misc
                    .emit_dashboard(&self.session.agent, &self.paced_emitter);
            }
            ControlCommand::SessionResume { session_id } => {
                self.prepare_session_switch();
                if self
                    .lifecycle
                    .resume_session(&mut self.session.agent, &self.cancel, &session_id)
                {
                    self.sync_emitter_session();
                    self.paced_emitter.emit_domain(DomainEvent::Control(
                        qaqh_domain::ControlEvent::SessionStateChanged {
                            session_id,
                            state: qaqh_domain::SessionState::Resumed,
                        },
                    ));
                } else {
                    self.emit_operation_failed(
                        command_id,
                        qaqh_domain::ErrorScope::Control,
                        "session_resume_failed",
                        "session could not be resumed",
                    );
                }
            }
            // daemon 拦截层已处理（仅 lease attach）；到达 actor 即异常路径，
            // 静默完成即可 —— 绝不能走到 resume/重建（会杀死运行中的子代理回合）。
            ControlCommand::SessionAttach { .. } => {
                log::debug!("SessionAttach is handled by the daemon lease layer");
                self.emit_operation_completed(command_id, qaqh_domain::ErrorScope::Control);
            }
            ControlCommand::SessionShutdown => {
                self.pending.shutdown = true;
                self.emit_operation_completed(command_id, qaqh_domain::ErrorScope::Control);
            }
            ControlCommand::AgentReloadConfig => {
                self.lifecycle
                    .reload_config(&mut self.session.agent, &self.cancel);
                self.emit_operation_completed(command_id, qaqh_domain::ErrorScope::Control);
            }
            ControlCommand::SetToolMode {
                tool_mode,
                custom_tools,
            } => {
                self.session
                    .agent
                    .apply_tool_mode(&tool_mode, &custom_tools);
                self.emit_operation_completed(command_id, qaqh_domain::ErrorScope::Control);
            }
            ControlCommand::SkillsReload => self.emit_ringing_skills_status(),
            ControlCommand::SkillsActivate { name } => {
                let _ = self.session.agent.skills.queue_request(&name, "user");
                self.emit_ringing_skills_status();
            }
            ControlCommand::SkillsOperation {
                operation_id,
                action,
                name,
            } => {
                let (success, _revision, error) = self.session.agent.skills.apply_ui_operation(
                    &operation_id,
                    expected_revision,
                    &action,
                    &name,
                );
                self.emit_ringing_skills_status();
                if !success {
                    self.paced_emitter
                        .emit_domain(qaqh_domain::DomainEvent::Control(
                            qaqh_domain::ControlEvent::OperationFailed {
                                occurrence_id: operation_id.clone(),
                                scope: qaqh_domain::ErrorScope::Control,
                                error: qaqh_domain::DomainError {
                                    error_id: operation_id.clone(),
                                    code: "skill_operation_failed".into(),
                                    message: error
                                        .unwrap_or_else(|| "skill operation failed".into()),
                                    retryable: false,
                                    dedupe_key: Some(operation_id.clone()),
                                },
                                operation_id: Some(operation_id),
                            },
                        ));
                }
            }
            ControlCommand::SessionClose { .. } => {
                log::debug!("SessionClose is handled by daemon registry");
            }
            ControlCommand::SessionArchive { .. } => {
                log::debug!("SessionArchive is handled by daemon registry");
            }
            ControlCommand::SessionUnarchive { .. } => {
                log::debug!("SessionUnarchive is handled by daemon registry");
            }
            ControlCommand::SessionDelete { .. } => {
                log::debug!("SessionDelete is handled by daemon registry");
            }
            ControlCommand::InteractionAskRespond {
                interaction_id,
                answers,
            } => {
                if self.reject_already_resolved_interaction(command_id, &interaction_id, "ask") {
                    return;
                }
                // answers 已是 domain AskAnswer（Ringing 命令直接携带）。
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
                let outcome = self.session.turn.handle_ask_response(
                    &mut ctx,
                    &mut self.session.tool,
                    &interaction_id,
                    command_id,
                    &answers,
                    actor.clone(),
                );
                let _ = ctx;
                if !self.admit_legacy_interaction_resolution(command_id, &interaction_id, &outcome)
                {
                    return;
                }
                self.apply_outcome(outcome);
            }
            ControlCommand::InteractionAskDismiss { interaction_id } => {
                if self.reject_already_resolved_interaction(command_id, &interaction_id, "ask") {
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
                let outcome = self.session.turn.handle_ask_dismiss(
                    &mut ctx,
                    &mut self.session.tool,
                    &interaction_id,
                    command_id,
                    actor.clone(),
                );
                let _ = ctx;
                if !self.admit_legacy_interaction_resolution(command_id, &interaction_id, &outcome)
                {
                    return;
                }
                self.apply_outcome(outcome);
            }
            ControlCommand::DriverClaim {
                client_session_id,
                stale_holder,
            } => self.handle_driver_claim(command_id, &client_session_id, stale_holder.as_deref()),
            ControlCommand::DriverRelease {
                client_session_id,
                expected_epoch,
            } => self.handle_driver_release(command_id, &client_session_id, expected_epoch),
            ControlCommand::PlanReviewRespond {
                interaction_id,
                approved,
                message,
                autonomous,
            } => {
                if self.reject_already_resolved_interaction(
                    command_id,
                    &interaction_id,
                    "plan review",
                ) {
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
                let outcome = self.session.turn.handle_plan_response(
                    &mut ctx,
                    &mut self.session.tool,
                    &interaction_id,
                    command_id,
                    approved,
                    &message.unwrap_or_default(),
                    autonomous,
                    actor,
                );
                let _ = ctx;
                if !self.admit_legacy_interaction_resolution(command_id, &interaction_id, &outcome)
                {
                    return;
                }
                self.apply_outcome(outcome);
            }
        }
    }

    /// Canonical driver claim.
    ///
    /// The ledger is the single canonical writer, so it also owns
    /// `driver_epoch` allocation. The daemon's HTTP ack is therefore an
    /// optimistic "requested"; the authoritative seat reaches clients as the
    /// reliable `DriverChanged` event published from the appended fact.
    fn handle_driver_claim(&mut self, command_id: &str, holder: &str, stale_holder: Option<&str>) {
        let now = unix_ms();
        let event_id = EventId::new(generate_ulid());
        let causation_id = driver_causation_id(command_id);
        let outcome: Result<qaqh_session::canonical::DriverClaimOutcome, String> = {
            match self.session.agent.tool_ledger_mut() {
                Ok(Some(ledger)) => ledger
                    .ensure_lease(now, tool_ledger_lease_ms())
                    .map_err(|error| error.to_string())
                    .and_then(|()| {
                        ledger
                            .claim_driver(holder, stale_holder, event_id, causation_id, now)
                            .map_err(|error| error.to_string())
                    }),
                Ok(None) => Err("session has no canonical ledger".to_string()),
                Err(error) => Err(error.to_string()),
            }
        };
        match outcome {
            Ok(qaqh_session::canonical::DriverClaimOutcome::Claimed { driver_epoch }) => {
                self.emit_driver_changed(Some(holder), driver_epoch);
            }
            Ok(qaqh_session::canonical::DriverClaimOutcome::AlreadyHeld { .. }) => {
                self.emit_operation_completed(command_id, qaqh_domain::ErrorScope::Control);
            }
            Ok(qaqh_session::canonical::DriverClaimOutcome::Busy {
                holder,
                driver_epoch,
            }) => self.emit_operation_failed(
                command_id,
                qaqh_domain::ErrorScope::Control,
                "driver_busy",
                &format!("driver seat is held by {holder} at epoch {driver_epoch}"),
            ),
            Err(error) => self.emit_operation_failed(
                command_id,
                qaqh_domain::ErrorScope::Control,
                "driver_claim_failed",
                &error,
            ),
        }
    }

    /// Canonical driver release; only the recorded holder may release, and an
    /// `expected_epoch` CAS guards delayed reclaims.
    fn handle_driver_release(
        &mut self,
        command_id: &str,
        holder: &str,
        expected_epoch: Option<u64>,
    ) {
        let now = unix_ms();
        let event_id = EventId::new(generate_ulid());
        let causation_id = driver_causation_id(command_id);
        let outcome: Result<qaqh_session::canonical::DriverReleaseOutcome, String> = {
            match self.session.agent.tool_ledger_mut() {
                Ok(Some(ledger)) => ledger
                    .ensure_lease(now, tool_ledger_lease_ms())
                    .map_err(|error| error.to_string())
                    .and_then(|()| {
                        ledger
                            .release_driver(holder, expected_epoch, event_id, causation_id, now)
                            .map_err(|error| error.to_string())
                    }),
                Ok(None) => Err("session has no canonical ledger".to_string()),
                Err(error) => Err(error.to_string()),
            }
        };
        match outcome {
            Ok(qaqh_session::canonical::DriverReleaseOutcome::Released { driver_epoch }) => {
                self.emit_driver_changed(None, driver_epoch);
            }
            Ok(qaqh_session::canonical::DriverReleaseOutcome::NotDriver { .. }) => {
                self.emit_operation_failed(
                    command_id,
                    qaqh_domain::ErrorScope::Control,
                    "not_driver",
                    "client does not hold the driver seat",
                );
            }
            Ok(qaqh_session::canonical::DriverReleaseOutcome::StaleEpoch { driver_epoch }) => {
                self.emit_operation_failed(
                    command_id,
                    qaqh_domain::ErrorScope::Control,
                    "stale_driver_epoch",
                    &format!("driver seat moved on (current epoch {driver_epoch})"),
                );
            }
            Err(error) => self.emit_operation_failed(
                command_id,
                qaqh_domain::ErrorScope::Control,
                "driver_release_failed",
                &error,
            ),
        }
    }

    fn emit_driver_changed(&self, holder: Option<&str>, driver_epoch: u64) {
        self.paced_emitter.emit_domain(DomainEvent::Control(
            qaqh_domain::ControlEvent::DriverChanged {
                holder: holder.map(str::to_string),
                driver_epoch,
            },
        ));
    }
}

fn driver_causation_id(command_id: &str) -> Option<EventId> {
    causation_for_command(command_id)
}
