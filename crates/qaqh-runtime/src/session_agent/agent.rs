//! Single-owner SessionAgent implementation (RLC-3.1).
//!
//! One owner folds the committed prefix into its phase, commits every
//! transition through [`SessionCommit`], and dispatches effects through a
//! supervisor. Display facts (`AssistantBlockSealed`) are produced for every
//! model attempt so the projection has a transcript source.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use qaqh_agentloop_core::{
    DispatchEvidence, EffectIdentity,
    driver::{InputHandler, Step},
    mailbox::{CompletionReservation, MailboxHandle},
    supervisor::{DispatchTicket, Supervisor, TaskOutcome},
};
use qaqh_session::{
    canonical::{BatchReceipt, causation_for_command, generate_ulid, sha256_content_hash},
    session_commit::{CommittedExecution, SessionCommit},
    session_fact_v2::{
        ActorKind, ActorRef, ContentRef, ContextMessageCommitted, ContextMessageOrigin, EventId,
        ExecutionId, FactPayload, FactSchema, InputAccepted, InputId, InputKind, InputPurpose,
        InteractionChoice, InteractionChoiceTone, InteractionDecision, InteractionExpiryReason,
        InteractionId, InteractionKind, InteractionRequested, InteractionResolved, InterruptReason,
        PolicyDecisionRef,
        ProviderFinish, RecoveryAction, RecoveryId, RecoveryOutcome, RecoveryRef, SessionFact,
        MetadataSource, SessionMetadataChanged, SessionRecovered, SessionTitleChanged, SideEffectClass, TitleSource, ToolCallId, ToolError, ToolFinished, ToolIntent,
        ToolIntentPolicyOutcome, ToolMetrics, ToolReplayCapability, ToolTerminalStatus, TurnFinished,
        TurnId, TurnInterrupted, TurnMode, TurnStarted, TurnTerminal,
    },
};
use tokio::sync::watch;

use crate::{
    activity::SessionActivityTracker,
    provider_job::{ProviderOutcome, ProviderRequestSpec, build_model_round_started, prepare_response_commit_facts, run_verified},
    tool_job::{
        ToolAdmission, ToolApprovalGrant, ToolOutcome, ToolRequest, WorkspaceToolPort, run as run_tool,
    },
};

use super::types::{
    ActivePhase, AgentCommand, AgentCompletion, AgentControl, AgentOutcome, QueuedUserInput,
    SessionAgentError, SessionPhase,
};

/// Bounded in-owner input queue. Durable acceptance is the committed
/// `InputAccepted` fact, not this queue.
const MAX_QUEUED_INPUTS: usize = 32;
/// Completion capacity reserved before a provider dispatch. Oversized results
/// stay owned by the supervisor instead of being silently dropped.
const PROVIDER_COMPLETION_BYTES: u32 = 1024 * 1024;
const TOOL_COMPLETION_BYTES: u32 = 256 * 1024;

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

fn tool_result_message(call_id: &str, text: &str, ok: bool) -> qaqh_types::Message {
    qaqh_types::Message {
        msg_id: None,
        role: "tool".into(),
        name: None,
        content: vec![qaqh_types::ContentBlock::ToolResult {
            tool_use_id: call_id.to_string(),
            result: if ok {
                qaqh_types::ToolResult::ok(text)
            } else {
                qaqh_types::ToolResult::error(text)
            },
        }],
    }
}

fn recovery_ref(seed: &str) -> RecoveryRef {
    RecoveryRef {
        recovery_id: RecoveryId::new(format!("recovery_{}", generate_ulid())),
        recovery_event_id: EventId::new(generate_ulid()),
        recovery_input_fingerprint: sha256_content_hash(seed.as_bytes()),
    }
}

/// Optional daemon-side activity tap. Absent in unit tests.
#[derive(Clone)]
pub struct ActivityTap {
    pub tracker: SessionActivityTracker,
    pub generation: u64,
}

impl ActivityTap {
    fn observe(&self, session_id: &str, event: serde_json::Value) {
        self.tracker.observe(session_id, self.generation, &event);
    }
}

/// A tool that has been declared and intended, but whose result is still owed.
struct ActiveTool {
    identity: EffectIdentity,
    execution_id: ExecutionId,
}

/// A tool waiting for a user permission decision.
struct PendingPermission {
    interaction_id: InteractionId,
    execution_id: ExecutionId,
    args_hash: String,
    tool_name: String,
    args: Vec<u8>,
}

enum PendingInteraction {
    Ask {
        call_id: ToolCallId,
        interaction_id: InteractionId,
        execution_id: ExecutionId,
    },
    Permission {
        call_id: ToolCallId,
    },
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
    pub(crate) activity: Option<ActivityTap>,
    pub(crate) active_provider_identity: Option<EffectIdentity>,
    pub(crate) active_tool_identities: HashMap<ToolCallId, ActiveTool>,
    pub(crate) pending_permissions: HashMap<ToolCallId, PendingPermission>,
    pub(crate) pending_interaction: Option<PendingInteraction>,
    pub(crate) queued_inputs: VecDeque<QueuedUserInput>,
    /// Causation of the command currently being processed.
    pub(crate) causation: Option<EventId>,
    system_prompt_seeded: bool,
    /// 首条输入后是否已经就标题做过决定（只探一次投影，避免每条输入都重放）。
    title_decided: bool,
}

impl SessionAgent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: String,
        incarnation: u64,
        mut commit: SessionCommit,
        supervisor: Arc<Mutex<Supervisor<AgentOutcome, CommittedExecution>>>,
        mailbox_handle: MailboxHandle<AgentCommand, AgentControl, AgentCompletion>,
        provider_spec: ProviderRequestSpec,
        tool_port: Arc<WorkspaceToolPort>,
        activity: Option<ActivityTap>,
    ) -> Self {
        let mut queued_inputs = VecDeque::new();
        if let Err(error) = recover_open_work(&mut commit, &mut queued_inputs) {
            log::error!("[session-agent] recovery failed for {session_id}: {error}");
        }
        let mut agent = Self {
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
            activity,
            active_provider_identity: None,
            active_tool_identities: HashMap::new(),
            pending_permissions: HashMap::new(),
            pending_interaction: None,
            queued_inputs,
            causation: None,
            system_prompt_seeded: false,
            title_decided: false,
        };
        // Resume inputs that were durably accepted but never started.
        if agent.phase == SessionPhase::Ready
            && let Err(error) = agent.start_next_turn()
        {
            log::error!(
                "[session-agent] failed to resume queued input for {}: {error}",
                agent.session_id
            );
            agent.phase = SessionPhase::Blocked(error.to_string());
        }
        agent
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

    pub fn set_provider_spec(&mut self, spec: ProviderRequestSpec) {
        self.provider_spec = spec;
    }

    fn observe(&self, event: serde_json::Value) {
        if let Some(tap) = &self.activity {
            tap.observe(&self.session_id, event);
        }
    }

    fn fact(
        &self,
        event_id: EventId,
        payload: FactPayload,
        turn_id: Option<TurnId>,
        call_id: Option<ToolCallId>,
        interaction_id: Option<InteractionId>,
    ) -> SessionFact {
        SessionFact {
            schema: FactSchema::v2(),
            session_id: self.commit.session_id().clone(),
            log_id: self.commit.log_id().clone(),
            fact_seq: 0,
            event_id,
            ts_ms: now_ms(),
            causation_id: self.causation.clone(),
            turn_id,
            call_id,
            interaction_id,
            payload,
        }
    }

    fn commit_batch(
        &mut self,
        batch_id: &str,
        facts: Vec<SessionFact>,
    ) -> Result<BatchReceipt, SessionAgentError> {
        let head = self.commit.head()?;
        Ok(self.commit.commit(batch_id, head, facts)?)
    }

    /// Reserve completion capacity *before* an intent is committed, so a full
    /// lane can never strand already-durable work.
    fn reserve_completion(
        &self,
        bytes: u32,
    ) -> Result<CompletionReservation<AgentCompletion>, SessionAgentError> {
        self.mailbox_handle
            .try_reserve_completion(bytes)
            .map_err(SessionAgentError::Mailbox)
    }

    fn dispatch_task<F, Fut>(
        &mut self,
        ticket: DispatchTicket,
        reservation: CompletionReservation<AgentCompletion>,
        make: F,
    ) -> Result<(), SessionAgentError>
    where
        F: FnOnce(CommittedExecution, watch::Receiver<bool>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = AgentOutcome> + Send + 'static,
    {
        let identity = ticket.identity().clone();
        let sup_lock = Arc::clone(&self.supervisor);
        {
            let mut sup = sup_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            sup.start(ticket, reservation, move |execution, cancel_rx| async move {
                make(execution, cancel_rx).await
            })
            .map_err(SessionAgentError::Supervisor)?;
        }

        let sup_clone = Arc::clone(&self.supervisor);
        let ident_clone = identity;
        tokio::spawn(async move {
            loop {
                {
                    let mut sup = sup_clone
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if sup.is_finished(&ident_clone) {
                        // `collect` completes on the first poll once the handle
                        // is finished, so it is polled outside async hold.
                        match poll_once(sup.collect(&ident_clone)) {
                            Some(Ok(_completion)) => {
                                let _ = sup.deliver(&ident_clone);
                            }
                            Some(Err(error)) => log::warn!(
                                "[session-agent] completion collection failed for {}: {error:?}",
                                ident_clone.effect_id
                            ),
                            None => {}
                        }
                        return;
                    }
                }
                // The task is still running; never block the actor on it.
                tokio::task::yield_now().await;
            }
        });
        Ok(())
    }


    fn ensure_system_prompt(&mut self) -> Result<(), SessionAgentError> {
        if self.system_prompt_seeded {
            return Ok(());
        }
        self.system_prompt_seeded = true;
        if !self.commit.request_messages()?.is_empty() {
            return Ok(());
        }
        let prompt = crate::session_agent::prompt::system_prompt(&self.provider_spec);
        let message = text_message("system", &prompt);
        let bytes =
            serde_json::to_vec(&message).map_err(|e| SessionAgentError::Internal(e.to_string()))?;
        let message_ref = self.commit.put_blob(&bytes)?;
        let fact = self.fact(
            EventId::new(generate_ulid()),
            FactPayload::ContextMessageCommitted(ContextMessageCommitted {
                message_ref,
                origin: ContextMessageOrigin::FrozenPrefix,
            }),
            None,
            None,
            None,
        );
        self.commit_batch(
            &format!("batch-prefix-{}", self.commit.head()? + 1),
            vec![fact],
        )?;
        Ok(())
    }

    fn accept_input(&mut self, input: &QueuedUserInput) -> Result<(), SessionAgentError> {
        let fact = self.fact(
            EventId::new(generate_ulid()),
            FactPayload::InputAccepted(InputAccepted {
                input_id: input.input_id.clone(),
                input_kind: InputKind::UserText,
                input_purpose: InputPurpose::TriggerTurn,
                content_ref: None,
                inline_text: Some(truncate_text(&input.text, 8 * 1024)),
                attachments: input.attachments.clone(),
                actor: ActorRef {
                    kind: ActorKind::User,
                    id: "user".into(),
                    display_name: None,
                },
                client_request_id: input.client_request_id.clone(),
            }),
            None,
            None,
            None,
        );
        let mut facts = vec![fact];
        // 首条用户输入顺带定标题：与 `InputAccepted` 同一批次，原子可见。
        let title_written = match self.auto_title_fact(&input.text) {
            Some(title_fact) => {
                facts.push(title_fact);
                true
            }
            None => false,
        };
        self.commit_batch(
            &format!("batch-input-{}", input.input_id.as_str()),
            facts,
        )?;
        if title_written {
            // 标题已进日志，但 `SessionManager::list` 读的是 index.jsonl 缓存。
            qaqh_session::meta::refresh_index(self.commit.session_dir());
        }
        Ok(())
    }

    /// 首条用户输入派生标题（不调模型、不花额外 token）。
    ///
    /// 只在会话尚无标题时写 `SessionTitleChanged`（`TitleSource::Auto`），
    /// 与用户手改标题走同一条读路径（投影 → `MetaDelta::TitleChanged` →
    /// `session.list`）。要升级成模型摘要，只需把 [`derive_title`] 换成
    /// 一次 provider 调用；事实形状不变。
    fn auto_title_fact(&mut self, text: &str) -> Option<SessionFact> {
        if self.title_decided {
            return None;
        }
        self.title_decided = true;
        let Ok(snapshot) = self.commit.snapshot() else {
            // 读不到投影就不写标题：宁可没标题，也不要覆盖用户已有的标题。
            return None;
        };
        if snapshot
            .meta
            .title
            .as_deref()
            .is_some_and(|title| !title.trim().is_empty())
        {
            return None;
        }
        let title = derive_title(text)?;
        Some(self.fact(
            EventId::new(generate_ulid()),
            FactPayload::SessionTitleChanged(SessionTitleChanged {
                title,
                source: TitleSource::Auto,
                changed_at_ms: now_ms(),
            }),
            None,
            None,
            None,
        ))
    }

    fn push_input(&mut self, input: QueuedUserInput) -> Result<(), SessionAgentError> {
        if self.queued_inputs.len() >= MAX_QUEUED_INPUTS {
            return Err(SessionAgentError::Internal(
                "input queue is full; retry after the current turn settles".into(),
            ));
        }
        let mut input = input;
        if !input.accepted {
            self.accept_input(&input)?;
            input.accepted = true;
        }
        self.queued_inputs.push_back(input);
        Ok(())
    }

    fn start_next_turn(&mut self) -> Result<(), SessionAgentError> {
        if self.phase != SessionPhase::Ready {
            return Ok(());
        }
        let Some(mut input) = self.queued_inputs.pop_front() else {
            return Ok(());
        };
        if !input.accepted {
            self.accept_input(&input)?;
            input.accepted = true;
        }
        self.begin_turn(input)
    }

    fn begin_turn(&mut self, input: QueuedUserInput) -> Result<(), SessionAgentError> {
        self.ensure_system_prompt()?;
        let turn_id = TurnId::new(format!("turn_{}", generate_ulid()));
        self.active_turn = Some(turn_id.clone());
        self.active_input = Some(input.input_id.clone());
        self.current_round = 1;
        self.current_attempt = 1;

        let started = self.fact(
            EventId::new(generate_ulid()),
            FactPayload::TurnStarted(TurnStarted {
                turn_id: turn_id.clone(),
                input_id: input.input_id.clone(),
                mode: TurnMode::Normal,
                recovery_ref: None,
            }),
            Some(turn_id.clone()),
            None,
            None,
        );

        let user_msg = text_message("user", &input.text);
        let msg_bytes = serde_json::to_vec(&user_msg)
            .map_err(|e| SessionAgentError::Internal(e.to_string()))?;
        let message_ref = self.commit.put_blob(&msg_bytes)?;
        let context_fact = self.fact(
            EventId::new(generate_ulid()),
            FactPayload::ContextMessageCommitted(ContextMessageCommitted {
                message_ref,
                origin: ContextMessageOrigin::Input,
            }),
            Some(turn_id.clone()),
            None,
            None,
        );

        self.phase = SessionPhase::Active(ActivePhase::PreparingContext);
        self.commit_batch(
            &format!("batch-{}-start", turn_id.as_str()),
            vec![started, context_fact],
        )?;
        self.observe(serde_json::json!({
            "type": "turn_start",
            "turn_id": turn_id.as_str(),
        }));
        self.start_round(1)
    }

    fn start_round(&mut self, round: u32) -> Result<(), SessionAgentError> {
        let Some(turn_id) = self.active_turn.clone() else {
            return Err(SessionAgentError::Internal(
                "model round without an active turn".into(),
            ));
        };
        self.current_round = round;
        self.current_attempt = 1;
        let payload = build_model_round_started(
            &self.commit,
            turn_id.clone(),
            round,
            self.current_attempt,
            &self.provider_spec,
        )?;
        let round_event_id = EventId::new(generate_ulid());
        let fact = self.fact(
            round_event_id.clone(),
            FactPayload::ModelRoundStarted(payload),
            Some(turn_id.clone()),
            None,
            None,
        );

        let reservation = self.reserve_completion(PROVIDER_COMPLETION_BYTES)?;
        let head = self.commit.head()?;
        let (_, capability) = self.commit.commit_with_effect(
            &format!("batch-{}-r{}", turn_id.as_str(), round),
            head,
            vec![fact],
            round_event_id.as_str(),
        )?;
        let capability = capability.ok_or_else(|| {
            SessionAgentError::Internal("expected capability for ModelRoundStarted".into())
        })?;
        let identity = capability.identity().clone();
        self.active_provider_identity = Some(identity.clone());

        let ticket = {
            let mut sup = self
                .supervisor
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            sup.authorize(identity, capability)
                .map_err(SessionAgentError::Supervisor)?
        };

        let spec = self.provider_spec.clone();
        self.dispatch_task(ticket, reservation, move |execution, cancel_rx| async move {
            AgentOutcome::Provider(run_verified(spec, execution, cancel_rx).await)
        })?;
        self.phase = SessionPhase::Active(ActivePhase::AwaitingProvider {
            round,
            attempt: self.current_attempt,
        });
        Ok(())
    }

    fn next_round(&mut self) -> Result<(), SessionAgentError> {
        if self.active_turn.is_none() {
            return Ok(());
        }
        if !self.active_tool_identities.is_empty() || self.pending_interaction.is_some() {
            return Ok(());
        }
        let round = self.current_round.saturating_add(1);
        self.start_round(round)
    }

    fn finish_turn(&mut self, terminal: TurnTerminal, usage: Option<qaqh_types::UsageInfo>) {
        self.active_turn = None;
        self.active_input = None;
        self.active_provider_identity = None;
        self.active_tool_identities.clear();
        self.pending_permissions.clear();
        self.pending_interaction = None;
        self.observe(match terminal {
            TurnTerminal::Cancelled => serde_json::json!({ "type": "cancelled" }),
            TurnTerminal::Failed => serde_json::json!({ "type": "failed" }),
            TurnTerminal::Completed => serde_json::json!({ "type": "turn_end" }),
        });
        let _ = usage;
        self.phase = SessionPhase::Ready;
        if let Err(error) = self.start_next_turn() {
            log::error!(
                "[session-agent] failed to start queued turn for {}: {error}",
                self.session_id
            );
            self.phase = SessionPhase::Blocked(error.to_string());
        }
    }

    fn interrupt_turn(
        &mut self,
        reason: InterruptReason,
        usage: Option<qaqh_types::UsageInfo>,
    ) -> Result<(), SessionAgentError> {
        let Some(turn_id) = self.active_turn.clone() else {
            return Ok(());
        };
        let head = self.commit.head()?;
        // Every open obligation must be settled in the same batch: the
        // business reducer refuses a turn terminal with unsettled work.
        let mut facts = Vec::new();
        if let Some(interaction) = self.pending_interaction.take() {
            let interaction_id = match &interaction {
                PendingInteraction::Ask {
                    interaction_id, ..
                } => interaction_id.clone(),
                PendingInteraction::Permission { call_id } => self
                    .pending_permissions
                    .remove(call_id)
                    .map(|pending| pending.interaction_id)
                    .ok_or_else(|| {
                        SessionAgentError::Internal(
                            "permission interaction lost before interruption".into(),
                        )
                    })?,
            };
            facts.push(self.fact(
                EventId::new(generate_ulid()),
                FactPayload::InteractionExpired(qaqh_session::session_fact_v2::InteractionExpired {
                    interaction_id: interaction_id.clone(),
                    reason: InteractionExpiryReason::TurnCancelled,
                    recovery_ref: None,
                    expired_at_ms: now_ms(),
                }),
                Some(turn_id.clone()),
                None,
                Some(interaction_id),
            ));
        }
        let open_tools: Vec<(ToolCallId, ExecutionId)> = self
            .active_tool_identities
            .iter()
            .map(|(call_id, tool)| (call_id.clone(), tool.execution_id.clone()))
            .collect();
        for (call_id, execution_id) in open_tools {
            facts.push(self.tool_finished_fact(
                &turn_id,
                &call_id,
                Some(execution_id),
                ToolTerminalStatus::Indeterminate,
                Vec::new(),
                Some(ToolError {
                    code: "tool_interrupted".into(),
                    message: "turn interrupted before the tool result was observed".into(),
                    retryable: true,
                    details_ref: None,
                }),
                false,
            )?);
        }
        facts.push(self.fact(
            EventId::new(generate_ulid()),
            FactPayload::TurnInterrupted(TurnInterrupted {
                turn_id: turn_id.clone(),
                reason,
                last_fact_seq: head,
                recovery_ref: recovery_ref(reason_name(reason)),
            }),
            Some(turn_id.clone()),
            None,
            None,
        ));
        self.commit_batch(&format!("batch-{}-interrupt", turn_id.as_str()), facts)?;
        let terminal = match reason {
            InterruptReason::CancelBeforeSeal => TurnTerminal::Cancelled,
            _ => TurnTerminal::Failed,
        };
        self.finish_turn(terminal, usage);
        Ok(())
    }

    fn tool_finished_fact(
        &self,
        turn_id: &TurnId,
        call_id: &ToolCallId,
        execution_id: Option<ExecutionId>,
        status: ToolTerminalStatus,
        output: Vec<u8>,
        error: Option<ToolError>,
        backgrounded: bool,
    ) -> Result<SessionFact, SessionAgentError> {
        let output_ref = Some(self.commit.put_blob(&output)?);
        let finished_at_ms = now_ms();
        Ok(self.fact(
            EventId::new(generate_ulid()),
            FactPayload::ToolFinished(ToolFinished {
                call_id: call_id.clone(),
                execution_id,
                terminal_status: status,
                output_ref,
                error,
                metrics: ToolMetrics {
                    started_at_ms: finished_at_ms.saturating_sub(1),
                    finished_at_ms,
                    retry_count: 0,
                    output_bytes: output.len() as u64,
                    progress_bytes_total: output.len() as u64,
                },
                reconciled: false,
                evidence_ref: None,
                evidence_fact_seq: None,
                evidence_event_id: None,
                recovery_ref: None,
                finished_at_ms,
            }),
            Some(turn_id.clone()),
            Some(call_id.clone()),
            None,
        )
        .with_backgrounded(backgrounded))
    }

    fn tool_result_context_fact(
        &self,
        turn_id: &TurnId,
        call_id: &ToolCallId,
        text: &str,
        ok: bool,
    ) -> Result<SessionFact, SessionAgentError> {
        let message = tool_result_message(call_id.as_str(), text, ok);
        let bytes = serde_json::to_vec(&message)
            .map_err(|e| SessionAgentError::Internal(e.to_string()))?;
        let message_ref = self.commit.put_blob(&bytes)?;
        Ok(self.fact(
            EventId::new(generate_ulid()),
            FactPayload::ContextMessageCommitted(ContextMessageCommitted {
                message_ref,
                origin: ContextMessageOrigin::ToolResult,
            }),
            Some(turn_id.clone()),
            Some(call_id.clone()),
            None,
        ))
    }

    fn deny_tool(
        &mut self,
        turn_id: &TurnId,
        call_id: &ToolCallId,
        reason: &str,
    ) -> Result<(), SessionAgentError> {
        let finished = self.tool_finished_fact(
            turn_id,
            call_id,
            None,
            ToolTerminalStatus::Denied,
            Vec::new(),
            Some(ToolError {
                code: "tool_denied".into(),
                message: truncate_text(reason, 512),
                retryable: false,
                details_ref: None,
            }),
            false,
        )?;
        let context = self.tool_result_context_fact(turn_id, call_id, reason, false)?;
        self.commit_batch(
            &format!("batch-{}-deny-{}", turn_id.as_str(), call_id.as_str()),
            vec![finished, context],
        )?;
        Ok(())
    }

    fn request_permission(
        &mut self,
        turn_id: &TurnId,
        call_id: &ToolCallId,
        execution_id: &ExecutionId,
        tool_name: &str,
        args: &[u8],
        reason: &str,
    ) -> Result<(), SessionAgentError> {
        let interaction_id = InteractionId::new(format!("int_{}", generate_ulid()));
        let choices = permission_choices();
        let request = serde_json::json!({
            "tool": tool_name,
            "reason": reason,
            "choices": &choices,
        });
        let request_bytes = serde_json::to_vec(&request)
            .map_err(|e| SessionAgentError::Internal(e.to_string()))?;
        let request_ref = self.commit.put_blob(&request_bytes)?;
        let fact = self.fact(
            EventId::new(generate_ulid()),
            FactPayload::InteractionRequested(InteractionRequested {
                interaction_id: interaction_id.clone(),
                call_id: Some(call_id.clone()),
                turn_id: turn_id.clone(),
                kind: InteractionKind::Permission,
                request_ref,
                choices,
                expires_at_ms: None,
                requested_at_ms: now_ms(),
            }),
            Some(turn_id.clone()),
            Some(call_id.clone()),
            Some(interaction_id.clone()),
        );
        self.commit_batch(
            &format!("batch-perm-{}", interaction_id.as_str()),
            vec![fact],
        )?;
        self.pending_permissions.insert(
            call_id.clone(),
            PendingPermission {
                interaction_id,
                execution_id: execution_id.clone(),
                args_hash: WorkspaceToolPort::args_hash(args),
                tool_name: tool_name.to_string(),
                args: args.to_vec(),
            },
        );
        self.pending_interaction = Some(PendingInteraction::Permission {
            call_id: call_id.clone(),
        });
        self.phase = SessionPhase::Active(ActivePhase::AwaitingUser {
            interaction_id: InteractionId::new("permission"),
        });
        self.observe(serde_json::json!({ "type": "permission_request" }));
        Ok(())
    }

    fn dispatch_tool(
        &mut self,
        turn_id: &TurnId,
        call_id: &ToolCallId,
        tool_name: &str,
        args: &[u8],
        execution_id: &ExecutionId,
    ) -> Result<(), SessionAgentError> {
        let args_ref = self.commit.put_blob(args)?;
        let args_hash = sha256_content_hash(args);
        let intent_event_id = EventId::new(generate_ulid());
        let intent = self.fact(
            intent_event_id.clone(),
            FactPayload::ToolIntent(ToolIntent {
                call_id: call_id.clone(),
                execution_id: execution_id.clone(),
                idempotency_key: None,
                replay_capability: ToolReplayCapability::NoReplay,
                policy_decision: PolicyDecisionRef {
                    outcome: ToolIntentPolicyOutcome::Allow,
                    rule_id: "policy.evaluated".into(),
                    decided_at_ms: now_ms(),
                    reason_ref: None,
                },
                effective_args_ref: Some(args_ref),
                effective_args_hash: Some(args_hash),
                sandbox_spec_hash: sha256_content_hash(b"sandbox_spec"),
                side_effect_class: SideEffectClass::WorkspaceWrite,
                intent_at_ms: now_ms(),
            }),
            Some(turn_id.clone()),
            Some(call_id.clone()),
            None,
        );

        let reservation = self.reserve_completion(TOOL_COMPLETION_BYTES)?;
        let head = self.commit.head()?;
        let (_, capability) = self.commit.commit_with_effect(
            &format!("batch-{}-intent-{}", turn_id.as_str(), call_id.as_str()),
            head,
            vec![intent],
            intent_event_id.as_str(),
        )?;
        let capability = capability.ok_or_else(|| {
            SessionAgentError::Internal("expected capability for ToolIntent".into())
        })?;
        let identity = capability.identity().clone();
        self.active_tool_identities.insert(
            call_id.clone(),
            ActiveTool {
                identity: identity.clone(),
                execution_id: execution_id.clone(),
            },
        );
        let ticket = {
            let mut sup = self
                .supervisor
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            sup.authorize(identity, capability)
                .map_err(SessionAgentError::Supervisor)?
        };
        let port = Arc::clone(&self.tool_port);
        let name = tool_name.to_string();
        self.dispatch_task(ticket, reservation, move |execution, cancel_rx| async move {
            AgentOutcome::Tool(run_tool(port, name, execution, cancel_rx).await)
        })?;
        Ok(())
    }

    fn declaration_request(&self, call_id: &ToolCallId, name: &str, args: &[u8]) -> ToolRequest {
        ToolRequest {
            execution_id: format!("exec_{}", generate_ulid()),
            call_id: call_id.as_str().to_string(),
            tool_name: name.to_string(),
            side_effect_class: SideEffectClass::WorkspaceWrite,
            replay: ToolReplayCapability::NoReplay,
            args: args.to_vec(),
        }
    }
}

/// Small helper so the backgrounded flag is visible at the fact construction
/// site even though the schema keeps it in `terminal_status`.
trait FactBackgrounded {
    fn with_backgrounded(self, backgrounded: bool) -> Self;
}

impl FactBackgrounded for SessionFact {
    fn with_backgrounded(mut self, backgrounded: bool) -> Self {
        if backgrounded && let FactPayload::ToolFinished(payload) = &mut self.payload {
            payload.terminal_status = ToolTerminalStatus::Backgrounded;
        }
        self
    }
}

/// Poll a future exactly once without an async context.
fn poll_once<F: std::future::Future>(future: F) -> Option<F::Output> {
    let mut future = std::pin::pin!(future);
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    match future.as_mut().poll(&mut context) {
        std::task::Poll::Ready(value) => Some(value),
        std::task::Poll::Pending => None,
    }
}

fn truncate_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// 权限交互的固定选项。文案与语义由 daemon 拥有，客户端只负责渲染。
///
/// 刻意**不提供**「本会话一直允许」：权限层没有对应的持久化语义，
/// 放一个按钮却无法兑现，就是把「不知道」写成「知道」。
fn permission_choices() -> Vec<InteractionChoice> {
    vec![
        InteractionChoice {
            decision: InteractionDecision::Approved,
            label: "允许".to_string(),
            description: None,
            tone: InteractionChoiceTone::Primary,
        },
        InteractionChoice {
            decision: InteractionDecision::Rejected,
            label: "拒绝".to_string(),
            description: None,
            tone: InteractionChoiceTone::Destructive,
        },
    ]
}

/// 从首条用户消息派生标题：压平空白后取前 `MAX_CHARS` 个字符。
///
/// 刻意不调模型：一次后台摘要要多一次 provider 请求，还要自建重试与失败
/// 降级；标题只是列表页的导航辅助，原文前 40 字已经够用，而且**不会把
/// 首条消息再发给模型一遍**。要换成模型摘要时，只替换本函数即可。
fn derive_title(text: &str) -> Option<String> {
    const MAX_CHARS: usize = 40;
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    let mut title: String = collapsed.chars().take(MAX_CHARS).collect();
    if collapsed.chars().count() > MAX_CHARS {
        title.push('…');
    }
    Some(title)
}

fn reason_name(reason: InterruptReason) -> &'static str {
    match reason {
        InterruptReason::Crash => "interrupt_crash",
        InterruptReason::Restart => "interrupt_restart",
        InterruptReason::CancelBeforeSeal => "interrupt_cancel",
        InterruptReason::UnknownFact => "interrupt_unknown_fact",
    }
}

/// Settle every obligation left open by a previous incarnation, then rebuild
/// the durable input queue. Runs once, before the owner accepts any command.
fn recover_open_work(
    commit: &mut SessionCommit,
    queued: &mut VecDeque<QueuedUserInput>,
) -> Result<(), SessionAgentError> {
    let obligations = commit.obligations()?;
    if obligations.has_open_work() {
        let head = commit.head()?;
        let mut facts = Vec::new();
        let mut actions = Vec::new();
        for interaction in &obligations.unsettled_interactions {
            facts.push(SessionFact {
                schema: FactSchema::v2(),
                session_id: commit.session_id().clone(),
                log_id: commit.log_id().clone(),
                fact_seq: 0,
                event_id: EventId::new(generate_ulid()),
                ts_ms: now_ms(),
                causation_id: None,
                turn_id: Some(interaction.turn_id.clone()),
                call_id: interaction.call_id.clone(),
                interaction_id: Some(interaction.interaction_id.clone()),
                payload: FactPayload::InteractionExpired(
                    qaqh_session::session_fact_v2::InteractionExpired {
                        interaction_id: interaction.interaction_id.clone(),
                        reason: InteractionExpiryReason::RestartPolicy,
                        recovery_ref: None,
                        expired_at_ms: now_ms(),
                    },
                ),
            });
            actions.push(RecoveryAction::InteractionExpired {
                interaction_id: interaction.interaction_id.clone(),
                reason: InteractionExpiryReason::RestartPolicy,
            });
        }
        for tool in &obligations.unsettled_tools {
            let status = if tool.execution_id.is_some() {
                ToolTerminalStatus::Indeterminate
            } else {
                ToolTerminalStatus::Cancelled
            };
            facts.push(SessionFact {
                schema: FactSchema::v2(),
                session_id: commit.session_id().clone(),
                log_id: commit.log_id().clone(),
                fact_seq: 0,
                event_id: EventId::new(generate_ulid()),
                ts_ms: now_ms(),
                causation_id: None,
                turn_id: Some(tool.turn_id.clone()),
                call_id: Some(tool.call_id.clone()),
                interaction_id: None,
                payload: FactPayload::ToolFinished(ToolFinished {
                    call_id: tool.call_id.clone(),
                    execution_id: tool.execution_id.clone(),
                    terminal_status: status,
                    output_ref: None,
                    error: Some(ToolError {
                        code: "tool_indeterminate".into(),
                        message: "previous incarnation ended before the tool settled".into(),
                        retryable: true,
                        details_ref: None,
                    }),
                    metrics: ToolMetrics {
                        started_at_ms: now_ms().saturating_sub(1),
                        finished_at_ms: now_ms(),
                        retry_count: 0,
                        output_bytes: 0,
                        progress_bytes_total: 0,
                    },
                    reconciled: true,
                    evidence_ref: None,
                    evidence_fact_seq: None,
                    evidence_event_id: None,
                    recovery_ref: None,
                    finished_at_ms: now_ms(),
                }),
            });
        }
        if let Some(turn_id) = &obligations.active_turn {
            facts.push(SessionFact {
                schema: FactSchema::v2(),
                session_id: commit.session_id().clone(),
                log_id: commit.log_id().clone(),
                fact_seq: 0,
                event_id: EventId::new(generate_ulid()),
                ts_ms: now_ms(),
                causation_id: None,
                turn_id: Some(turn_id.clone()),
                call_id: None,
                interaction_id: None,
                payload: FactPayload::TurnInterrupted(TurnInterrupted {
                    turn_id: turn_id.clone(),
                    reason: InterruptReason::Restart,
                    last_fact_seq: head,
                    recovery_ref: recovery_ref("recovery_restart"),
                }),
            });
            actions.push(RecoveryAction::TurnInterrupted {
                turn_id: turn_id.clone(),
                last_fact_seq: head,
            });
            facts.push(SessionFact {
                schema: FactSchema::v2(),
                session_id: commit.session_id().clone(),
                log_id: commit.log_id().clone(),
                fact_seq: 0,
                event_id: EventId::new(generate_ulid()),
                ts_ms: now_ms(),
                causation_id: None,
                turn_id: None,
                call_id: None,
                interaction_id: None,
                payload: FactPayload::SessionRecovered(SessionRecovered {
                    recovery_id: RecoveryId::new(format!("recovery_{}", generate_ulid())),
                    recovery_event_id: EventId::new(generate_ulid()),
                    recovery_input_fingerprint: sha256_content_hash(b"agent_restart"),
                    outcome: RecoveryOutcome::Writable,
                    last_good_fact_seq: head,
                    torn_tail: false,
                    torn_bytes: None,
                    actions,
                    recovered_at_ms: now_ms(),
                }),
            });
        }
        if !facts.is_empty() {
            let batch = match &obligations.active_turn {
                Some(turn_id) => format!("batch-{}-recovered", turn_id.as_str()),
                None => format!("batch-recovered-{}", generate_ulid()),
            };
            commit.commit(&batch, head, facts)?;
        }
        log::warn!(
            "[session-agent] recovered {} from open obligations",
            commit.session_id().as_str()
        );
    }

    // Re-enqueue accepted-but-unstarted inputs with their committed text.
    let pending = commit.obligations()?.pending_inputs;
    if pending.is_empty() {
        return Ok(());
    }
    let mut texts: HashMap<String, (String, Vec<ContentRef>, Option<String>)> = HashMap::new();
    for fact in commit.committed_facts()? {
        if let FactPayload::InputAccepted(input) = &fact.payload {
            texts.insert(
                input.input_id.as_str().to_string(),
                (
                    input.inline_text.clone().unwrap_or_default(),
                    input.attachments.clone(),
                    input.client_request_id.clone(),
                ),
            );
        }
    }
    for input_id in pending {
        let (text, attachments, client_request_id) = texts
            .remove(input_id.as_str())
            .unwrap_or_else(|| (String::new(), Vec::new(), None));
        queued.push_back(QueuedUserInput {
            input_id,
            text,
            attachments,
            client_request_id,
            accepted: true,
        });
    }
    Ok(())
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
                self.causation = client_request_id
                    .as_deref()
                    .and_then(causation_for_command);
                let input = QueuedUserInput {
                    input_id: input_id.clone(),
                    text: text.clone(),
                    attachments: attachments.clone(),
                    client_request_id: client_request_id.clone(),
                    accepted: false,
                };
                if self.phase == SessionPhase::Ready && self.queued_inputs.is_empty() {
                    let mut input = input;
                    self.accept_input(&input)?;
                    input.accepted = true;
                    self.begin_turn(input)?;
                } else {
                    self.push_input(input)?;
                }
                Ok(Step::Continue)
            }
            AgentCommand::QueueInput {
                input_id,
                text,
                attachments,
                client_request_id,
            } => {
                self.causation = client_request_id
                    .as_deref()
                    .and_then(causation_for_command);
                self.push_input(QueuedUserInput {
                    input_id: input_id.clone(),
                    text: text.clone(),
                    attachments: attachments.clone(),
                    client_request_id: client_request_id.clone(),
                    accepted: false,
                })?;
                self.start_next_turn()?;
                Ok(Step::Continue)
            }
            AgentCommand::SetMetadata { patch } => {
                let fact = self.fact(
                    EventId::new(generate_ulid()),
                    FactPayload::SessionMetadataChanged(SessionMetadataChanged {
                        patch: patch.clone(),
                        source: MetadataSource::User,
                        changed_at_ms: now_ms(),
                    }),
                    None,
                    None,
                    None,
                );
                self.commit_batch(
                    &format!("batch-meta-{}", self.commit.head()? + 1),
                    vec![fact],
                )?;
                qaqh_session::meta::refresh_index(self.commit.session_dir());
                Ok(Step::Continue)
            }
            AgentCommand::SetTitle { title, source } => {
                let fact = self.fact(
                    EventId::new(generate_ulid()),
                    FactPayload::SessionTitleChanged(SessionTitleChanged {
                        title: truncate_text(title, 4 * 1024),
                        source: *source,
                        changed_at_ms: now_ms(),
                    }),
                    None,
                    None,
                    None,
                );
                self.commit_batch(
                    &format!("batch-title-{}", self.commit.head()? + 1),
                    vec![fact],
                )?;
                qaqh_session::meta::refresh_index(self.commit.session_dir());
                Ok(Step::Continue)
            }
        }
    }

    fn control(&mut self, control: &AgentControl) -> Result<Step, Self::Error> {
        match control {
            AgentControl::Shutdown => {
                self.phase = SessionPhase::Closing;
                // Never exit with open obligations: the next owner would have
                // to reconcile a turn nobody can prove finished.
                if self.active_turn.is_some() {
                    let _ = self.interrupt_turn(InterruptReason::CancelBeforeSeal, None);
                }
                Ok(Step::Stop)
            }
            AgentControl::CancelTurn { turn_id, reason } => {
                self.causation = None;
                let matches_turn = self
                    .active_turn
                    .as_ref()
                    .is_some_and(|turn| turn_id.as_ref().is_none_or(|want| want == turn));
                if !matches_turn {
                    return Ok(Step::Continue);
                }
                let identities: Vec<EffectIdentity> = self
                    .active_provider_identity
                    .iter()
                    .cloned()
                    .chain(
                        self.active_tool_identities
                            .values()
                            .map(|tool| tool.identity.clone()),
                    )
                    .collect();
                {
                    let mut sup = self
                        .supervisor
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    for identity in identities {
                        let _ = sup.request_cancel(&identity);
                    }
                }
                self.active_provider_identity = None;
                self.interrupt_turn(InterruptReason::CancelBeforeSeal, None)?;
                log::info!(
                    "[session-agent] cancelled turn for {} ({reason})",
                    self.session_id
                );
                Ok(Step::Continue)
            }
            AgentControl::ResolveInteraction {
                interaction_id,
                answers,
            } => {
                let Some(PendingInteraction::Ask {
                    call_id,
                    interaction_id: expected,
                    execution_id,
                }) = self.pending_interaction.take()
                else {
                    return Ok(Step::Continue);
                };
                if &expected != interaction_id {
                    // Stale answer for a different request: keep waiting.
                    self.pending_interaction = Some(PendingInteraction::Ask {
                        call_id,
                        interaction_id: expected,
                        execution_id,
                    });
                    return Ok(Step::Continue);
                }
                let Some(turn_id) = self.active_turn.clone() else {
                    return Ok(Step::Continue);
                };
                let answers_json = serde_json::to_vec(answers)
                    .map_err(|e| SessionAgentError::Internal(e.to_string()))?;
                let decision_ref = self.commit.put_blob(&answers_json)?;
                let resolved = self.fact(
                    EventId::new(generate_ulid()),
                    FactPayload::InteractionResolved(InteractionResolved {
                        interaction_id: interaction_id.clone(),
                        decision_ref,
                        decision: Some(InteractionDecision::Answered),
                        resolved_by: ActorRef {
                            kind: ActorKind::User,
                            id: "user".into(),
                            display_name: None,
                        },
                        resolution_seq: 1,
                        resolved_at_ms: now_ms(),
                    }),
                    Some(turn_id.clone()),
                    Some(call_id.clone()),
                    Some(interaction_id.clone()),
                );
                let finished = self.tool_finished_fact(
                    &turn_id,
                    &call_id,
                    Some(execution_id),
                    ToolTerminalStatus::Succeeded,
                    answers_json.clone(),
                    None,
                    false,
                )?;
                let context = self.tool_result_context_fact(
                    &turn_id,
                    &call_id,
                    &String::from_utf8_lossy(&answers_json),
                    true,
                )?;
                self.commit_batch(
                    &format!("batch-{}-ask-res-{}", turn_id.as_str(), call_id.as_str()),
                    vec![resolved, finished, context],
                )?;
                self.observe(serde_json::json!({ "type": "ask_resolved" }));
                self.phase = SessionPhase::Active(ActivePhase::AwaitingProvider {
                    round: self.current_round,
                    attempt: self.current_attempt,
                });
                self.next_round()?;
                Ok(Step::Continue)
            }
            AgentControl::ToolPermissionRespond {
                tool_call_id,
                approved,
                trust_folder,
            } => {
                let Some(pending) = self.pending_permissions.remove(tool_call_id) else {
                    return Ok(Step::Continue);
                };
                let Some(PendingInteraction::Permission { call_id }) =
                    self.pending_interaction.take()
                else {
                    return Ok(Step::Continue);
                };
                if &call_id != tool_call_id {
                    return Ok(Step::Continue);
                }
                let Some(turn_id) = self.active_turn.clone() else {
                    return Ok(Step::Continue);
                };
                let workspace_root = self.tool_port.workspace_root();
                if *trust_folder {
                    self.tool_port.trust_dir(workspace_root.clone());
                }
                let decision = if *approved {
                    InteractionDecision::Approved
                } else {
                    InteractionDecision::Rejected
                };
                let decision_bytes = serde_json::to_vec(&serde_json::json!({
                    "approved": approved,
                    "trust_folder": trust_folder,
                }))
                .map_err(|e| SessionAgentError::Internal(e.to_string()))?;
                let decision_ref = self.commit.put_blob(&decision_bytes)?;
                let resolved = self.fact(
                    EventId::new(generate_ulid()),
                    FactPayload::InteractionResolved(InteractionResolved {
                        interaction_id: pending.interaction_id.clone(),
                        decision_ref,
                        decision: Some(decision),
                        resolved_by: ActorRef {
                            kind: ActorKind::User,
                            id: "user".into(),
                            display_name: None,
                        },
                        resolution_seq: 1,
                        resolved_at_ms: now_ms(),
                    }),
                    Some(turn_id.clone()),
                    Some(tool_call_id.clone()),
                    Some(pending.interaction_id.clone()),
                );
                self.commit_batch(
                    &format!("batch-{}-perm-{}", turn_id.as_str(), pending.interaction_id.as_str()),
                    vec![resolved],
                )?;

                if *approved {
                    self.tool_port
                        .grant_approval(ToolApprovalGrant {
                            interaction_id: pending.interaction_id.as_str().to_string(),
                            call_id: tool_call_id.as_str().to_string(),
                            execution_id: pending.execution_id.as_str().to_string(),
                            args_hash: pending.args_hash.clone(),
                            workspace_root,
                        });
                    if let Err(error) = self.dispatch_tool(
                        &turn_id,
                        tool_call_id,
                        &pending.tool_name,
                        &pending.args,
                        &pending.execution_id,
                    ) {
                        // A refused dispatch must still settle the call.
                        self.deny_tool(
                            &turn_id,
                            tool_call_id,
                            &format!("tool dispatch failed: {error}"),
                        )?;
                        self.next_round()?;
                        return Ok(Step::Continue);
                    }
                    self.phase = SessionPhase::Active(ActivePhase::AwaitingTools {
                        active_calls: self.active_tool_identities.keys().cloned().collect(),
                    });
                } else {
                    self.deny_tool(&turn_id, tool_call_id, "permission denied by user")?;
                    self.next_round()?;
                }
                Ok(Step::Continue)
            }
            AgentControl::ReloadProvider { spec } => {
                self.provider_spec = (**spec).clone();
                Ok(Step::Continue)
            }
        }
    }

    fn completion(&mut self, completion: &Arc<AgentCompletion>) -> Result<Step, Self::Error> {
        // Settlement first: acknowledgement releases the retained evidence, so
        // it may only happen after the business commit succeeded.
        let result = self.settle_completion(completion);
        match &result {
            Ok(()) => {
                let mut sup = self
                    .supervisor
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Err(error) = sup.acknowledge(&completion.identity, completion) {
                    log::warn!(
                        "[session-agent] acknowledge failed for {}: {error:?}",
                        completion.identity.effect_id
                    );
                }
            }
            Err(error) => log::error!(
                "[session-agent] settlement failed for {}: {error}",
                completion.identity.effect_id
            ),
        }
        result.map(|()| Step::Continue)
    }
}

impl SessionAgent {
    fn settle_completion(&mut self, completion: &Arc<AgentCompletion>) -> Result<(), SessionAgentError> {
        let identity = &completion.identity;
        match &completion.outcome {
            TaskOutcome::JoinFailed(err) => {
                log::error!(
                    "[session-agent] task {identity:?} joined with failure: {err}"
                );
                self.interrupt_turn(InterruptReason::Crash, None)
            }
            TaskOutcome::Returned(AgentOutcome::Provider(outcome)) => {
                self.settle_provider(identity, outcome)
            }
            TaskOutcome::Returned(AgentOutcome::Tool(outcome)) => {
                self.settle_tool(identity, outcome)
            }
        }
    }

    fn settle_provider(
        &mut self,
        identity: &EffectIdentity,
        outcome: &ProviderOutcome,
    ) -> Result<(), SessionAgentError> {
        let Some(turn_id) = self.active_turn.clone() else {
            log::warn!(
                "[session-agent] late provider completion for {} ignored",
                identity.effect_id
            );
            return Ok(());
        };
        if self
            .active_provider_identity
            .as_ref()
            .is_some_and(|active| active.effect_id != identity.effect_id)
        {
            log::warn!(
                "[session-agent] provider completion {} does not match the active attempt; ignored",
                identity.effect_id
            );
            return Ok(());
        }
        self.active_provider_identity = None;

        if outcome.evidence == DispatchEvidence::NotStarted {
            // Nothing reached the wire; the round never produced a response.
            let cancelled = outcome
                .error
                .as_deref()
                .is_some_and(|message| message.contains("cancelled"));
            return self.interrupt_turn(
                if cancelled {
                    InterruptReason::CancelBeforeSeal
                } else {
                    InterruptReason::Crash
                },
                None,
            );
        }

        let model = self.provider_spec.provider.model.clone();
        let mut facts = prepare_response_commit_facts(
            &self.commit,
            &turn_id,
            self.current_round,
            self.current_attempt,
            &model,
            outcome,
            EventId::new(generate_ulid()),
            |_| EventId::new(generate_ulid()),
            |_| EventId::new(generate_ulid()),
            now_ms(),
        )?;
        for fact in &mut facts {
            fact.causation_id = self.causation.clone();
        }

        match outcome.finish {
            Some(ProviderFinish::Stop) => {
                facts.push(self.fact(
                    EventId::new(generate_ulid()),
                    FactPayload::TurnFinished(TurnFinished {
                        turn_id: turn_id.clone(),
                        terminal: TurnTerminal::Completed,
                        usage: outcome.usage.clone(),
                        error: None,
                        finished_at_ms: now_ms(),
                    }),
                    Some(turn_id.clone()),
                    None,
                    None,
                ));
                self.commit_batch(
                    &format!(
                        "batch-{}-r{}-finish",
                        turn_id.as_str(),
                        self.current_round
                    ),
                    facts,
                )?;
                self.finish_turn(TurnTerminal::Completed, outcome.usage.clone());
                Ok(())
            }
            Some(ProviderFinish::ToolUse) => {
                self.commit_batch(
                    &format!(
                        "batch-{}-r{}-declared",
                        turn_id.as_str(),
                        self.current_round
                    ),
                    facts,
                )?;
                self.start_declared_tools(&turn_id, outcome)
            }
            _ => {
                facts.push(self.fact(
                    EventId::new(generate_ulid()),
                    FactPayload::TurnInterrupted(TurnInterrupted {
                        turn_id: turn_id.clone(),
                        reason: InterruptReason::Crash,
                        last_fact_seq: self.commit.head()?,
                        recovery_ref: recovery_ref("provider_incomplete"),
                    }),
                    Some(turn_id.clone()),
                    None,
                    None,
                ));
                self.commit_batch(
                    &format!(
                        "batch-{}-r{}-damaged",
                        turn_id.as_str(),
                        self.current_round
                    ),
                    facts,
                )?;
                self.finish_turn(TurnTerminal::Failed, outcome.usage.clone());
                Ok(())
            }
        }
    }

    fn start_declared_tools(
        &mut self,
        turn_id: &TurnId,
        outcome: &ProviderOutcome,
    ) -> Result<(), SessionAgentError> {
        let Some(message) = &outcome.message else {
            return self.interrupt_turn(InterruptReason::Crash, None);
        };
        for block in &message.content {
            let qaqh_types::ContentBlock::ToolUse { id, name, input } = block else {
                continue;
            };
            let call_id = crate::provider_job::canonical_tool_call_id(id);
            let args = serde_json::to_vec(input)
                .map_err(|e| SessionAgentError::Internal(e.to_string()))?;

            if name == "ask" {
                let interaction_id = InteractionId::new(format!("int_{}", generate_ulid()));
                let execution_id = ExecutionId::new(format!("exec_{}", generate_ulid()));
                // 请求内容就是已经校验过的 ask 参数（问题/选项都在里面）。
                // 旧实现写死 `b"ask-question"`，客户端拿到 request_ref 也读不出问题。
                let request_ref = self.commit.put_blob(&args)?;
                // 同一份内容，复用同一个 ref（content-addressed，不必再写一次）。
                let args_ref = request_ref.clone();
                let intent = self.fact(
                    EventId::new(generate_ulid()),
                    FactPayload::ToolIntent(ToolIntent {
                        call_id: call_id.clone(),
                        execution_id: execution_id.clone(),
                        idempotency_key: None,
                        replay_capability: ToolReplayCapability::NoReplay,
                        policy_decision: PolicyDecisionRef {
                            outcome: ToolIntentPolicyOutcome::Allow,
                            rule_id: "ask.allow".into(),
                            decided_at_ms: now_ms(),
                            reason_ref: None,
                        },
                        effective_args_ref: Some(args_ref),
                        effective_args_hash: Some(sha256_content_hash(&args)),
                        sandbox_spec_hash: sha256_content_hash(b"sandbox_spec"),
                        side_effect_class: SideEffectClass::ReadOnly,
                        intent_at_ms: now_ms(),
                    }),
                    Some(turn_id.clone()),
                    Some(call_id.clone()),
                    None,
                );
                let requested = self.fact(
                    EventId::new(generate_ulid()),
                    FactPayload::InteractionRequested(InteractionRequested {
                        interaction_id: interaction_id.clone(),
                        call_id: Some(call_id.clone()),
                        turn_id: turn_id.clone(),
                        kind: InteractionKind::Ask,
                        request_ref,
                        choices: Vec::new(),
                        expires_at_ms: None,
                        requested_at_ms: now_ms(),
                    }),
                    Some(turn_id.clone()),
                    Some(call_id.clone()),
                    Some(interaction_id.clone()),
                );
                self.commit_batch(
                    &format!("batch-{}-ask-req-{}", turn_id.as_str(), call_id.as_str()),
                    vec![intent, requested],
                )?;
                self.pending_interaction = Some(PendingInteraction::Ask {
                    call_id,
                    interaction_id: interaction_id.clone(),
                    execution_id,
                });
                self.phase = SessionPhase::Active(ActivePhase::AwaitingUser { interaction_id });
                self.observe(serde_json::json!({ "type": "ask_user" }));
                return Ok(());
            }

            let execution_id = ExecutionId::new(format!("exec_{}", generate_ulid()));
            let request = self.declaration_request(&call_id, name, &args);
            match self.tool_port.precheck(&request) {
                ToolAdmission::Authorized => {
                    self.dispatch_tool(turn_id, &call_id, name, &args, &execution_id)?;
                }
                ToolAdmission::Denied(reason) => {
                    self.deny_tool(turn_id, &call_id, &reason)?;
                }
                ToolAdmission::ApprovalRequired { reason } => {
                    self.request_permission(
                        turn_id,
                        &call_id,
                        &execution_id,
                        name,
                        &args,
                        &reason,
                    )?;
                    return Ok(());
                }
            }
        }

        if self.active_tool_identities.is_empty() && self.pending_interaction.is_none() {
            self.next_round()?;
        } else if !self.active_tool_identities.is_empty() {
            self.phase = SessionPhase::Active(ActivePhase::AwaitingTools {
                active_calls: self.active_tool_identities.keys().cloned().collect(),
            });
        }
        Ok(())
    }

    fn settle_tool(
        &mut self,
        identity: &EffectIdentity,
        outcome: &ToolOutcome,
    ) -> Result<(), SessionAgentError> {
        let Some(turn_id) = self.active_turn.clone() else {
            log::warn!(
                "[session-agent] late tool completion {} ignored",
                identity.effect_id
            );
            return Ok(());
        };
        let Some((call_id, tool)) = self
            .active_tool_identities
            .iter()
            .find(|(_, tool)| tool.identity.effect_id == identity.effect_id)
            .map(|(call_id, tool)| (call_id.clone(), tool.execution_id.clone()))
        else {
            // Cancellation and recovery settle these; a late completion must
            // not abort the whole actor.
            log::warn!(
                "[session-agent] unrecognized tool completion {} ignored",
                identity.effect_id
            );
            return Ok(());
        };

        let status = match (outcome.evidence, outcome.terminal) {
            (DispatchEvidence::StartedUnknown, None) => ToolTerminalStatus::Indeterminate,
            (_, Some(status)) => status,
            (DispatchEvidence::NotStarted, None) => ToolTerminalStatus::Failed,
            _ => ToolTerminalStatus::Failed,
        };
        let error = outcome.error.as_ref().map(|message| ToolError {
            code: match status {
                ToolTerminalStatus::Denied => "tool_denied",
                ToolTerminalStatus::Indeterminate => "tool_indeterminate",
                ToolTerminalStatus::Backgrounded => "tool_backgrounded",
                _ => "tool_error",
            }
            .into(),
            message: truncate_text(message, 512),
            retryable: matches!(
                status,
                ToolTerminalStatus::Indeterminate | ToolTerminalStatus::Failed
            ),
            details_ref: None,
        });
        let finished = self.tool_finished_fact(
            &turn_id,
            &call_id,
            Some(tool),
            status,
            outcome.output.clone(),
            error,
            outcome.backgrounded,
        )?;
        let text = if outcome.output.is_empty() {
            outcome.error.clone().unwrap_or_default()
        } else {
            String::from_utf8_lossy(&outcome.output).to_string()
        };
        let ok = !matches!(
            status,
            ToolTerminalStatus::Failed
                | ToolTerminalStatus::Denied
                | ToolTerminalStatus::Indeterminate
                | ToolTerminalStatus::Cancelled
                | ToolTerminalStatus::TimedOut
        );
        let context = self.tool_result_context_fact(&turn_id, &call_id, &text, ok)?;
        self.commit_batch(
            &format!("batch-{}-tool-{}", turn_id.as_str(), call_id.as_str()),
            vec![finished, context],
        )?;
        self.active_tool_identities.remove(&call_id);
        if self.active_tool_identities.is_empty() && self.pending_interaction.is_none() {
            self.next_round()?;
        }
        Ok(())
    }
}
