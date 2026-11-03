//! Canonical Ringing v2 projection hub.
//!
//! The hub is installed as a [`ProjectionSink`]. It keeps a rebuildable
//! projection state per canonical session, publishes committed projection
//! events to live subscribers, and replays from the committed canonical log.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use qaqh_ringing::{
    CanonicalCursor, CursorToken, RingingV2Delivery, RingingV2EventEnvelope, RingingV2ResetReason,
    RingingV2ResetRequired, RingingV2StreamKey,
};
use qaqh_session::canonical::generate_ulid;
use qaqh_session::projection::{
    ControlInteractionState, ConversationTurnOutcome, Projection, ProjectionSet,
    ProjectionSetSnapshot, ProjectionSink, projection_events_for_fact,
    projection_replaceable_events_for_fact, replaceable_identity,
};
use qaqh_ringing_contract::team::{TeamAgentResidency, TeamBoardSnapshot, TeamDelta, TeamTaskSnapshot};
use qaqh_session::session_fact_v2::{
    ActivityState as FactActivityState, CompactStatus, ConversationDelta, Delivery, EventId,
    InteractionKind, InterruptReason, LogId, MetaDelta, OperationFailure, ProjectionEvent,
    ProjectionPayload, SessionFact, SessionId, StreamKey, TitleSource, ToolCallId, TurnId,
    TurnTerminal,
};
use tokio::sync::broadcast;

const LIVE_CAPACITY: usize = 1024;

pub type V2Envelope = RingingV2EventEnvelope<ProjectionPayload>;

/// 压缩终态的载荷（`ConversationDelta::CompactFinished`）。
///
/// 收敛成结构体而不是继续加形参：终态字段已经 6 个，逐条平铺既触发
/// 参数过多，也让"调用方漏传原因"变成默认合法。
#[derive(Debug, Clone)]
pub struct CompactFinishedView<'a> {
    pub compact_id: &'a str,
    pub status: CompactStatus,
    pub summary_chars: Option<usize>,
    pub turns_compacted: Option<u32>,
    pub turns_removed: Option<u32>,
    /// 失败 / 跳过 / 取消的中文原因；成功为 `None`。
    pub reason: Option<&'a str>,
}

#[derive(Debug)]
pub enum V2HubError {
    SessionMissing(String),
    SnapshotMissing(String),
    Canonical(String),
    InvalidCursor(String),
    InvalidEvent(String),
}

impl std::fmt::Display for V2HubError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionMissing(session_id) => {
                write!(formatter, "session {session_id} has no canonical log")
            }
            Self::SnapshotMissing(session_id) => {
                write!(
                    formatter,
                    "session {session_id} has no committed snapshot cursor"
                )
            }
            Self::Canonical(message) => write!(formatter, "canonical log error: {message}"),
            Self::InvalidCursor(message) => write!(formatter, "invalid v2 cursor: {message}"),
            Self::InvalidEvent(message) => {
                write!(formatter, "invalid v2 projection event: {message}")
            }
        }
    }
}

impl std::error::Error for V2HubError {}

#[derive(Debug, Clone)]
pub struct V2BootstrapSnapshot {
    pub server_epoch: String,
    pub session_id: String,
    pub log_id: LogId,
    pub snapshot_cursor: CursorToken,
    pub last_fact_seq: u64,
    pub projections: ProjectionSetSnapshot,
}

pub struct V2ProjectionHub {
    epoch: String,
    live_capacity: usize,
    sessions: RwLock<HashMap<SessionId, Arc<V2Session>>>,
}

struct V2Session {
    state: Mutex<V2SessionState>,
}

struct V2SessionState {
    last_fact_seq: u64,
    projections: ProjectionSet,
    /// Daemon-local residency overlays keyed by agent id. These deliberately do
    /// not persist across process restart: no worker survives the daemon.
    runtime_residency: HashMap<SessionId, TeamAgentResidency>,
    replaceables: BTreeMap<String, ProjectionEvent>,
    title_revision: u64,
    /// 配置变更广播的单调 revision（daemon 全局，按会话各记一份）。
    config_revision: u64,
    live_tx: broadcast::Sender<V2Envelope>,
    /// Monotonic revision for ephemeral task board deltas.
    task_revision: u64,
    /// Monotonic revision for ephemeral message board snapshot deltas.
    board_revision: u64,
    /// Monotonic revision shared by transient compaction lifecycle deltas.
    compact_revision: u64,
}

pub struct V2Subscription {
    replay: VecDeque<V2Envelope>,
    live_rx: broadcast::Receiver<V2Envelope>,
    server_epoch: String,
    session_id: String,
    log_id: LogId,
    initial_reset: Option<RingingV2ResetRequired>,
}

#[derive(Debug)]
pub enum V2StreamItem {
    Event(Box<V2Envelope>),
    Reset(RingingV2ResetRequired),
}

impl V2ProjectionHub {
    pub fn new(server_epoch: impl Into<String>) -> Self {
        Self {
            epoch: server_epoch.into(),
            live_capacity: LIVE_CAPACITY,
            sessions: RwLock::new(HashMap::new()),
        }
    }

    /// Test-only: shrink the per-session live broadcast capacity so the
    /// `replay_overflow` reset path is reachable without publishing thousands
    /// of events. Production always uses [`LIVE_CAPACITY`].
    #[doc(hidden)]
    pub fn with_live_capacity(server_epoch: impl Into<String>, live_capacity: usize) -> Self {
        Self {
            epoch: server_epoch.into(),
            live_capacity: live_capacity.max(1),
            sessions: RwLock::new(HashMap::new()),
        }
    }

    pub fn server_epoch(&self) -> &str {
        &self.epoch
    }

    /// Synthetic lifecycle test hook; does not expose subscriber contents.
    #[doc(hidden)]
    pub fn subscriber_count_for_test(&self, session_id: &SessionId) -> usize {
        self.sessions
            .read()
            .ok()
            .and_then(|sessions| sessions.get(session_id).cloned())
            .and_then(|session| {
                session
                    .state
                    .lock()
                    .ok()
                    .map(|state| state.live_tx.receiver_count())
            })
            .unwrap_or(0)
    }

    pub fn memory_components(&self) -> Vec<qaqh_memwatch::ComponentMemory> {
        let Ok(sessions) = self.sessions.read() else {
            return Vec::new();
        };
        let mut lower_bound = sessions
            .capacity()
            .saturating_mul(std::mem::size_of::<(SessionId, Arc<V2Session>)>());
        let mut overlay_items = 0_u64;
        let mut replaceable_items = 0_u64;
        let mut overlay_payload_bytes = 0_u64;
        let mut projection_payload_bytes = 0_u64;
        for session in sessions.values() {
            lower_bound = lower_bound.saturating_add(std::mem::size_of::<V2Session>());
            if let Ok(state) = session.state.lock() {
                lower_bound = lower_bound
                    .saturating_add(std::mem::size_of::<V2SessionState>())
                    .saturating_add(
                        state
                            .runtime_residency
                            .len()
                            .saturating_mul(std::mem::size_of::<(SessionId, TeamAgentResidency)>()),
                    )
                    .saturating_add(
                        state
                            .replaceables
                            .len()
                            .saturating_mul(std::mem::size_of::<(String, ProjectionEvent)>()),
                    );
                lower_bound = lower_bound.saturating_add(
                    state
                        .runtime_residency
                        .capacity()
                        .saturating_mul(std::mem::size_of::<(SessionId, TeamAgentResidency)>()),
                );
                for (id, residency) in &state.runtime_residency {
                    lower_bound = lower_bound.saturating_add(id.as_str().len());
                    overlay_payload_bytes = overlay_payload_bytes
                        .saturating_add(id.as_str().len() as u64)
                        .saturating_add(std::mem::size_of_val(residency) as u64);
                }
                for (key, event) in &state.replaceables {
                    let _ = event;
                    lower_bound = lower_bound.saturating_add(key.capacity());
                }
                let (projection_items, projection_bytes, projection_heap) =
                    state.projections.memory_usage_estimate();
                lower_bound = lower_bound.saturating_add(projection_heap as usize);
                projection_payload_bytes = projection_payload_bytes
                    .saturating_add(projection_bytes)
                    .saturating_add(projection_items);
                overlay_items = overlay_items.saturating_add(state.runtime_residency.len() as u64);
                replaceable_items =
                    replaceable_items.saturating_add(state.replaceables.len() as u64);
            }
        }
        vec![
            qaqh_memwatch::ComponentMemory {
                name: "ringing.v2.projection_sessions".into(),
                item_count: sessions.len() as u64,
                payload_bytes: Some(projection_payload_bytes),
                heap_estimate_bytes: Some(lower_bound as u64),
                ..Default::default()
            },
            qaqh_memwatch::ComponentMemory {
                name: "ringing.v2.runtime_overlays".into(),
                item_count: overlay_items,
                payload_bytes: Some(overlay_payload_bytes),
                heap_estimate_bytes: Some(overlay_payload_bytes),
                ..Default::default()
            },
            qaqh_memwatch::ComponentMemory {
                name: "ringing.v2.replaceable_events".into(),
                item_count: replaceable_items,
                payload_bytes: None,
                heap_estimate_bytes: None,
                ..Default::default()
            },
        ]
    }

    /// Install this hub as the process-wide canonical projection sink.
    pub fn install(self: &Arc<Self>) -> Result<(), Arc<dyn ProjectionSink>> {
        qaqh_session::projection::install_projection_sink(self.clone())
    }

    pub fn bootstrap(
        &self,
        session_dir: impl AsRef<Path>,
        session_id: &str,
    ) -> Result<V2BootstrapSnapshot, V2HubError> {
        let session_dir = session_dir.as_ref();
        let (canonical_session_id, log_id) = resolve_identity(session_dir, session_id)?;
        let session = self.session_for(session_dir, canonical_session_id, log_id.clone())?;
        let state = session
            .state
            .lock()
            .map_err(|_| V2HubError::Canonical("v2 session lock poisoned".into()))?;
        if state.last_fact_seq == 0 {
            return Err(V2HubError::SnapshotMissing(session_id.to_string()));
        }
        let cursor = CanonicalCursor::snapshot(log_id.as_str(), state.last_fact_seq);
        let snapshot_cursor = CursorToken::encode_snapshot(&cursor)
            .map_err(|error| V2HubError::InvalidCursor(error.to_string()))?;
        Ok(V2BootstrapSnapshot {
            server_epoch: self.epoch.clone(),
            session_id: session_id.to_string(),
            log_id,
            snapshot_cursor,
            last_fact_seq: state.last_fact_seq,
            projections: state.projections.snapshot(),
        })
    }

    /// Control-channel interaction states for a seed.
    ///
    /// Cheaper than [`Self::bootstrap`] because it clones only the control
    /// projection, not the conversation/timeline/resource snapshots. Used by
    /// the command path to detect an interaction that was already resolved.
    pub fn control_interactions(
        &self,
        session_dir: impl AsRef<Path>,
        session_id: &str,
    ) -> Result<Vec<ControlInteractionState>, V2HubError> {
        let session_dir = session_dir.as_ref();
        let (canonical_session_id, log_id) = resolve_identity(session_dir, session_id)?;
        let session = self.session_for(session_dir, canonical_session_id, log_id)?;
        let state = session
            .state
            .lock()
            .map_err(|_| V2HubError::Canonical("v2 session lock poisoned".into()))?;
        Ok(state.projections.control.snapshot().interactions)
    }

    /// 统一运行状态（`SessionRunStatus`，2026-10-06 归一裁决）——**只对已加载
    /// 会话调用**：首次访问会把该会话的 canonical fact log 从磁盘重放进投影，
    /// 对全量 `session.list` 逐条调用未加载会话会造成全库重放。
    ///
    /// 派生规则（优先级从高到低）：
    /// 1. 挂起交互（control 投影里未 resolve 且未 expire 的最后一条）→
    ///    `waiting_permission` / `waiting_ask` / `waiting_plan`——等待盖过
    ///    working，因为交互发生在回合中途；
    /// 2. control activity = `running` → `working`；
    /// 3. 最近一个回合的终态（conversation 投影按序保留 outcome）→
    ///    `Finished{cancelled}` → `canceled`，`Finished{failed}` → `error`，
    ///    `Interrupted`（crash/restart/unknown_fact 等非用户原因）→ `error`；
    /// 4. 其余（无回合 / 最近回合 completed）→ `idle`。
    pub fn projected_run_status(
        &self,
        session_dir: impl AsRef<Path>,
        session_id: &str,
    ) -> Result<qaqh_types::SessionRunStatus, V2HubError> {
        let session_dir = session_dir.as_ref();
        let (canonical_session_id, log_id) = resolve_identity(session_dir, session_id)?;
        let session = self.session_for(session_dir, canonical_session_id, log_id)?;
        let state = session
            .state
            .lock()
            .map_err(|_| V2HubError::Canonical("v2 session lock poisoned".into()))?;
        let control = state.projections.control.snapshot();
        if let Some(kind) = control
            .interactions
            .iter()
            .rev()
            .find(|i| i.resolution.is_none() && i.expired_reason.is_none())
            .map(|i| i.kind)
        {
            return Ok(match kind {
                InteractionKind::Permission => qaqh_types::SessionRunStatus::WaitingPermission,
                InteractionKind::Ask => qaqh_types::SessionRunStatus::WaitingAsk,
                InteractionKind::Plan => qaqh_types::SessionRunStatus::WaitingPlan,
            });
        }
        if control.activity == FactActivityState::Running {
            return Ok(qaqh_types::SessionRunStatus::Working);
        }
        let conversation = state.projections.conversation.snapshot();
        let last_outcome = conversation
            .turns
            .last()
            .and_then(|turn| turn.outcome.as_ref());
        Ok(match last_outcome {
            Some(ConversationTurnOutcome::Finished {
                terminal: TurnTerminal::Cancelled,
                ..
            }) => qaqh_types::SessionRunStatus::Canceled,
            Some(ConversationTurnOutcome::Finished {
                terminal: TurnTerminal::Failed,
                ..
            }) => qaqh_types::SessionRunStatus::Error,
            Some(ConversationTurnOutcome::Interrupted {
                reason: InterruptReason::CancelBeforeSeal,
                ..
            }) => qaqh_types::SessionRunStatus::Canceled,
            // cancel_before_seal 是用户取消，不是故障；crash/restart/unknown_fact 仍归 error。
            Some(ConversationTurnOutcome::Interrupted { .. }) => {
                qaqh_types::SessionRunStatus::Error
            }
            _ => qaqh_types::SessionRunStatus::Idle,
        })
    }

    /// Overlay daemon-local worker residency for one logical agent.
    ///
    /// The overlay is intentionally ephemeral: `loaded` is never written to
    /// the canonical log, so restart/bootstrap rebuilds as `unloaded` until a
    /// worker is actually resident again. A changed overlay is broadcast as a
    /// Team `AgentResidencyChanged` delta.
    pub fn set_team_residency(
        &self,
        session_dir: impl AsRef<Path>,
        session_id: &str,
        agent_id: &SessionId,
        residency: TeamAgentResidency,
    ) -> Result<(), V2HubError> {
        let session_dir = session_dir.as_ref();
        let (canonical_session_id, log_id) = resolve_identity(session_dir, session_id)?;
        let session = self.session_for(session_dir, canonical_session_id, log_id)?;
        let mut state = session
            .state
            .lock()
            .map_err(|_| V2HubError::Canonical("v2 session lock poisoned".into()))?;
        state.runtime_residency.insert(agent_id.clone(), residency);
        let Some(delta) = state
            .projections
            .team
            .apply_runtime_residency(agent_id, residency)
        else {
            return Ok(());
        };
        let last_fact_seq = state.last_fact_seq;
        if let Some(envelope) =
            ephemeral_team_envelope(&self.epoch, session_id, last_fact_seq, delta)
        {
            let _ = state.live_tx.send(envelope);
        }
        Ok(())
    }

    /// Broadcast an ephemeral task board delta on the root session's stream.
    ///
    /// The task board is a separate canonical aggregate, so its facts do not
    /// advance the session `last_fact_seq`. Clients recover the full board
    /// from the daemon team endpoint and apply these deltas afterwards.
    pub fn publish_task_delta(
        &self,
        session_dir: impl AsRef<Path>,
        session_id: &str,
        task: TeamTaskSnapshot,
    ) -> Result<(), V2HubError> {
        let session_dir = session_dir.as_ref();
        let (canonical_session_id, log_id) = resolve_identity(session_dir, session_id)?;
        let session = self.session_for(session_dir, canonical_session_id, log_id)?;
        let mut state = session
            .state
            .lock()
            .map_err(|_| V2HubError::Canonical("v2 session lock poisoned".into()))?;
        state.task_revision = state.task_revision.saturating_add(1);
        let delta = TeamDelta::TaskChanged {
            revision: state.task_revision,
            task: Box::new(task),
        };
        let last_fact_seq = state.last_fact_seq;
        if let Some(envelope) =
            ephemeral_team_envelope(&self.epoch, session_id, last_fact_seq, delta)
        {
            let _ = state.live_tx.send(envelope);
        }
        Ok(())
    }

    /// Broadcast an ephemeral message board snapshot on the root session's stream.
    ///
    /// The board is a separate canonical aggregate, so its facts do not advance
    /// the session `last_fact_seq`. Clients recover the full board from the
    /// daemon team endpoint and replace their local snapshot on this delta.
    pub fn publish_board_change(
        &self,
        session_dir: impl AsRef<Path>,
        session_id: &str,
        board: TeamBoardSnapshot,
    ) -> Result<(), V2HubError> {
        let session_dir = session_dir.as_ref();
        let (canonical_session_id, log_id) = resolve_identity(session_dir, session_id)?;
        let session = self.session_for(session_dir, canonical_session_id, log_id)?;
        let mut state = session
            .state
            .lock()
            .map_err(|_| V2HubError::Canonical("v2 session lock poisoned".into()))?;
        state.board_revision = state.board_revision.saturating_add(1);
        let delta = TeamDelta::BoardChanged {
            revision: state.board_revision,
            board: Box::new(board),
        };
        let last_fact_seq = state.last_fact_seq;
        if let Some(envelope) =
            ephemeral_team_envelope(&self.epoch, session_id, last_fact_seq, delta)
        {
            let _ = state.live_tx.send(envelope);
        }
        Ok(())
    }

    /// Publish an auto-title update on the v2 Meta stream. The title is already
    /// persisted in session metadata; this replaceable event makes connected
    /// clients update immediately and is replayed to reconnecting subscribers.
    pub fn publish_title_changed(
        &self,
        session_dir: impl AsRef<Path>,
        session_id: &str,
        title: String,
        source: TitleSource,
    ) -> Result<(), V2HubError> {
        let session_dir = session_dir.as_ref();
        let (canonical_session_id, log_id) = resolve_identity(session_dir, session_id)?;
        let session = self.session_for(session_dir, canonical_session_id, log_id)?;
        let mut state = session
            .state
            .lock()
            .map_err(|_| V2HubError::Canonical("v2 session lock poisoned".into()))?;
        let meta_revision = state.projections.meta.snapshot().revision;
        state.title_revision = state.title_revision.max(meta_revision).saturating_add(1);
        let revision = state.title_revision;
        let event_id = EventId::new(generate_ulid());
        let event = ProjectionEvent {
            event_id: event_id.clone(),
            source_fact_seq: state.last_fact_seq.max(1),
            source_event_id: event_id,
            causation_id: None,
            ts_ms: None,
            stream_key: StreamKey::Channel(qaqh_domain::RingingChannel::Control),
            delivery: Delivery::Replaceable { revision },
            projection_slot: None,
            projection_index: None,
            payload: ProjectionPayload::MetaDelta(MetaDelta::TitleChanged {
                revision,
                title,
                source,
            }),
        };
        state
            .replaceables
            .insert("meta:title".into(), event.clone());
        if let Some(envelope) = event_to_envelope(&self.epoch, session_id, &event) {
            let _ = state.live_tx.send(envelope);
        }
        Ok(())
    }

    /// 配置写后广播：给**每个驻留会话**发一条可替换的信号事件
    /// （`MetaDelta::ConfigChanged`），客户端收到后重拉 `config.load`。
    ///
    /// 为什么按会话广播：v2 只有按会话的流，而配置是 daemon 全局的；
    /// 每个会话都受同一份配置约束，所以「本会话的配置变了」对每个会话都成立。
    /// 未驻留的会话不在这里补，它们在下次 `subscribe` 时照常走 `config.load`。
    /// 只发信号不发内容：配置 DTO 由 `config.load` 独占定义，避免两份真相漂移。
    pub fn publish_config_changed(&self) -> Result<usize, V2HubError> {
        let sessions: Vec<(SessionId, Arc<V2Session>)> = self
            .sessions
            .read()
            .map_err(|_| V2HubError::Canonical("v2 hub lock poisoned".into()))?
            .iter()
            .map(|(id, session)| (id.clone(), session.clone()))
            .collect();
        let mut published = 0usize;
        for (session_id, session) in sessions {
            let mut state = session
                .state
                .lock()
                .map_err(|_| V2HubError::Canonical("v2 session lock poisoned".into()))?;
            state.config_revision = state.config_revision.saturating_add(1);
            let revision = state.config_revision;
            let event_id = EventId::new(generate_ulid());
            let event = ProjectionEvent {
                event_id: event_id.clone(),
                source_fact_seq: state.last_fact_seq.max(1),
                source_event_id: event_id,
                causation_id: None,
                ts_ms: None,
                stream_key: StreamKey::Channel(qaqh_domain::RingingChannel::Control),
                delivery: Delivery::Replaceable { revision },
                projection_slot: None,
                projection_index: None,
                payload: ProjectionPayload::MetaDelta(MetaDelta::ConfigChanged { revision }),
            };
            state
                .replaceables
                .insert("meta:config".into(), event.clone());
            if let Some(envelope) = event_to_envelope(&self.epoch, session_id.as_str(), &event) {
                let _ = state.live_tx.send(envelope);
            }
            published += 1;
        }
        Ok(published)
    }

    /// Publish a manual/auto context compaction start on the v2 Conversation
    /// stream. Mirrors `ConversationEvent::CompactStarted`; the card stays
    /// replayable until [`Self::publish_compact_finished`] clears the slot.
    pub fn publish_compact_started(
        &self,
        session_dir: impl AsRef<Path>,
        session_id: &str,
        compact_id: &str,
        turns_total: u32,
        turns_keeping: u32,
    ) -> Result<(), V2HubError> {
        self.publish_compact_delta(
            session_dir.as_ref(),
            session_id,
            compact_id,
            false,
            |revision| ConversationDelta::CompactStarted {
                revision,
                compact_id: compact_id.to_string(),
                turns_total,
                turns_keeping,
            },
        )
    }

    /// Publish the streaming compaction summary. `text` is the cumulative
    /// summary so far (the worker bridge coalesces provider chunks), which is
    /// what makes a dropped or duplicated delivery harmless.
    pub fn publish_compact_progress(
        &self,
        session_dir: impl AsRef<Path>,
        session_id: &str,
        compact_id: &str,
        text: String,
    ) -> Result<(), V2HubError> {
        self.publish_compact_delta(
            session_dir.as_ref(),
            session_id,
            compact_id,
            false,
            |revision| ConversationDelta::CompactProgress {
                revision,
                compact_id: compact_id.to_string(),
                delta: text,
            },
        )
    }

    /// Publish the compaction terminal state and clear the in-flight slot, so a
    /// later reconnect never replays a finished compaction as a running card.
    pub fn publish_compact_finished(
        &self,
        session_dir: impl AsRef<Path>,
        session_id: &str,
        view: CompactFinishedView<'_>,
    ) -> Result<(), V2HubError> {
        let compact_id = view.compact_id;
        self.publish_compact_delta(
            session_dir.as_ref(),
            session_id,
            compact_id,
            true,
            |revision| ConversationDelta::CompactFinished {
                revision,
                compact_id: compact_id.to_string(),
                status: view.status,
                summary_chars: view.summary_chars,
                turns_compacted: view.turns_compacted,
                turns_removed: view.turns_removed,
                reason: view.reason.map(str::to_string),
            },
        )
    }

    /// Publish in-flight assistant text for one stable block identity.
    ///
    /// `text` is cumulative. The slot is keyed by block id, so a reconnect is
    /// given the latest value only — never every token.
    pub fn publish_assistant_progress(
        &self,
        session_dir: impl AsRef<Path>,
        session_id: &str,
        turn_id: &str,
        block_id: &str,
        block_kind: qaqh_session::session_fact_v2::AssistantBlockKind,
        model: &str,
        text: &str,
    ) -> Result<(), V2HubError> {
        let turn_id = TurnId::new(turn_id.to_string());
        let block = qaqh_session::session_fact_v2::BlockId::new(block_id.to_string());
        self.publish_replaceable_conversation(
            session_dir.as_ref(),
            session_id,
            format!("conversation:block:{block_id}"),
            false,
            |revision| ConversationDelta::AssistantBlockProgress {
                revision,
                turn_id,
                block_id: block,
                block_kind,
                model: model.to_string(),
                text: text.to_string(),
            },
        )
    }

    /// Publish in-flight tool arguments. Display only; the call is executable
    /// only after `ToolCallDeclared` carries a validated argument body.
    pub fn publish_tool_input_progress(
        &self,
        session_dir: impl AsRef<Path>,
        session_id: &str,
        turn_id: &str,
        call_id: &str,
        tool_name: &str,
        args: &str,
    ) -> Result<(), V2HubError> {
        let turn_id = TurnId::new(turn_id.to_string());
        let call_id = ToolCallId::new(call_id.to_string());
        self.publish_replaceable_conversation(
            session_dir.as_ref(),
            session_id,
            format!("conversation:tool_input:{call_id:?}"),
            false,
            |revision| ConversationDelta::ToolInputProgress {
                revision,
                turn_id,
                call_id,
                tool_name: tool_name.to_string(),
                args: args.to_string(),
            },
        )
    }

    /// Drop an in-flight block slot once the attempt sealed, failed or was
    /// cancelled. A stale slot would resurrect old output on reconnect.
    pub fn clear_assistant_progress(
        &self,
        session_dir: impl AsRef<Path>,
        session_id: &str,
        block_id: &str,
    ) -> Result<(), V2HubError> {
        self.clear_progress_slot(
            session_dir.as_ref(),
            session_id,
            &format!("conversation:block:{block_id}"),
        )
    }

    pub fn clear_tool_input_progress(
        &self,
        session_dir: impl AsRef<Path>,
        session_id: &str,
        call_id: &str,
    ) -> Result<(), V2HubError> {
        self.clear_progress_slot(
            session_dir.as_ref(),
            session_id,
            &format!("conversation:tool_input:{call_id}"),
        )
    }

    fn clear_progress_slot(
        &self,
        session_dir: &Path,
        session_id: &str,
        slot: &str,
    ) -> Result<(), V2HubError> {
        let (canonical_session_id, log_id) = resolve_identity(session_dir, session_id)?;
        let session = self.session_for(session_dir, canonical_session_id, log_id)?;
        let mut state = session
            .state
            .lock()
            .map_err(|_| V2HubError::Canonical("v2 session lock poisoned".into()))?;
        state.replaceables.remove(slot);
        Ok(())
    }

    fn publish_replaceable_conversation(
        &self,
        session_dir: &Path,
        session_id: &str,
        slot: String,
        clear: bool,
        build: impl FnOnce(u64) -> ConversationDelta,
    ) -> Result<(), V2HubError> {
        let (canonical_session_id, log_id) = resolve_identity(session_dir, session_id)?;
        let session = self.session_for(session_dir, canonical_session_id, log_id)?;
        let mut state = session
            .state
            .lock()
            .map_err(|_| V2HubError::Canonical("v2 session lock poisoned".into()))?;
        let revision = state
            .replaceables
            .get(&slot)
            .and_then(|event| match &event.delivery {
                Delivery::Replaceable { revision } => Some(*revision),
                _ => None,
            })
            .map_or(1, |previous| previous.saturating_add(1));
        let event_id = EventId::new(generate_ulid());
        let event = ProjectionEvent {
            event_id: event_id.clone(),
            source_fact_seq: state.last_fact_seq.max(1),
            source_event_id: event_id,
            causation_id: None,
            ts_ms: None,
            stream_key: StreamKey::Channel(qaqh_domain::RingingChannel::Conversation),
            delivery: Delivery::Replaceable { revision },
            projection_slot: None,
            projection_index: None,
            payload: ProjectionPayload::ConversationDelta(build(revision)),
        };
        if clear {
            state.replaceables.remove(&slot);
        } else {
            state.replaceables.insert(slot, event.clone());
        }
        if let Some(envelope) = event_to_envelope(&self.epoch, session_id, &event) {
            let _ = state.live_tx.send(envelope);
        }
        Ok(())
    }

    /// 转发一条「命令业务失败」通知（ephemeral，control 频道）。
    ///
    /// 和压缩三态的关键差异：失败是**一次性通知**而不是状态，所以刻意不进
    /// `replaceables`——重连后补弹一个早已失效的失败只会误导用户。canonical
    /// log 不为它新增 fact（spec §6 冻结磁盘格式），因此这就是它唯一的出口：
    /// 在线订阅者收到即用，掉线即丢。
    pub fn publish_operation_failed(
        &self,
        session_dir: impl AsRef<Path>,
        session_id: &str,
        failure: OperationFailure,
    ) -> Result<(), V2HubError> {
        let session_dir = session_dir.as_ref();
        let (canonical_session_id, log_id) = resolve_identity(session_dir, session_id)?;
        let session = self.session_for(session_dir, canonical_session_id, log_id)?;
        let state = session
            .state
            .lock()
            .map_err(|_| V2HubError::Canonical("v2 session lock poisoned".into()))?;
        let event = ProjectionEvent {
            event_id: EventId::new(generate_ulid()),
            source_fact_seq: state.last_fact_seq.max(1),
            source_event_id: EventId::new(generate_ulid()),
            causation_id: None,
            ts_ms: Some(failure.failed_at_ms),
            stream_key: StreamKey::Channel(qaqh_domain::RingingChannel::Control),
            delivery: Delivery::Ephemeral,
            projection_slot: None,
            projection_index: None,
            payload: ProjectionPayload::OperationFailed(failure),
        };
        if let Some(envelope) = event_to_envelope(&self.epoch, session_id, &event) {
            let _ = state.live_tx.send(envelope);
        }
        Ok(())
    }

    /// Shared shape for the three transient compaction deltas: same
    /// double-tagged payload as the fact-derived conversation deltas, published
    /// straight to live subscribers plus a per-`compact_id` current-value slot.
    fn publish_compact_delta(
        &self,
        session_dir: &Path,
        session_id: &str,
        compact_id: &str,
        terminal: bool,
        build: impl FnOnce(u64) -> ConversationDelta,
    ) -> Result<(), V2HubError> {
        let (canonical_session_id, log_id) = resolve_identity(session_dir, session_id)?;
        let session = self.session_for(session_dir, canonical_session_id, log_id)?;
        let mut state = session
            .state
            .lock()
            .map_err(|_| V2HubError::Canonical("v2 session lock poisoned".into()))?;
        state.compact_revision = state.compact_revision.saturating_add(1);
        let revision = state.compact_revision;
        let event_id = EventId::new(generate_ulid());
        let event = ProjectionEvent {
            event_id: event_id.clone(),
            source_fact_seq: state.last_fact_seq.max(1),
            source_event_id: event_id,
            causation_id: None,
            ts_ms: None,
            stream_key: StreamKey::Channel(qaqh_domain::RingingChannel::Conversation),
            delivery: Delivery::Replaceable { revision },
            projection_slot: None,
            projection_index: None,
            payload: ProjectionPayload::ConversationDelta(build(revision)),
        };
        let slot = format!("conversation:compact:{compact_id}");
        if terminal {
            state.replaceables.remove(&slot);
        } else {
            state.replaceables.insert(slot, event.clone());
        }
        if let Some(envelope) = event_to_envelope(&self.epoch, session_id, &event) {
            let _ = state.live_tx.send(envelope);
        }
        Ok(())
    }

    /// Open a per-seed single stream.
    ///
    /// Every event keeps its `stream_key`; the client demuxes. There is no
    /// per-channel filter any more — see the 2026-09-24 frozen revision
    /// (`tui-ringing-v2-frozen-2026-09-24-single-stream`).
    pub fn subscribe(
        &self,
        session_dir: impl AsRef<Path>,
        session_id: &str,
        since_cursor: Option<&CursorToken>,
    ) -> Result<V2Subscription, V2HubError> {
        let session_dir = session_dir.as_ref();
        let (canonical_session_id, log_id) = resolve_identity(session_dir, session_id)?;
        let session =
            self.session_for(session_dir, canonical_session_id.clone(), log_id.clone())?;
        let state = session
            .state
            .lock()
            .map_err(|_| V2HubError::Canonical("v2 session lock poisoned".into()))?;

        let live_rx = state.live_tx.subscribe();
        let snapshot_cursor = if state.last_fact_seq == 0 {
            None
        } else {
            CursorToken::encode_snapshot(&CanonicalCursor::snapshot(
                log_id.as_str(),
                state.last_fact_seq,
            ))
            .ok()
        };
        let (since_fact_seq, initial_reset) = match since_cursor {
            Some(token) => {
                let cursor = token
                    .decode()
                    .map_err(|error| V2HubError::InvalidCursor(error.to_string()))?;
                if cursor.log_id != log_id.as_str() {
                    (
                        state.last_fact_seq,
                        Some(RingingV2ResetRequired {
                            schema: qaqh_ringing::RINGING_SCHEMA.into(),
                            version: qaqh_ringing::RINGING_V2_VERSION,
                            server_epoch: self.epoch.clone(),
                            session_id: session_id.to_string(),
                            log_id: Some(log_id.as_str().to_string()),
                            snapshot_cursor: snapshot_cursor.clone(),
                            reason: RingingV2ResetReason::LogIdMismatch,
                        }),
                    )
                } else if cursor.fact_seq > state.last_fact_seq {
                    (
                        state.last_fact_seq,
                        Some(RingingV2ResetRequired {
                            schema: qaqh_ringing::RINGING_SCHEMA.into(),
                            version: qaqh_ringing::RINGING_V2_VERSION,
                            server_epoch: self.epoch.clone(),
                            session_id: session_id.to_string(),
                            log_id: Some(log_id.as_str().to_string()),
                            snapshot_cursor: snapshot_cursor.clone(),
                            reason: RingingV2ResetReason::UnknownFact,
                        }),
                    )
                } else {
                    (cursor.fact_seq, None)
                }
            }
            None => (state.last_fact_seq, None),
        };

        let mut replay = if initial_reset.is_some() || since_fact_seq == state.last_fact_seq {
            // The receiver is registered while holding the same state lock as
            // publication; at the current watermark there is nothing to replay.
            VecDeque::new()
        } else {
            replay_after(
                session_dir,
                &canonical_session_id,
                &log_id,
                &self.epoch,
                since_fact_seq,
            )?
            .into_iter()
            .collect::<VecDeque<_>>()
        };
        if initial_reset.is_none() {
            // Replaceable history is never replayed. Reconnect/rebaseline gets
            // only the latest value for each stable identity.
            for event in state.replaceables.values() {
                if let Some(envelope) = event_to_envelope(&self.epoch, session_id, event) {
                    replay.push_back(envelope);
                }
            }
        }
        drop(state);

        Ok(V2Subscription {
            replay,
            live_rx,
            server_epoch: self.epoch.clone(),
            session_id: session_id.to_string(),
            log_id,
            initial_reset,
        })
    }

    fn session_for(
        &self,
        session_dir: &Path,
        session_id: SessionId,
        log_id: LogId,
    ) -> Result<Arc<V2Session>, V2HubError> {
        if let Some(session) = self
            .sessions
            .read()
            .map_err(|_| V2HubError::Canonical("v2 hub lock poisoned".into()))?
            .get(&session_id)
            .cloned()
        {
            return Ok(session);
        }

        let state =
            load_session_state(session_dir, session_id.clone(), log_id, self.live_capacity)?;
        let session = Arc::new(V2Session {
            state: Mutex::new(state),
        });
        let mut sessions = self
            .sessions
            .write()
            .map_err(|_| V2HubError::Canonical("v2 hub lock poisoned".into()))?;
        Ok(sessions
            .entry(session_id)
            .or_insert_with(|| session.clone())
            .clone())
    }
}

impl ProjectionSink for V2ProjectionHub {
    fn publish(&self, session_dir: &Path, fact: &SessionFact, events: &[ProjectionEvent]) {
        let session_id = session_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let Ok(session) =
            self.session_for(session_dir, fact.session_id.clone(), fact.log_id.clone())
        else {
            log::error!("[ringing-v2] failed to load session {session_id} for projection publish");
            return;
        };
        let Ok(mut state) = session.state.lock() else {
            log::error!("[ringing-v2] projection state lock poisoned for {session_id}");
            return;
        };
        if state.last_fact_seq < fact.fact_seq {
            let _ = state.projections.apply(fact);
            state.last_fact_seq = fact.fact_seq;
        }
        let overlays: Vec<_> = state
            .runtime_residency
            .iter()
            .map(|(agent_id, residency)| (agent_id.clone(), *residency))
            .collect();
        let mut overlay_deltas = Vec::new();
        for (agent_id, residency) in overlays {
            if let Some(delta) = state
                .projections
                .team
                .apply_runtime_residency(&agent_id, residency)
            {
                overlay_deltas.push(delta);
            }
        }
        for event in events {
            if matches!(event.delivery, Delivery::Replaceable { .. })
                && let Some(identity) = replaceable_identity(&event.payload)
            {
                state.replaceables.insert(identity, event.clone());
            }
            let Some(envelope) = event_to_envelope(&self.epoch, session_id, event) else {
                continue;
            };
            let _ = state.live_tx.send(envelope);
        }
        for delta in overlay_deltas {
            if let Some(envelope) =
                ephemeral_team_envelope(&self.epoch, session_id, state.last_fact_seq, delta)
            {
                let _ = state.live_tx.send(envelope);
            }
        }
    }
}

impl V2Subscription {
    /// 非阻塞消费：先排空 replay，再 `try_recv` live。空 → `None`。
    /// 供进程内桥接线程轮询（hub-fact-bus spec 阶段 2.2），语义与
    /// [`Self::next`] 的 async 路径一致：Lagged 上报 Reset，Closed 静默终止。
    pub fn try_next(&mut self) -> Option<V2StreamItem> {
        self.try_next_status().ok().flatten()
    }

    /// Distinguish an idle subscription from a permanently closed publisher.
    pub fn try_next_status(&mut self) -> Result<Option<V2StreamItem>, ()> {
        if let Some(reset) = self.initial_reset.take() {
            return Ok(Some(V2StreamItem::Reset(reset)));
        }
        if let Some(event) = self.replay.pop_front() {
            return Ok(Some(V2StreamItem::Event(Box::new(event))));
        }
        match self.live_rx.try_recv() {
            Ok(event) => Ok(Some(V2StreamItem::Event(Box::new(event)))),
            Err(broadcast::error::TryRecvError::Empty) => Ok(None),
            Err(broadcast::error::TryRecvError::Lagged(_)) => {
                Ok(Some(V2StreamItem::Reset(RingingV2ResetRequired {
                    schema: qaqh_ringing::RINGING_SCHEMA.into(),
                    version: qaqh_ringing::RINGING_V2_VERSION,
                    server_epoch: self.server_epoch.clone(),
                    session_id: self.session_id.clone(),
                    log_id: Some(self.log_id.as_str().to_string()),
                    snapshot_cursor: None,
                    reason: RingingV2ResetReason::ReplayOverflow,
                })))
            }
            Err(broadcast::error::TryRecvError::Closed) => Err(()),
        }
    }

    pub async fn next(&mut self) -> V2StreamItem {
        if let Some(reset) = self.initial_reset.take() {
            return V2StreamItem::Reset(reset);
        }
        if let Some(event) = self.replay.pop_front() {
            return V2StreamItem::Event(Box::new(event));
        }
        // 单流：不再按 channel 过滤，收到什么就是什么。
        match self.live_rx.recv().await {
            Ok(event) => V2StreamItem::Event(Box::new(event)),
            Err(broadcast::error::RecvError::Lagged(_)) => {
                V2StreamItem::Reset(RingingV2ResetRequired {
                    schema: qaqh_ringing::RINGING_SCHEMA.into(),
                    version: qaqh_ringing::RINGING_V2_VERSION,
                    server_epoch: self.server_epoch.clone(),
                    session_id: self.session_id.clone(),
                    log_id: Some(self.log_id.as_str().to_string()),
                    snapshot_cursor: None,
                    reason: RingingV2ResetReason::ReplayOverflow,
                })
            }
            Err(broadcast::error::RecvError::Closed) => {
                V2StreamItem::Reset(RingingV2ResetRequired {
                    schema: qaqh_ringing::RINGING_SCHEMA.into(),
                    version: qaqh_ringing::RINGING_V2_VERSION,
                    server_epoch: self.server_epoch.clone(),
                    session_id: self.session_id.clone(),
                    log_id: Some(self.log_id.as_str().to_string()),
                    snapshot_cursor: None,
                    reason: RingingV2ResetReason::PerConnectionOverflow,
                })
            }
        }
    }
}

fn resolve_identity(
    session_dir: &Path,
    session_id: &str,
) -> Result<(SessionId, LogId), V2HubError> {
    if !session_dir.exists() {
        return Err(V2HubError::SessionMissing(session_id.to_string()));
    }
    // Hard cut: only the batch journal is a storage generation. A directory
    // carrying the removed `events.commit.json` layout is reported as such
    // instead of being read through a second code path.
    if !qaqh_session::canonical::has_committed_facts(session_dir) {
        return Err(V2HubError::Canonical(format!(
            "session {session_id}: no batch journal (legacy storage removed)"
        )));
    }
    let identity = qaqh_session::canonical::open_session_identity(session_dir)
        .map_err(|error| V2HubError::Canonical(error.to_string()))?;
    Ok((identity.session_id, identity.log_id))
}

fn load_session_state(
    session_dir: &Path,
    session_id: SessionId,
    log_id: LogId,
    live_capacity: usize,
) -> Result<V2SessionState, V2HubError> {
    let facts = crate::agent_graph::read_session_facts(session_dir, session_id, log_id.clone())
        .map_err(|error| V2HubError::Canonical(error.to_string()))?;
    let last_fact_seq = facts.last().map(|fact| fact.fact_seq).unwrap_or(0);
    let mut projections = ProjectionSet::default();
    let mut replaceables = BTreeMap::new();
    for fact in &facts {
        let deltas = projections.apply(fact);
        for event in projection_replaceable_events_for_fact(fact, &deltas)
            .map_err(|error| V2HubError::InvalidEvent(error.to_string()))?
        {
            if let Some(identity) = replaceable_identity(&event.payload) {
                replaceables.insert(identity, event);
            }
        }
    }
    let (live_tx, _) = broadcast::channel(live_capacity);
    Ok(V2SessionState {
        last_fact_seq,
        title_revision: projections.meta.snapshot().revision,
        config_revision: 0,
        projections,
        runtime_residency: HashMap::new(),
        replaceables,
        live_tx,
        task_revision: 0,
        board_revision: 0,
        compact_revision: 0,
    })
}

fn replay_after(
    session_dir: &Path,
    session_id: &SessionId,
    log_id: &LogId,
    server_epoch: &str,
    since_fact_seq: u64,
) -> Result<VecDeque<V2Envelope>, V2HubError> {
    let facts =
        crate::agent_graph::read_session_facts(session_dir, session_id.clone(), log_id.clone())
            .map_err(|error| V2HubError::Canonical(error.to_string()))?;
    let mut projections = ProjectionSet::default();
    let mut replay = VecDeque::new();
    for fact in facts {
        let deltas = projections.apply(&fact);
        if fact.fact_seq <= since_fact_seq {
            continue;
        }
        let events = projection_events_for_fact(&fact, &deltas)
            .map_err(|error| V2HubError::InvalidEvent(error.to_string()))?;
        for event in events {
            if let Some(envelope) = event_to_envelope(
                server_epoch,
                session_dir
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default(),
                &event,
            ) {
                replay.push_back(envelope);
            }
        }
    }
    Ok(replay)
}

fn ephemeral_team_envelope(
    server_epoch: &str,
    session_id: &str,
    last_fact_seq: u64,
    delta: TeamDelta,
) -> Option<V2Envelope> {
    let event = ProjectionEvent {
        event_id: qaqh_session::session_fact_v2::EventId::new(generate_ulid()),
        source_fact_seq: last_fact_seq.max(1),
        source_event_id: qaqh_session::session_fact_v2::EventId::new(generate_ulid()),
        causation_id: None,
        ts_ms: None,
        stream_key: StreamKey::Channel(qaqh_domain::RingingChannel::Control),
        delivery: Delivery::Ephemeral,
        projection_slot: None,
        projection_index: None,
        payload: ProjectionPayload::TeamDelta(delta),
    };
    event_to_envelope(server_epoch, session_id, &event)
}

fn event_to_envelope(
    server_epoch: &str,
    session_id: &str,
    event: &ProjectionEvent,
) -> Option<V2Envelope> {
    let stream_key = match &event.stream_key {
        StreamKey::Channel(channel) => RingingV2StreamKey::Channel(*channel),
        StreamKey::Resource { kind, id } => RingingV2StreamKey::Resource {
            kind: serde_json::to_value(kind).ok()?.as_str()?.to_string(),
            id: id.as_str().to_string(),
        },
    };
    let (delivery, cursor, log_id, fact_seq, projection_index, revision) = match &event.delivery {
        Delivery::Reliable { cursor } => {
            let token = CursorToken::encode_reliable(&CanonicalCursor::new(
                cursor.log_id.as_str(),
                cursor.fact_seq,
                cursor.projection_index,
            ))
            .ok()?;
            (
                RingingV2Delivery::Reliable,
                Some(token),
                Some(cursor.log_id.as_str().to_string()),
                Some(cursor.fact_seq),
                Some(cursor.projection_index),
                event.payload.revision().or(Some(cursor.fact_seq)),
            )
        }
        Delivery::Replaceable { revision } => (
            RingingV2Delivery::Replaceable,
            None,
            None,
            Some(event.source_fact_seq),
            None,
            Some(*revision),
        ),
        Delivery::Ephemeral => (RingingV2Delivery::Ephemeral, None, None, None, None, None),
    };
    Some(RingingV2EventEnvelope {
        schema: qaqh_ringing::RINGING_SCHEMA.into(),
        version: qaqh_ringing::RINGING_V2_VERSION,
        server_epoch: server_epoch.to_string(),
        session_id: session_id.to_string(),
        event_id: event.event_id.as_str().to_string(),
        stream_key,
        delivery,
        cursor,
        log_id,
        fact_seq,
        projection_index,
        revision,
        causation_id: event
            .causation_id
            .as_ref()
            .map(|causation| causation.as_str().to_string()),
        correlation_id: None,
        ts_ms: event.ts_ms.and_then(|value| u64::try_from(value).ok()),
        payload: event.payload.clone(),
    })
}
