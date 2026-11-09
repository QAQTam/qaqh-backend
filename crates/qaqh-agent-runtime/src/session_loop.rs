//! 会话回合循环:owner 驱动的 输入 → provider → 流式增量 → 工具 → 终态。
//!
//! 旧 SessionAgent/agentloop 证明体系的全部运行职责收拢到这里(报告一 §12.5-3):
//! - 状态与事件只经 [`SessionOwner`](qaqh_session::owner::SessionOwner):
//!   SQLite 业务表 + 单流 `seq` 广播;
//! - 上下文从 SQLite blocks/turns 组装;系统提示每回合按当前配置重建;
//! - 工具直调 [`WorkspaceToolPort`],生命周期/实时输出/effects 走 owner 通道;
//! - 取消经 per-turn watch + AtomicBool;权限与 ask 经 owner 交互;
//! - Steer/Interject 在回合内下一轮合并(Transcript 立即落块)。

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use qaqh_ringing::ConversationSendMessage;
use qaqh_ringing_contract::events::InteractionEndStatus;
use qaqh_ringing_contract::fact::AssistantBlockKind;
use qaqh_ringing_contract::fact::{
    ActorKind, ActorRef, BlockId, ContentValue, InputPurpose, InteractionChoice,
    InteractionDecision, InteractionId, InteractionKind, InterruptReason, ToolCallId,
    ToolCallOutcome, TurnId,
};
use qaqh_ringing_contract::state::{
    InteractionView, PermissionRequestDetails, TimelineEntry, ToolCallState, TurnOutcome,
};
use qaqh_ringing_contract::ulid::generate_ulid;
use qaqh_session::owner::{SessionOwner, ToolSettlement};
use qaqh_types::{ContentBlock, Message, ToolResult as ToolResultPayload, UsageInfo};
use qaqh_workspace::ask_user::normalize_ask_user;
use tokio::sync::{oneshot, watch};

use crate::progress::{AttemptBlocks, ProgressSink, ProgressStream};
use crate::provider_job::{self, Evidence, ProviderOutcome, ProviderRequestSpec};
use crate::session_agent::prompt;
use crate::tool_job::{
    ToolAdmission, ToolApprovalGrant, ToolFailure, ToolRequest, WorkspaceToolPort,
};
use qaqh_ringing_contract::fact::ProviderFinish;

/// 权限交互的应答。
#[derive(Debug, Clone, Copy)]
pub struct PermissionAnswer {
    pub approved: bool,
    pub trust_folder: bool,
}

enum AskReply {
    Answers(Vec<(String, String)>),
    Dismissed,
}

/// 回合循环:一个会话一个实例;回合互斥、输入排队、交互登记都在这里。
pub struct SessionLoop {
    session_id: String,
    owner: Arc<SessionOwner>,
    port: Arc<WorkspaceToolPort>,
    lifecycle: qaqh_session::owner::ToolLifecycle,
    spec: RwLock<ProviderRequestSpec>,
    cwd: RwLock<PathBuf>,
    inner: Mutex<LoopInner>,
    /// 最近一次回合活动的毫秒时钟(idle 卸载的判据)。
    last_activity_ms: AtomicU64,
}

struct LoopInner {
    closed: bool,
    queue: VecDeque<ConversationSendMessage>,
    active: Option<ActiveTurn>,
    permissions: HashMap<String, oneshot::Sender<PermissionAnswer>>,
    asks: HashMap<String, oneshot::Sender<AskReply>>,
}

/// 回合任务统一 spawn 口:daemon 在 tokio 上下文里直接 spawn;CLI/测试等
/// 同步调用方回落进程级共享运行时(旧 host.rs 的 driver_runtime 同款兜底)。
pub(crate) fn spawn_task<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.spawn(future),
        Err(_) => shared_runtime().spawn(future),
    }
}

fn shared_runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("qaqh-loop-runtime")
            .build()
            .expect("build shared loop runtime")
    })
}

fn unix_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

struct ActiveTurn {
    turn_id: String,
    cancel: watch::Sender<bool>,
    /// 运行中合并(Steer/Interject):下一轮 provider 调用前注入。
    steers: Vec<String>,
    /// 本回合升起、未决的交互(中断时统一取消)。
    pending_interactions: Vec<InteractionId>,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Debug, thiserror::Error)]
pub enum LoopError {
    #[error("session loop is closed")]
    Closed,
    #[error(transparent)]
    Db(#[from] qaqh_session::db::DbError),
}

impl SessionLoop {
    pub fn new(
        session_id: impl Into<String>,
        owner: Arc<SessionOwner>,
        port: Arc<WorkspaceToolPort>,
        spec: ProviderRequestSpec,
        cwd: PathBuf,
    ) -> Arc<Self> {
        let lifecycle = owner.lifecycle();
        Arc::new(Self {
            session_id: session_id.into(),
            owner,
            port,
            lifecycle,
            spec: RwLock::new(spec),
            cwd: RwLock::new(cwd),
            inner: Mutex::new(LoopInner {
                closed: false,
                queue: VecDeque::new(),
                active: None,
                permissions: HashMap::new(),
                asks: HashMap::new(),
            }),
            last_activity_ms: AtomicU64::new(unix_now_ms().max(0) as u64),
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// 当前活动回合(无则 None)。
    pub fn active_turn(&self) -> Option<String> {
        self.lock().active.as_ref().map(|turn| turn.turn_id.clone())
    }

    /// 循环侧活动状态(契约三态):活动回合 = Running,空闲 = Idle。
    /// 等待用户由 [`Self::waiting_user`] 单独表达。
    pub fn activity_state(&self) -> qaqh_ringing_contract::fact::ActivityState {
        use qaqh_ringing_contract::fact::ActivityState;
        if self.lock().active.is_some() {
            ActivityState::Running
        } else {
            ActivityState::Idle
        }
    }

    /// 回合内挂起交互(权限/ask 应答槽非空)。
    pub fn waiting_user(&self) -> bool {
        let inner = self.lock();
        !inner.permissions.is_empty() || !inner.asks.is_empty()
    }

    /// 距最近一次回合活动(开回合/收尾)的秒数,idle 卸载判据。
    pub fn idle_secs(&self) -> u64 {
        let last = self.last_activity_ms.load(Ordering::Acquire) as i64;
        let delta = unix_now_ms() - last;
        u64::try_from(delta).unwrap_or(0) / 1000
    }

    /// 待处理输入数:排队输入 + 活动回合未合并的 steer。
    /// wait_agent 的输入到达检测只看这个,不依赖任何持久化游标。
    pub fn pending_inputs(&self) -> usize {
        let inner = self.lock();
        inner.queue.len()
            + inner
                .active
                .as_ref()
                .map(|active| active.steers.len())
                .unwrap_or(0)
    }

    /// 关停(abandon 语义):取消活动回合、丢弃排队输入、释放交互应答槽。
    /// 不等待回合任务退出——owner 泵线程持有全部落库职责,任务收尾与
    /// 实例生命周期解耦;回合任务感知取消后自行把挂起交互按 Cancelled 结算。
    pub fn abandon(&self) {
        let mut inner = self.lock();
        inner.closed = true;
        if let Some(active) = inner.active.as_ref() {
            let _ = active.cancel.send(true);
        }
        inner.queue.clear();
        inner.permissions.clear();
        inner.asks.clear();
        self.last_activity_ms
            .store(unix_now_ms().max(0) as u64, Ordering::Release);
    }

    /// 输入入口:空闲开回合;活动中 Transcript 立即落块,Steer/Interject
    /// 合并进下一轮,其余入队(下一回合)。
    pub fn send_input(self: &Arc<Self>, input: ConversationSendMessage) -> Result<(), LoopError> {
        let merges_into_round = matches!(
            input.input_purpose,
            InputPurpose::Steer | InputPurpose::Interject
        );
        let mut inner = self.lock();
        if inner.closed {
            return Err(LoopError::Closed);
        }
        if let Some(active) = inner.active.as_mut() {
            if merges_into_round {
                let turn = self.owner_input_block(&active.turn_id, &input);
                self.owner.queue_block(turn)?;
                active.steers.push(input.text);
                return Ok(());
            }
            inner.queue.push_back(input);
            return Ok(());
        }
        // QueueOnly 语义(契约):投递但不触发回合——入队,等下一个回合
        // 开始时作为输入块落盘。
        if matches!(input.input_purpose, InputPurpose::QueueOnly) {
            inner.queue.push_back(input);
            return Ok(());
        }
        drop(inner);
        self.spawn_turn(input)
    }

    /// 取消活动回合(给出的 turn_id 不匹配时忽略,返回是否生效)。
    pub fn cancel_turn(&self, turn_id: Option<&str>) -> bool {
        {
            let inner = self.lock();
            let Some(active) = &inner.active else {
                return false;
            };
            if let Some(want) = turn_id
                && want != active.turn_id
            {
                return false;
            }
            self.owner.queue_turn_phase(
                &active.turn_id,
                qaqh_ringing_contract::TurnPhase::Cancelling,
            );
            let _ = active.cancel.send(true);
        }
        true
    }

    /// 工具权限应答(call_id 不匹配 = 忽略)。
    pub fn respond_permission(&self, call_id: &str, answer: PermissionAnswer) -> bool {
        let sender = self.lock().permissions.remove(call_id);
        sender
            .map(|sender| sender.send(answer).is_ok())
            .unwrap_or(false)
    }

    /// ask 交互应答(interaction_id 不匹配 = 忽略)。
    pub fn respond_interaction(
        &self,
        interaction_id: &str,
        answers: Vec<(String, String)>,
    ) -> bool {
        let sender = self.lock().asks.remove(interaction_id);
        sender
            .map(|sender| sender.send(AskReply::Answers(answers)).is_ok())
            .unwrap_or(false)
    }

    pub fn dismiss_ask(&self, interaction_id: &str) {
        if let Some(sender) = self.lock().asks.remove(interaction_id) {
            let _ = sender.send(AskReply::Dismissed);
        }
    }

    /// 配置重载后同步 provider、工具目录与执行策略(下一轮生效)。
    pub fn reload_runtime(
        &self,
        mut spec: ProviderRequestSpec,
        policy: crate::tool_job::ToolPolicySnapshot,
        allowed_tools: Vec<String>,
        global_allowed_tools: Option<Vec<String>>,
    ) {
        let tools = self
            .port
            .update_tool_allowlists(allowed_tools, global_allowed_tools);
        spec.tools = (!tools.is_empty()).then_some(tools);
        *self
            .cwd
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = policy.workspace_root.clone();
        self.port.update_policy(policy);
        *self
            .spec
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = spec;
    }

    /// 关停:取消活动回合并等待任务退出。
    pub async fn shutdown(self: Arc<Self>) {
        self.abandon();
        let task = {
            let mut inner = self.lock();
            let Some(active) = inner.active.take() else {
                return;
            };
            let _ = active.cancel.send(true);
            active.task
        };
        let _ = task.await;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LoopInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn spawn_turn(self: &Arc<Self>, input: ConversationSendMessage) -> Result<(), LoopError> {
        let mut inner = self.lock();

        if inner.closed {
            return Err(LoopError::Closed);
        }
        if inner.active.is_some() {
            inner.queue.push_back(input);
            return Ok(());
        }
        let loop_ = Arc::clone(self);
        let task = spawn_task(async move {
            loop_.run_turn(input).await;
        });
        self.last_activity_ms
            .store(unix_now_ms().max(0) as u64, Ordering::Release);
        let turn_id = format!("turn_{}", generate_ulid());
        let (cancel, _) = watch::channel(false);
        inner.active = Some(ActiveTurn {
            turn_id,
            cancel,
            steers: Vec::new(),
            pending_interactions: Vec::new(),
            task,
        });
        Ok(())
    }

    fn owner_input_block(&self, turn_id: &str, input: &ConversationSendMessage) -> TimelineEntry {
        TimelineEntry::Input {
            block_id: BlockId::new(format!("block_{}", generate_ulid())),
            turn_id: Some(TurnId::new(turn_id.to_string())),
            purpose: input.input_purpose,
            content: ContentValue::Inline {
                text: input.text.clone(),
            },
            attachments: input
                .attachments
                .as_ref()
                .into_iter()
                .flatten()
                .cloned()
                .collect(),
            images: input.images.clone(),
            inter_agent: input.inter_agent.clone(),
            subagent_terminal: input.subagent_terminal.clone(),
            actor: ActorRef {
                kind: if input.as_system {
                    ActorKind::System
                } else {
                    ActorKind::User
                },
                id: if input.as_system { "system" } else { "user" }.into(),
                display_name: None,
            },
        }
    }

    async fn run_turn(self: Arc<Self>, input: ConversationSendMessage) {
        let (turn_id, cancel_rx) = {
            let inner = self.lock();
            let Some(active) = inner.active.as_ref() else {
                return;
            };
            (active.turn_id.clone(), active.cancel.subscribe())
        };
        let turn = TurnId::new(turn_id.clone());
        let input_id = input
            .message_id
            .clone()
            .unwrap_or_else(|| format!("input_{}", generate_ulid()));
        if let Err(error) = self.owner.start_turn(&turn_id, Some(input_id)).await {
            log::error!("[loop] {} start turn failed: {error}", self.session_id);
            self.finish_active(&turn_id).await;
            return;
        }
        if let Err(error) = self
            .owner
            .append_block(self.owner_input_block(&turn_id, &input))
            .await
        {
            log::error!("[loop] {} append input failed: {error}", self.session_id);
            self.interrupt(&turn, InterruptReason::Crash).await;
            self.finish_active(&turn_id).await;
            return;
        }

        // 上下文:SQLite 组装 + 每回合重建的系统提示前缀(不落盘:配置即真相)。
        let mut messages = match self.assemble_context().await {
            Ok(messages) => messages,
            Err(error) => {
                log::error!(
                    "[loop] {} context assembly failed: {error}",
                    self.session_id
                );
                self.finish_failed(&turn, None, "context_assembly_failed", &error.to_string())
                    .await;
                self.finish_active(&turn_id).await;
                return;
            }
        };
        let cwd = self
            .cwd
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let system = prompt::system_prompt(&self.current_spec(), &cwd);
        messages.insert(
            0,
            Message {
                msg_id: None,
                role: "system".into(),
                name: None,
                content: vec![ContentBlock::Text { text: system }],
            },
        );

        'rounds: loop {
            // Steer/Interject 合并:下一轮请求前注入 user 消息。
            let steers = {
                let mut inner = self.lock();
                match inner.active.as_mut() {
                    Some(active) => std::mem::take(&mut active.steers),
                    None => Vec::new(),
                }
            };
            for text in steers {
                messages.push(Message {
                    msg_id: None,
                    role: "user".into(),
                    name: None,
                    content: vec![ContentBlock::Text { text }],
                });
            }

            let spec = self.current_spec();
            let blocks = AttemptBlocks::new(&turn_id, &spec.provider.model);
            let progress = ProgressStream {
                sink: Arc::new(OwnerSink {
                    owner: Arc::clone(&self.owner),
                }),
                blocks: blocks.clone(),
            };
            let outcome = provider_job::run_round(
                spec,
                messages.clone(),
                self.session_id.clone(),
                cancel_rx.clone(),
                Some(progress),
            )
            .await;
            match outcome.evidence {
                // 未出发:确认取消或 provider 准备失败,无任何可见输出。
                Evidence::NotStarted => {
                    if outcome.cancelled {
                        self.finish_cancelled(&turn, outcome.usage.clone()).await;
                    } else {
                        self.finish_failed(
                            &turn,
                            outcome.usage.clone(),
                            "provider_failed",
                            outcome
                                .error
                                .as_deref()
                                .unwrap_or("provider request did not start"),
                        )
                        .await;
                    }
                    break;
                }
                // 已出发但无终态:保留已流出文本,按取消或 provider 失败结算。
                Evidence::StartedUnknown => {
                    self.seal_partial(&turn, &blocks, &outcome).await;
                    if outcome.failure_kind == Some(provider_job::ProviderFailureKind::WorkerCrash)
                    {
                        self.interrupt(&turn, InterruptReason::Crash).await;
                    } else if outcome.cancelled {
                        self.finish_cancelled(&turn, outcome.usage.clone()).await;
                    } else {
                        self.finish_failed(
                            &turn,
                            outcome.usage.clone(),
                            "provider_failed",
                            outcome
                                .error
                                .as_deref()
                                .unwrap_or("provider request failed"),
                        )
                        .await;
                    }
                    break;
                }
                Evidence::Terminal => {
                    self.seal_response(&turn, &blocks, &outcome).await;
                    match outcome.finish {
                        Some(ProviderFinish::Stop) => {
                            self.finish_completed(&turn, outcome.usage.clone()).await;
                            break;
                        }
                        Some(ProviderFinish::ToolUse) => {
                            let message = outcome.message.clone().unwrap_or_else(empty_assistant);
                            let tool_uses = message
                                .content
                                .iter()
                                .filter_map(|block| match block {
                                    ContentBlock::ToolUse { id, name, input } => {
                                        Some((id.clone(), name.clone(), input.clone()))
                                    }
                                    _ => None,
                                })
                                .collect::<Vec<_>>();
                            if tool_uses.is_empty() {
                                self.finish_failed(
                                    &turn,
                                    outcome.usage.clone(),
                                    "provider_incomplete",
                                    "provider returned a tool-use finish without tool declarations",
                                )
                                .await;
                                break;
                            }
                            for (call_id, name, input) in &tool_uses {
                                self.declare(&turn, call_id, name, input).await;
                            }
                            let mut tool_messages = Vec::with_capacity(tool_uses.len());
                            for (call_id, name, input) in &tool_uses {
                                let (ok, text) = match self
                                    .dispatch_tool(&turn, call_id, name, input, &cancel_rx)
                                    .await
                                {
                                    Ok(result) => result,
                                    Err(error) => {
                                        log::error!(
                                            "[loop] {} tool {name} dispatch failed: {error}",
                                            self.session_id
                                        );
                                        (false, "tool dispatch failed".to_string())
                                    }
                                };
                                // ToolResult is the durable RingingV2 history fact. Keeping a
                                // result only in this turn's in-memory request makes the next
                                // context rebuild synthesize `missing tool result`.
                                if let Err(error) = self
                                    .owner
                                    .append_block(TimelineEntry::ToolResult {
                                        call_id: ToolCallId::new(call_id.clone()),
                                        turn_id: Some(turn.clone()),
                                        content: ContentValue::Inline { text: text.clone() },
                                    })
                                    .await
                                {
                                    let message = error.to_string();
                                    log::error!(
                                        "[loop] {} tool {name} result persistence failed: {message}",
                                        self.session_id
                                    );
                                    self.finish_failed(
                                        &turn,
                                        outcome.usage.clone(),
                                        "tool_result_persist_failed",
                                        &message,
                                    )
                                    .await;
                                    break 'rounds;
                                }
                                tool_messages.push(tool_result_message(call_id, &text, ok));
                            }
                            messages.push(message);
                            messages.extend(tool_messages);
                        }
                        _ => {
                            // Provider 给出了终态,但响应不完整或不可用:已确认失败。
                            self.finish_failed(
                                &turn,
                                outcome.usage.clone(),
                                "provider_incomplete",
                                outcome
                                    .incomplete_reason
                                    .as_deref()
                                    .unwrap_or("provider returned an incomplete response"),
                            )
                            .await;
                            break;
                        }
                    }
                }
            }
        }
        self.finish_active(&turn_id).await;
    }

    async fn declare(
        &self,
        turn: &TurnId,
        call_id: &str,
        tool_name: &str,
        input: &serde_json::Value,
    ) {
        let call = ToolCallId::new(call_id);
        if let Err(error) = self
            .owner
            .declare_tool_call(&call, turn, tool_name, Some(input.clone()))
            .await
        {
            log::error!("[loop] declare {call_id} failed: {error}");
        }
        self.owner
            .append_block(TimelineEntry::ToolCall {
                call_id: call,
                turn_id: Some(turn.clone()),
            })
            .await
            .ok();
    }

    /// 单个工具的完整执行:precheck → 权限/ask 交互 → port 派发。
    /// 返回 (是否成功, 回传模型的文本)。port 自身结算生命周期事件。
    async fn dispatch_tool(
        &self,
        turn: &TurnId,
        call_id: &str,
        tool_name: &str,
        input: &serde_json::Value,
        cancel_rx: &watch::Receiver<bool>,
    ) -> Result<(bool, String), LoopError> {
        // ask:答案来自交互应答,不走 port。
        if tool_name == "ask" {
            return self.run_ask(turn, call_id, input, cancel_rx).await;
        }
        let args = serde_json::to_vec(input).map_err(|error| {
            LoopError::Db(qaqh_session::db::DbError::Corrupt(error.to_string()))
        })?;
        let args_json = Some(String::from_utf8_lossy(&args).into_owned());
        let request = ToolRequest {
            execution_id: generate_ulid(),
            call_id: call_id.to_string(),
            turn_id: turn.as_str().to_string(),
            tool_name: tool_name.to_string(),
            read_only: false,
            args: args.clone(),
        };

        match self.port.precheck(&request) {
            ToolAdmission::Denied(reason) => {
                self.settle_failed(call_id, tool_name, args_json, "permission_denied", &reason);
                return Ok((false, reason));
            }
            ToolAdmission::ApprovalRequired { details } => {
                let Some((interaction_id, answer)) = self
                    .request_permission(turn, call_id, &details, cancel_rx)
                    .await
                else {
                    if *cancel_rx.borrow() {
                        self.settle_cancelled(
                            call_id,
                            tool_name,
                            args_json,
                            qaqh_types::ToolCancellationReason::TurnCancel,
                        );
                        return Ok((false, "cancelled".into()));
                    }
                    self.settle_failed(
                        call_id,
                        tool_name,
                        args_json,
                        "permission_denied",
                        "permission interaction failed",
                    );
                    return Ok((false, "permission interaction failed".into()));
                };
                if answer.trust_folder {
                    let cwd = self
                        .cwd
                        .read()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone();
                    self.port.trust_dir(cwd);
                }
                if !answer.approved {
                    self.settle_failed(
                        call_id,
                        tool_name,
                        args_json,
                        "permission_denied",
                        "denied by user",
                    );
                    return Ok((false, "denied by user".into()));
                }
                // 迁移表:权限允许后回到 declared,等真正进入执行再 running。
                self.owner
                    .tool_back_to_declared(&ToolCallId::new(call_id.to_string()))
                    .await
                    .ok();
                self.port.grant_approval(ToolApprovalGrant {
                    interaction_id: interaction_id.as_ref().to_string(),
                    call_id: call_id.to_string(),
                    execution_id: request.execution_id.clone(),
                    args_hash: WorkspaceToolPort::args_hash(&args),
                    workspace_root: self.port.workspace_root(),
                });
            }
            ToolAdmission::Authorized => {}
        }

        let flag = Arc::new(AtomicBool::new(*cancel_rx.borrow()));
        {
            let mut rx = cancel_rx.clone();
            let flag = Arc::clone(&flag);
            spawn_task(async move {
                loop {
                    if rx.changed().await.is_err() {
                        break;
                    }
                    if *rx.borrow() {
                        flag.store(true, Ordering::Release);
                        break;
                    }
                }
            });
        }
        let port = Arc::clone(&self.port);
        let joined = tokio::task::spawn_blocking(move || port.execute(&request, flag))
            .await
            .unwrap_or_else(|error| {
                Err(ToolFailure::AfterStart(format!(
                    "tool worker join failed: {error}"
                )))
            });
        Ok(match joined {
            Ok(output) => (true, output),
            // port 已在 execute 内结算生命周期;这里只取回传文本。
            Err(failure) => (false, failure_message(&failure)),
        })
    }

    /// 权限交互:登记 oneshot、升起交互、等待应答。None = 升起失败。
    async fn request_permission(
        &self,
        turn: &TurnId,
        call_id: &str,
        details: &PermissionRequestDetails,
        cancel_rx: &watch::Receiver<bool>,
    ) -> Option<(InteractionId, PermissionAnswer)> {
        let call = ToolCallId::new(call_id.to_string());
        let interaction_id = InteractionId::new(format!("int_{}", generate_ulid()));
        // 等待迁移实时可见且快照一致(E07):awaiting_interaction 落库,owner 泵定序。
        self.owner
            .tool_awaiting_interaction(&call, &interaction_id)
            .await
            .ok();
        let (tx, rx) = oneshot::channel();
        {
            let mut inner = self.lock();
            inner.permissions.insert(call_id.to_string(), tx);
            if let Some(active) = inner.active.as_mut() {
                active.pending_interactions.push(interaction_id.clone());
            }
        }
        let view = InteractionView {
            interaction_id: interaction_id.clone(),
            call_id: Some(call),
            turn_id: Some(turn.clone()),
            kind: InteractionKind::Permission,
            request: ContentValue::Inline {
                text: serde_json::to_string(details)
                    .expect("permission request details are serializable"),
            },
            choices: vec![
                InteractionChoice {
                    decision: InteractionDecision::Approved,
                    label: "允许".into(),
                    description: None,
                    tone: Default::default(),
                },
                InteractionChoice {
                    decision: InteractionDecision::Rejected,
                    label: "拒绝".into(),
                    description: None,
                    tone: Default::default(),
                },
            ],
            expires_at_ms: None,
            resolution: None,
        };
        if let Err(error) = self.owner.interaction_requested(view).await {
            log::error!("[loop] permission interaction failed: {error}");
            self.lock().permissions.remove(call_id);
            return None;
        }
        self.owner
            .set_turn_phase(turn.as_ref(), qaqh_ringing_contract::TurnPhase::WaitingUser)
            .await
            .ok();
        let mut cancel_rx = cancel_rx.clone();
        let answer = tokio::select! {
            answer = rx => answer.ok(),
            _ = wait_for_turn_cancel(&mut cancel_rx) => None,
        };
        let Some(answer) = answer else {
            {
                let mut inner = self.lock();
                inner.permissions.remove(call_id);
                if let Some(active) = inner.active.as_mut() {
                    active
                        .pending_interactions
                        .retain(|pending| pending != &interaction_id);
                }
            }
            self.owner
                .interaction_resolved(&interaction_id, None, InteractionEndStatus::Cancelled)
                .await
                .ok();
            return None;
        };
        self.owner
            .interaction_resolved(
                &interaction_id,
                Some(if answer.approved {
                    InteractionDecision::Approved
                } else {
                    InteractionDecision::Rejected
                }),
                InteractionEndStatus::Resolved,
            )
            .await
            .ok();
        if !self.waiting_user() {
            self.owner
                .set_turn_phase(turn.as_ref(), qaqh_ringing_contract::TurnPhase::Running)
                .await
                .ok();
        }
        Some((interaction_id, answer))
    }

    /// ask 交互:问题即请求,答案即工具结果。
    async fn run_ask(
        &self,
        turn: &TurnId,
        call_id: &str,
        input: &serde_json::Value,
        cancel_rx: &watch::Receiver<bool>,
    ) -> Result<(bool, String), LoopError> {
        let ask = match normalize_ask_user(input) {
            Ok(ask) => ask,
            Err(error) => {
                self.settle_failed(
                    call_id,
                    "ask",
                    Some(input.to_string()),
                    error.code,
                    &error.message,
                );
                return Ok((false, error.message));
            }
        };
        let text = serde_json::to_string(&ask)
            .map_err(|error| qaqh_session::db::DbError::Corrupt(error.to_string()))?;
        let interaction_id = InteractionId::new(format!("int_{}", generate_ulid()));
        let (tx, rx) = oneshot::channel();
        {
            let mut inner = self.lock();
            inner.asks.insert(interaction_id.as_ref().to_string(), tx);
            if let Some(active) = inner.active.as_mut() {
                active.pending_interactions.push(interaction_id.clone());
            }
        }
        let view = InteractionView {
            interaction_id: interaction_id.clone(),
            call_id: Some(ToolCallId::new(call_id.to_string())),
            turn_id: Some(turn.clone()),
            kind: InteractionKind::Ask,
            request: ContentValue::Inline { text },
            choices: vec![],
            expires_at_ms: None,
            resolution: None,
        };
        self.owner.interaction_requested(view).await?;
        self.owner
            .set_turn_phase(turn.as_ref(), qaqh_ringing_contract::TurnPhase::WaitingUser)
            .await
            .ok();
        let mut cancel_rx = cancel_rx.clone();
        let reply = tokio::select! {
            reply = rx => reply.ok(),
            _ = wait_for_turn_cancel(&mut cancel_rx) => None,
        };
        let Some(AskReply::Answers(answers)) = reply else {
            {
                let mut inner = self.lock();
                inner.asks.remove(interaction_id.as_ref());
                if let Some(active) = inner.active.as_mut() {
                    active
                        .pending_interactions
                        .retain(|pending| pending != &interaction_id);
                }
            }
            let turn_cancelled = *cancel_rx.borrow();
            let (decision, status, reason) = if matches!(reply, Some(AskReply::Dismissed)) {
                (
                    Some(InteractionDecision::Dismissed),
                    InteractionEndStatus::Resolved,
                    qaqh_types::ToolCancellationReason::InteractionDismissed,
                )
            } else {
                (
                    None,
                    InteractionEndStatus::Cancelled,
                    qaqh_types::ToolCancellationReason::TurnCancel,
                )
            };
            self.owner
                .interaction_resolved(&interaction_id, decision, status)
                .await
                .ok();
            if !turn_cancelled && !self.waiting_user() {
                self.owner
                    .set_turn_phase(turn.as_ref(), qaqh_ringing_contract::TurnPhase::Running)
                    .await
                    .ok();
            }
            self.settle_cancelled(call_id, "ask", None, reason);
            return Ok((false, "cancelled".into()));
        };
        let text = answers
            .iter()
            .map(|(question, answer)| format!("{question}: {answer}"))
            .collect::<Vec<_>>()
            .join("\n");
        self.owner
            .interaction_resolved(
                &interaction_id,
                Some(InteractionDecision::Answered),
                InteractionEndStatus::Resolved,
            )
            .await
            .ok();
        if !self.waiting_user() {
            self.owner
                .set_turn_phase(turn.as_ref(), qaqh_ringing_contract::TurnPhase::Running)
                .await
                .ok();
        }
        self.settle_finished_text(call_id, "ask", &text);
        Ok((true, text))
    }

    fn settle_failed(
        &self,
        call_id: &str,
        tool_name: &str,
        args_json: Option<String>,
        code: &str,
        message: &str,
    ) {
        self.lifecycle.settled(
            call_id,
            tool_name,
            args_json,
            ToolSettlement {
                call_id: call_id.to_string(),
                tool_name: tool_name.to_string(),
                outcome: ToolCallOutcome::Failed {
                    error: qaqh_ringing_contract::fact::ToolError {
                        code: code.to_string(),
                        message: message.chars().take(512).collect(),
                        retryable: false,
                        details_ref: None,
                    },
                },
                output_text: None,
                output_truncated: false,
                metrics: None,
                code_delta: None,
                effects: Vec::new(),
            },
        );
    }

    fn settle_finished_text(&self, call_id: &str, tool_name: &str, text: &str) {
        self.lifecycle.settled(
            call_id,
            tool_name,
            None,
            ToolSettlement {
                call_id: call_id.to_string(),
                tool_name: tool_name.to_string(),
                outcome: ToolCallOutcome::Completed,
                output_text: Some(text.to_string()),
                output_truncated: false,
                metrics: None,
                code_delta: None,
                effects: Vec::new(),
            },
        );
    }

    fn settle_cancelled(
        &self,
        call_id: &str,
        tool_name: &str,
        args_json: Option<String>,
        reason: qaqh_types::ToolCancellationReason,
    ) {
        self.lifecycle.settled(
            call_id,
            tool_name,
            args_json,
            ToolSettlement {
                call_id: call_id.to_string(),
                tool_name: tool_name.to_string(),
                outcome: ToolCallOutcome::Cancelled { reason },
                output_text: None,
                output_truncated: false,
                metrics: None,
                code_delta: None,
                effects: Vec::new(),
            },
        );
    }

    /// 终态 seal:推理块 + 答案块,复用流式展示的 block 身份。
    async fn seal_response(
        &self,
        turn: &TurnId,
        blocks: &AttemptBlocks,
        outcome: &ProviderOutcome,
    ) {
        let message = outcome.message.clone().unwrap_or_else(empty_assistant);
        let mut reasoning = String::new();
        let mut answer = String::new();
        for block in &message.content {
            match block {
                ContentBlock::Reasoning {
                    reasoning: text, ..
                } => reasoning.push_str(text),
                ContentBlock::Text { text } => answer.push_str(text),
                _ => {}
            }
        }
        let reasoning = if reasoning.is_empty() {
            outcome.partial_reasoning.clone().unwrap_or_default()
        } else {
            reasoning
        };
        let answer = if answer.is_empty() {
            outcome.partial_text.clone().unwrap_or_default()
        } else {
            answer
        };
        if !reasoning.is_empty() {
            self.owner
                .append_block(TimelineEntry::AssistantBlock {
                    block_id: BlockId::new(blocks.reasoning_block_id.clone()),
                    turn_id: turn.clone(),
                    block_kind: AssistantBlockKind::Reasoning,
                    content: ContentValue::Inline { text: reasoning },
                    model: blocks.model.clone(),
                    usage: None,
                    opaque: vec![],
                })
                .await
                .ok();
        }
        self.owner
            .append_block(TimelineEntry::AssistantBlock {
                block_id: BlockId::new(blocks.answer_block_id.clone()),
                turn_id: turn.clone(),
                block_kind: AssistantBlockKind::Answer,
                content: ContentValue::Inline { text: answer },
                model: blocks.model.clone(),
                usage: outcome.usage.clone(),
                opaque: outcome.opaque_items.clone(),
            })
            .await
            .ok();
    }

    /// 中断路径的 seal:只保留已流出文本。
    async fn seal_partial(&self, turn: &TurnId, blocks: &AttemptBlocks, outcome: &ProviderOutcome) {
        let reasoning = outcome.partial_reasoning.clone().unwrap_or_default();
        let answer = outcome.partial_text.clone().unwrap_or_default();
        if !reasoning.is_empty() {
            self.owner
                .append_block(TimelineEntry::AssistantBlock {
                    block_id: BlockId::new(blocks.reasoning_block_id.clone()),
                    turn_id: turn.clone(),
                    block_kind: AssistantBlockKind::Reasoning,
                    content: ContentValue::Inline { text: reasoning },
                    model: blocks.model.clone(),
                    usage: None,
                    opaque: vec![],
                })
                .await
                .ok();
        }
        if !answer.is_empty() {
            self.owner
                .append_block(TimelineEntry::AssistantBlock {
                    block_id: BlockId::new(blocks.answer_block_id.clone()),
                    turn_id: turn.clone(),
                    block_kind: AssistantBlockKind::Answer,
                    content: ContentValue::Inline { text: answer },
                    model: blocks.model.clone(),
                    usage: outcome.usage.clone(),
                    opaque: vec![],
                })
                .await
                .ok();
        }
    }

    async fn finish_completed(&self, turn: &TurnId, usage: Option<UsageInfo>) {
        self.owner
            .finish_turn(
                turn.as_ref(),
                "finished",
                TurnOutcome::Finished {
                    terminal: qaqh_ringing_contract::fact::TurnTerminal::Completed,
                    usage,
                    error: None,
                },
            )
            .await
            .ok();
    }

    async fn finish_cancelled(&self, turn: &TurnId, usage: Option<UsageInfo>) {
        self.resolve_pending_interactions().await;
        self.owner
            .finish_turn(
                turn.as_ref(),
                "finished",
                TurnOutcome::Finished {
                    terminal: qaqh_ringing_contract::fact::TurnTerminal::Cancelled,
                    usage,
                    error: None,
                },
            )
            .await
            .ok();
    }

    async fn finish_failed(
        &self,
        turn: &TurnId,
        usage: Option<UsageInfo>,
        code: &str,
        message: &str,
    ) {
        self.owner
            .finish_turn(
                turn.as_ref(),
                "finished",
                TurnOutcome::Finished {
                    terminal: qaqh_ringing_contract::fact::TurnTerminal::Failed,
                    usage,
                    error: Some(qaqh_ringing_contract::fact::TurnError {
                        code: code.to_string(),
                        message: message.to_string(),
                        retryable: false,
                        details_ref: None,
                    }),
                },
            )
            .await
            .ok();
    }

    async fn resolve_pending_interactions(&self) {
        let pending = self
            .lock()
            .active
            .as_ref()
            .map(|active| active.pending_interactions.clone())
            .unwrap_or_default();
        for interaction_id in pending {
            self.owner
                .interaction_resolved(&interaction_id, None, InteractionEndStatus::Cancelled)
                .await
                .ok();
        }
    }

    async fn interrupt(&self, turn: &TurnId, reason: InterruptReason) {
        // 挂起交互随回合取消。
        self.resolve_pending_interactions().await;
        self.owner
            .finish_turn(
                turn.as_ref(),
                "interrupted",
                TurnOutcome::Interrupted { reason },
            )
            .await
            .ok();
    }

    /// 回合收尾:清活动槽并续接队首输入。
    async fn finish_active(self: &Arc<Self>, turn_id: &str) {
        self.last_activity_ms
            .store(unix_now_ms().max(0) as u64, Ordering::Release);
        let next = {
            let mut inner = self.lock();
            match &inner.active {
                Some(active) if active.turn_id == turn_id => {
                    inner.active = None;
                }
                _ => return,
            }
            inner.queue.pop_front()
        };
        if let Some(input) = next {
            // 队首输入直接开新回合(不经 send_input:QueueOnly 项已在队内,
            // 续接即处理)。
            let _ = self.spawn_turn(input);
        }
    }

    /// 从 SQLite blocks/turns 组装 provider 消息流。
    ///
    /// 分组规则:相邻 AssistantBlock + ToolCall 归并为一条 assistant 消息,
    /// Input/ToolResult/Compaction 是边界。工具名/参数从 tool_calls 表 join。
    pub(crate) async fn assemble_context(&self) -> Result<Vec<Message>, LoopError> {
        let state = self.owner.snapshot().await?;
        let tools_by_id: HashMap<String, &qaqh_ringing_contract::state::ToolCallView> = state
            .tools
            .iter()
            .map(|tool| (tool.call_id.as_ref().to_string(), tool))
            .collect();
        let mut messages: Vec<Message> = Vec::new();
        let mut group: Option<Message> = None;
        for entry in state.timeline {
            match entry {
                TimelineEntry::Input {
                    content,
                    images,
                    actor,
                    ..
                } => {
                    flush_assistant(&mut messages, group.take());
                    let mut content_blocks = Vec::new();
                    if let ContentValue::Inline { text } = content
                        && !text.is_empty()
                    {
                        content_blocks.push(ContentBlock::Text { text });
                    }
                    content_blocks.extend(images.into_iter().map(|image| ContentBlock::Image {
                        mime_type: image.mime_type,
                        data: image.data,
                    }));
                    if !content_blocks.is_empty() {
                        messages.push(Message {
                            msg_id: None,
                            role: if actor.kind == ActorKind::System {
                                "system"
                            } else {
                                "user"
                            }
                            .into(),
                            name: None,
                            content: content_blocks,
                        });
                    }
                }
                TimelineEntry::AssistantBlock {
                    block_kind,
                    content,
                    opaque,
                    ..
                } => {
                    let message = group.get_or_insert_with(empty_assistant);
                    if let ContentValue::Inline { text } = content
                        && !text.is_empty()
                    {
                        match block_kind {
                            AssistantBlockKind::Reasoning => {
                                message.content.push(ContentBlock::Reasoning {
                                    reasoning: text,
                                    kind: None,
                                })
                            }
                            AssistantBlockKind::Answer => {
                                message.content.push(ContentBlock::Text { text })
                            }
                            // 工具调用锚点块不携带文本;tool_use 由 ToolCall 条目补。
                            AssistantBlockKind::ToolCall => {}
                        }
                    }
                    for item in opaque {
                        message
                            .content
                            .push(ContentBlock::ResponseOutputItem { item });
                    }
                }
                TimelineEntry::ToolCall { call_id, .. } => {
                    let Some(message) = group.as_mut() else {
                        continue;
                    };
                    let (name, args) = tools_by_id
                        .get(call_id.as_ref())
                        .map(|tool| {
                            (
                                tool.tool_name.clone().unwrap_or_default(),
                                tool.args.clone().unwrap_or(serde_json::Value::Null),
                            )
                        })
                        .unwrap_or_default();
                    message.content.push(ContentBlock::ToolUse {
                        id: call_id.as_ref().to_string(),
                        name,
                        input: args,
                    });
                }
                TimelineEntry::ToolResult {
                    call_id, content, ..
                } => {
                    flush_assistant(&mut messages, group.take());
                    // E06:重放不可变的调用 outcome;yielded 是成功返回,不查
                    // 进程当前状态推断调用成功,也不因后来 exit 1 翻成历史 error。
                    let ok = tools_by_id
                        .get(call_id.as_ref())
                        .map(|tool| match &tool.state {
                            ToolCallState::Finished { outcome } => outcome.is_success(),
                            _ => false,
                        })
                        .unwrap_or(false);
                    let text = match content {
                        ContentValue::Inline { text } => text,
                        _ => String::new(),
                    };
                    messages.push(tool_result_message(call_id.as_ref(), &text, ok));
                }
                TimelineEntry::Compaction { .. } => {
                    // 压缩尚无写入方;出现时按边界刷新。
                    flush_assistant(&mut messages, group.take());
                }
            }
        }
        flush_assistant(&mut messages, group.take());
        Ok(messages)
    }

    fn current_spec(&self) -> ProviderRequestSpec {
        self.spec
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

fn empty_assistant() -> Message {
    Message {
        msg_id: None,
        role: "assistant".into(),
        name: None,
        content: Vec::new(),
    }
}

fn flush_assistant(messages: &mut Vec<Message>, group: Option<Message>) {
    if let Some(message) = group
        && !message.content.is_empty()
    {
        messages.push(message);
    }
}

fn tool_result_message(call_id: &str, text: &str, ok: bool) -> Message {
    Message {
        msg_id: None,
        role: "tool".into(),
        name: None,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: call_id.to_string(),
            result: if ok {
                ToolResultPayload::ok(text)
            } else {
                ToolResultPayload::error(text)
            },
        }],
    }
}

async fn wait_for_turn_cancel(cancel: &mut watch::Receiver<bool>) {
    while !*cancel.borrow() {
        if cancel.changed().await.is_err() {
            return;
        }
    }
}

fn failure_message(failure: &ToolFailure) -> String {
    match failure {
        ToolFailure::Denied(reason)
        | ToolFailure::NotStarted(reason)
        | ToolFailure::AfterStart(reason) => reason.clone(),
        ToolFailure::TerminalError(message) => message.clone(),
    }
}

/// owner 直通的流式展示:进度帧只进事件流,不落库。
struct OwnerSink {
    owner: Arc<SessionOwner>,
}

impl ProgressSink for OwnerSink {
    fn assistant_block(&self, update: crate::progress::AssistantProgress<'_>) {
        let block_kind = match update.kind {
            crate::progress::ProgressBlockKind::Reasoning => AssistantBlockKind::Reasoning,
            crate::progress::ProgressBlockKind::Answer => AssistantBlockKind::Answer,
        };
        self.owner.message_delta(
            &TurnId::new(update.turn_id.to_string()),
            &BlockId::new(update.block_id.to_string()),
            block_kind,
            update.text,
        );
    }

    fn tool_input(&self, update: crate::progress::ToolInputProgress<'_>) {
        self.owner.tool_arguments_delta(
            &TurnId::new(update.turn_id.to_string()),
            &ToolCallId::new(update.call_id.to_string()),
            update.tool_name,
            update.args,
        );
    }

    fn attempt_finished(&self, _turn_id: &str, _block_ids: &[String], _call_ids: &[String]) {
        // owner 无在飞身份登记;快照 + seq 水位自愈。
    }
}
