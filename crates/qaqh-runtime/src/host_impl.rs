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
use qaqh_session::canonical::{CanonicalSessionIdentity, CommittedFactReader, EVENTS_COMMIT_FILE};
use qaqh_session::projection::{MailboxProjection, Projection};
use qaqh_subagent::{
    ArmSubagentCollectorRequest, ContentRef, EventBatch, InterruptAgentRequest, InterruptedAgent,
    ListedAgent, SendAgentMessageRequest, SentAgentMessage, SpawnSubagentRequest, SpawnedSubagent,
    StartSubagentRequest, SubagentHost, WaitAgentOutcome, WaitAgentRequest,
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
        if request.text.len() > 8 * 1024 {
            return Err(format!(
                "agent message is {} bytes; maximum inline size is {}",
                request.text.len(),
                8 * 1024
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
                                if env.seed != seed {
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
        seed: env.seed.clone(),
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
