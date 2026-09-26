//! `SubagentHost` 的进程内实现（Knife-1 step-2 收尾）。
//!
//! 主 session 与子代理 loop 均作为 daemon 线程内 in-process actor 运行
//! （PR #17/#19/#21），`spawn_subagent` 工具 handler 也在 daemon 进程内
//! 执行——此前它经 daemon HTTP/SSE 回连自己，属于多余的进程内回环。
//!
//! 本模块让 `QaqhService` 直接实现 [`qaqh_subagent::SubagentHost`]：
//! 工具 handler 通过该宿主句柄直达进程内 `AgentRegistry` + `RingingHub`，
//! 不再建立 HTTP/SSE 连接。事件订阅直接走 hub 进程内 broadcast。

use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use qaqh_domain::RingingChannel;
use qaqh_ringing::{RingingEventEnvelope, RingingWorkerCommandEnvelope};
use qaqh_session::canonical::{
    CanonicalSessionIdentity, CommittedFactReader, EVENTS_COMMIT_FILE, generate_ulid,
};
use qaqh_session::projection::{MailboxProjection, Projection};
use qaqh_session::session_fact_v2::{
    AgentPath, ContentHash, ContentRef as CanonicalContentRef, EventId, LogId, SessionId,
    TeamBoardChannel, TeamBoardPost, TeamBoardSnapshot, TeamBoardSubscription,
    TeamBoardSubscriptionTarget, TeamBoardThread, TeamTaskArtifact, TeamTaskSnapshot,
};
use qaqh_session::team::{
    BoardFact, BoardId, BoardPayload, BoardStore, BoardSubscriptionTarget, ChannelCreated,
    ChannelId, PostCreated, PostId, SubscriptionChanged, TaskAcceptanceSet, TaskArtifactAttached,
    TaskCancelled, TaskClaimed, TaskClosed, TaskCompleted, TaskCreated, TaskDependencyAdded,
    TaskId, TaskReleased, TaskState, TeamActor, TeamCreated, TeamFact, TeamId, TeamPayload,
    TeamStore, ThreadCreated, ThreadId, new_board_schema, new_team_schema,
};
use qaqh_subagent::{
    ArmSubagentCollectorRequest, BoardChannel, BoardChannelCreateRequest, BoardHost,
    BoardListRequest, BoardNotificationSkip, BoardPost, BoardPostOutcome, BoardPostRequest,
    BoardSnapshot, BoardSubscription, BoardSubscriptionAction, BoardSubscriptionRequest,
    BoardSubscriptionTargetKind, BoardThread, BoardThreadCreateRequest, ContentRef, EventBatch,
    InterruptAgentRequest, InterruptedAgent, ListedAgent, ListedAgentResidency, ListedAgentStatus,
    SendAgentMessageRequest, SentAgentMessage, SpawnSubagentRequest, SpawnedSubagent,
    StartSubagentRequest, SubagentHost, TaskBoardArtifact, TaskBoardHost, TaskBoardTask,
    TaskClaimAction, TaskClaimRequest, TaskCloseAction, TaskCloseRequest, TaskCreateRequest,
    TaskListRequest, TaskUpdateAction, TaskUpdateRequest, WaitAgentOutcome, WaitAgentRequest,
};

use super::QaqhService;

impl QaqhService {
    /// Read mailbox activity strictly after `after_fact_seq`.
    ///
    /// Returns `(mailbox_activity_fact_seq, committed_fact_seq)`. Reopening the
    /// reader each poll intentionally follows the durable commit marker rather
    /// than an in-memory cache, so queue-only delivery is observable even when
    /// the caller is blocked inside a tool.
    fn mailbox_activity_after(
        &self,
        session_id: &str,
        after_fact_seq: Option<u64>,
    ) -> Result<(u64, u64), String> {
        let session_dir = self
            .sessions
            .session_dir_for_id(session_id)?
            .ok_or_else(|| format!("mailbox session {session_id} has no canonical directory"))?;
        let identity = CanonicalSessionIdentity::open(&session_dir)
            .map_err(|error| format!("open canonical identity for {session_id}: {error}"))?;
        if identity.session_id.as_str() != session_id {
            return Err(format!(
                "mailbox session {session_id} identity is {}",
                identity.session_id
            ));
        }
        // A freshly allocated session can exist before its actor appends the
        // first canonical fact. Treat that as an empty mailbox rather than a
        // recovery error; the next poll will observe the newly committed marker.
        if !session_dir.join(EVENTS_COMMIT_FILE).exists() {
            return Ok((0, after_fact_seq.unwrap_or(0)));
        }
        let reader = CommittedFactReader::open(
            &session_dir,
            identity.session_id.clone(),
            identity.log_id.clone(),
        )
        .map_err(|error| format!("open canonical facts for {session_id}: {error}"))?;
        let facts = match after_fact_seq {
            Some(fact_seq) => reader
                .read_after(fact_seq)
                .map_err(|error| format!("read canonical facts for {session_id}: {error}"))?,
            None => reader
                .read_all()
                .map_err(|error| format!("read canonical facts for {session_id}: {error}"))?,
        };
        let mut mailbox = MailboxProjection::default();
        for fact in &facts {
            mailbox.apply(fact);
        }
        Ok((
            mailbox.last_activity_fact_seq(),
            reader.committed().committed_fact_seq,
        ))
    }

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
        author: &str,
        recipient: &str,
    ) -> Result<u64, String> {
        let Some(session_dir) = self.sessions.session_dir_for_id(target_session_id)? else {
            return Ok(0);
        };
        if !session_dir.join(EVENTS_COMMIT_FILE).exists() {
            return Ok(0);
        }
        let identity = CanonicalSessionIdentity::open(&session_dir)
            .map_err(|error| format!("open canonical identity for {target_session_id}: {error}"))?;
        if identity.session_id.as_str() != target_session_id {
            return Err(format!(
                "mailbox session {target_session_id} identity is {}",
                identity.session_id
            ));
        }
        let reader = CommittedFactReader::open(
            &session_dir,
            identity.session_id.clone(),
            identity.log_id.clone(),
        )
        .map_err(|error| format!("open canonical facts for {target_session_id}: {error}"))?;
        let facts = reader
            .read_all()
            .map_err(|error| format!("read canonical facts for {target_session_id}: {error}"))?;
        let mut mailbox = MailboxProjection::default();
        for fact in &facts {
            mailbox.apply(fact);
        }
        Ok(mailbox
            .pending()
            .filter(|message| {
                message.communication.author.as_str() == author
                    && message.communication.recipient.as_str() == recipient
            })
            .count() as u64)
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
        } = request;
        // BUG-2026-09-13-24 + BETA-01：子代理也先分配 canonical identity，
        // 目录名与 `child_session_id` 必须是同一个 UUID。子代理目录保持
        // unindexed，由 V2 residency 决定生命周期。
        let identity = self
            .sessions
            .allocate_agent_session(workspace.filter(|w| !w.is_empty() && *w != "."))
            .map_err(|error| format!("allocate child session failed: {error}"))?;
        let seed = identity.session_id.as_str().to_string();
        if let Some(workspace) = workspace.filter(|w| !w.is_empty() && *w != ".") {
            log::info!("[SUBAGENT-HOST] inherited workspace for seed={seed}: {workspace}");
        }
        let spawned = self.registry()?.spawn_subagent_v2(
            &seed,
            parent_session_id,
            requested_name,
            crate::registry::SubagentSpawnOptions {
                tools,
                model,
                base_url,
                max_tokens,
                ephemeral: false,
            },
        )?;
        log::info!(
            "[SUBAGENT-HOST] spawned subagent seed={seed} path={} tools={}",
            spawned.child_agent_path,
            tools.len()
        );
        Ok(SpawnedSubagent {
            seed,
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

        let message_id = format!("msg_{}", qaqh_session::canonical::generate_ulid());
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
                    seed: target.agent_id.as_str(),
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
        } = request;
        let (baseline_activity, mut cursor) =
            self.mailbox_activity_after(caller_session_id, None)?;
        let deadline = Instant::now() + timeout;
        loop {
            if should_cancel() {
                return Ok(WaitAgentOutcome::Cancelled);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(WaitAgentOutcome::TimedOut {
                    activity_fact_seq: baseline_activity,
                });
            }
            let remaining = deadline.saturating_duration_since(now);
            std::thread::sleep(remaining.min(Duration::from_millis(25)));

            let (activity_fact_seq, committed_fact_seq) =
                self.mailbox_activity_after(caller_session_id, Some(cursor))?;
            cursor = committed_fact_seq;
            if activity_fact_seq > baseline_activity {
                return Ok(WaitAgentOutcome::Activity { activity_fact_seq });
            }
        }
    }

    fn interrupt_agent(
        &self,
        request: InterruptAgentRequest<'_>,
    ) -> Result<InterruptedAgent, String> {
        let mut registry = self.registry()?;
        let caller = registry
            .list_agents_for_caller(request.caller_session_id, "/root")
            .and_then(|agents| {
                agents
                    .into_iter()
                    .find(|agent| agent.agent_id.as_str() == request.caller_session_id)
                    .ok_or_else(|| {
                        format!(
                            "caller agent metadata missing for {}",
                            request.caller_session_id
                        )
                    })
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
            message_id: format!("msg_{}", qaqh_session::canonical::generate_ulid()),
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
        self.registry()?.mark_collector_armed(&child_session_id);
        Ok(())
    }

    fn rollback_subagent(&self, seed: &str, child_session_id: &str, process_id: u32) {
        qaqh_workspace::process_registry::ProcessRegistry::set_answer(
            process_id,
            "[ABORTED] canonical SubagentSpawned edge was not committed".to_string(),
        );
        qaqh_workspace::process_registry::ProcessRegistry::mark_exited(process_id, 1);
        match self.registry() {
            Ok(mut registry) => registry.rollback_subagent(seed, child_session_id),
            Err(error) => {
                log::warn!("[SUBAGENT-HOST] rollback registry unavailable for {seed}: {error}");
                let _ = self.close(seed);
            }
        }
    }

    fn abort_subagent(&self, seed: &str, process_id: u32) {
        qaqh_workspace::process_registry::ProcessRegistry::set_answer(
            process_id,
            "[ABORTED] subagent closed after canonical spawn edge".to_string(),
        );
        qaqh_workspace::process_registry::ProcessRegistry::mark_exited(process_id, 1);
        if let Err(error) = self.close(seed) {
            log::warn!("[SUBAGENT-HOST] abort close failed for {seed}: {error}");
        }
    }

    fn send_ringing(
        &self,
        seed: &str,
        command: qaqh_ringing::RingingCommand,
    ) -> Result<(), String> {
        let id = format!("host-{:x}", nanos());
        let env = RingingWorkerCommandEnvelope::new(seed, id, command);
        self.registry()?.send_ringing(seed, &env)
    }

    fn subscribe(&self, seed: &str) -> mpsc::Receiver<EventBatch> {
        let (tx, rx) = mpsc::channel::<EventBatch>();
        let hub = match self.hub.get() {
            Some(hub) => hub.clone(),
            None => {
                log::error!("[SUBAGENT-HOST] subscribe {seed}: Ringing hub not attached");
                return rx;
            }
        };
        let epoch = hub.epoch().to_string();
        let seed_own = seed.to_string();
        for channel in [
            RingingChannel::Control,
            RingingChannel::Conversation,
            RingingChannel::Tool,
        ] {
            // BUG-2026-09-12-12：按 (channel, seed) 分片订阅——桥接只需本
            // seed 的事件，分片订阅既省掉每事件的 seed 过滤，也不再被其它
            // 会话的风暴推向 Lagged。
            let mut hub_rx = hub.subscribe(channel, &seed_own);
            let tx = tx.clone();
            let seed = seed_own.clone();
            let epoch = epoch.clone();
            std::thread::Builder::new()
                .name(format!("qaqh-subagent-sub-{seed_own}"))
                .spawn(move || {
                    // broadcast::Receiver 非 Send… 但 tokio broadcast Receiver 是 Send。
                    // 用 try_recv 轮询（无 block_on 依赖），聚合到 std mpsc。
                    loop {
                        match hub_rx.try_recv() {
                            Ok(env) => {
                                if env.session_id != seed {
                                    continue;
                                }
                                let batch = envelope_to_batch(channel, env, &epoch);
                                if tx.send(batch).is_err() {
                                    break;
                                }
                            }
                            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                                std::thread::sleep(Duration::from_millis(20));
                            }
                            Err(_) => break, // Closed / Lagged：终止桥接
                        }
                    }
                })
                .ok();
        }
        rx
    }

    fn download_content(&self, seed: &str, reference: &ContentRef) -> Result<Vec<u8>, String> {
        let hub = self
            .hub
            .get()
            .ok_or_else(|| "Ringing hub not attached".to_string())?;
        let entry = hub
            .get_content(seed, &reference.content_id)
            .ok_or_else(|| format!("content {} not found", reference.content_id))?;
        let digest = {
            use sha2::Digest;
            let hash = sha2::Sha256::digest(&entry.bytes);
            hash.iter().map(|b| format!("{b:02x}")).collect::<String>()
        };
        if !digest.eq_ignore_ascii_case(&reference.sha256) {
            return Err(format!(
                "content digest mismatch for {}: expected {}, received {digest}",
                reference.content_id, reference.sha256
            ));
        }
        Ok(entry.bytes)
    }

    fn close(&self, seed: &str) -> Result<(), String> {
        // 与 daemon action `session.close`/`SessionClose` 拦截一致：关闭 registry
        // 实例并清理临时会话；结果已注入主会话 + 终态已回写注册表，残留不丢数据。
        self.close_session(seed, None)
    }
}

const MAX_TASKS_PER_ROOT: usize = 1024;

impl TaskBoardHost for QaqhService {
    fn task_create(&self, request: TaskCreateRequest<'_>) -> Result<TaskBoardTask, String> {
        if request.title.trim().is_empty() {
            return Err("task title must not be empty".to_string());
        }
        let (store, actor) = self.task_store_for_caller(request.caller_session_id)?;
        let description_ref = request
            .description_ref
            .map(canonical_content_ref)
            .transpose()?;
        let task_id = TaskId::generate();
        let mut store = store.lock().unwrap_or_else(|error| error.into_inner());
        if store.snapshot().tasks.len() >= MAX_TASKS_PER_ROOT {
            return Err(format!(
                "task board limit {MAX_TASKS_PER_ROOT} reached for this root tree"
            ));
        }
        let root = store.team_id().as_str().to_string();
        store
            .append(team_fact(
                &root,
                actor.clone(),
                TeamPayload::TaskCreated(TaskCreated {
                    task_id: task_id.clone(),
                    title: request.title.to_string(),
                    description_ref,
                    created_by: actor,
                    created_at_ms: unix_ms(),
                }),
            ))
            .map_err(|error| format!("task_create failed: {error}"))?;
        self.publish_task_change(&root, &store, task_id.as_str());
        find_task(&store, task_id.as_str())
    }

    fn task_claim(&self, request: TaskClaimRequest<'_>) -> Result<TaskBoardTask, String> {
        let (store, actor) = self.task_store_for_caller(request.caller_session_id)?;
        let task_id = TaskId::new(request.task_id);
        let mut store = store.lock().unwrap_or_else(|error| error.into_inner());
        let current = find_task(&store, task_id.as_str())?;
        let root = store.team_id().as_str().to_string();
        let payload = match request.action {
            TaskClaimAction::Claim => TeamPayload::TaskClaimed(TaskClaimed {
                task_id: task_id.clone(),
                owner: actor.clone(),
                claim_epoch: current.claim_epoch.saturating_add(1),
                claimed_at_ms: unix_ms(),
            }),
            TaskClaimAction::Release => TeamPayload::TaskReleased(TaskReleased {
                task_id: task_id.clone(),
                owner: actor.clone(),
                claim_epoch: current.claim_epoch,
                reason: request.reason.unwrap_or("released").to_string(),
                released_at_ms: unix_ms(),
            }),
        };
        store
            .append(team_fact(&root, actor, payload))
            .map_err(|error| format!("task_claim failed: {error}"))?;
        self.publish_task_change(&root, &store, task_id.as_str());
        find_task(&store, task_id.as_str())
    }

    fn task_update(&self, request: TaskUpdateRequest<'_>) -> Result<TaskBoardTask, String> {
        let (store, actor) = self.task_store_for_caller(request.caller_session_id)?;
        let task_id = TaskId::new(request.task_id);
        let mut store = store.lock().unwrap_or_else(|error| error.into_inner());
        let _ = find_task(&store, task_id.as_str())?;
        let root = store.team_id().as_str().to_string();
        let payload = match request.action {
            TaskUpdateAction::AddDependency => {
                let depends_on = request
                    .depends_on
                    .ok_or_else(|| "task_update add_dependency requires depends_on".to_string())?;
                TeamPayload::TaskDependencyAdded(TaskDependencyAdded {
                    task_id: task_id.clone(),
                    depends_on: TaskId::new(depends_on),
                    added_at_ms: unix_ms(),
                })
            }
            TaskUpdateAction::AttachArtifact => {
                let artifact_ref = request.artifact_ref.ok_or_else(|| {
                    "task_update attach_artifact requires artifact_ref".to_string()
                })?;
                let media_type = request
                    .media_type
                    .ok_or_else(|| "task_update attach_artifact requires media_type".to_string())?;
                TeamPayload::TaskArtifactAttached(TaskArtifactAttached {
                    task_id: task_id.clone(),
                    artifact_ref: canonical_content_ref(artifact_ref)?,
                    media_type: media_type.to_string(),
                    added_at_ms: unix_ms(),
                })
            }
            TaskUpdateAction::SetAcceptance => {
                let acceptance = request
                    .acceptance
                    .ok_or_else(|| "task_update set_acceptance requires acceptance".to_string())?;
                TeamPayload::TaskAcceptanceSet(TaskAcceptanceSet {
                    task_id: task_id.clone(),
                    acceptance: acceptance.to_vec(),
                    updated_at_ms: unix_ms(),
                })
            }
        };
        store
            .append(team_fact(&root, actor, payload))
            .map_err(|error| format!("task_update failed: {error}"))?;
        self.publish_task_change(&root, &store, task_id.as_str());
        find_task(&store, task_id.as_str())
    }

    fn task_close(&self, request: TaskCloseRequest<'_>) -> Result<TaskBoardTask, String> {
        let (store, actor) = self.task_store_for_caller(request.caller_session_id)?;
        let task_id = TaskId::new(request.task_id);
        let mut store = store.lock().unwrap_or_else(|error| error.into_inner());
        let current = find_task(&store, task_id.as_str())?;
        let root = store.team_id().as_str().to_string();
        let payload = match request.action {
            TaskCloseAction::Complete => TeamPayload::TaskCompleted(TaskCompleted {
                task_id: task_id.clone(),
                owner: actor.clone(),
                claim_epoch: current.claim_epoch,
                result_ref: request.result_ref.map(canonical_content_ref).transpose()?,
                completed_at_ms: unix_ms(),
            }),
            TaskCloseAction::Close => TeamPayload::TaskClosed(TaskClosed {
                task_id: task_id.clone(),
                closed_by: actor.clone(),
                closed_at_ms: unix_ms(),
            }),
            TaskCloseAction::Cancel => TeamPayload::TaskCancelled(TaskCancelled {
                task_id: task_id.clone(),
                cancelled_by: actor.clone(),
                reason: request.reason.unwrap_or("cancelled").to_string(),
                cancelled_at_ms: unix_ms(),
            }),
        };
        store
            .append(team_fact(&root, actor, payload))
            .map_err(|error| format!("task_close failed: {error}"))?;
        self.publish_task_change(&root, &store, task_id.as_str());
        find_task(&store, task_id.as_str())
    }

    fn task_list(&self, request: TaskListRequest<'_>) -> Result<Vec<TaskBoardTask>, String> {
        let (store, _actor) = self.task_store_for_caller(request.caller_session_id)?;
        let store = store.lock().unwrap_or_else(|error| error.into_inner());
        let state_filter = request.state.map(parse_task_state).transpose()?;
        Ok(store
            .snapshot()
            .tasks
            .iter()
            .filter(|task| state_filter.is_none_or(|state| task.state == state))
            .map(task_to_dto)
            .collect())
    }
}

impl BoardHost for QaqhService {
    fn board_channel_create(
        &self,
        request: BoardChannelCreateRequest<'_>,
    ) -> Result<BoardChannel, String> {
        let (store, actor) = self.board_store_for_caller(request.caller_session_id)?;
        let channel_id = ChannelId::generate();
        let mut store = store.lock().unwrap_or_else(|error| error.into_inner());
        let root = store.board_id().as_str().to_string();
        store
            .append(board_fact(
                &root,
                actor.clone(),
                BoardPayload::ChannelCreated(ChannelCreated {
                    channel_id: channel_id.clone(),
                    name: request.name.to_string(),
                    topic: request.topic.map(str::to_string),
                    created_by: actor,
                    created_at_ms: unix_ms(),
                }),
            ))
            .map_err(|error| format!("board_channel_create failed: {error}"))?;
        self.publish_board_change(&root, &store);
        find_board_channel(&store, channel_id.as_str())
    }

    fn board_thread_create(
        &self,
        request: BoardThreadCreateRequest<'_>,
    ) -> Result<BoardThread, String> {
        let task_id = request.task_id.map(TaskId::new);
        if let Some(task_id) = &task_id {
            self.require_task_exists(request.caller_session_id, task_id.as_str())?;
        }
        let (store, actor) = self.board_store_for_caller(request.caller_session_id)?;
        let thread_id = ThreadId::generate();
        let mut store = store.lock().unwrap_or_else(|error| error.into_inner());
        let root = store.board_id().as_str().to_string();
        store
            .append(board_fact(
                &root,
                actor.clone(),
                BoardPayload::ThreadCreated(ThreadCreated {
                    thread_id: thread_id.clone(),
                    channel_id: ChannelId::new(request.channel_id),
                    title: request.title.to_string(),
                    task_id,
                    created_by: actor,
                    created_at_ms: unix_ms(),
                }),
            ))
            .map_err(|error| format!("board_thread_create failed: {error}"))?;
        self.publish_board_change(&root, &store);
        find_board_thread(&store, thread_id.as_str())
    }

    fn board_post(&self, request: BoardPostRequest<'_>) -> Result<BoardPostOutcome, String> {
        let (store, actor) = self.board_store_for_caller(request.caller_session_id)?;
        let post_id = PostId::generate();
        let (post, targets) = {
            let mut store = store.lock().unwrap_or_else(|error| error.into_inner());
            let thread = store
                .snapshot()
                .threads
                .iter()
                .find(|thread| thread.thread_id.as_str() == request.thread_id)
                .cloned()
                .ok_or_else(|| format!("board thread not found: {}", request.thread_id))?;
            let requested_task = request.task_id.map(TaskId::new);
            let task_id = match (&thread.task_id, requested_task) {
                (Some(thread_task), Some(post_task)) if thread_task != &post_task => {
                    return Err(format!(
                        "post task {post_task} does not match thread task {thread_task}"
                    ));
                }
                (Some(thread_task), _) => Some(thread_task.clone()),
                (None, post_task) => post_task,
            };
            if let Some(task_id) = &task_id {
                self.require_task_exists(request.caller_session_id, task_id.as_str())?;
            }
            let root = store.board_id().as_str().to_string();
            store
                .append(board_fact(
                    &root,
                    actor.clone(),
                    BoardPayload::PostCreated(PostCreated {
                        post_id: post_id.clone(),
                        thread_id: thread.thread_id.clone(),
                        task_id: task_id.clone(),
                        author: actor.clone(),
                        body: request.body.to_string(),
                        created_at_ms: unix_ms(),
                        reply_to: None,
                    }),
                ))
                .map_err(|error| format!("board_post failed: {error}"))?;
            self.publish_board_change(&root, &store);
            let post = find_board_post(&store, post_id.as_str())?;
            let targets = board_notification_targets(
                &store.snapshot(),
                &thread.channel_id,
                &thread.thread_id,
                actor.agent_path.as_str(),
            );
            (post, targets)
        };
        let (notified, skipped) = self.notify_board_post(request.caller_session_id, &post, targets);
        Ok(BoardPostOutcome {
            post,
            notified,
            skipped,
        })
    }

    fn board_subscribe(
        &self,
        request: BoardSubscriptionRequest<'_>,
    ) -> Result<BoardSubscription, String> {
        let (store, actor) = self.board_store_for_caller(request.caller_session_id)?;
        let target = match request.target_kind {
            BoardSubscriptionTargetKind::Channel => BoardSubscriptionTarget::Channel {
                channel_id: ChannelId::new(request.target_id),
            },
            BoardSubscriptionTargetKind::Thread => BoardSubscriptionTarget::Thread {
                thread_id: ThreadId::new(request.target_id),
            },
        };
        let mut store = store.lock().unwrap_or_else(|error| error.into_inner());
        let root = store.board_id().as_str().to_string();
        store
            .append(board_fact(
                &root,
                actor.clone(),
                BoardPayload::SubscriptionChanged(SubscriptionChanged {
                    target: target.clone(),
                    subscriber: actor.clone(),
                    subscribed: request.action == BoardSubscriptionAction::Subscribe,
                    updated_at_ms: unix_ms(),
                }),
            ))
            .map_err(|error| format!("board_subscribe failed: {error}"))?;
        self.publish_board_change(&root, &store);
        find_board_subscription(&store, &target, actor.agent_path.as_str())
    }

    fn board_list(&self, request: BoardListRequest<'_>) -> Result<BoardSnapshot, String> {
        let (store, _actor) = self.board_store_for_caller(request.caller_session_id)?;
        let store = store.lock().unwrap_or_else(|error| error.into_inner());
        board_snapshot_view(
            &store.snapshot(),
            request.channel_id,
            request.thread_id,
            request.include_posts,
            request.post_limit,
        )
    }
}

impl QaqhService {
    fn task_store_for_caller(
        &self,
        caller_session_id: &str,
    ) -> Result<(Arc<std::sync::Mutex<TeamStore>>, TeamActor), String> {
        let (root_session_id, caller_actor) = {
            let registry = self.registry()?;
            let caller = registry
                .agent_metadata(caller_session_id)
                .ok_or_else(|| format!("caller agent metadata missing for {caller_session_id}"))?;
            let actor = TeamActor::new(caller.agent_path.clone(), Some(caller.agent_id.clone()));
            (caller.root_session_id.as_str().to_string(), actor)
        };
        let mut stores = self
            .task_stores
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(store) = stores.get(&root_session_id) {
            return Ok((Arc::clone(store), caller_actor));
        }
        let dir = self.sessions.data_dir().join("team").join(&root_session_id);
        let mut store = TeamStore::open_or_create(
            dir,
            TeamId::new(SessionId::new(root_session_id.clone())),
            unix_ms(),
        )
        .map_err(|error| format!("open task board for {root_session_id}: {error}"))?;
        if store.snapshot().team_id.is_none() {
            let root_actor = TeamActor::new(
                AgentPath::root(),
                Some(SessionId::new(root_session_id.clone())),
            );
            store
                .append(team_fact(
                    &root_session_id,
                    root_actor,
                    TeamPayload::TeamCreated(TeamCreated {
                        root_session_id: SessionId::new(root_session_id.clone()),
                        created_at_ms: unix_ms(),
                    }),
                ))
                .map_err(|error| format!("initialize task board for {root_session_id}: {error}"))?;
        }
        let store = Arc::new(std::sync::Mutex::new(store));
        stores.insert(root_session_id, Arc::clone(&store));
        Ok((store, caller_actor))
    }

    /// Snapshot the caller's root task board for the daemon team endpoint.
    #[doc(hidden)]
    pub fn task_board_snapshot(
        &self,
        caller_session_id: &str,
    ) -> Result<qaqh_session::team::TaskBoardSnapshot, String> {
        let (store, _actor) = self.task_store_for_caller(caller_session_id)?;
        let store = store.lock().unwrap_or_else(|error| error.into_inner());
        Ok(store.snapshot())
    }

    fn publish_task_change(&self, root_session_id: &str, store: &TeamStore, task_id: &str) {
        let snapshot = store.snapshot();
        let Some(task) = snapshot
            .tasks
            .iter()
            .find(|task| task.task_id.as_str() == task_id)
        else {
            return;
        };
        let Some(v2_hub) = self.v2_hub.get() else {
            return;
        };
        let session_dir = self.sessions.session_path_dir(root_session_id);
        if let Err(error) =
            v2_hub.publish_task_delta(&session_dir, root_session_id, task_view_to_wire(task))
        {
            log::warn!("[team] task delta publish failed for {root_session_id}: {error}");
        }
    }

    fn board_store_for_caller(
        &self,
        caller_session_id: &str,
    ) -> Result<(Arc<std::sync::Mutex<BoardStore>>, TeamActor), String> {
        let (root_session_id, caller_actor) = {
            let registry = self.registry()?;
            let caller = registry
                .agent_metadata(caller_session_id)
                .ok_or_else(|| format!("caller agent metadata missing for {caller_session_id}"))?;
            let actor = TeamActor::new(caller.agent_path.clone(), Some(caller.agent_id.clone()));
            (caller.root_session_id.as_str().to_string(), actor)
        };
        let mut stores = self
            .board_stores
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(store) = stores.get(&root_session_id) {
            return Ok((Arc::clone(store), caller_actor));
        }
        let dir = self
            .sessions
            .data_dir()
            .join("team")
            .join(&root_session_id)
            .join("board");
        let mut store = BoardStore::open_or_create(
            dir,
            BoardId::new(SessionId::new(root_session_id.clone())),
            unix_ms(),
        )
        .map_err(|error| format!("open message board for {root_session_id}: {error}"))?;
        if store.snapshot().board_id.is_none() {
            let root_actor = TeamActor::new(
                AgentPath::root(),
                Some(SessionId::new(root_session_id.clone())),
            );
            store
                .append(board_fact(
                    &root_session_id,
                    root_actor,
                    BoardPayload::BoardCreated(qaqh_session::team::BoardCreated {
                        root_session_id: SessionId::new(root_session_id.clone()),
                        created_at_ms: unix_ms(),
                    }),
                ))
                .map_err(|error| {
                    format!("initialize message board for {root_session_id}: {error}")
                })?;
        }
        let store = Arc::new(std::sync::Mutex::new(store));
        stores.insert(root_session_id, Arc::clone(&store));
        Ok((store, caller_actor))
    }

    fn require_task_exists(&self, caller_session_id: &str, task_id: &str) -> Result<(), String> {
        let (store, _actor) = self.task_store_for_caller(caller_session_id)?;
        let store = store.lock().unwrap_or_else(|error| error.into_inner());
        find_task(&store, task_id).map(|_| ())
    }

    /// Snapshot the caller's root message board for the daemon team endpoint.
    #[doc(hidden)]
    pub fn board_snapshot(&self, caller_session_id: &str) -> Result<TeamBoardSnapshot, String> {
        let (store, _actor) = self.board_store_for_caller(caller_session_id)?;
        let store = store.lock().unwrap_or_else(|error| error.into_inner());
        Ok(board_to_wire(&store.snapshot()))
    }

    fn publish_board_change(&self, root_session_id: &str, store: &BoardStore) {
        let Some(v2_hub) = self.v2_hub.get() else {
            return;
        };
        let session_dir = self.sessions.session_path_dir(root_session_id);
        if let Err(error) = v2_hub.publish_board_change(
            &session_dir,
            root_session_id,
            board_to_wire(&store.snapshot()),
        ) {
            log::warn!("[team] board delta publish failed for {root_session_id}: {error}");
        }
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

fn team_fact(root_session_id: &str, actor: TeamActor, payload: TeamPayload) -> TeamFact {
    TeamFact {
        schema: new_team_schema(),
        team_id: TeamId::new(SessionId::new(root_session_id)),
        // The store owns log identity and overwrites this placeholder.
        log_id: LogId::new(""),
        fact_seq: 0,
        event_id: EventId::new(generate_ulid()),
        ts_ms: unix_ms(),
        causation_id: None,
        actor,
        payload,
    }
}

fn board_fact(root_session_id: &str, actor: TeamActor, payload: BoardPayload) -> BoardFact {
    BoardFact {
        schema: new_board_schema(),
        board_id: BoardId::new(SessionId::new(root_session_id)),
        // The store owns log identity and overwrites this placeholder.
        log_id: LogId::new(""),
        fact_seq: 0,
        event_id: EventId::new(generate_ulid()),
        ts_ms: unix_ms(),
        causation_id: None,
        actor,
        payload,
    }
}

fn find_board_channel(store: &BoardStore, channel_id: &str) -> Result<BoardChannel, String> {
    store
        .snapshot()
        .channels
        .iter()
        .find(|channel| channel.channel_id.as_str() == channel_id)
        .map(board_channel_to_dto)
        .ok_or_else(|| format!("board channel not found: {channel_id}"))
}

fn find_board_thread(store: &BoardStore, thread_id: &str) -> Result<BoardThread, String> {
    store
        .snapshot()
        .threads
        .iter()
        .find(|thread| thread.thread_id.as_str() == thread_id)
        .map(board_thread_to_dto)
        .ok_or_else(|| format!("board thread not found: {thread_id}"))
}

fn find_board_post(store: &BoardStore, post_id: &str) -> Result<BoardPost, String> {
    store
        .snapshot()
        .posts
        .iter()
        .find(|post| post.post_id.as_str() == post_id)
        .map(board_post_to_dto)
        .ok_or_else(|| format!("board post not found: {post_id}"))
}

fn find_board_subscription(
    store: &BoardStore,
    target: &BoardSubscriptionTarget,
    subscriber: &str,
) -> Result<BoardSubscription, String> {
    store
        .snapshot()
        .subscriptions
        .iter()
        .find(|subscription| {
            &subscription.target == target
                && subscription.subscriber.agent_path.as_str() == subscriber
        })
        .map(board_subscription_to_dto)
        .ok_or_else(|| "board subscription not found after append".to_string())
}

fn board_snapshot_view(
    snapshot: &qaqh_session::team::BoardSnapshot,
    channel_id: Option<&str>,
    thread_id: Option<&str>,
    include_posts: bool,
    post_limit: Option<usize>,
) -> Result<BoardSnapshot, String> {
    let requested_channel = channel_id.map(ChannelId::new);
    let requested_thread = thread_id.map(ThreadId::new);
    let selected_thread = match &requested_thread {
        Some(thread_id) => Some(
            snapshot
                .threads
                .iter()
                .find(|thread| &thread.thread_id == thread_id)
                .cloned()
                .ok_or_else(|| format!("board thread not found: {thread_id}"))?,
        ),
        None => None,
    };
    if let (Some(channel_id), Some(thread)) = (&requested_channel, &selected_thread)
        && &thread.channel_id != channel_id
    {
        return Err(format!(
            "board thread {} does not belong to channel {}",
            thread.thread_id, channel_id
        ));
    }
    let selected_channel = requested_channel.clone().or_else(|| {
        selected_thread
            .as_ref()
            .map(|thread| thread.channel_id.clone())
    });
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
                && requested_thread
                    .as_ref()
                    .is_none_or(|selected| &thread.thread_id == selected)
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
        .filter(|subscription| match &subscription.target {
            BoardSubscriptionTarget::Channel { channel_id } => selected_channel
                .as_ref()
                .is_none_or(|selected| selected == channel_id),
            BoardSubscriptionTarget::Thread { thread_id } => {
                requested_thread
                    .as_ref()
                    .is_none_or(|selected| selected == thread_id)
                    && selected_channel.as_ref().is_none_or(|selected| {
                        snapshot.threads.iter().any(|thread| {
                            &thread.thread_id == thread_id && &thread.channel_id == selected
                        })
                    })
            }
        })
        .map(board_subscription_to_dto)
        .collect::<Vec<_>>();

    Ok(BoardSnapshot {
        board_id: snapshot.board_id.as_ref().map(|id| id.as_str().to_string()),
        revision: snapshot.revision,
        last_fact_seq: snapshot.last_fact_seq,
        channels,
        threads,
        posts,
        subscriptions,
    })
}

fn board_channel_to_dto(channel: &qaqh_session::team::BoardChannelView) -> BoardChannel {
    BoardChannel {
        channel_id: channel.channel_id.as_str().to_string(),
        name: channel.name.clone(),
        topic: channel.topic.clone(),
        created_by: channel.created_by.agent_path.as_str().to_string(),
        created_at_ms: channel.created_at_ms,
    }
}

fn board_thread_to_dto(thread: &qaqh_session::team::BoardThreadView) -> BoardThread {
    BoardThread {
        thread_id: thread.thread_id.as_str().to_string(),
        channel_id: thread.channel_id.as_str().to_string(),
        title: thread.title.clone(),
        task_id: thread
            .task_id
            .as_ref()
            .map(|task_id| task_id.as_str().to_string()),
        created_by: thread.created_by.agent_path.as_str().to_string(),
        created_at_ms: thread.created_at_ms,
        post_count: thread.post_count,
    }
}

fn board_post_to_dto(post: &qaqh_session::team::BoardPostView) -> BoardPost {
    BoardPost {
        post_id: post.post_id.as_str().to_string(),
        thread_id: post.thread_id.as_str().to_string(),
        task_id: post
            .task_id
            .as_ref()
            .map(|task_id| task_id.as_str().to_string()),
        author: post.author.agent_path.as_str().to_string(),
        body: post.body.clone(),
        created_at_ms: post.created_at_ms,
        reply_to: post
            .reply_to
            .as_ref()
            .map(|post_id| post_id.as_str().to_string()),
    }
}

fn board_subscription_to_dto(
    subscription: &qaqh_session::team::BoardSubscriptionView,
) -> BoardSubscription {
    let target = match &subscription.target {
        BoardSubscriptionTarget::Channel { channel_id } => {
            qaqh_subagent::BoardSubscriptionTarget::Channel {
                channel_id: channel_id.as_str().to_string(),
            }
        }
        BoardSubscriptionTarget::Thread { thread_id } => {
            qaqh_subagent::BoardSubscriptionTarget::Thread {
                thread_id: thread_id.as_str().to_string(),
            }
        }
    };
    BoardSubscription {
        target,
        subscriber: subscription.subscriber.agent_path.as_str().to_string(),
        subscribed: subscription.subscribed,
        updated_at_ms: subscription.updated_at_ms,
    }
}

fn board_notification_targets(
    snapshot: &qaqh_session::team::BoardSnapshot,
    channel_id: &ChannelId,
    thread_id: &ThreadId,
    author: &str,
) -> Vec<String> {
    let mut targets = std::collections::BTreeSet::new();
    for subscription in &snapshot.subscriptions {
        if !subscription.subscribed {
            continue;
        }
        let matches = match &subscription.target {
            BoardSubscriptionTarget::Channel {
                channel_id: subscribed,
            } => subscribed == channel_id,
            BoardSubscriptionTarget::Thread {
                thread_id: subscribed,
            } => subscribed == thread_id,
        };
        if matches && subscription.subscriber.agent_path.as_str() != author {
            targets.insert(subscription.subscriber.agent_path.as_str().to_string());
        }
    }
    targets.into_iter().collect()
}

fn board_notification_text(post: &BoardPost) -> String {
    let task_id = post.task_id.as_deref().unwrap_or("-");
    format!(
        "[message_board]\npost_id: {}\nthread_id: {}\ntask_id: {}\n\n{}",
        post.post_id, post.thread_id, task_id, post.body
    )
}

fn board_to_wire(snapshot: &qaqh_session::team::BoardSnapshot) -> TeamBoardSnapshot {
    TeamBoardSnapshot {
        board_id: snapshot.board_id.as_ref().map(|id| id.0.clone()),
        revision: snapshot.revision,
        last_fact_seq: snapshot.last_fact_seq,
        channels: snapshot
            .channels
            .iter()
            .map(|channel| TeamBoardChannel {
                channel_id: channel.channel_id.as_str().to_string(),
                name: channel.name.clone(),
                topic: channel.topic.clone(),
                created_by: channel.created_by.agent_path.clone(),
                created_at_ms: channel.created_at_ms,
            })
            .collect(),
        threads: snapshot
            .threads
            .iter()
            .map(|thread| TeamBoardThread {
                thread_id: thread.thread_id.as_str().to_string(),
                channel_id: thread.channel_id.as_str().to_string(),
                title: thread.title.clone(),
                task_id: thread
                    .task_id
                    .as_ref()
                    .map(|task_id| task_id.as_str().to_string()),
                created_by: thread.created_by.agent_path.clone(),
                created_at_ms: thread.created_at_ms,
                post_count: thread.post_count,
            })
            .collect(),
        posts: snapshot
            .posts
            .iter()
            .map(|post| TeamBoardPost {
                post_id: post.post_id.as_str().to_string(),
                thread_id: post.thread_id.as_str().to_string(),
                task_id: post
                    .task_id
                    .as_ref()
                    .map(|task_id| task_id.as_str().to_string()),
                author: post.author.agent_path.clone(),
                body: post.body.clone(),
                created_at_ms: post.created_at_ms,
                reply_to: post
                    .reply_to
                    .as_ref()
                    .map(|post_id| post_id.as_str().to_string()),
            })
            .collect(),
        subscriptions: snapshot
            .subscriptions
            .iter()
            .map(|subscription| TeamBoardSubscription {
                target: match &subscription.target {
                    BoardSubscriptionTarget::Channel { channel_id } => {
                        TeamBoardSubscriptionTarget::Channel {
                            channel_id: channel_id.as_str().to_string(),
                        }
                    }
                    BoardSubscriptionTarget::Thread { thread_id } => {
                        TeamBoardSubscriptionTarget::Thread {
                            thread_id: thread_id.as_str().to_string(),
                        }
                    }
                },
                subscriber: subscription.subscriber.agent_path.clone(),
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

fn find_task(store: &TeamStore, task_id: &str) -> Result<TaskBoardTask, String> {
    store
        .snapshot()
        .tasks
        .iter()
        .find(|task| task.task_id.as_str() == task_id)
        .map(task_to_dto)
        .ok_or_else(|| format!("task not found: {task_id}"))
}

fn task_to_dto(task: &qaqh_session::team::TaskView) -> TaskBoardTask {
    TaskBoardTask {
        task_id: task.task_id.as_str().to_string(),
        title: task.title.clone(),
        state: task_state_name(task.state).to_string(),
        owner: task
            .owner
            .as_ref()
            .map(|owner| owner.agent_path.as_str().to_string()),
        claim_epoch: task.claim_epoch,
        depends_on: task
            .depends_on
            .iter()
            .map(|task_id| task_id.as_str().to_string())
            .collect(),
        artifacts: task
            .artifacts
            .iter()
            .map(|artifact| TaskBoardArtifact {
                content_id: artifact.artifact_ref.hash().as_str().to_string(),
                media_type: artifact.media_type.clone(),
                added_at_ms: artifact.added_at_ms,
            })
            .collect(),
        acceptance: task.acceptance.clone(),
        result_ref: task
            .result_ref
            .as_ref()
            .map(|content_ref| content_ref.hash().as_str().to_string()),
        created_at_ms: task.created_at_ms,
        updated_at_ms: task.updated_at_ms,
    }
}

fn task_state_name(state: TaskState) -> &'static str {
    match state {
        TaskState::Open => "open",
        TaskState::Claimed => "claimed",
        TaskState::Completed => "completed",
        TaskState::Closed => "closed",
        TaskState::Cancelled => "cancelled",
    }
}

fn task_view_to_wire(task: &qaqh_session::team::TaskView) -> TeamTaskSnapshot {
    TeamTaskSnapshot {
        task_id: task.task_id.as_str().to_string(),
        title: task.title.clone(),
        state: task_state_name(task.state).to_string(),
        owner: task.owner.as_ref().map(|owner| owner.agent_path.clone()),
        claim_epoch: task.claim_epoch,
        depends_on: task
            .depends_on
            .iter()
            .map(|task_id| task_id.as_str().to_string())
            .collect(),
        artifacts: task
            .artifacts
            .iter()
            .map(|artifact| TeamTaskArtifact {
                content_ref: artifact.artifact_ref.clone(),
                media_type: artifact.media_type.clone(),
                added_at_ms: artifact.added_at_ms,
            })
            .collect(),
        acceptance: task.acceptance.clone(),
        result_ref: task.result_ref.clone(),
        created_at_ms: task.created_at_ms,
        updated_at_ms: task.updated_at_ms,
    }
}

fn parse_task_state(state: &str) -> Result<TaskState, String> {
    match state.trim().to_ascii_lowercase().as_str() {
        "open" => Ok(TaskState::Open),
        "claimed" => Ok(TaskState::Claimed),
        "completed" => Ok(TaskState::Completed),
        "closed" => Ok(TaskState::Closed),
        "cancelled" | "canceled" => Ok(TaskState::Cancelled),
        other => Err(format!("unknown task state {other:?}")),
    }
}

/// 把单条 hub 事件信封包装为规范 EventBatch（与 client 的 `envelope_to_batch` 同构）。
fn envelope_to_batch(
    channel: RingingChannel,
    env: RingingEventEnvelope,
    server_epoch: &str,
) -> EventBatch {
    let seq = env.stream_seq;
    EventBatch {
        schema: qaqh_ringing::protocol::RINGING_SCHEMA.to_string(),
        version: qaqh_ringing::protocol::RINGING_VERSION,
        channel,
        session_id: env.session_id.clone(),
        server_epoch: server_epoch.to_string(),
        from_stream_seq: seq,
        to_stream_seq: seq,
        envelopes: vec![env],
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
