use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema::{
    CancelNotification, ContentBlock, ContentChunk, Implementation, InitializeRequest,
    InitializeResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    SessionId, SessionNotification, SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::{Agent, Error, Result, Stdio};
use qaqh_client::contract_events::SessionEvent;
use qaqh_client::contract_state::TurnOutcome;
use qaqh_client::fact::{AssistantBlockKind, TurnId, TurnTerminal};
use qaqh_client::ringing_v2::RingingV2CommandAck;
use qaqh_client::{
    Client, ClientOptions, ClientV2SubscriptionEvent, CommandOptions, CommandTarget,
    ConversationSendMessage, RingingCommand, RingingCommandAckStatus, RingingCommandState,
    SessionCreateResult,
};
use qaqh_profile::ProfilesDocument;
use tokio::sync::{Mutex, OnceCell, watch};

#[derive(Default)]
struct AcpAgent {
    client: OnceCell<Client>,
    sessions: Mutex<HashMap<SessionId, Arc<Session>>>,
}

#[derive(Default)]
struct Session {
    prompt: Arc<Mutex<()>>,
    cancellation: watch::Sender<bool>,
}

impl AcpAgent {
    async fn create_session(&self, request: NewSessionRequest) -> Result<NewSessionResponse> {
        let cwd = request
            .cwd
            .into_os_string()
            .into_string()
            .map_err(|_| Error::invalid_params().data("Workspace path must be UTF-8"))?;
        let client = self
            .client
            .get_or_try_init(|| Client::connect_async(ClientOptions::default()))
            .await
            .map_err(backend_error)?;
        let catalog: ProfilesDocument = command_result(
            send_command(
                client,
                CommandTarget::Global,
                RingingCommand::ProviderList {},
            )
            .await?,
        )?;
        let selection = catalog.default_model_selection.ok_or_else(|| {
            Error::invalid_params()
                .data("Set a default model in QAQHarness before opening a Zed thread")
        })?;
        let created: SessionCreateResult = command_result(
            send_command(
                client,
                CommandTarget::Global,
                RingingCommand::SessionCreate {
                    cwd: Some(cwd),
                    title: None,
                    profile: None,
                    effort: None,
                    model_selection: Some(selection),
                    tool_mode: None,
                    custom_tools: None,
                },
            )
            .await?,
        )?;
        let session_id = SessionId::new(created.session_id);
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), Arc::new(Session::default()));
        Ok(NewSessionResponse::new(session_id))
    }

    async fn session(&self, id: &SessionId) -> Result<Arc<Session>> {
        self.sessions
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| Error::invalid_params().data("Unknown QAQHarness session"))
    }

    async fn prompt(
        &self,
        request: PromptRequest,
        connection: &agent_client_protocol::v1::ConnectionTo<agent_client_protocol::Client>,
        mut cancellation: watch::Receiver<bool>,
    ) -> Result<PromptResponse> {
        let mut parts = Vec::new();
        for block in request.prompt {
            match block {
                ContentBlock::Text(text) => parts.push(text.text),
                _ => return Err(Error::invalid_params().data("This prototype accepts text only")),
            }
        }
        let text = parts.join("\n");
        if text.trim().is_empty() {
            return Err(Error::invalid_params().data("Prompt is empty"));
        }
        let client = self
            .client
            .get()
            .ok_or_else(|| backend_error("Missing daemon connection"))?;
        let session_id = request.session_id;
        let target = CommandTarget::Session {
            session_id: session_id.to_string(),
        };
        // The daemon subscribes before sending SSE headers. Open the stream before
        // sending the command so a fast turn cannot finish outside our window.
        let mut events = client
            .subscribe_v2(&session_id.to_string(), None)
            .await
            .map_err(backend_error)?;
        if *cancellation.borrow() {
            return Ok(PromptResponse::new(StopReason::Cancelled));
        }
        let input_id = uuid::Uuid::new_v4().to_string();
        let ack = send_command(
            client,
            target.clone(),
            RingingCommand::ConversationSendMessage(ConversationSendMessage {
                text,
                images: Vec::new(),
                attachments: None,
                message_id: Some(input_id.clone()),
                input_purpose: Default::default(),
                as_system: false,
                inter_agent: None,
                subagent_terminal: None,
            }),
        )
        .await?;

        let mut turn: Option<TurnId> = None;
        let mut cancel_sent = false;
        let mut unsupported_interaction = None;
        let mut blocks = HashMap::<_, String>::new();
        let mut receipt_poll = tokio::time::interval(Duration::from_secs(1));
        loop {
            if let Some(turn_id) = &turn
                && !cancel_sent
                && (*cancellation.borrow() || unsupported_interaction.is_some())
            {
                send_command(
                    client,
                    target.clone(),
                    RingingCommand::ConversationCancel {
                        turn_id: Some(turn_id.to_string()),
                    },
                )
                .await?;
                cancel_sent = true;
            }
            let event = tokio::select! {
                event = events.next() => event.map_err(backend_error)?,
                changed = cancellation.changed() => {
                    changed.map_err(backend_error)?;
                    continue;
                }
                _ = receipt_poll.tick(), if turn.is_none() => {
                    let status = client.command_status(&ack.command_id).await.map_err(backend_error)?;
                    if matches!(status.state, RingingCommandState::Failed | RingingCommandState::Rejected) {
                        return Err(backend_error(status.error_code.unwrap_or(status.command_id)));
                    }
                    continue;
                }
            };
            let frame = match event {
                Some(ClientV2SubscriptionEvent::Event(frame)) => frame,
                Some(ClientV2SubscriptionEvent::KeepAlive) => continue,
                Some(ClientV2SubscriptionEvent::Reset(reset)) => {
                    return Err(backend_error(format!(
                        "Session stream reset: {:?}",
                        reset.reason
                    )));
                }
                None => return Err(backend_error("Session stream closed")),
            };
            match frame.event {
                SessionEvent::TurnStarted {
                    turn_id,
                    input_id: Some(id),
                    ..
                } if id.as_str() == input_id => {
                    turn = Some(turn_id);
                }
                SessionEvent::MessageDelta {
                    turn_id,
                    block_id,
                    block_kind,
                    text,
                } if turn.as_ref() == Some(&turn_id) => {
                    let previous = blocks.entry(block_id).or_default();
                    let chunk = text.strip_prefix(previous.as_str()).ok_or_else(|| {
                        backend_error("Session text changed after being streamed")
                    })?;
                    if !chunk.is_empty() {
                        let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(chunk)));
                        let update = match block_kind {
                            AssistantBlockKind::Answer => SessionUpdate::AgentMessageChunk(chunk),
                            AssistantBlockKind::Reasoning => {
                                SessionUpdate::AgentThoughtChunk(chunk)
                            }
                            AssistantBlockKind::ToolCall => continue,
                        };
                        connection.send_notification(SessionNotification::new(
                            session_id.clone(),
                            update,
                        ))?;
                    }
                    *previous = text;
                }
                SessionEvent::InteractionRequested { interaction }
                    if turn.is_some() && interaction.turn_id == turn =>
                {
                    unsupported_interaction = Some(interaction.kind);
                }
                SessionEvent::TurnEnded { turn_id, outcome } if turn.as_ref() == Some(&turn_id) => {
                    if let Some(kind) = unsupported_interaction {
                        return Err(backend_error(format!(
                            "This prototype cannot handle {kind:?} interactions"
                        )));
                    }
                    let reason = match outcome {
                        TurnOutcome::Finished {
                            terminal: TurnTerminal::Completed,
                            ..
                        } => StopReason::EndTurn,
                        TurnOutcome::Finished {
                            terminal: TurnTerminal::Cancelled,
                            ..
                        } => StopReason::Cancelled,
                        TurnOutcome::Finished { error, .. } => {
                            return Err(backend_error(error.map_or_else(
                                || "QAQHarness turn failed".to_owned(),
                                |error| error.message,
                            )));
                        }
                        TurnOutcome::Interrupted {
                            reason: qaqh_client::fact::InterruptReason::CancelBeforeSeal,
                        } => StopReason::Cancelled,
                        TurnOutcome::Interrupted { reason } => {
                            return Err(backend_error(format!(
                                "QAQHarness turn interrupted: {reason:?}"
                            )));
                        }
                    };
                    return Ok(PromptResponse::new(reason));
                }
                _ => {}
            }
        }
    }
}

async fn send_command(
    client: &Client,
    target: CommandTarget,
    command: RingingCommand,
) -> Result<RingingV2CommandAck> {
    let ack = client
        .send_command(target, command, CommandOptions::default())
        .await
        .map_err(backend_error)?;
    match ack.status {
        RingingCommandAckStatus::Accepted => Ok(ack),
        RingingCommandAckStatus::Rejected => Err(backend_error(
            ack.message.or(ack.code).unwrap_or(ack.command_id),
        )),
    }
}

fn command_result<T: serde::de::DeserializeOwned>(ack: RingingV2CommandAck) -> Result<T> {
    let value = ack
        .result
        .ok_or_else(|| backend_error(format!("Missing result for command {}", ack.command_id)))?;
    serde_json::from_value(value).map_err(backend_error)
}

fn backend_error(error: impl std::fmt::Display) -> Error {
    Error::internal_error().data(error.to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    let agent = Arc::new(AcpAgent::default());
    let sessions = agent.clone();
    let prompts = agent.clone();
    Agent
        .v1()
        .name("qaqh-acp")
        .on_receive_request(
            async |_: InitializeRequest, responder, _connection| {
                responder.respond(InitializeResponse::new(ProtocolVersion::V1).agent_info(
                    Implementation::new("qaqh-acp", env!("CARGO_PKG_VERSION")).title("QAQHarness"),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest, responder, _connection| {
                responder.respond_with_result(sessions.create_session(request).await)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, connection| {
                let session = match prompts.session(&request.session_id).await {
                    Ok(session) => session,
                    Err(error) => return responder.respond_with_error(error),
                };
                let Ok(prompt_guard) = session.prompt.clone().try_lock_owned() else {
                    return responder.respond_with_error(
                        Error::invalid_params().data("A prompt is already running"),
                    );
                };
                session.cancellation.send_replace(false);
                let cancellation = session.cancellation.subscribe();
                let agent = prompts.clone();
                let prompt_connection = connection.clone();
                // Keep the dispatcher free to receive session/cancel during the turn.
                connection.spawn(async move {
                    let _prompt_guard = prompt_guard;
                    responder.respond_with_result(
                        agent
                            .prompt(request, &prompt_connection, cancellation)
                            .await,
                    )
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: CancelNotification, _connection| {
                if let Ok(session) = agent.session(&notification.session_id).await {
                    session.cancellation.send_replace(true);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await
}
