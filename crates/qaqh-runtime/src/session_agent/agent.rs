//! Single-owner SessionAgent implementation (RLC-3.1).

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use qaqh_agentloop_core::{
    DispatchEvidence, EffectIdentity,
    driver::{InputHandler, Step},
    mailbox::MailboxHandle,
    supervisor::{DispatchTicket, Supervisor, TaskOutcome},
};
use qaqh_session::{
    canonical::sha256_content_hash,
    session_commit::{CommittedExecution, SessionCommit},
    session_fact_v2::{
        ActorKind, ActorRef, ContentRef, EventId, ExecutionId, FactPayload,
        FactSchema, InputAccepted, InputId, InputKind, InputPurpose, InteractionDecision,
        InteractionId, InteractionKind, InteractionRequested, InteractionResolved, InterruptReason,
        PolicyDecisionRef, RecoveryId, RecoveryRef, SessionFact, SideEffectClass, ToolCallId,
        ToolError, ToolFinished, ToolIntent, ToolIntentPolicyOutcome, ToolMetrics,
        ToolReplayCapability, ToolTerminalStatus, TurnFinished, TurnId, TurnInterrupted,
        TurnMode, TurnStarted, TurnTerminal, ProviderFinish,
    },
};
use tokio::sync::{Mutex, watch};

use crate::{
    provider_job::{
        ProviderRequestSpec, build_model_round_started,
        prepare_response_commit_facts, run_verified,
    },
    tool_job::{
        ToolOutcome, ToolPort, ToolRequest, WorkspaceToolPort,
    },
};

use super::types::{
    ActivePhase, AgentCommand, AgentCompletion, AgentControl, AgentOutcome, SessionAgentError,
    SessionPhase,
};

static EVENT_ORDINAL_GENERATOR: AtomicU64 = AtomicU64::new(100);

fn next_event_id() -> EventId {
    let ord = EVENT_ORDINAL_GENERATOR.fetch_add(1, Ordering::Relaxed);
    EventId::new(format!("01J{ord:023}"))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn text_message(role: &str, text: &str) -> qaqh_types::Message {
    qaqh_types::Message {
        msg_id: None,
        role: role.into(),
        name: None,
        content: vec![qaqh_types::ContentBlock::Text { text: text.into() }],
    }
}

pub struct SessionAgent {
    pub(crate) session_id: String,
    pub(crate) incarnation: u64,
    pub(crate) phase: SessionPhase,
    pub(crate) active_turn: Option<TurnId>,
    pub(crate) active_input: Option<InputId>,
    pub(crate) current_round: u32,
    pub(crate) current_attempt: u32,
    pub(crate) commit: SessionCommit,
    pub(crate) supervisor: Arc<Mutex<Supervisor<AgentOutcome, CommittedExecution>>>,
    pub(crate) mailbox_handle: MailboxHandle<AgentCommand, AgentControl, AgentCompletion>,
    pub(crate) provider_spec: ProviderRequestSpec,
    pub(crate) tool_port: Arc<WorkspaceToolPort>,
    pub(crate) active_provider_identity: Option<EffectIdentity>,
    pub(crate) active_tool_identities: HashMap<ToolCallId, (EffectIdentity, ExecutionId)>,
    pub(crate) pending_ask_id: Option<(ToolCallId, InteractionId, ExecutionId)>,
    pub(crate) queued_inputs: VecDeque<(InputId, String, Vec<ContentRef>, Option<String>)>,
}

impl SessionAgent {
    pub fn new(
        session_id: String,
        incarnation: u64,
        commit: SessionCommit,
        supervisor: Arc<Mutex<Supervisor<AgentOutcome, CommittedExecution>>>,
        mailbox_handle: MailboxHandle<AgentCommand, AgentControl, AgentCompletion>,
        provider_spec: ProviderRequestSpec,
        tool_port: Arc<WorkspaceToolPort>,
    ) -> Self {
        Self {
            session_id,
            incarnation,
            phase: SessionPhase::Ready,
            active_turn: None,
            active_input: None,
            current_round: 1,
            current_attempt: 1,
            commit,
            supervisor,
            mailbox_handle,
            provider_spec,
            tool_port,
            active_provider_identity: None,
            active_tool_identities: HashMap::new(),
            pending_ask_id: None,
            queued_inputs: VecDeque::new(),
        }
    }

    pub fn phase(&self) -> &SessionPhase {
        &self.phase
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn incarnation(&self) -> u64 {
        self.incarnation
    }

    pub(crate) fn dispatch_task<F, Fut>(
        &mut self,
        ticket: DispatchTicket,
        make: F,
    ) -> Result<(), SessionAgentError>
    where
        F: FnOnce(CommittedExecution, watch::Receiver<bool>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = AgentOutcome> + Send + 'static,
    {
        let reservation = self
            .mailbox_handle
            .try_reserve_completion(60_000)
            .map_err(SessionAgentError::Mailbox)?;
        let identity = ticket.identity().clone();
        let sup_lock = Arc::clone(&self.supervisor);
        let mut sup = sup_lock
            .try_lock()
            .map_err(|_| SessionAgentError::Internal("supervisor lock contention".into()))?;

        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        sup.start(ticket, reservation, move |execution, cancel_rx| async move {
            let res = make(execution, cancel_rx).await;
            let _ = done_tx.send(());
            res
        })
        .map_err(SessionAgentError::Supervisor)?;

        let sup_clone = Arc::clone(&self.supervisor);
        let ident_clone = identity;
        tokio::spawn(async move {
            let _ = done_rx.await;
            let mut sup = sup_clone.lock().await;
            if let Ok(_comp) = sup.collect(&ident_clone).await {
                let _ = sup.deliver(&ident_clone);
            }
        });

        Ok(())
    }

    fn start_turn(
        &mut self,
        input_id: InputId,
        text: String,
        attachments: Vec<ContentRef>,
        client_request_id: Option<String>,
    ) -> Result<(), SessionAgentError> {
        let turn_event_id = next_event_id();
        let turn_id = TurnId::new(format!("turn_{}", turn_event_id.as_str()));
        self.active_turn = Some(turn_id.clone());
        self.active_input = Some(input_id.clone());
        self.current_round = 1;
        self.current_attempt = 1;

        let input_fact = SessionFact {
            schema: FactSchema::v2(),
            session_id: self.commit.session_id().clone(),
            log_id: self.commit.log_id().clone(),
            fact_seq: 0,
            event_id: next_event_id(),
            ts_ms: now_ms(),
            causation_id: None,
            turn_id: None,
            call_id: None,
            interaction_id: None,
            payload: FactPayload::InputAccepted(InputAccepted {
                input_id: input_id.clone(),
                input_kind: InputKind::UserText,
                input_purpose: InputPurpose::TriggerTurn,
                content_ref: None,
                inline_text: Some(text.clone()),
                attachments,
                actor: ActorRef {
                    kind: ActorKind::User,
                    id: "user".into(),
                    display_name: None,
                },
                client_request_id,
            }),
        };

        let turn_started_fact = SessionFact {
            schema: FactSchema::v2(),
            session_id: self.commit.session_id().clone(),
            log_id: self.commit.log_id().clone(),
            fact_seq: 0,
            event_id: turn_event_id,
            ts_ms: now_ms(),
            causation_id: None,
            turn_id: Some(turn_id.clone()),
            call_id: None,
            interaction_id: None,
            payload: FactPayload::TurnStarted(TurnStarted {
                turn_id: turn_id.clone(),
                input_id,
                mode: TurnMode::Normal,
                recovery_ref: None,
            }),
        };

        let user_msg = text_message("user", &text);
        let msg_bytes = serde_json::to_vec(&user_msg)
            .map_err(|e| SessionAgentError::Internal(e.to_string()))?;
        let message_ref = self.commit.put_blob(&msg_bytes)?;

        let context_fact = SessionFact {
            schema: FactSchema::v2(),
            session_id: self.commit.session_id().clone(),
            log_id: self.commit.log_id().clone(),
            fact_seq: 0,
            event_id: next_event_id(),
            ts_ms: now_ms(),
            causation_id: None,
            turn_id: Some(turn_id.clone()),
            call_id: None,
            interaction_id: None,
            payload: FactPayload::ContextMessageCommitted(
                qaqh_session::session_fact_v2::ContextMessageCommitted {
                    message_ref,
                    origin: qaqh_session::session_fact_v2::ContextMessageOrigin::Input,
                },
            ),
        };

        let head = self.commit.head()?;
        let prep_batch = format!("batch-turn-prep-{}", turn_id.as_str());
        let prep_facts = vec![input_fact, turn_started_fact, context_fact];
        self.commit.commit(&prep_batch, head, prep_facts)?;

        let head2 = self.commit.head()?;
        let round_started_payload = build_model_round_started(
            &self.commit,
            turn_id.clone(),
            1,
            1,
            &self.provider_spec,
        )?;

        let round_event_id = next_event_id();
        let round_started_fact = SessionFact {
            schema: FactSchema::v2(),
            session_id: self.commit.session_id().clone(),
            log_id: self.commit.log_id().clone(),
            fact_seq: 0,
            event_id: round_event_id.clone(),
            ts_ms: now_ms(),
            causation_id: None,
            turn_id: Some(turn_id.clone()),
            call_id: None,
            interaction_id: None,
            payload: FactPayload::ModelRoundStarted(round_started_payload),
        };

        let batch_id = format!("batch-turn-{}-r1", turn_id.as_str());
        let (_receipt, capability) = self.commit.commit_with_effect(
            &batch_id,
            head2,
            vec![round_started_fact],
            round_event_id.as_str(),
        )?;

        let cap = capability.ok_or_else(|| {
            SessionAgentError::Internal("expected capability for ModelRoundStarted".into())
        })?;
        let identity = cap.identity().clone();
        self.active_provider_identity = Some(identity.clone());

        let ticket = {
            let mut sup = self
                .supervisor
                .try_lock()
                .map_err(|_| SessionAgentError::Internal("supervisor lock contention".into()))?;
            sup.authorize(identity, cap)
                .map_err(SessionAgentError::Supervisor)?
        };

        let spec = self.provider_spec.clone();
        self.dispatch_task(ticket, move |execution, cancel_rx| async move {
            let outcome = run_verified(spec, execution, cancel_rx).await;
            AgentOutcome::Provider(outcome)
        })?;

        self.phase = SessionPhase::Active(ActivePhase::AwaitingProvider {
            round: 1,
            attempt: 1,
        });

        Ok(())
    }
}

impl InputHandler<AgentCommand, AgentControl, AgentCompletion> for SessionAgent {
    type Error = SessionAgentError;

    fn command(&mut self, command: &AgentCommand) -> Result<Step, Self::Error> {
        match command {
            AgentCommand::UserInput {
                input_id,
                text,
                attachments,
                client_request_id,
            } => {
                if self.phase != SessionPhase::Ready {
                    self.queued_inputs.push_back((
                        input_id.clone(),
                        text.clone(),
                        attachments.clone(),
                        client_request_id.clone(),
                    ));
                    return Ok(Step::Continue);
                }
                self.start_turn(
                    input_id.clone(),
                    text.clone(),
                    attachments.clone(),
                    client_request_id.clone(),
                )?;
                Ok(Step::Continue)
            }
            AgentCommand::QueueInput {
                input_id,
                text,
                attachments,
                client_request_id,
            } => {
                self.queued_inputs.push_back((
                    input_id.clone(),
                    text.clone(),
                    attachments.clone(),
                    client_request_id.clone(),
                ));
                Ok(Step::Continue)
            }
        }
    }

    fn control(&mut self, control: &AgentControl) -> Result<Step, Self::Error> {
        match control {
            AgentControl::Shutdown => {
                self.phase = SessionPhase::Closing;
                Ok(Step::Stop)
            }
            AgentControl::CancelTurn { turn_id, reason } => {
                if let Some(turn) = &self.active_turn {
                    if turn_id.as_ref().is_none_or(|t| t == turn) {
                        if let Some(ident) = self.active_provider_identity.take() {
                            let mut sup = self.supervisor.try_lock().map_err(|_| {
                                SessionAgentError::Internal("supervisor contention".into())
                            })?;
                            let _ = sup.request_cancel(&ident);
                        }
                        for (_, (ident, _)) in self.active_tool_identities.drain() {
                            let mut sup = self.supervisor.try_lock().map_err(|_| {
                                SessionAgentError::Internal("supervisor contention".into())
                            })?;
                            let _ = sup.request_cancel(&ident);
                        }

                        let head = self.commit.head()?;
                        let cancel_fact = SessionFact {
                            schema: FactSchema::v2(),
                            session_id: self.commit.session_id().clone(),
                            log_id: self.commit.log_id().clone(),
                            fact_seq: 0,
                            event_id: next_event_id(),
                            ts_ms: now_ms(),
                            causation_id: None,
                            turn_id: Some(turn.clone()),
                            call_id: None,
                            interaction_id: None,
                            payload: FactPayload::TurnInterrupted(TurnInterrupted {
                                turn_id: turn.clone(),
                                reason: InterruptReason::CancelBeforeSeal,
                                last_fact_seq: head,
                                recovery_ref: RecoveryRef {
                                    recovery_id: RecoveryId::new(format!(
                                        "recovery_{}",
                                        next_event_id().as_str()
                                    )),
                                    recovery_event_id: next_event_id(),
                                    recovery_input_fingerprint: sha256_content_hash(
                                        reason.as_bytes(),
                                    ),
                                },
                            }),
                        };
                        let batch_id = format!("batch-cancel-{}", turn.as_str());
                        self.commit.commit(&batch_id, head, vec![cancel_fact])?;
                        self.active_turn = None;
                        self.active_input = None;
                        self.phase = SessionPhase::Ready;
                    }
                }
                Ok(Step::Continue)
            }
            AgentControl::ResolveInteraction {
                interaction_id,
                answers,
            } => {
                if let Some((tool_call_id, expected_interaction_id, exec_id)) = self.pending_ask_id.take() {
                    if expected_interaction_id == *interaction_id {
                        let turn = self.active_turn.as_ref().ok_or_else(|| {
                            SessionAgentError::Internal("active turn missing for interaction".into())
                        })?;
                        let head = self.commit.head()?;

                        let ans_json = serde_json::to_vec(answers)
                            .map_err(|e| SessionAgentError::Internal(e.to_string()))?;
                        let ans_ref = self.commit.put_blob(&ans_json)?;

                        let resolved_fact = SessionFact {
                            schema: FactSchema::v2(),
                            session_id: self.commit.session_id().clone(),
                            log_id: self.commit.log_id().clone(),
                            fact_seq: 0,
                            event_id: next_event_id(),
                            ts_ms: now_ms(),
                            causation_id: None,
                            turn_id: Some(turn.clone()),
                            call_id: Some(tool_call_id.clone()),
                            interaction_id: Some(interaction_id.clone()),
                            payload: FactPayload::InteractionResolved(InteractionResolved {
                                interaction_id: interaction_id.clone(),
                                decision_ref: ans_ref.clone(),
                                decision: Some(InteractionDecision::Answered),
                                resolved_by: ActorRef {
                                    kind: ActorKind::User,
                                    id: "user".into(),
                                    display_name: None,
                                },
                                resolution_seq: 1,
                                resolved_at_ms: now_ms(),
                            }),
                        };

                        let finished_fact = SessionFact {
                            schema: FactSchema::v2(),
                            session_id: self.commit.session_id().clone(),
                            log_id: self.commit.log_id().clone(),
                            fact_seq: 0,
                            event_id: next_event_id(),
                            ts_ms: now_ms(),
                            causation_id: None,
                            turn_id: Some(turn.clone()),
                            call_id: Some(tool_call_id.clone()),
                            interaction_id: None,
                            payload: FactPayload::ToolFinished(ToolFinished {
                                call_id: tool_call_id.clone(),
                                execution_id: Some(exec_id),
                                terminal_status: ToolTerminalStatus::Succeeded,
                                output_ref: Some(ans_ref),
                                error: None,
                                metrics: ToolMetrics {
                                    started_at_ms: now_ms() - 10,
                                    finished_at_ms: now_ms(),
                                    retry_count: 0,
                                    output_bytes: ans_json.len() as u64,
                                    progress_bytes_total: ans_json.len() as u64,
                                },
                                reconciled: false,
                                evidence_ref: None,
                                evidence_fact_seq: None,
                                evidence_event_id: None,
                                recovery_ref: None,
                                finished_at_ms: now_ms(),
                            }),
                        };

                        let tool_msg = qaqh_types::Message {
                            msg_id: None,
                            role: "tool".into(),
                            name: None,
                            content: vec![qaqh_types::ContentBlock::ToolResult {
                                tool_use_id: tool_call_id.as_str().to_string(),
                                result: qaqh_types::ToolResult::ok(String::from_utf8_lossy(&ans_json)),
                            }],
                        };
                        let tool_msg_bytes = serde_json::to_vec(&tool_msg)
                            .map_err(|e| SessionAgentError::Internal(e.to_string()))?;
                        let tool_msg_ref = self.commit.put_blob(&tool_msg_bytes)?;
                        let tool_ctx_fact = SessionFact {
                            schema: FactSchema::v2(),
                            session_id: self.commit.session_id().clone(),
                            log_id: self.commit.log_id().clone(),
                            fact_seq: 0,
                            event_id: next_event_id(),
                            ts_ms: now_ms(),
                            causation_id: None,
                            turn_id: Some(turn.clone()),
                            call_id: Some(tool_call_id.clone()),
                            interaction_id: None,
                            payload: FactPayload::ContextMessageCommitted(
                                qaqh_session::session_fact_v2::ContextMessageCommitted {
                                    message_ref: tool_msg_ref,
                                    origin: qaqh_session::session_fact_v2::ContextMessageOrigin::ToolResult,
                                },
                            ),
                        };

                        let batch_id = format!("batch-resolve-{}", interaction_id.as_str());
                        self.commit.commit(
                            &batch_id,
                            head,
                            vec![resolved_fact, finished_fact, tool_ctx_fact],
                        )?;

                        // Advance round
                        self.current_round += 1;
                        self.current_attempt = 1;
                        let r_payload = build_model_round_started(
                            &self.commit,
                            turn.clone(),
                            self.current_round,
                            self.current_attempt,
                            &self.provider_spec,
                        )?;
                        let round_event_id = next_event_id();
                        let r_fact = SessionFact {
                            schema: FactSchema::v2(),
                            session_id: self.commit.session_id().clone(),
                            log_id: self.commit.log_id().clone(),
                            fact_seq: 0,
                            event_id: round_event_id.clone(),
                            ts_ms: now_ms(),
                            causation_id: None,
                            turn_id: Some(turn.clone()),
                            call_id: None,
                            interaction_id: None,
                            payload: FactPayload::ModelRoundStarted(r_payload),
                        };
                        let head2 = self.commit.head()?;
                        let r_batch = format!("batch-r{}-next", self.current_round);
                        let (_, cap) = self.commit.commit_with_effect(
                            &r_batch,
                            head2,
                            vec![r_fact],
                            round_event_id.as_str(),
                        )?;
                        let cap = cap.ok_or_else(|| {
                            SessionAgentError::Internal("expected capability for round".into())
                        })?;
                        let identity = cap.identity().clone();
                        self.active_provider_identity = Some(identity.clone());
                        let ticket = {
                            let mut sup = self.supervisor.try_lock().map_err(|_| {
                                SessionAgentError::Internal("supervisor lock contention".into())
                            })?;
                            sup.authorize(identity, cap)
                                .map_err(SessionAgentError::Supervisor)?
                        };
                        let spec = self.provider_spec.clone();
                        self.dispatch_task(ticket, move |exec, cancel_rx| async move {
                            let out = run_verified(spec, exec, cancel_rx).await;
                            AgentOutcome::Provider(out)
                        })?;
                        self.phase = SessionPhase::Active(ActivePhase::AwaitingProvider {
                            round: self.current_round,
                            attempt: self.current_attempt,
                        });
                    }
                }
                Ok(Step::Continue)
            }
            AgentControl::ToolPermissionRespond { .. } => Ok(Step::Continue),
        }
    }

    fn completion(&mut self, completion: &Arc<AgentCompletion>) -> Result<Step, Self::Error> {
        let identity = &completion.identity;
        {
            let mut sup = self
                .supervisor
                .try_lock()
                .map_err(|_| SessionAgentError::Internal("supervisor lock contention".into()))?;
            sup.acknowledge(identity, completion)
                .map_err(SessionAgentError::Supervisor)?;
        }

        match &completion.outcome {
            TaskOutcome::JoinFailed(err) => {
                if let Some(turn) = self.active_turn.clone() {
                    let head = self.commit.head()?;
                    let fact = SessionFact {
                        schema: FactSchema::v2(),
                        session_id: self.commit.session_id().clone(),
                        log_id: self.commit.log_id().clone(),
                        fact_seq: 0,
                        event_id: next_event_id(),
                        ts_ms: now_ms(),
                        causation_id: None,
                        turn_id: Some(turn.clone()),
                        call_id: None,
                        interaction_id: None,
                        payload: FactPayload::TurnInterrupted(TurnInterrupted {
                            turn_id: turn.clone(),
                            reason: InterruptReason::Crash,
                            last_fact_seq: head,
                            recovery_ref: RecoveryRef {
                                recovery_id: RecoveryId::new(format!(
                                    "recovery_{}",
                                    next_event_id().as_str()
                                )),
                                recovery_event_id: next_event_id(),
                                recovery_input_fingerprint: sha256_content_hash(err.as_bytes()),
                            },
                        }),
                    };
                    self.commit.commit(
                        &format!("batch-interrupted-{}", turn.as_str()),
                        head,
                        vec![fact],
                    )?;
                    self.active_turn = None;
                    self.active_input = None;
                    self.phase = SessionPhase::Ready;
                }
            }
            TaskOutcome::Returned(AgentOutcome::Provider(outcome)) => {
                let Some(turn) = self.active_turn.clone() else {
                    // Turn was already cancelled or interrupted: discard late completion gracefully.
                    return Ok(Step::Continue);
                };
                let head = self.commit.head()?;

                if outcome.finish == Some(ProviderFinish::Stop) {
                    let facts = prepare_response_commit_facts(
                        &self.commit,
                        &turn,
                        self.current_round,
                        self.current_attempt,
                        outcome,
                        next_event_id(),
                        |_| next_event_id(),
                        now_ms(),
                    )?;
                    let finish_fact = SessionFact {
                        schema: FactSchema::v2(),
                        session_id: self.commit.session_id().clone(),
                        log_id: self.commit.log_id().clone(),
                        fact_seq: 0,
                        event_id: next_event_id(),
                        ts_ms: now_ms(),
                        causation_id: None,
                        turn_id: Some(turn.clone()),
                        call_id: None,
                        interaction_id: None,
                        payload: FactPayload::TurnFinished(TurnFinished {
                            turn_id: turn.clone(),
                            terminal: TurnTerminal::Completed,
                            usage: outcome.usage.clone(),
                            error: None,
                            finished_at_ms: now_ms(),
                        }),
                    };
                    let mut all_facts = facts;
                    all_facts.push(finish_fact);

                    let batch_id = format!("batch-finish-{}-r{}", turn.as_str(), self.current_round);
                    self.commit.commit(&batch_id, head, all_facts)?;

                    self.active_turn = None;
                    self.active_input = None;
                    self.active_provider_identity = None;

                    // Drain queued input if available
                    if let Some((next_in, text, attachments, req_id)) = self.queued_inputs.pop_front()
                    {
                        self.start_turn(next_in, text, attachments, req_id)?;
                    } else {
                        self.phase = SessionPhase::Ready;
                    }
                } else if outcome.finish == Some(ProviderFinish::ToolUse) {
                    let response_facts = prepare_response_commit_facts(
                        &self.commit,
                        &turn,
                        self.current_round,
                        self.current_attempt,
                        outcome,
                        next_event_id(),
                        |_| next_event_id(),
                        now_ms(),
                    )?;

                    let batch_id = format!("batch-tools-declared-{}-r{}", turn.as_str(), self.current_round);
                    self.commit.commit(&batch_id, head, response_facts)?;

                    if let Some(msg) = &outcome.message {
                        for block in &msg.content {
                            if let qaqh_types::ContentBlock::ToolUse { id, name, input } = block {
                                let call_id = ToolCallId::new(id.clone());
                                if name == "ask" {
                                    let ask_interaction_id =
                                        InteractionId::new(format!("int_{}", next_event_id().as_str()));
                                    let exec_id = ExecutionId::new(format!("exec_{}", next_event_id().as_str()));
                                    self.pending_ask_id =
                                        Some((call_id.clone(), ask_interaction_id.clone(), exec_id.clone()));

                                    let head2 = self.commit.head()?;
                                    let req_blob = self.commit.put_blob(b"ask-question")?;
                                    let tool_intent_fact = SessionFact {
                                        schema: FactSchema::v2(),
                                        session_id: self.commit.session_id().clone(),
                                        log_id: self.commit.log_id().clone(),
                                        fact_seq: 0,
                                        event_id: next_event_id(),
                                        ts_ms: now_ms(),
                                        causation_id: None,
                                        turn_id: Some(turn.clone()),
                                        call_id: Some(call_id.clone()),
                                        interaction_id: None,
                                        payload: FactPayload::ToolIntent(ToolIntent {
                                            call_id: call_id.clone(),
                                            execution_id: exec_id.clone(),
                                            idempotency_key: None,
                                            replay_capability: ToolReplayCapability::NoReplay,
                                            policy_decision: PolicyDecisionRef {
                                                outcome: ToolIntentPolicyOutcome::Allow,
                                                rule_id: "ask.allow".into(),
                                                decided_at_ms: now_ms(),
                                                reason_ref: None,
                                            },
                                            effective_args_ref: Some(req_blob.clone()),
                                            effective_args_hash: Some(sha256_content_hash(b"ask-question")),
                                            sandbox_spec_hash: sha256_content_hash(b"sandbox_spec"),
                                            side_effect_class: SideEffectClass::WorkspaceWrite,
                                            intent_at_ms: now_ms(),
                                        }),
                                    };
                                    let ask_req_fact = SessionFact {
                                        schema: FactSchema::v2(),
                                        session_id: self.commit.session_id().clone(),
                                        log_id: self.commit.log_id().clone(),
                                        fact_seq: 0,
                                        event_id: next_event_id(),
                                        ts_ms: now_ms(),
                                        causation_id: None,
                                        turn_id: Some(turn.clone()),
                                        call_id: Some(call_id.clone()),
                                        interaction_id: Some(ask_interaction_id.clone()),
                                        payload: FactPayload::InteractionRequested(
                                            InteractionRequested {
                                                interaction_id: ask_interaction_id.clone(),
                                                call_id: Some(call_id.clone()),
                                                turn_id: turn.clone(),
                                                kind: InteractionKind::Ask,
                                                request_ref: req_blob,
                                                expires_at_ms: None,
                                                requested_at_ms: now_ms(),
                                            },
                                        ),
                                    };
                                    self.commit.commit(
                                        &format!("batch-ask-req-{}", ask_interaction_id.as_str()),
                                        head2,
                                        vec![tool_intent_fact, ask_req_fact],
                                    )?;
                                    self.phase = SessionPhase::Active(ActivePhase::AwaitingUser {
                                        interaction_id: ask_interaction_id,
                                    });
                                    return Ok(Step::Continue);
                                } else {
                                    let args_bytes = serde_json::to_vec(input)
                                        .map_err(|e| SessionAgentError::Internal(e.to_string()))?;
                                    let args_ref = self.commit.put_blob(&args_bytes)?;
                                    let args_hash = sha256_content_hash(&args_bytes);
                                    let tool_intent_event_id = next_event_id();
                                    let exec_id = ExecutionId::new(format!("exec_{}", next_event_id().as_str()));
                                    let tool_intent_fact = SessionFact {
                                        schema: FactSchema::v2(),
                                        session_id: self.commit.session_id().clone(),
                                        log_id: self.commit.log_id().clone(),
                                        fact_seq: 0,
                                        event_id: tool_intent_event_id.clone(),
                                        ts_ms: now_ms(),
                                        causation_id: None,
                                        turn_id: Some(turn.clone()),
                                        call_id: Some(call_id.clone()),
                                        interaction_id: None,
                                        payload: FactPayload::ToolIntent(ToolIntent {
                                            call_id: call_id.clone(),
                                            execution_id: exec_id.clone(),
                                            idempotency_key: None,
                                            replay_capability: ToolReplayCapability::NoReplay,
                                            policy_decision: PolicyDecisionRef {
                                                outcome: ToolIntentPolicyOutcome::Allow,
                                                rule_id: "auto".into(),
                                                decided_at_ms: now_ms(),
                                                reason_ref: None,
                                            },
                                            effective_args_ref: Some(args_ref),
                                            effective_args_hash: Some(args_hash),
                                            sandbox_spec_hash: sha256_content_hash(b"sandbox_spec"),
                                            side_effect_class: SideEffectClass::WorkspaceWrite,
                                            intent_at_ms: now_ms(),
                                        }),
                                    };

                                    let head_tool = self.commit.head()?;
                                    let batch_tool = format!("batch-intent-{}", call_id.as_str());
                                    let (_, capability) = self.commit.commit_with_effect(
                                        &batch_tool,
                                        head_tool,
                                        vec![tool_intent_fact],
                                        tool_intent_event_id.as_str(),
                                    )?;

                                    let cap = capability.ok_or_else(|| {
                                        SessionAgentError::Internal(
                                            "expected capability for ToolIntent".into(),
                                        )
                                    })?;
                                    let tool_identity = cap.identity().clone();
                                    self.active_tool_identities
                                        .insert(call_id.clone(), (tool_identity.clone(), exec_id.clone()));

                                    let ticket = {
                                        let mut sup = self.supervisor.try_lock().map_err(|_| {
                                            SessionAgentError::Internal(
                                                "supervisor lock contention".into(),
                                            )
                                        })?;
                                        sup.authorize(tool_identity, cap)
                                            .map_err(SessionAgentError::Supervisor)?
                                    };

                                    let port = Arc::clone(&self.tool_port);
                                    let t_name = name.clone();
                                    let t_call_id = call_id.as_str().to_string();
                                    self.dispatch_task(ticket, move |execution, mut cancel_rx| async move {
                                        let parts = execution.into_parts().expect("parts");
                                        let args = parts.tool_args.unwrap_or_default();
                                        let req = ToolRequest {
                                            execution_id: format!("exec_{t_call_id}"),
                                            call_id: t_call_id,
                                            tool_name: t_name,
                                            side_effect_class: SideEffectClass::WorkspaceWrite,
                                            replay: ToolReplayCapability::NoReplay,
                                            args,
                                        };
                                        let cancel_flag = Arc::new(AtomicBool::new(false));
                                        let cancel_watch = Arc::clone(&cancel_flag);
                                        tokio::spawn(async move {
                                            let _ = cancel_rx.changed().await;
                                            cancel_watch.store(true, Ordering::Release);
                                        });
                                        let out = port.execute(&req, &cancel_flag);
                                        match out {
                                            Ok(out) => AgentOutcome::Tool(ToolOutcome {
                                                evidence: DispatchEvidence::StartedTerminal,
                                                terminal: Some(ToolTerminalStatus::Succeeded),
                                                output: out.output,
                                                error: None,
                                            }),
                                            Err(err) => AgentOutcome::Tool(ToolOutcome {
                                                evidence: DispatchEvidence::NotStarted,
                                                terminal: Some(ToolTerminalStatus::Failed),
                                                output: vec![],
                                                error: Some(format!("{err:?}")),
                                            }),
                                        }
                                    })?;
                                }
                            }
                        }
                    }

                    self.phase = SessionPhase::Active(ActivePhase::AwaitingTools {
                        active_calls: self.active_tool_identities.keys().cloned().collect(),
                    });
                } else {
                    // Damaged turn / incomplete response
                    let response_facts = prepare_response_commit_facts(
                        &self.commit,
                        &turn,
                        self.current_round,
                        self.current_attempt,
                        outcome,
                        next_event_id(),
                        |_| next_event_id(),
                        now_ms(),
                    )?;
                    let interrupt_fact = SessionFact {
                        schema: FactSchema::v2(),
                        session_id: self.commit.session_id().clone(),
                        log_id: self.commit.log_id().clone(),
                        fact_seq: 0,
                        event_id: next_event_id(),
                        ts_ms: now_ms(),
                        causation_id: None,
                        turn_id: Some(turn.clone()),
                        call_id: None,
                        interaction_id: None,
                        payload: FactPayload::TurnInterrupted(TurnInterrupted {
                            turn_id: turn.clone(),
                            reason: InterruptReason::Crash,
                            last_fact_seq: head,
                            recovery_ref: RecoveryRef {
                                recovery_id: RecoveryId::new(format!(
                                    "recovery_{}",
                                    next_event_id().as_str()
                                )),
                                recovery_event_id: next_event_id(),
                                recovery_input_fingerprint: sha256_content_hash(
                                    b"provider-incomplete",
                                ),
                            },
                        }),
                    };
                    let mut facts = response_facts;
                    facts.push(interrupt_fact);
                    self.commit.commit(
                        &format!("batch-damaged-{}", turn.as_str()),
                        head,
                        facts,
                    )?;
                    self.active_turn = None;
                    self.active_input = None;
                    self.phase = SessionPhase::Ready;
                }
            }
            TaskOutcome::Returned(AgentOutcome::Tool(outcome)) => {
                let Some(turn) = self.active_turn.clone() else {
                    return Ok(Step::Continue);
                };
                let head = self.commit.head()?;

                let (call_id, exec_id) = self
                    .active_tool_identities
                    .iter()
                    .find(|(_, (ident, _))| ident.effect_id == identity.effect_id)
                    .map(|(cid, (_, eid))| (cid.clone(), eid.clone()))
                    .ok_or_else(|| {
                        SessionAgentError::Internal(format!(
                            "unrecognized tool completion effect_id: {}",
                            identity.effect_id
                        ))
                    })?;

                let out_ref = self.commit.put_blob(&outcome.output)?;

                let finished_fact = SessionFact {
                    schema: FactSchema::v2(),
                    session_id: self.commit.session_id().clone(),
                    log_id: self.commit.log_id().clone(),
                    fact_seq: 0,
                    event_id: next_event_id(),
                    ts_ms: now_ms(),
                    causation_id: None,
                    turn_id: Some(turn.clone()),
                    call_id: Some(call_id.clone()),
                    interaction_id: None,
                    payload: FactPayload::ToolFinished(ToolFinished {
                        call_id: call_id.clone(),
                        execution_id: Some(exec_id),
                        terminal_status: if outcome.error.is_none() {
                            ToolTerminalStatus::Succeeded
                        } else {
                            ToolTerminalStatus::Failed
                        },
                        output_ref: Some(out_ref),
                        error: outcome.error.as_ref().map(|msg| ToolError {
                            code: "tool_error".into(),
                            message: msg.clone(),
                            retryable: false,
                            details_ref: None,
                        }),
                        metrics: ToolMetrics {
                            started_at_ms: now_ms() - 10,
                            finished_at_ms: now_ms(),
                            retry_count: 0,
                            output_bytes: outcome.output.len() as u64,
                            progress_bytes_total: outcome.output.len() as u64,
                        },
                        reconciled: false,
                        evidence_ref: None,
                        evidence_fact_seq: None,
                        evidence_event_id: None,
                        recovery_ref: None,
                        finished_at_ms: now_ms(),
                    }),
                };

                let tool_res_text = String::from_utf8_lossy(&outcome.output).to_string();
                let tool_msg = qaqh_types::Message {
                    msg_id: None,
                    role: "tool".into(),
                    name: None,
                    content: vec![qaqh_types::ContentBlock::ToolResult {
                        tool_use_id: call_id.as_str().to_string(),
                        result: if outcome.error.is_none() {
                            qaqh_types::ToolResult::ok(tool_res_text)
                        } else {
                            qaqh_types::ToolResult::error(tool_res_text)
                        },
                    }],
                };
                let tool_msg_bytes = serde_json::to_vec(&tool_msg)
                    .map_err(|e| SessionAgentError::Internal(e.to_string()))?;
                let tool_msg_ref = self.commit.put_blob(&tool_msg_bytes)?;

                let tool_ctx_fact = SessionFact {
                    schema: FactSchema::v2(),
                    session_id: self.commit.session_id().clone(),
                    log_id: self.commit.log_id().clone(),
                    fact_seq: 0,
                    event_id: next_event_id(),
                    ts_ms: now_ms(),
                    causation_id: None,
                    turn_id: Some(turn.clone()),
                    call_id: Some(call_id.clone()),
                    interaction_id: None,
                    payload: FactPayload::ContextMessageCommitted(
                        qaqh_session::session_fact_v2::ContextMessageCommitted {
                            message_ref: tool_msg_ref,
                            origin: qaqh_session::session_fact_v2::ContextMessageOrigin::ToolResult,
                        },
                    ),
                };

                let batch_id = format!("batch-tool-fin-{}", call_id.as_str());
                self.commit.commit(&batch_id, head, vec![finished_fact, tool_ctx_fact])?;

                self.active_tool_identities.remove(&call_id);

                if self.active_tool_identities.is_empty() {
                    // Advance to next round
                    self.current_round += 1;
                    self.current_attempt = 1;
                    let round_payload = build_model_round_started(
                        &self.commit,
                        turn.clone(),
                        self.current_round,
                        self.current_attempt,
                        &self.provider_spec,
                    )?;
                    let round_event_id = next_event_id();
                    let round_fact = SessionFact {
                        schema: FactSchema::v2(),
                        session_id: self.commit.session_id().clone(),
                        log_id: self.commit.log_id().clone(),
                        fact_seq: 0,
                        event_id: round_event_id.clone(),
                        ts_ms: now_ms(),
                        causation_id: None,
                        turn_id: Some(turn.clone()),
                        call_id: None,
                        interaction_id: None,
                        payload: FactPayload::ModelRoundStarted(round_payload),
                    };

                    let head2 = self.commit.head()?;
                    let batch_r = format!("batch-r{}-next", self.current_round);
                    let (_, cap) = self.commit.commit_with_effect(
                        &batch_r,
                        head2,
                        vec![round_fact],
                        round_event_id.as_str(),
                    )?;
                    let cap = cap.ok_or_else(|| {
                        SessionAgentError::Internal("expected capability for next round".into())
                    })?;
                    let ident = cap.identity().clone();
                    self.active_provider_identity = Some(ident.clone());
                    let ticket = {
                        let mut sup = self.supervisor.try_lock().map_err(|_| {
                            SessionAgentError::Internal("supervisor lock contention".into())
                        })?;
                        sup.authorize(ident, cap)
                            .map_err(SessionAgentError::Supervisor)?
                    };
                    let spec = self.provider_spec.clone();
                    self.dispatch_task(ticket, move |exec, cancel_rx| async move {
                        let out = run_verified(spec, exec, cancel_rx).await;
                        AgentOutcome::Provider(out)
                    })?;
                    self.phase = SessionPhase::Active(ActivePhase::AwaitingProvider {
                        round: self.current_round,
                        attempt: self.current_attempt,
                    });
                }
            }
        }

        Ok(Step::Continue)
    }
}
