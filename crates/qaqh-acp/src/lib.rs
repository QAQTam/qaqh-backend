//! QAQHarness 的 ACP 桥实现:把 RingingV2 事实/命令面映射为 Agent Client
//! Protocol,供 Zed 等 ACP 宿主经 stdio 使用。入口是 [`run_stdio`];
//! `qaqh-acp` 与 `qaqh-daemon acp` 两个可执行形态共用同一实现。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema::{
    AgentCapabilities, CancelNotification, Content, ContentBlock, ContentChunk, Implementation,
    InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionId, PermissionOptionKind, PromptCapabilities, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, SessionId, SessionNotification,
    SessionUpdate, StopReason, TextContent, ToolCall as AcpToolCall, ToolCallContent,
    ToolCallId as AcpToolCallId, ToolCallLocation, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind,
};
use agent_client_protocol::{Agent, Error, Result, Stdio};
use qaqh_client::contract_events::{OutputStream, SessionEvent};
use qaqh_client::contract_state::{
    InteractionView, PermissionRequestDetails, ToolCallState, TurnOutcome,
};
use qaqh_client::fact::{
    AssistantBlockKind, ContentValue, InteractionDecision, InteractionKind, ToolCallOutcome,
    TurnId, TurnTerminal,
};
use qaqh_client::ringing_v2::RingingV2CommandAck;
use qaqh_client::{
    Client, ClientOptions, ClientV2SubscriptionEvent, CommandOptions, CommandTarget,
    ConversationSendMessage, ImageBlock, RingingCommand, RingingCommandAckStatus,
    RingingCommandState, SessionCreateResult,
};
use qaqh_profile::ProfilesDocument;
use tokio::sync::{Mutex, OnceCell, watch};

/// 终态内容与流式更新的尾窗上限;替换语义下避免全量重发。
const OUTPUT_TAIL_LIMIT: usize = 8 * 1024;
/// 工具输出流式的最小新增量,低于该值不推送。
const OUTPUT_STREAM_CHUNK: usize = 2 * 1024;

#[derive(Default)]
struct AcpAgent {
    client: OnceCell<Arc<Client>>,
    sessions: Mutex<HashMap<SessionId, Arc<Session>>>,
}

#[derive(Default)]
struct Session {
    prompt: Arc<Mutex<()>>,
    cancellation: watch::Sender<bool>,
    /// 本回合内已建立卡片的工具调用（含参数仍在生成中的）:终态后移除。
    tools: Mutex<HashMap<String, ToolStream>>,
}

#[derive(Default)]
struct ToolStream {
    output: String,
    streamed: usize,
    /// 最近一次已发布的参数进度标题。40ms 心跳里大多数帧行差数字没变，同值就
    /// 不再补发一帧（ACP 的 tool_call_update 是替换语义，重复发只会让客户端重画）。
    progress_title: String,
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
            .get_or_try_init(|| async {
                Client::connect_async(ClientOptions {
                    client_kind: Some(qaqh_client::RingingV2ClientKind::Acp),
                    // 桥由 Zed 拉起,daemon 可能还没跑:同二进制形态
                    // (`qaqh-daemon acp`)下按 current_exe 邻接规则自拉起。
                    launch_daemon_if_missing: true,
                    ..Default::default()
                })
                .await
                .map(Arc::new)
            })
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
                    model_selection: Some(selection),
                    tool_mode: None,
                    custom_tools: None,
                    handoff_from: None,
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
        session: Arc<Session>,
        request: PromptRequest,
        connection: &agent_client_protocol::v1::ConnectionTo<agent_client_protocol::Client>,
        mut cancellation: watch::Receiver<bool>,
    ) -> Result<PromptResponse> {
        let mut parts = Vec::new();
        let mut images = Vec::new();
        for block in request.prompt {
            match block {
                ContentBlock::Text(text) => parts.push(text.text),
                ContentBlock::Image(image) => images.push(ImageBlock {
                    mime_type: image.mime_type,
                    data: image.data,
                }),
                _ => {
                    return Err(
                        Error::invalid_params().data("This agent accepts text and image prompts only")
                    )
                }
            }
        }
        let text = parts.join("\n");
        if text.trim().is_empty() {
            return Err(Error::invalid_params().data("Prompt is empty"));
        }
        let client = self
            .client
            .get()
            .ok_or_else(|| backend_error("Missing daemon connection"))?
            .clone();
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
            &client,
            target.clone(),
            RingingCommand::ConversationSendMessage(ConversationSendMessage {
                text,
                images,
                attachments: None,
                message_id: Some(input_id.clone()),
                input_purpose: Default::default(),
                as_system: false,
                inter_agent: None,
                subagent_terminal: None,
                widget_context: None,
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
                    &client,
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
                SessionEvent::ToolDeclared {
                    call_id,
                    turn_id,
                    tool_name,
                    args,
                } if turn.as_ref() == Some(&turn_id) => {
                    let key = call_id.to_string();
                    let established_by_args_stream = session
                        .tools
                        .lock()
                        .await
                        .insert(key.clone(), ToolStream::default())
                        .is_some();
                    let title = tool_title(&tool_name, args.as_ref());
                    if established_by_args_stream {
                        // 卡片已由参数生成进度建立:补一次 update 把标题/输入/类型
                        // 定稿,绝不再发一个 ToolCall——同一个 call_id 在客户端只该有一行。
                        let mut fields = ToolCallUpdateFields::new()
                            .title(title)
                            .kind(tool_kind(&tool_name))
                            .status(ToolCallStatus::Pending);
                        if let Some(args) = args {
                            fields = fields.raw_input(args);
                        }
                        connection.send_notification(SessionNotification::new(
                            session_id.clone(),
                            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                                AcpToolCallId::new(key),
                                fields,
                            )),
                        ))?;
                    } else {
                        let mut call = AcpToolCall::new(AcpToolCallId::new(key), title)
                            .name(tool_name.clone())
                            .kind(tool_kind(&tool_name))
                            .status(ToolCallStatus::Pending);
                        if let Some(args) = args {
                            call = call.raw_input(args);
                        }
                        connection.send_notification(SessionNotification::new(
                            session_id.clone(),
                            SessionUpdate::ToolCall(call),
                        ))?;
                    }
                }
                SessionEvent::ToolArgumentsDelta {
                    call_id,
                    turn_id,
                    tool_name,
                    lines_added,
                    lines_removed,
                    ..
                } if turn.as_ref() == Some(&turn_id) => {
                    // 只有拿得出行差估算的调用（三个写工具）才值得提前出卡片;
                    // read/grep 一类没有进度可画,照旧等 ToolDeclared。
                    let (Some(lines_added), Some(lines_removed)) = (lines_added, lines_removed)
                    else {
                        continue;
                    };
                    let key = call_id.to_string();
                    let title = format!("{tool_name} +{lines_added} −{lines_removed} lines");
                    let mut tools = session.tools.lock().await;
                    let fresh = !tools.contains_key(&key);
                    let record = tools.entry(key.clone()).or_default();
                    if record.progress_title == title {
                        continue;
                    }
                    record.progress_title = title.clone();
                    drop(tools);
                    let update = if fresh {
                        SessionUpdate::ToolCall(
                            AcpToolCall::new(AcpToolCallId::new(key.clone()), title)
                                .name(tool_name.clone())
                                .kind(tool_kind(&tool_name))
                                .status(ToolCallStatus::Pending),
                        )
                    } else {
                        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                            AcpToolCallId::new(key),
                            ToolCallUpdateFields::new().title(title),
                        ))
                    };
                    connection.send_notification(SessionNotification::new(
                        session_id.clone(),
                        update,
                    ))?;
                }
                SessionEvent::ToolStateChanged {
                    call_id,
                    turn_id,
                    state,
                } if turn.as_ref() == Some(&turn_id) => {
                    if matches!(state, ToolCallState::Running) {
                        connection.send_notification(SessionNotification::new(
                            session_id.clone(),
                            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                                AcpToolCallId::new(call_id.to_string()),
                                ToolCallUpdateFields::new()
                                    .status(ToolCallStatus::InProgress),
                            )),
                        ))?;
                    }
                    // AwaitingInteraction 由权限卡片呈现;Cancelling 无 ACP 对应;
                    // 终态由 ToolFinished 表达。
                }
                SessionEvent::ToolOutputDelta {
                    call_id,
                    stream,
                    text,
                } => {
                    let mut tools = session.tools.lock().await;
                    let Some(record) = tools.get_mut(&call_id.to_string()) else {
                        continue;
                    };
                    if matches!(stream, OutputStream::Stderr) && !text.is_empty() {
                        record.output.push_str("[stderr] ");
                    }
                    record.output.push_str(&text);
                    if record.output.len() - record.streamed < OUTPUT_STREAM_CHUNK {
                        continue;
                    }
                    let tail = output_tail(&record.output, OUTPUT_TAIL_LIMIT).to_owned();
                    record.streamed = record.output.len();
                    drop(tools);
                    connection.send_notification(SessionNotification::new(
                        session_id.clone(),
                        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                            AcpToolCallId::new(call_id.to_string()),
                            ToolCallUpdateFields::new().content(vec![ToolCallContent::Content(
                                Content::new(ContentBlock::Text(TextContent::new(tail))),
                            )]),
                        )),
                    ))?;
                }
                SessionEvent::ToolFinished {
                    call_id,
                    outcome,
                    output,
                    output_truncated,
                    code_delta,
                    ..
                } => {
                    let declared = session
                        .tools
                        .lock()
                        .await
                        .remove(&call_id.to_string())
                        .is_some();
                    if !declared {
                        continue;
                    }
                    let mut text = resolve_output(&client, output).await;
                    if output_truncated {
                        text.push_str("\n(output truncated by QAQHarness)");
                    }
                    let (status, failure) = match &outcome {
                        ToolCallOutcome::Completed | ToolCallOutcome::Yielded { .. } => {
                            (ToolCallStatus::Completed, None)
                        }
                        ToolCallOutcome::Failed { error } => (
                            ToolCallStatus::Failed,
                            Some(format!("failed: {}", error.message)),
                        ),
                        ToolCallOutcome::Cancelled { reason } => (
                            ToolCallStatus::Failed,
                            Some(format!("cancelled: {reason:?}")),
                        ),
                        ToolCallOutcome::Interrupted { reason } => (
                            ToolCallStatus::Failed,
                            Some(format!("interrupted: {reason:?}")),
                        ),
                    };
                    if let Some(failure) = failure {
                        if text.is_empty() {
                            text = failure;
                        } else {
                            text = format!("{failure}\n{text}");
                        }
                    }
                    let mut fields = ToolCallUpdateFields::new().status(status);
                    if let Some(delta) = &code_delta {
                        let mut title = format!(
                            "+{} −{} lines",
                            delta.lines_added, delta.lines_removed
                        );
                        if delta.files_created > 0 {
                            title.push_str(&format!(", {} created", delta.files_created));
                        }
                        if delta.files_deleted > 0 {
                            title.push_str(&format!(", {} deleted", delta.files_deleted));
                        }
                        if let Some(file) = &delta.file {
                            title.push_str(&format!(" ({file})"));
                            fields = fields
                                .locations(vec![ToolCallLocation::new(PathBuf::from(file))]);
                        }
                        fields = fields.title(title);
                    }
                    if !text.is_empty() {
                        fields = fields.content(vec![ToolCallContent::Content(Content::new(
                            ContentBlock::Text(TextContent::new(output_tail(
                                &text,
                                OUTPUT_TAIL_LIMIT,
                            )
                            .to_owned())),
                        ))]);
                    }
                    connection.send_notification(SessionNotification::new(
                        session_id.clone(),
                        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                            AcpToolCallId::new(call_id.to_string()),
                            fields,
                        )),
                    ))?;
                }
                SessionEvent::InteractionRequested { interaction }
                    if turn.is_some() && interaction.turn_id == turn =>
                {
                    match interaction.kind {
                        InteractionKind::Permission => {
                            let task = PermissionPrompt {
                                session_id: session_id.clone(),
                                target: target.clone(),
                                client: client.clone(),
                                connection: connection.clone(),
                            };
                            tokio::spawn(task.request(interaction));
                        }
                        unsupported => unsupported_interaction = Some(unsupported),
                    }
                }
                SessionEvent::TurnEnded { turn_id, outcome } if turn.as_ref() == Some(&turn_id) => {
                    if let Some(kind) = unsupported_interaction {
                        return Err(backend_error(format!(
                            "QAQHarness cannot surface {kind:?} interactions over ACP yet"
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

/// 把 `InteractionKind::Permission` 呈现为 Zed 的审批卡片,并把用户决定
/// 回写为 `tool.permission.respond`。
struct PermissionPrompt {
    session_id: SessionId,
    target: CommandTarget,
    client: Arc<Client>,
    connection: agent_client_protocol::v1::ConnectionTo<agent_client_protocol::Client>,
}

impl PermissionPrompt {
    async fn request(self, interaction: InteractionView) {
        if let Err(error) = self.request_permission(interaction).await {
            // 审批失败不终结回合:工具保持等待,由交互超时/回合取消兜底。
            eprintln!("[qaqh-acp] permission request failed: {error}");
        }
    }

    async fn request_permission(self, interaction: InteractionView) -> Result<()> {
        let call_id = interaction
            .call_id
            .ok_or_else(|| backend_error("Permission interaction without a tool call"))?;
        let details: PermissionRequestDetails = match &interaction.request {
            ContentValue::Inline { text } => serde_json::from_str(text).map_err(backend_error)?,
            _ => return Err(backend_error("Permission request payload is not inline JSON")),
        };
        let mut title = details.tool_name.clone();
        if let Some(summary) = &details.action_summary {
            title.push_str(" — ");
            title.push_str(summary);
        }
        let mut fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Pending)
            .title(title)
            .content(vec![ToolCallContent::Content(Content::new(ContentBlock::Text(
                TextContent::new(permission_text(&details)),
            )))]);
        if !details.paths.is_empty() {
            fields = fields.locations(
                details
                    .paths
                    .iter()
                    .map(|path| ToolCallLocation::new(PathBuf::from(path)))
                    .collect::<Vec<_>>(),
            );
        }
        let options = interaction
            .choices
            .iter()
            .filter_map(|choice| {
                let kind = match choice.decision {
                    InteractionDecision::Approved => PermissionOptionKind::AllowOnce,
                    InteractionDecision::Rejected => PermissionOptionKind::RejectOnce,
                    // Answered/Dismissed 不属于权限交互,出现即不该展示。
                    _ => return None,
                };
                Some(PermissionOption::new(
                    PermissionOptionId::new(decision_id(choice.decision)),
                    choice.label.clone(),
                    kind,
                ))
            })
            .collect::<Vec<_>>();
        let response = self
            .connection
            .send_request(RequestPermissionRequest::new(
                self.session_id.clone(),
                ToolCallUpdate::new(AcpToolCallId::new(call_id.to_string()), fields),
                options,
            ))
            // block_task:SentRequest 不能在 dispatch 循环内 await,
            // 本方法只在独立 spawn 的任务里运行。
            .block_task()
            .await
            .map_err(backend_error)?;
        // 回合取消时 Zed 必答 cancelled;取消已由 ConversationCancel 传达,无需回写。
        if let RequestPermissionOutcome::Selected(selected) = response.outcome {
            let approved = selected.option_id.0.as_ref() == "approved";
            send_command(
                &self.client,
                self.target.clone(),
                RingingCommand::ToolPermissionRespond {
                    tool_call_id: call_id.to_string(),
                    approved,
                    trust_folder: false,
                },
            )
            .await?;
        }
        Ok(())
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

/// 工具终态输出:Inline 直取;Ref 经 daemon 回读并校验;二进制只报大小。
async fn resolve_output(client: &Client, output: Option<ContentValue>) -> String {
    match output {
        Some(ContentValue::Inline { text }) => text,
        Some(ContentValue::Ref { content_ref }) => match client.download_content(&content_ref).await
        {
            Ok(bytes) => String::from_utf8(bytes).unwrap_or_else(|error| {
                format!("[binary output: {} bytes]", error.into_bytes().len())
            }),
            Err(_) => "[output unavailable]".to_owned(),
        },
        Some(ContentValue::Unavailable(_)) | None => String::new(),
    }
}

fn output_tail(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text.get(start..).unwrap_or(text)
}

fn tool_kind(tool_name: &str) -> ToolKind {
    let name = tool_name.to_ascii_lowercase();
    if name.contains("search") || name.contains("grep") || name.contains("glob") || name.contains("find")
    {
        ToolKind::Search
    } else if name.contains("read") || name.contains("list") || name.contains("open") {
        ToolKind::Read
    } else if name.contains("write")
        || name.contains("edit")
        || name.contains("patch")
        || name.contains("apply")
        || name.contains("create")
    {
        ToolKind::Edit
    } else if name.contains("delete") || name.contains("remove") {
        ToolKind::Delete
    } else if name.contains("move") || name.contains("rename") {
        ToolKind::Move
    } else if name.contains("fetch") || name.contains("http") || name.contains("download") {
        ToolKind::Fetch
    } else if name.contains("think") || name.contains("plan") || name.contains("todo") {
        ToolKind::Think
    } else if name.contains("exec")
        || name.contains("bash")
        || name.contains("shell")
        || name.contains("cmd")
        || name.contains("pty")
        || name.contains("run")
    {
        ToolKind::Execute
    } else {
        ToolKind::Other
    }
}

const TITLE_KEYS: [&str; 7] = ["path", "command", "file", "url", "query", "pattern", "skill"];

fn tool_title(tool_name: &str, args: Option<&serde_json::Value>) -> String {
    let Some(serde_json::Value::Object(fields)) = args else {
        return tool_name.to_owned();
    };
    for key in TITLE_KEYS {
        if let Some(serde_json::Value::String(value)) = fields.get(key) {
            let value: String = value.chars().take(96).collect();
            return format!("{tool_name}: {value}");
        }
    }
    tool_name.to_owned()
}

fn decision_id(decision: InteractionDecision) -> &'static str {
    match decision {
        InteractionDecision::Approved => "approved",
        InteractionDecision::Rejected => "rejected",
        InteractionDecision::Answered => "answered",
        InteractionDecision::Dismissed => "dismissed",
    }
}

fn permission_text(details: &PermissionRequestDetails) -> String {
    let mut text = format!(
        "Reason: {}\nConsequence: {}\nRisk: {} (level {})",
        details.reason, details.consequence, details.risk, details.level
    );
    if !details.paths.is_empty() {
        text.push_str("\nPaths:");
        for path in &details.paths {
            text.push_str(&format!("\n- {path}"));
        }
    }
    text
}

fn backend_error(error: impl std::fmt::Display) -> Error {
    Error::internal_error().data(error.to_string())
}

/// Zed 等 ACP 宿主以 stdio 子进程方式拉起本桥:stdout 是 JSON-RPC 协议通道,
/// 任何诊断只允许走 stderr(或 daemon 的文件日志)。
pub async fn run_stdio() -> Result<()> {
    let agent = Arc::new(AcpAgent::default());
    let sessions = agent.clone();
    let prompts = agent.clone();
    Agent
        .v1()
        .name("qaqh-acp")
        .on_receive_request(
            async |_: InitializeRequest, responder, _connection| {
                // 声明图片能力:prompt() 已实现 ContentBlock::Image → images,
                // 不声明 Zed 会在客户端侧拒发图片。
                let agent_capabilities = AgentCapabilities::default()
                    .prompt_capabilities(PromptCapabilities::default().image(true));
                responder.respond(
                    InitializeResponse::new(ProtocolVersion::V1)
                        .agent_info(
                            Implementation::new("qaqh-acp", env!("CARGO_PKG_VERSION"))
                                .title("QAQHarness"),
                        )
                        .agent_capabilities(agent_capabilities),
                )
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
                            .prompt(session, request, &prompt_connection, cancellation)
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
