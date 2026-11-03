//! `SubagentHost` 的进程内实现（Knife-1 step-2 收尾）。
//!
//! 主 session 与子代理 loop 均作为 daemon 线程内 in-process actor 运行
//! （PR #17/#19/#21），`spawn_subagent` 工具 handler 也在 daemon 进程内
//! 执行——此前它经 daemon HTTP/SSE 回连自己，属于多余的进程内回环。
//!
//! 本模块让 `QaqhService` 直接实现 [`qaqh_subagent::SubagentHost`]：
//! 工具 handler 通过该宿主句柄直达进程内 `AgentRegistry` + `RingingHub`，
//! 不再建立 HTTP/SSE 连接。事件订阅直接走 hub 进程内 broadcast。

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use qaqh_ringing::RingingWorkerCommandEnvelope;
use qaqh_ringing_contract::team::{
    ChannelId, PostId, TaskId, ThreadId, TeamBoardChannel, TeamBoardPost, TeamBoardSnapshot,
    TeamBoardSubscription, TeamBoardSubscriptionTarget, TeamBoardThread,
};
use qaqh_session::session_fact_v2::{
    AssistantBlockKind, ContentHash, ContentRef as CanonicalContentRef, ContentValue,
};
use qaqh_subagent::{
    ArmSubagentCollectorRequest, BoardChannel, BoardChannelCreateRequest, BoardHost,
    BoardListRequest, BoardNotificationSkip, BoardPost, BoardPostOutcome, BoardPostRequest,
    BoardSnapshot, BoardSubscription, BoardSubscriptionAction, BoardSubscriptionRequest,
    BoardSubscriptionTargetKind, BoardThread, BoardThreadCreateRequest, CollectorBatch,
    CollectorEvent, CollectorSubscription, ContentRef, InterruptAgentRequest, InterruptedAgent,
    ListedAgent, ListedAgentResidency, ListedAgentStatus, SendAgentMessageRequest,
    SentAgentMessage, SpawnSubagentRequest, SpawnedSubagent, StartSubagentRequest, SubagentHost,
    TaskBoardArtifact, TaskBoardHost, TaskBoardTask, TaskClaimAction, TaskClaimRequest,
    TaskCloseAction, TaskCloseRequest, TaskCreateRequest, TaskListRequest, TaskUpdateAction,
    TaskUpdateRequest, WaitAgentOutcome, WaitAgentRequest,
};

use super::QaqhService;

use qaqh_session::db::team as dbteam;

/// 调用方上下文:root 会话 + actor 标识(team/board 写路径共用)。
struct CallerContext {
    root_session_id: String,
    actor_path: String,
    actor_session_id: String,
}

/// root 的 message board 全量行。
struct BoardRows {
    channels: Vec<dbteam::BoardChannelRow>,
    threads: Vec<dbteam::BoardThreadRow>,
    posts: Vec<dbteam::BoardPostRow>,
    subscriptions: Vec<dbteam::BoardSubscriptionRow>,
}

impl QaqhService {
    /// Count queued messages from `author` to `recipient` in the target's
    /// canonical mailbox.
    ///
    /// This is the authoritative in-flight signal for the Phase 3 message
    /// quota: an `InterAgentCommunication` is in-flight until the matching
    /// `InputAccepted` is committed. The target log is read on demand because
    /// sends are a tool-call path, not a hot loop.
    fn count_in_flight_messages(
        &self,
        target_session_id: &str,
        _author: &str,
        _recipient: &str,
    ) -> Result<u64, String> {
        // 进程内无第二份 mailbox:排队输入本身就是 in-flight(配额是安全阀,
        // 不区分 author 的近似计数)。
        let registry = self.registry()?;
        Ok(registry
            .loop_for(target_session_id)
            .map(|loop_| loop_.pending_inputs() as u64)
            .unwrap_or(0))
    }
}

impl SubagentHost for QaqhService {
    fn spawn_subagent(&self, request: SpawnSubagentRequest<'_>) -> Result<SpawnedSubagent, String> {
        let SpawnSubagentRequest {
            parent_session_id,
            requested_name,
            tools,
            model,
            base_url,
            max_tokens,
            workspace,
            permission_level,
        } = request;
        // BUG-2026-09-13-24 + BETA-01：子代理也先分配 canonical identity，
        // 目录名与 `child_session_id` 必须是同一个 UUID。子代理目录保持
        // unindexed，由 V2 residency 决定生命周期。
        let session_id = self
            .sessions
            .allocate_session()
            .map_err(|error| format!("allocate child session failed: {error}"))?;
        if let Some(workspace) = workspace.filter(|w| !w.is_empty() && *w != ".") {
            log::info!("[SUBAGENT-HOST] inherited workspace for seed={session_id}: {workspace}");
        }
        let spawned = self.registry()?.spawn_subagent_v2(
            &session_id,
            parent_session_id,
            requested_name,
            crate::registry::SubagentSpawnOptions {
                tools,
                model,
                base_url,
                max_tokens,
                // The requested workspace goes straight into the child's
                // genesis fact; the session tree never re-derives it.
                workspace: workspace.filter(|w| !w.is_empty() && *w != "."),
                ephemeral: false,
                permission_level,
            },
        )?;
        log::info!(
            "[SUBAGENT-HOST] spawned subagent seed={session_id} path={} tools={}",
            spawned.child_agent_path,
            tools.len()
        );
        Ok(SpawnedSubagent {
            session_id,
            child_session_id: spawned.child_session_id.as_str().to_string(),
            parent_agent_path: spawned.parent_agent_path.as_str().to_string(),
            child_agent_path: spawned.child_agent_path.as_str().to_string(),
        })
    }

    fn list_agents(
        &self,
        caller_session_id: &str,
        path_prefix: &str,
    ) -> Result<Vec<ListedAgent>, String> {
        self.registry()?
            .list_agents_with_state_for_caller(caller_session_id, path_prefix)
            .map_err(|error| error.to_string())
    }

    fn send_agent_message(
        &self,
        request: SendAgentMessageRequest<'_>,
    ) -> Result<SentAgentMessage, String> {
        if request.text.trim().is_empty() {
            return Err("agent message text must not be empty".to_string());
        }
        let target_ref = request.target.trim();
        if target_ref.starts_with('@') {
            return Err(format!(
                "broadcast target {target_ref:?} is not supported; send to one agent at a time"
            ));
        }
        if request.delivery == qaqh_domain::InterAgentDelivery::Interrupt {
            return Err("interrupt delivery is not implemented yet".to_string());
        }
        let (caller, target) = {
            let mut registry = self.registry()?;
            let target =
                registry.resolve_agent_for_caller(request.caller_session_id, request.target)?;
            let caller = registry
                .agent_metadata(request.caller_session_id)
                .ok_or_else(|| {
                    format!(
                        "caller agent metadata missing for {}",
                        request.caller_session_id
                    )
                })?;
            (caller, target)
        };
        if caller.agent_id == target.agent_id {
            return Err("cannot send an agent message to self".to_string());
        }
        if request.delivery == qaqh_domain::InterAgentDelivery::Trigger
            && target.agent_path.is_root()
            && !caller.agent_path.is_root()
        {
            return Err("child agents cannot trigger the root agent".to_string());
        }
        if matches!(
            request.delivery,
            qaqh_domain::InterAgentDelivery::Steer | qaqh_domain::InterAgentDelivery::Interject
        ) && target.agent_path.is_root()
            && !caller.agent_path.is_root()
        {
            return Err("child agents cannot steer or interject into the root agent".to_string());
        }

        // Phase 3 quota: in-flight is derived from the target's canonical
        // mailbox; outbound attempts are a runtime safety-valve counter.
        let in_flight = self.count_in_flight_messages(
            target.agent_id.as_str(),
            caller.agent_path.as_str(),
            target.agent_path.as_str(),
        )?;
        {
            let mut registry = self.registry()?;
            registry.admit_outbound_message(
                caller.root_session_id.as_str(),
                caller.agent_path.as_str(),
                in_flight,
            )?;
        }

        let message_id = format!("msg_{}", qaqh_ringing_contract::ulid::generate_ulid());
        let envelope = qaqh_domain::InterAgentEnvelope {
            message_id: message_id.clone(),
            root_session_id: target.root_session_id.as_str().to_string(),
            author: caller.agent_path.as_str().to_string(),
            recipient: target.agent_path.as_str().to_string(),
            other_recipients: vec![],
            task_id: None,
            reply_to: None,
            causation_id: None,
            delivery: request.delivery,
            created_at_ms: (nanos() / 1_000_000) as i64,
        };
        let input_purpose = match request.delivery {
            qaqh_domain::InterAgentDelivery::Queue => {
                qaqh_domain::ConversationInputPurpose::QueueOnly
            }
            qaqh_domain::InterAgentDelivery::Trigger => {
                qaqh_domain::ConversationInputPurpose::TriggerTurn
            }
            qaqh_domain::InterAgentDelivery::Steer => qaqh_domain::ConversationInputPurpose::Steer,
            qaqh_domain::InterAgentDelivery::Interject => {
                qaqh_domain::ConversationInputPurpose::Interject
            }
            qaqh_domain::InterAgentDelivery::Interrupt => unreachable!(),
        };
        let arm_spec = if request.delivery == qaqh_domain::InterAgentDelivery::Trigger
            && !target.agent_path.is_root()
        {
            self.registry()?
                .prepare_collector_arm(target.agent_id.as_str())?
        } else {
            None
        };
        if let Some(spec) = arm_spec {
            let collector_host: Arc<dyn SubagentHost> = Arc::new(self.clone());
            if let Err(error) = qaqh_subagent::arm_subagent_collector(
                collector_host,
                ArmSubagentCollectorRequest {
                    session_id: target.agent_id.as_str(),
                    child_session_id: &spec.child_session_id,
                    name: &spec.name,
                    parent_session_id: &spec.parent_session_id,
                    parent_call_id: &spec.parent_call_id,
                    timeout_secs: spec.timeout_secs,
                    root_session_id: &spec.root_session_id,
                    parent_agent_path: &spec.parent_agent_path,
                    child_agent_path: &spec.child_agent_path,
                },
            ) {
                self.registry()?
                    .unmark_collector_armed(target.agent_id.as_str());
                return Err(format!("arm subagent collector: {error}"));
            }
        }
        self.send_ringing(
            target.agent_id.as_str(),
            qaqh_ringing::RingingCommand::Conversation(
                qaqh_domain::ConversationCommand::ConversationSendMessage {
                    text: request.text.to_string(),
                    images: vec![],
                    attachments: None,
                    message_id: Some(message_id.clone()),
                    input_purpose,
                    as_system: false,
                    inter_agent: Some(envelope),
                    subagent_terminal: None,
                },
            ),
        )?;
        Ok(SentAgentMessage {
            message_id,
            recipient: target.agent_path.as_str().to_string(),
            delivery: request.delivery,
        })
    }

    fn wait_agent(&self, request: WaitAgentRequest<'_>) -> Result<WaitAgentOutcome, String> {
        let WaitAgentRequest {
            caller_session_id,
            timeout,
            should_cancel,
            targets,
        } = request;
        if !targets.is_empty() {
            return self.wait_for_agent_outcomes(
                caller_session_id,
                timeout,
                should_cancel,
                targets,
            );
        }
        // 输入到达检测 = 回合循环的内存队列(排队输入 + 未合并 steer)。
        // 旧 canonical commit 通知随旧框架退役;50ms 轮询对工具等待足够。
        let deadline = Instant::now() + timeout;
        loop {
            if should_cancel() {
                return Ok(WaitAgentOutcome::Cancelled);
            }
            if Instant::now() >= deadline {
                return Ok(WaitAgentOutcome::TimedOut {
                    activity_fact_seq: 0,
                });
            }
            {
                let registry = self.registry()?;
                if let Some(loop_) = registry.loop_for(caller_session_id)
                    && loop_.pending_inputs() > 0
                {
                    return Ok(WaitAgentOutcome::Activity {
                        activity_fact_seq: 1,
                    });
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn interrupt_agent(
        &self,
        request: InterruptAgentRequest<'_>,
    ) -> Result<InterruptedAgent, String> {
        let mut registry = self.registry()?;
        let caller = registry
            .list_agents_for_caller(request.caller_session_id, "/root")?
            .into_iter()
            .find(|agent| agent.agent_id.as_str() == request.caller_session_id)
            .ok_or_else(|| {
                format!(
                    "caller agent metadata missing for {}",
                    request.caller_session_id
                )
            })?;
        let target =
            registry.resolve_agent_for_caller(request.caller_session_id, request.target)?;
        if caller.agent_id == target.agent_id {
            return Err("cannot interrupt self".to_string());
        }
        if target.agent_path.is_root() {
            return Err("cannot interrupt the root agent".to_string());
        }

        let target_id = target.agent_id.as_str();
        if !registry.is_running(target_id) {
            return Ok(InterruptedAgent {
                recipient: target.agent_path.as_str().to_string(),
                previous_status: "unloaded".to_string(),
            });
        }
        let previous_status = registry
            .activity(target_id)
            .map(|activity| match activity.state {
                qaqh_domain::ActivityState::Starting => "starting",
                qaqh_domain::ActivityState::Idle => "idle",
                qaqh_domain::ActivityState::Working => "working",
                qaqh_domain::ActivityState::WaitingUser => "waiting_user",
                qaqh_domain::ActivityState::Disconnected => "disconnected",
                qaqh_domain::ActivityState::Failed => "failed",
            })
            .unwrap_or("running")
            .to_string();
        let command_id = format!("interrupt-{:x}", nanos());
        let env = RingingWorkerCommandEnvelope::new(
            target_id,
            command_id,
            qaqh_ringing::RingingCommand::Conversation(
                qaqh_domain::ConversationCommand::ConversationCancel { turn_id: None },
            ),
        );
        registry.send_ringing(target_id, &env)?;
        Ok(InterruptedAgent {
            recipient: target.agent_path.as_str().to_string(),
            previous_status,
        })
    }

    fn start_subagent(&self, request: StartSubagentRequest<'_>) -> Result<(), String> {
        let host =
            qaqh_subagent::host().ok_or_else(|| "subagent host is not installed".to_string())?;
        let metadata = self
            .registry()?
            .agent_metadata(request.child_session_id)
            .ok_or_else(|| {
                format!(
                    "subagent metadata missing for child {}",
                    request.child_session_id
                )
            })?;
        let author = metadata
            .parent_agent_path
            .as_ref()
            .map(|path| path.as_str().to_string())
            .unwrap_or_else(|| "/root".to_string());
        let envelope = qaqh_domain::InterAgentEnvelope {
            message_id: format!("msg_{}", qaqh_ringing_contract::ulid::generate_ulid()),
            root_session_id: metadata.root_session_id.as_str().to_string(),
            author,
            recipient: metadata.agent_path.as_str().to_string(),
            other_recipients: vec![],
            task_id: None,
            reply_to: None,
            causation_id: None,
            delivery: qaqh_domain::InterAgentDelivery::Trigger,
            created_at_ms: (nanos() / 1_000_000) as i64,
        };
        let child_session_id = request.child_session_id.to_string();
        qaqh_subagent::start_subagent_collector(
            host,
            StartSubagentRequest {
                inter_agent: Some(envelope),
                ..request
            },
        )?;
        self.registry()?.mark_collector_armed(&child_session_id, request.parent_call_id);
        Ok(())
    }

    fn rollback_subagent(&self, session_id: &str, child_session_id: &str) {
        // 边未成为权威事实：目录注册与结果面一起撤销。子代理从未登记进
        // 进程注册表，故这里不再有 process_id 可回收。
        match self.registry() {
            Ok(mut registry) => registry.rollback_subagent(session_id, child_session_id),
            Err(error) => {
                log::warn!(
                    "[SUBAGENT-HOST] rollback registry unavailable for {session_id}: {error}"
                );
                let _ = self.close(session_id);
            }
        }
    }

    fn abort_subagent(&self, session_id: &str) {
        if let Err(error) = self.close(session_id) {
            log::warn!("[SUBAGENT-HOST] abort close failed for {session_id}: {error}");
        }
    }

    fn finish_subagent(&self, child_session_id: &str, answer: &str, exit_code: i32) {
        match self.registry() {
            Ok(mut registry) => {
                registry.record_subagent_outcome(child_session_id, answer, exit_code);
            }
            Err(error) => {
                log::warn!(
                    "[SUBAGENT-HOST] outcome registry unavailable for {child_session_id}: {error}"
                );
            }
        }
    }

    fn send_ringing(
        &self,
        session_id: &str,
        command: qaqh_ringing::RingingCommand,
    ) -> Result<(), String> {
        let id = format!("host-{:x}", nanos());
        let env = RingingWorkerCommandEnvelope::new(session_id, id, command);
        self.registry()?
            .send_ringing(session_id, &env)
            .map_err(|error| error.to_string())
    }

    fn subscribe(&self, session_id: &str) -> CollectorSubscription {
        let (tx, rx) = mpsc::channel::<CollectorBatch>();
        // owner 单流广播即子代理输出源:Answer 块 seal → AnswerSealed,
        // 回合终态 → TurnFinished。只收 live 事件(订阅点即起点),
        // 与旧桥接的订阅时机语义一致。
        let mut event_rx = self.owners().get_or_create(session_id).subscribe();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let bridge_session = session_id.to_string();
        let bridge = std::thread::Builder::new()
            .name(format!("qaqh-subagent-sub-{session_id}"))
            .spawn(move || {
                loop {
                    if stop_rx.try_recv().is_ok() {
                        break;
                    }
                    match event_rx.blocking_recv() {
                        Ok(frame) => {
                            if let Some(event) =
                                collector_event_from_session_event(&frame.event)
                            {
                                let batch = CollectorBatch {
                                    session_id: bridge_session.clone(),
                                    events: vec![event],
                                };
                                if tx.send(batch).is_err() {
                                    break;
                                }
                            }
                        }
                        // Lagged:收集器侧有超时与 Disconnected 兜底,丢一段
                        // 输出不中断收集;Closed = owner 已销毁。
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            })
            .ok();
        CollectorSubscription::new(rx, move || {
            let _ = stop_tx.send(());
            if let Some(bridge) = bridge {
                let _ = bridge.join();
            }
        })
    }

    fn download_content(
        &self,
        session_id: &str,
        reference: &ContentRef,
    ) -> Result<Vec<u8>, String> {
        let canonical_ref = canonical_content_ref(&reference.content_id)?;
        let expected = reference
            .sha256
            .strip_prefix("sha256:")
            .unwrap_or(&reference.sha256);
        let bytes = if let Some(entry) = self.hub.get().and_then(|hub| {
            let id = reference
                .content_id
                .strip_prefix("sha256:")
                .unwrap_or(&reference.content_id);
            hub.get_content(session_id, id)
        }) {
            entry.bytes
        } else {
            return Err(format!(
                "content {} not found",
                reference.content_id
            ));
        };
        let digest = {
            use sha2::Digest;
            let hash = sha2::Sha256::digest(&bytes);
            hash.iter().map(|b| format!("{b:02x}")).collect::<String>()
        };
        let content_hash = canonical_ref.hash().as_str().trim_start_matches("sha256:");
        if !digest.eq_ignore_ascii_case(expected) || !digest.eq_ignore_ascii_case(content_hash) {
            return Err(format!(
                "content digest mismatch for {}: expected {}, received {digest}",
                reference.content_id, reference.sha256
            ));
        }
        Ok(bytes)
    }

    fn close(&self, session_id: &str) -> Result<(), String> {
        // 与 daemon action `session.close`/`SessionClose` 拦截一致：关闭 registry
        // 实例并清理临时会话；结果已注入主会话 + 终态已回写注册表，残留不丢数据。
        self.close_session(session_id, None)
    }
}

const MAX_TASKS_PER_ROOT: usize = 1024;

impl TaskBoardHost for QaqhService {
    fn task_create(&self, request: TaskCreateRequest<'_>) -> Result<TaskBoardTask, String> {
        if request.title.trim().is_empty() {
            return Err("task title must not be empty".to_string());
        }
        let caller = self.caller_context(request.caller_session_id)?;
        let description_ref = request
            .description_ref
            .map(canonical_content_ref)
            .transpose()?
            .map(|content_ref| content_ref.hash().as_str().to_string());
        let task_id = TaskId::generate();
        let closure_task_id = task_id.clone();
        let now = unix_ms();
        let title = request.title.trim().to_string();
        let caller_session = request.caller_session_id.to_string();
        self.owners
            .db()
            .write_blocking(move |conn| {
                if dbteam::task_count(conn, &caller.root_session_id)? >= MAX_TASKS_PER_ROOT as i64
                {
                    return Err(qaqh_session::db::DbError::Corrupt(format!(
                        "task board limit {MAX_TASKS_PER_ROOT} reached for this root tree"
                    )));
                }
                dbteam::task_insert(
                    conn,
                    &dbteam::NewTeamTask {
                        task_id: closure_task_id.as_str(),
                        root_session_id: &caller.root_session_id,
                        title: &title,
                        description_ref: description_ref.as_deref(),
                        created_by: &caller.actor_path,
                        created_at_ms: now,
                    },
                )?;
                Ok(())
            })
            .map_err(|error| format!("task_create failed: {error}"))?;
        self.task_get(&caller_session, task_id.as_str())
    }

    fn task_claim(&self, request: TaskClaimRequest<'_>) -> Result<TaskBoardTask, String> {
        let caller = self.caller_context(request.caller_session_id)?;
        let now = unix_ms();
        let task_id = request.task_id.to_string();
        let closure_task_id = task_id.clone();
        let caller_session = request.caller_session_id.to_string();
        let action = request.action;
        self.owners
            .db()
            .write_blocking(move |conn| {
                match action {
                    TaskClaimAction::Claim => dbteam::task_claim(
                        conn,
                        &caller.root_session_id,
                        &closure_task_id,
                        &caller.actor_path,
                        &caller.actor_session_id,
                        now,
                    ),
                    TaskClaimAction::Release => dbteam::task_release(
                        conn,
                        &caller.root_session_id,
                        &closure_task_id,
                        &caller.actor_path,
                        now,
                    ),
                }
                .map_err(qaqh_session::db::DbError::Corrupt)
            })
            .map_err(|error| format!("task_claim failed: {error}"))?;
        self.task_get(&caller_session, &task_id)
    }

    fn task_update(&self, request: TaskUpdateRequest<'_>) -> Result<TaskBoardTask, String> {
        let caller = self.caller_context(request.caller_session_id)?;
        let now = unix_ms();
        let task_id = request.task_id.to_string();
        let closure_task_id = task_id.clone();
        let caller_session = request.caller_session_id.to_string();
        let action = request.action;
        let depends_on = request.depends_on.map(str::to_string);
        let artifact_ref = request.artifact_ref.map(str::to_string);
        let media_type = request.media_type.map(str::to_string);
        let acceptance = request.acceptance.map(<[String]>::to_vec);
        self.owners
            .db()
            .write_blocking(move |conn| match action {
                TaskUpdateAction::AddDependency => {
                    let depends_on = depends_on.ok_or_else(|| {
                        qaqh_session::db::DbError::Corrupt(
                            "task_update add_dependency requires depends_on".into(),
                        )
                    })?;
                    dbteam::task_add_dependency(
                        conn,
                        &caller.root_session_id,
                        &closure_task_id,
                        &depends_on,
                        now,
                    )
                    .map_err(qaqh_session::db::DbError::Corrupt)
                }
                TaskUpdateAction::AttachArtifact => {
                    let artifact_ref = artifact_ref.ok_or_else(|| {
                        qaqh_session::db::DbError::Corrupt(
                            "task_update attach_artifact requires artifact_ref".into(),
                        )
                    })?;
                    let media_type = media_type.ok_or_else(|| {
                        qaqh_session::db::DbError::Corrupt(
                            "task_update attach_artifact requires media_type".into(),
                        )
                    })?;
                    let content_id = canonical_content_ref(&artifact_ref)
                        .map_err(qaqh_session::db::DbError::Corrupt)?
                        .hash()
                        .as_str()
                        .trim_start_matches("sha256:")
                        .to_string();
                    dbteam::task_attach_artifact(
                        conn,
                        &caller.root_session_id,
                        &closure_task_id,
                        &content_id,
                        &media_type,
                        now,
                    )
                    .map_err(qaqh_session::db::DbError::Corrupt)
                }
                TaskUpdateAction::SetAcceptance => {
                    let acceptance = acceptance.ok_or_else(|| {
                        qaqh_session::db::DbError::Corrupt(
                            "task_update set_acceptance requires acceptance".into(),
                        )
                    })?;
                    let items_json = serde_json::to_string(&acceptance)
                        .map_err(|error| {
                            qaqh_session::db::DbError::Corrupt(format!("acceptance json: {error}"))
                        })?;
                    dbteam::task_set_acceptance(
                        conn,
                        &caller.root_session_id,
                        &closure_task_id,
                        &items_json,
                        now,
                    )
                    .map_err(qaqh_session::db::DbError::Corrupt)
                }
            })
            .map_err(|error| format!("task_update failed: {error}"))?;
        self.task_get(&caller_session, &task_id)
    }

    fn task_close(&self, request: TaskCloseRequest<'_>) -> Result<TaskBoardTask, String> {
        let caller = self.caller_context(request.caller_session_id)?;
        let now = unix_ms();
        let task_id = request.task_id.to_string();
        let closure_task_id = task_id.clone();
        let caller_session = request.caller_session_id.to_string();
        let action = request.action;
        let result_ref = request
            .result_ref
            .map(canonical_content_ref)
            .transpose()?
            .map(|content_ref| content_ref.hash().as_str().to_string());
        let reason = request.reason.unwrap_or("cancelled").to_string();
        self.owners
            .db()
            .write_blocking(move |conn| match action {
                TaskCloseAction::Complete => {
                    dbteam::task_complete(
                        conn,
                        &caller.root_session_id,
                        &closure_task_id,
                        &caller.actor_path,
                        result_ref.as_deref(),
                        now,
                    )
                    .map_err(qaqh_session::db::DbError::Corrupt)
                }
                TaskCloseAction::Close => dbteam::task_close(
                    conn,
                    &caller.root_session_id,
                    &closure_task_id,
                    now,
                )
                .map_err(qaqh_session::db::DbError::Corrupt),
                TaskCloseAction::Cancel => {
                    dbteam::task_cancel(conn, &caller.root_session_id, &closure_task_id, &reason, now)
                        .map_err(qaqh_session::db::DbError::Corrupt)
                }
            })
            .map_err(|error| format!("task_close failed: {error}"))?;
        self.task_get(&caller_session, &task_id)
    }

    fn task_list(&self, request: TaskListRequest<'_>) -> Result<Vec<TaskBoardTask>, String> {
        let caller = self.caller_context(request.caller_session_id)?;
        let state_filter = request
            .state
            .map(parse_task_state)
            .transpose()?
            .map(|state| task_state_wire(&state).to_string());
        let tasks = self
            .owners
            .db()
            .read_blocking(move |conn| {
                dbteam::task_list(conn, &caller.root_session_id, state_filter.as_deref())
            })
            .map_err(|error| format!("task_list failed: {error}"))?;
        Ok(tasks.iter().map(task_to_dto).collect())
    }
}

impl BoardHost for QaqhService {
    fn board_channel_create(
        &self,
        request: BoardChannelCreateRequest<'_>,
    ) -> Result<BoardChannel, String> {
        let caller = self.caller_context(request.caller_session_id)?;
        let channel_id = ChannelId::generate();
        let closure_channel_id = channel_id.as_str().to_string();
        let now = unix_ms();
        let name = request.name.to_string();
        let closure_name = name.clone();
        let topic = request.topic.map(str::to_string);
        let closure_topic = topic.clone();
        let actor_path = caller.actor_path.clone();
        self.owners
            .db()
            .write_blocking(move |conn| {
                dbteam::board_channel_insert(
                    conn,
                    &dbteam::BoardChannelRow {
                        channel_id: closure_channel_id,
                        root_session_id: caller.root_session_id.clone(),
                        name: closure_name,
                        topic: closure_topic,
                        created_by: actor_path,
                        created_at_ms: now,
                    },
                )
            })
            .map_err(|error| format!("board_channel_create failed: {error}"))?;
        Ok(BoardChannel {
            channel_id: channel_id.as_str().to_string(),
            name,
            topic,
            created_by: caller.actor_path,
            created_at_ms: now,
        })
    }

    fn board_thread_create(
        &self,
        request: BoardThreadCreateRequest<'_>,
    ) -> Result<BoardThread, String> {
        if let Some(task_id) = request.task_id {
            self.require_task_exists(request.caller_session_id, task_id)?;
        }
        let caller = self.caller_context(request.caller_session_id)?;
        let thread_id = ThreadId::generate();
        let closure_thread_id = thread_id.as_str().to_string();
        let now = unix_ms();
        let channel_id = request.channel_id.to_string();
        let closure_channel_id = channel_id.clone();
        let title = request.title.to_string();
        let closure_title = title.clone();
        let task_id = request.task_id.map(str::to_string);
        let closure_task_id = task_id.clone();
        let actor_path = caller.actor_path.clone();
        self.owners
            .db()
            .write_blocking(move |conn| {
                dbteam::board_thread_insert(
                    conn,
                    &dbteam::BoardThreadRow {
                        thread_id: closure_thread_id,
                        root_session_id: caller.root_session_id.clone(),
                        channel_id: closure_channel_id,
                        title: closure_title,
                        task_id: closure_task_id,
                        created_by: actor_path,
                        created_at_ms: now,
                        post_count: 0,
                    },
                )
            })
            .map_err(|error| format!("board_thread_create failed: {error}"))?;
        Ok(BoardThread {
            thread_id: thread_id.as_str().to_string(),
            channel_id,
            title,
            task_id,
            created_by: caller.actor_path,
            created_at_ms: now,
            post_count: 0,
        })
    }

    fn board_post(&self, request: BoardPostRequest<'_>) -> Result<BoardPostOutcome, String> {
        let caller = self.caller_context(request.caller_session_id)?;
        let post_id = PostId::generate();
        let now = unix_ms();
        let thread_id = request.thread_id.to_string();
        let closure_thread_id = thread_id.clone();
        let task_id = request.task_id.map(str::to_string);
        let closure_task_id = task_id.clone();
        let body = request.body.to_string();
        let caller_session = request.caller_session_id.to_string();
        let outcome = {
            // 帖子线程归属 + 任务一致性在写事务里裁决;订阅目标在写后读取。
            self.owners
                .db()
                .write_blocking(move |conn| {
                    let threads = dbteam::board_threads(conn, &caller.root_session_id)?;
                    let thread = threads
                        .iter()
                        .find(|thread| thread.thread_id == closure_thread_id)
                        .ok_or_else(|| {
                            qaqh_session::db::DbError::Corrupt(format!(
                                "board thread not found: {}",
                                closure_thread_id
                            ))
                        })?
                        .clone();
                    let requested_task = closure_task_id.clone();
                    let task_id = match (&thread.task_id, requested_task) {
                        (Some(thread_task), Some(post_task)) if thread_task != &post_task => {
                            return Err(qaqh_session::db::DbError::Corrupt(format!(
                                "post task {post_task} does not match thread task {thread_task}"
                            )));
                        }
                        (Some(thread_task), _) => Some(thread_task.clone()),
                        (None, post_task) => post_task,
                    };
                    if let Some(task_id) = &task_id {
                        dbteam::task_get(conn, &caller.root_session_id, task_id)?
                            .ok_or_else(|| {
                                qaqh_session::db::DbError::Corrupt(format!(
                                    "task not found: {task_id}"
                                ))
                            })?;
                    }
                    let post = dbteam::BoardPostRow {
                        post_id: post_id.as_str().to_string(),
                        root_session_id: caller.root_session_id.clone(),
                        thread_id: thread.thread_id.clone(),
                        task_id,
                        author_path: caller.actor_path.clone(),
                        author_session_id: Some(caller.actor_session_id.clone()),
                        body: body.clone(),
                        reply_to: None,
                        created_at_ms: now,
                    };
                    dbteam::board_post_insert(conn, &post)?;
                    let subscriptions =
                        dbteam::board_subscriptions(conn, &caller.root_session_id)?;
                    let mut targets = std::collections::BTreeSet::new();
                    for subscription in subscriptions {
                        if !subscription.subscribed
                            || subscription.subscriber_path == caller.actor_path
                        {
                            continue;
                        }
                        let matches = match subscription.target_kind.as_str() {
                            "channel" => subscription.target_id == thread.channel_id,
                            "thread" => subscription.target_id == thread.thread_id,
                            _ => false,
                        };
                        if matches {
                            targets.insert(subscription.subscriber_path);
                        }
                    }
                    Ok((post, targets.into_iter().collect::<Vec<_>>()))
                })
                .map_err(|error: qaqh_session::db::DbError| match error {
                    qaqh_session::db::DbError::Corrupt(message) => message,
                    other => format!("board_post failed: {other}"),
                })?
        };
        let (post, targets) = outcome;
        let post_dto = board_post_to_dto(&post);
        let (notified, skipped) =
            self.notify_board_post(&caller_session, &post_dto, targets);
        Ok(BoardPostOutcome {
            post: post_dto,
            notified,
            skipped,
        })
    }

    fn board_subscribe(
        &self,
        request: BoardSubscriptionRequest<'_>,
    ) -> Result<BoardSubscription, String> {
        let caller = self.caller_context(request.caller_session_id)?;
        let target_kind = match request.target_kind {
            BoardSubscriptionTargetKind::Channel => "channel",
            BoardSubscriptionTargetKind::Thread => "thread",
        };
        let target_id = request.target_id.to_string();
        let subscribed = request.action == BoardSubscriptionAction::Subscribe;
        let now = unix_ms();
        let root = caller.root_session_id.clone();
        let actor_path = caller.actor_path.clone();
        self.owners
            .db()
            .write_blocking(move |conn| {
                dbteam::board_subscription_set(
                    conn,
                    &dbteam::BoardSubscriptionRow {
                        root_session_id: root,
                        target_kind: target_kind.to_string(),
                        target_id,
                        subscriber_path: actor_path,
                        subscribed,
                        updated_at_ms: now,
                    },
                )
            })
            .map_err(|error| format!("board_subscribe failed: {error}"))?;
        Ok(BoardSubscription {
            target: match request.target_kind {
                BoardSubscriptionTargetKind::Channel => {
                    qaqh_subagent::BoardSubscriptionTarget::Channel {
                        channel_id: request.target_id.to_string(),
                    }
                }
                BoardSubscriptionTargetKind::Thread => {
                    qaqh_subagent::BoardSubscriptionTarget::Thread {
                        thread_id: request.target_id.to_string(),
                    }
                }
            },
            subscriber: caller.actor_path,
            subscribed,
            updated_at_ms: now,
        })
    }

    fn board_list(&self, request: BoardListRequest<'_>) -> Result<BoardSnapshot, String> {
        let caller = self.caller_context(request.caller_session_id)?;
        let snapshot = self.board_rows(&caller.root_session_id)?;
        board_snapshot_view(
            &snapshot,
            request.channel_id,
            request.thread_id,
            request.include_posts,
            request.post_limit,
        )
    }
}

impl QaqhService {
    /// Wait until every target agent has a recorded terminal outcome.
    ///
    /// This is the `wait_agent(targets=...)` path and the replacement for
    /// `process wait {id}` on subagents. The completion test is a pure
    /// in-memory read of the result plane (`subagent_outcomes`) — it never
    /// reads canonical logs, so a long wait costs no I/O. A target that is not
    /// a subagent (e.g. the root) simply never satisfies the test and the call
    /// returns `TimedOut`.
    fn wait_for_agent_outcomes(
        &self,
        caller_session_id: &str,
        timeout: std::time::Duration,
        should_cancel: &dyn Fn() -> bool,
        targets: &[String],
    ) -> Result<WaitAgentOutcome, String> {
        let agent_ids = {
            let mut registry = self.registry()?;
            let mut ids = Vec::with_capacity(targets.len());
            for target in targets {
                ids.push(
                    registry
                        .resolve_agent_for_caller(caller_session_id, target)?
                        .agent_id
                        .as_str()
                        .to_string(),
                );
            }
            ids
        };
        let deadline = Instant::now() + timeout;
        loop {
            if should_cancel() {
                return Ok(WaitAgentOutcome::Cancelled);
            }
            // 锁必须在 sleep 之前释放：`finish_subagent` 由 collector 线程持锁写入。
            let all_finished = {
                let registry = self.registry()?;
                agent_ids.iter().all(|id| registry.has_subagent_outcome(id))
            };
            if all_finished {
                return Ok(WaitAgentOutcome::Activity {
                    activity_fact_seq: 0,
                });
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(WaitAgentOutcome::TimedOut {
                    activity_fact_seq: 0,
                });
            }
            std::thread::sleep(
                deadline
                    .saturating_duration_since(now)
                    .min(Duration::from_millis(25)),
            );
        }
    }

    fn caller_context(&self, caller_session_id: &str) -> Result<CallerContext, String> {
        let caller = self
            .registry()?
            .agent_metadata(caller_session_id)
            .ok_or_else(|| format!("caller agent metadata missing for {caller_session_id}"))?;
        Ok(CallerContext {
            root_session_id: caller.root_session_id.as_str().to_string(),
            actor_path: caller.agent_path.as_str().to_string(),
            actor_session_id: caller.agent_id.as_str().to_string(),
        })
    }

    fn task_get(&self, caller_session_id: &str, task_id: &str) -> Result<TaskBoardTask, String> {
        let caller = self.caller_context(caller_session_id)?;
        let owned_task = task_id.to_string();
        let task = self
            .owners
            .db()
            .read_blocking(move |conn| dbteam::task_get(conn, &caller.root_session_id, &owned_task))
            .map_err(|error| format!("task read failed: {error}"))?;
        task.as_ref()
            .map(task_to_dto)
            .ok_or_else(|| format!("task not found: {task_id}"))
    }

    /// Snapshot the caller's root task board for the daemon team endpoint.
    #[doc(hidden)]
    pub fn task_board_snapshot(
        &self,
        caller_session_id: &str,
    ) -> Result<qaqh_ringing_contract::team::TaskBoardSnapshot, String> {
        let caller = self.caller_context(caller_session_id)?;
        let root_for_snapshot = caller.root_session_id.clone();
        let tasks = self
            .owners
            .db()
            .read_blocking(move |conn| dbteam::task_list(conn, &caller.root_session_id, None))
            .map_err(|error| format!("task board snapshot failed: {error}"))?;
        Ok(qaqh_ringing_contract::team::TaskBoardSnapshot {
            team_id: Some(qaqh_ringing_contract::team::TeamId::new(
                qaqh_ringing_contract::fact::SessionId::new(root_for_snapshot),
            )),
            revision: 0,
            last_fact_seq: 0,
            tasks: tasks.iter().map(task_view_of_row).collect(),
        })
    }

    fn require_task_exists(&self, caller_session_id: &str, task_id: &str) -> Result<(), String> {
        self.task_get(caller_session_id, task_id).map(|_| ())
    }

    /// Snapshot the caller's root message board for the daemon team endpoint.
    #[doc(hidden)]
    pub fn board_snapshot(&self, caller_session_id: &str) -> Result<TeamBoardSnapshot, String> {
        let caller = self.caller_context(caller_session_id)?;
        let snapshot = self.board_rows(&caller.root_session_id)?;
        Ok(board_rows_to_wire(&snapshot))
    }

    /// 一次读出 root 的 board 全量行(board_list / board_snapshot 共用)。
    fn board_rows(&self, root_session_id: &str) -> Result<BoardRows, String> {
        let root = root_session_id.to_string();
        self.owners
            .db()
            .read_blocking(move |conn| {
                Ok::<_, qaqh_session::db::DbError>(BoardRows {
                    channels: dbteam::board_channels(conn, &root)?,
                    threads: dbteam::board_threads(conn, &root)?,
                    posts: dbteam::board_posts(conn, &root)?,
                    subscriptions: dbteam::board_subscriptions(conn, &root)?,
                })
            })
            .map_err(|error| format!("board snapshot failed: {error}"))
    }

    fn notify_board_post(
        &self,
        caller_session_id: &str,
        post: &BoardPost,
        targets: Vec<String>,
    ) -> (Vec<String>, Vec<BoardNotificationSkip>) {
        let agents = match self.list_agents(caller_session_id, "/root") {
            Ok(agents) => agents,
            Err(error) => {
                return (
                    Vec::new(),
                    targets
                        .into_iter()
                        .map(|agent| BoardNotificationSkip {
                            agent,
                            reason: format!("list agents failed: {error}"),
                        })
                        .collect(),
                );
            }
        };
        let text = board_notification_text(post);
        let mut notified = Vec::new();
        let mut skipped = Vec::new();
        for target in targets {
            let Some(agent) = agents.iter().find(|agent| agent.agent_path == target) else {
                skipped.push(BoardNotificationSkip {
                    agent: target,
                    reason: "agent not found".to_string(),
                });
                continue;
            };
            if agent.status != ListedAgentStatus::Running {
                skipped.push(BoardNotificationSkip {
                    agent: target,
                    reason: "agent is not running".to_string(),
                });
                continue;
            }
            if agent.residency != ListedAgentResidency::Loaded {
                skipped.push(BoardNotificationSkip {
                    agent: target,
                    reason: "agent is not loaded".to_string(),
                });
                continue;
            }
            match self.send_agent_message(SendAgentMessageRequest {
                caller_session_id,
                target: &target,
                text: &text,
                delivery: qaqh_domain::InterAgentDelivery::Queue,
            }) {
                Ok(_) => notified.push(target),
                Err(error) => skipped.push(BoardNotificationSkip {
                    agent: target,
                    reason: error,
                }),
            }
        }
        (notified, skipped)
    }
}

fn task_to_dto(task: &dbteam::TeamTaskRow) -> TaskBoardTask {
    TaskBoardTask {
        task_id: task.task_id.clone(),
        title: task.title.clone(),
        state: task.state.clone(),
        owner: task.owner_path.clone(),
        claim_epoch: task.claim_epoch.max(0) as u64,
        depends_on: task.depends_on.clone(),
        artifacts: task
            .artifacts
            .iter()
            .map(|(content_id, media_type, added_at_ms)| TaskBoardArtifact {
                content_id: format!("sha256:{content_id}"),
                media_type: media_type.clone(),
                added_at_ms: *added_at_ms,
            })
            .collect(),
        acceptance: task.acceptance.clone(),
        result_ref: task.result_ref.clone(),
        created_at_ms: task.created_at_ms,
        updated_at_ms: task.updated_at_ms,
    }
}

fn task_view_of_row(task: &dbteam::TeamTaskRow) -> qaqh_ringing_contract::team::TaskView {
    qaqh_ringing_contract::team::TaskView {
        task_id: qaqh_ringing_contract::team::TaskId::new(task.task_id.clone()),
        title: task.title.clone(),
        description_ref: task
            .description_ref
            .as_deref()
            .map(|raw| qaqh_ringing_contract::fact::ContentRef::new(
                qaqh_ringing_contract::fact::ContentHash::new(raw),
            )),
        state: parse_task_state(&task.state)
            .unwrap_or(qaqh_ringing_contract::team::TaskState::Open),
        created_by: {
            let actor = qaqh_ringing_contract::team::TeamActor::new(
                qaqh_ringing_contract::fact::AgentPath::parse_absolute(&task.created_by)
                    .unwrap_or_else(|_| qaqh_ringing_contract::fact::AgentPath::root()),
                None,
            );
            actor
        },
        owner: task.owner_path.as_deref().map(|path| {
            qaqh_ringing_contract::team::TeamActor::new(
                qaqh_ringing_contract::fact::AgentPath::parse_absolute(path)
                    .unwrap_or_else(|_| qaqh_ringing_contract::fact::AgentPath::root()),
                None,
            )
        }),
        claim_epoch: task.claim_epoch.max(0) as u64,
        depends_on: task
            .depends_on
            .iter()
            .map(|task_id| qaqh_ringing_contract::team::TaskId::new(task_id.clone()))
            .collect(),
        artifacts: task
            .artifacts
            .iter()
            .map(|(content_id, media_type, added_at_ms)| qaqh_ringing_contract::team::TaskArtifact {
                artifact_ref: qaqh_ringing_contract::fact::ContentRef::new(
                    qaqh_ringing_contract::fact::ContentHash::new(format!("sha256:{content_id}")),
                ),
                media_type: media_type.clone(),
                added_at_ms: *added_at_ms,
            })
            .collect(),
        acceptance: task.acceptance.clone(),
        result_ref: task.result_ref.as_deref().map(|raw| {
            qaqh_ringing_contract::fact::ContentRef::new(qaqh_ringing_contract::fact::ContentHash::new(raw))
        }),
        created_at_ms: task.created_at_ms,
        updated_at_ms: task.updated_at_ms,
    }
}

fn board_snapshot_view(
    snapshot: &BoardRows,
    channel_id: Option<&str>,
    thread_id: Option<&str>,
    include_posts: bool,
    post_limit: Option<usize>,
) -> Result<BoardSnapshot, String> {
    let requested_channel = channel_id.map(str::to_string);
    let selected_thread = thread_id
        .map(|thread_id| {
            snapshot
                .threads
                .iter()
                .find(|thread| thread.thread_id == thread_id)
                .cloned()
                .ok_or_else(|| format!("board thread not found: {thread_id}"))
        })
        .transpose()?;
    if let (Some(channel_id), Some(thread)) = (&requested_channel, &selected_thread)
        && &thread.channel_id != channel_id
    {
        return Err(format!(
            "board thread {} does not belong to channel {}",
            thread.thread_id, channel_id
        ));
    }
    let selected_channel = requested_channel
        .clone()
        .or_else(|| selected_thread.as_ref().map(|thread| thread.channel_id.clone()));
    if let Some(channel_id) = &selected_channel
        && !snapshot
            .channels
            .iter()
            .any(|channel| &channel.channel_id == channel_id)
    {
        return Err(format!("board channel not found: {channel_id}"));
    }

    let channels = snapshot
        .channels
        .iter()
        .filter(|channel| {
            selected_channel
                .as_ref()
                .is_none_or(|selected| &channel.channel_id == selected)
        })
        .map(board_channel_to_dto)
        .collect::<Vec<_>>();
    let threads = snapshot
        .threads
        .iter()
        .filter(|thread| {
            selected_channel
                .as_ref()
                .is_none_or(|selected| &thread.channel_id == selected)
                && thread_id
                    .map(|selected| thread.thread_id == selected)
                    .unwrap_or(true)
        })
        .map(board_thread_to_dto)
        .collect::<Vec<_>>();

    let limit = match post_limit {
        Some(0) => return Err("board post_limit must be at least 1".to_string()),
        Some(limit) if limit > 200 => {
            return Err("board post_limit must be at most 200".to_string());
        }
        Some(limit) => limit,
        None => 50,
    };
    let thread_ids = threads
        .iter()
        .map(|thread| thread.thread_id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let mut posts = if include_posts {
        snapshot
            .posts
            .iter()
            .filter(|post| thread_ids.contains(post.thread_id.as_str()))
            .map(board_post_to_dto)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    if posts.len() > limit {
        posts.drain(0..posts.len() - limit);
    }

    let subscriptions = snapshot
        .subscriptions
        .iter()
        .filter(|subscription| match subscription.target_kind.as_str() {
            "channel" => selected_channel
                .as_ref()
                .is_none_or(|selected| &subscription.target_id == selected),
            "thread" => {
                thread_id
                    .map(|selected| subscription.target_id == selected)
                    .unwrap_or(true)
                    && selected_channel.as_ref().is_none_or(|selected| {
                        snapshot
                            .threads
                            .iter()
                            .any(|thread| {
                                &thread.thread_id == &subscription.target_id
                                    && &thread.channel_id == selected
                            })
                    })
            }
            _ => false,
        })
        .map(board_subscription_to_dto)
        .collect::<Vec<_>>();

    Ok(BoardSnapshot {
        board_id: Some(snapshot.channels.first().map_or_else(
            || "unset".to_string(),
            |channel| channel.root_session_id.clone(),
        )),
        revision: 0,
        last_fact_seq: 0,
        channels,
        threads,
        posts,
        subscriptions,
    })
}

fn board_channel_to_dto(channel: &dbteam::BoardChannelRow) -> BoardChannel {
    BoardChannel {
        channel_id: channel.channel_id.clone(),
        name: channel.name.clone(),
        topic: channel.topic.clone(),
        created_by: channel.created_by.clone(),
        created_at_ms: channel.created_at_ms,
    }
}

fn board_thread_to_dto(thread: &dbteam::BoardThreadRow) -> BoardThread {
    BoardThread {
        thread_id: thread.thread_id.clone(),
        channel_id: thread.channel_id.clone(),
        title: thread.title.clone(),
        task_id: thread.task_id.clone(),
        created_by: thread.created_by.clone(),
        created_at_ms: thread.created_at_ms,
        post_count: thread.post_count.max(0) as u64,
    }
}

fn board_post_to_dto(post: &dbteam::BoardPostRow) -> BoardPost {
    BoardPost {
        post_id: post.post_id.clone(),
        thread_id: post.thread_id.clone(),
        task_id: post.task_id.clone(),
        author: post.author_path.clone(),
        body: post.body.clone(),
        created_at_ms: post.created_at_ms,
        reply_to: post.reply_to.clone(),
    }
}

fn board_subscription_to_dto(
    subscription: &dbteam::BoardSubscriptionRow,
) -> BoardSubscription {
    let target = match subscription.target_kind.as_str() {
        "channel" => qaqh_subagent::BoardSubscriptionTarget::Channel {
            channel_id: subscription.target_id.clone(),
        },
        _ => qaqh_subagent::BoardSubscriptionTarget::Thread {
            thread_id: subscription.target_id.clone(),
        },
    };
    BoardSubscription {
        target,
        subscriber: subscription.subscriber_path.clone(),
        subscribed: subscription.subscribed,
        updated_at_ms: subscription.updated_at_ms,
    }
}

fn board_notification_text(post: &BoardPost) -> String {
    let task_id = post.task_id.as_deref().unwrap_or("-");
    format!(
        "[message_board]\npost_id: {}\nthread_id: {}\ntask_id: {}\n\n{}",
        post.post_id, post.thread_id, task_id, post.body
    )
}

fn board_rows_to_wire(snapshot: &BoardRows) -> TeamBoardSnapshot {
    TeamBoardSnapshot {
        board_id: snapshot
            .channels
            .first()
            .map(|channel| {
                qaqh_ringing_contract::fact::SessionId::new(channel.root_session_id.clone())
            }),
        revision: 0,
        last_fact_seq: 0,
        channels: snapshot
            .channels
            .iter()
            .map(|channel| TeamBoardChannel {
                channel_id: channel.channel_id.clone(),
                name: channel.name.clone(),
                topic: channel.topic.clone(),
                created_by: qaqh_ringing_contract::fact::AgentPath::parse_absolute(
                    &channel.created_by,
                )
                .unwrap_or_else(|_| qaqh_ringing_contract::fact::AgentPath::root()),
                created_at_ms: channel.created_at_ms,
            })
            .collect(),
        threads: snapshot
            .threads
            .iter()
            .map(|thread| TeamBoardThread {
                thread_id: thread.thread_id.clone(),
                channel_id: thread.channel_id.clone(),
                title: thread.title.clone(),
                task_id: thread.task_id.clone(),
                created_by: qaqh_ringing_contract::fact::AgentPath::parse_absolute(
                    &thread.created_by,
                )
                .unwrap_or_else(|_| qaqh_ringing_contract::fact::AgentPath::root()),
                created_at_ms: thread.created_at_ms,
                post_count: thread.post_count.max(0) as u64,
            })
            .collect(),
        posts: snapshot
            .posts
            .iter()
            .map(|post| TeamBoardPost {
                post_id: post.post_id.clone(),
                thread_id: post.thread_id.clone(),
                task_id: post.task_id.clone(),
                author: qaqh_ringing_contract::fact::AgentPath::parse_absolute(&post.author_path)
                    .unwrap_or_else(|_| qaqh_ringing_contract::fact::AgentPath::root()),
                body: post.body.clone(),
                created_at_ms: post.created_at_ms,
                reply_to: post.reply_to.clone(),
            })
            .collect(),
        subscriptions: snapshot
            .subscriptions
            .iter()
            .map(|subscription| TeamBoardSubscription {
                target: match subscription.target_kind.as_str() {
                    "channel" => TeamBoardSubscriptionTarget::Channel {
                        channel_id: subscription.target_id.clone(),
                    },
                    _ => TeamBoardSubscriptionTarget::Thread {
                        thread_id: subscription.target_id.clone(),
                    },
                },
                subscriber: qaqh_ringing_contract::fact::AgentPath::parse_absolute(
                    &subscription.subscriber_path,
                )
                .unwrap_or_else(|_| qaqh_ringing_contract::fact::AgentPath::root()),
                subscribed: subscription.subscribed,
                updated_at_ms: subscription.updated_at_ms,
            })
            .collect(),
    }
}

fn canonical_content_ref(raw: &str) -> Result<CanonicalContentRef, String> {
    let raw = raw.trim();
    let hex = raw.strip_prefix("sha256:").unwrap_or(raw);
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!(
            "content ref {raw:?} must be a 64-hex sha256 content id"
        ));
    }
    Ok(CanonicalContentRef::new(ContentHash::new(format!(
        "sha256:{}",
        hex.to_ascii_lowercase()
    ))))
}

fn task_state_wire(state: &qaqh_ringing_contract::team::TaskState) -> &'static str {
    match state {
        qaqh_ringing_contract::team::TaskState::Open => "open",
        qaqh_ringing_contract::team::TaskState::Claimed => "claimed",
        qaqh_ringing_contract::team::TaskState::Completed => "completed",
        qaqh_ringing_contract::team::TaskState::Closed => "closed",
        qaqh_ringing_contract::team::TaskState::Cancelled => "cancelled",
    }
}

fn parse_task_state(state: &str) -> Result<qaqh_ringing_contract::team::TaskState, String> {
    match state.trim().to_ascii_lowercase().as_str() {
        "open" => Ok(qaqh_ringing_contract::team::TaskState::Open),
        "claimed" => Ok(qaqh_ringing_contract::team::TaskState::Claimed),
        "completed" => Ok(qaqh_ringing_contract::team::TaskState::Completed),
        "closed" => Ok(qaqh_ringing_contract::team::TaskState::Closed),
        "cancelled" | "canceled" => Ok(qaqh_ringing_contract::team::TaskState::Cancelled),
        other => Err(format!("unknown task state {other:?}")),
    }
}

/// 会话事件 → 收集器事件。
///
/// 只翻译收集器关心的语义:终答正文(Answer 块 seal)与回合终态。
fn collector_event_from_session_event(
    event: &qaqh_ringing_contract::events::SessionEvent,
) -> Option<CollectorEvent> {
    use qaqh_ringing_contract::events::SessionEvent;
    use qaqh_ringing_contract::fact::InterruptReason;
    use qaqh_ringing_contract::state::{TimelineEntry, TurnOutcome};
    match event {
        SessionEvent::TimelineAppended {
            entry:
                TimelineEntry::AssistantBlock {
                    block_kind: AssistantBlockKind::Answer,
                    content,
                    ..
                },
        } => Some(CollectorEvent::AnswerSealed {
            text: match content {
                ContentValue::Inline { text } => Some(text.clone()),
                _ => None,
            },
            output_ref: None,
        }),
        SessionEvent::TurnEnded { outcome, .. } => Some(CollectorEvent::TurnFinished {
            failed: match outcome {
                TurnOutcome::Finished { error, .. } => error.is_some(),
                TurnOutcome::Interrupted { .. } => true,
            },
            cancelled: matches!(
                outcome,
                TurnOutcome::Interrupted {
                    reason: InterruptReason::CancelBeforeSeal,
                }
            ),
            error: match outcome {
                TurnOutcome::Finished {
                    error: Some(error), ..
                } => Some(format!("{}: {}", error.code, error.message)),
                TurnOutcome::Interrupted { reason } => Some(format!("{reason:?}")),
                _ => None,
            },
        }),
        _ => None,
    }
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn unix_ms() -> i64 {
    (nanos() / 1_000_000).min(i64::MAX as u128) as i64
}
