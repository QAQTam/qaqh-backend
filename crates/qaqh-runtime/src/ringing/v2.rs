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
use qaqh_session::canonical::{
    CANONICAL_IDENTITY_FILE, CanonicalSessionIdentity, CommittedFactReader, EVENTS_COMMIT_FILE,
    EVENTS_FILE, generate_ulid,
};
use qaqh_session::projection::{
    ControlDriverState, ControlInteractionState, ConversationTurnOutcome, Projection,
    ProjectionSet, ProjectionSetSnapshot, ProjectionSink, projection_events_for_fact,
    projection_replaceable_events_for_fact, replaceable_identity,
};
use qaqh_session::session_fact_v2::{
    ActivityState as FactActivityState, CompactStatus, ConversationDelta, Delivery, EventId,
    InteractionKind, InterruptReason, LogId, MetaDelta, ProjectionEvent, ProjectionPayload,
    SessionFact, SessionId, StreamKey, TeamAgentResidency, TeamBoardSnapshot, TeamDelta,
    TeamTaskSnapshot, TitleSource, TurnTerminal,
};
use tokio::sync::broadcast;

const LIVE_CAPACITY: usize = 1024;

pub type V2Envelope = RingingV2EventEnvelope<ProjectionPayload>;

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

    pub fn memory_components(&self) -> Vec<qaqh_memwatch::ComponentMemory> {
        let Ok(sessions) = self.sessions.read() else {
            return Vec::new();
        };
        let mut lower_bound = sessions
            .capacity()
            .saturating_mul(std::mem::size_of::<(SessionId, Arc<V2Session>)>());
        let mut overlay_items = 0_u64;
        let mut replaceable_items = 0_u64;
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
                overlay_items = overlay_items.saturating_add(state.runtime_residency.len() as u64);
                replaceable_items =
                    replaceable_items.saturating_add(state.replaceables.len() as u64);
            }
        }
        vec![
            qaqh_memwatch::ComponentMemory {
                name: "ringing.v2.projection_sessions".into(),
                item_count: sessions.len() as u64,
                payload_bytes: None,
                heap_estimate_bytes: Some(lower_bound as u64),
                ..Default::default()
            },
            qaqh_memwatch::ComponentMemory {
                name: "ringing.v2.runtime_overlays".into(),
                item_count: overlay_items,
                payload_bytes: None,
                heap_estimate_bytes: None,
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

    /// Canonical driver seat for a seed (`None` = no `DriverChanged` fact yet).
    ///
    /// Same cost profile as [`Self::control_interactions`]: control projection
    /// only, served from the hub's in-memory session after the first load.
    pub fn driver_state(
        &self,
        session_dir: impl AsRef<Path>,
        session_id: &str,
    ) -> Result<Option<ControlDriverState>, V2HubError> {
        let session_dir = session_dir.as_ref();
        let (canonical_session_id, log_id) = resolve_identity(session_dir, session_id)?;
        let session = self.session_for(session_dir, canonical_session_id, log_id)?;
        let state = session
            .state
            .lock()
            .map_err(|_| V2HubError::Canonical("v2 session lock poisoned".into()))?;
        Ok(state.projections.control.snapshot().driver)
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
                source: TitleSource::Auto,
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
        compact_id: &str,
        status: CompactStatus,
        summary_chars: Option<usize>,
        turns_compacted: Option<u32>,
        turns_removed: Option<u32>,
    ) -> Result<(), V2HubError> {
        self.publish_compact_delta(
            session_dir.as_ref(),
            session_id,
            compact_id,
            true,
            |revision| ConversationDelta::CompactFinished {
                revision,
                compact_id: compact_id.to_string(),
                status,
                summary_chars,
                turns_compacted,
                turns_removed,
            },
        )
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

        let mut replay = if initial_reset.is_some() {
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
        if let Some(reset) = self.initial_reset.take() {
            return Some(V2StreamItem::Reset(reset));
        }
        if let Some(event) = self.replay.pop_front() {
            return Some(V2StreamItem::Event(Box::new(event)));
        }
        match self.live_rx.try_recv() {
            Ok(event) => Some(V2StreamItem::Event(Box::new(event))),
            Err(broadcast::error::TryRecvError::Empty) => None,
            Err(broadcast::error::TryRecvError::Lagged(_)) => {
                Some(V2StreamItem::Reset(RingingV2ResetRequired {
                    schema: qaqh_ringing::RINGING_SCHEMA.into(),
                    version: qaqh_ringing::RINGING_V2_VERSION,
                    server_epoch: self.server_epoch.clone(),
                    session_id: self.session_id.clone(),
                    log_id: Some(self.log_id.as_str().to_string()),
                    snapshot_cursor: None,
                    reason: RingingV2ResetReason::ReplayOverflow,
                }))
            }
            Err(broadcast::error::TryRecvError::Closed) => None,
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
    if !session_dir.join(CANONICAL_IDENTITY_FILE).exists()
        && !session_dir.join(EVENTS_FILE).exists()
    {
        return Err(V2HubError::SessionMissing(session_id.to_string()));
    }
    let identity = CanonicalSessionIdentity::open_or_create(session_dir)
        .map_err(|error| V2HubError::Canonical(error.to_string()))?;
    // A session can exist (identity minted) before its first canonical commit.
    // There is no snapshot cursor to hand out yet, so surface the documented
    // `snapshot_missing` reason rather than a generic canonical error.
    if !session_dir.join(EVENTS_COMMIT_FILE).exists() {
        return Err(V2HubError::SnapshotMissing(session_id.to_string()));
    }
    Ok((identity.session_id, identity.log_id))
}

fn load_session_state(
    session_dir: &Path,
    session_id: SessionId,
    log_id: LogId,
    live_capacity: usize,
) -> Result<V2SessionState, V2HubError> {
    let facts = CommittedFactReader::open(session_dir, session_id, log_id.clone())
        .map_err(|error| V2HubError::Canonical(error.to_string()))?
        .read_all()
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
    let facts = CommittedFactReader::open(session_dir, session_id.clone(), log_id.clone())
        .map_err(|error| V2HubError::Canonical(error.to_string()))?
        .read_all()
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

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_session::canonical::{
        CanonicalSessionIdentity, CanonicalSessionStore, WriterId, generate_ulid,
    };
    use qaqh_session::session_fact_v2::{
        AgentPath, EventId, FactPayload, FactSchema, MetadataSource, SessionCreated, SessionFact,
        SessionMetadataChanged, SessionMetadataPatch, SubagentSpawned, TeamAgentResidency,
        TeamBoardSnapshot, TeamDelta, TeamTaskSnapshot, ToolCallId,
    };

    fn append_two_facts(
        dir: &Path,
        identity: &CanonicalSessionIdentity,
    ) -> (SessionFact, SessionFact) {
        let now = 1_789_830_000_000;
        let mut store =
            CanonicalSessionStore::open(dir, identity.session_id.clone(), identity.log_id.clone())
                .expect("open store");
        let lease = store
            .acquire_writer(WriterId::new("v2-hub-test"), now, 60_000)
            .expect("writer lease");
        let created = SessionFact {
            schema: FactSchema::v2(),
            session_id: identity.session_id.clone(),
            log_id: identity.log_id.clone(),
            fact_seq: 0,
            event_id: EventId::new(generate_ulid()),
            ts_ms: now,
            causation_id: None,
            turn_id: None,
            call_id: None,
            interaction_id: None,
            payload: FactPayload::SessionCreated(SessionCreated {
                created_at_ms: now,
                cwd: "/tmp".into(),
                model: "test".into(),
                parent_session_id: None,
                schema_caps: Vec::new(),
            }),
        };
        let created = store
            .append(&lease, created, now)
            .expect("append created")
            .fact;
        let changed = SessionFact {
            schema: FactSchema::v2(),
            session_id: identity.session_id.clone(),
            log_id: identity.log_id.clone(),
            fact_seq: 0,
            event_id: EventId::new(generate_ulid()),
            ts_ms: now + 1,
            causation_id: None,
            turn_id: None,
            call_id: None,
            interaction_id: None,
            payload: FactPayload::SessionMetadataChanged(SessionMetadataChanged {
                patch: SessionMetadataPatch {
                    cwd: None,
                    model: Some("test-2".into()),
                    archived: None,
                    search_visibility: None,
                    parent_session_id: None,
                    schema_caps: None,
                },
                source: MetadataSource::Api,
                changed_at_ms: now + 1,
            }),
        };
        let changed = store
            .append(&lease, changed, now + 1)
            .expect("append changed")
            .fact;
        (created, changed)
    }

    fn append_spawned_child(
        dir: &Path,
        identity: &CanonicalSessionIdentity,
        child: &SessionId,
    ) -> SessionFact {
        let now = 1_789_830_000_100;
        let mut store =
            CanonicalSessionStore::open(dir, identity.session_id.clone(), identity.log_id.clone())
                .expect("open store");
        let lease = store
            .acquire_writer(WriterId::new("v2-hub-residency-test"), now, 60_000)
            .expect("writer lease");
        let created = SessionFact {
            schema: FactSchema::v2(),
            session_id: identity.session_id.clone(),
            log_id: identity.log_id.clone(),
            fact_seq: 0,
            event_id: EventId::new(generate_ulid()),
            ts_ms: now,
            causation_id: None,
            turn_id: None,
            call_id: None,
            interaction_id: None,
            payload: FactPayload::SessionCreated(SessionCreated {
                created_at_ms: now,
                cwd: "/tmp".into(),
                model: "test".into(),
                parent_session_id: None,
                schema_caps: Vec::new(),
            }),
        };
        store.append(&lease, created, now).expect("append created");
        let spawned = SessionFact {
            schema: FactSchema::v2(),
            session_id: identity.session_id.clone(),
            log_id: identity.log_id.clone(),
            fact_seq: 0,
            event_id: EventId::new(generate_ulid()),
            ts_ms: now + 1,
            causation_id: None,
            turn_id: None,
            call_id: None,
            interaction_id: None,
            payload: FactPayload::SubagentSpawned(SubagentSpawned {
                child_session_id: child.clone(),
                parent_call_id: ToolCallId::new(format!("call_{}", generate_ulid())),
                parent_agent_path: Some(AgentPath::root()),
                child_agent_path: Some(
                    AgentPath::parse_absolute("/root/review").expect("child path"),
                ),
                role: Some("review".to_string()),
                spawn_config: None,
                spawned_at_ms: now + 1,
            }),
        };
        store
            .append(&lease, spawned, now + 1)
            .expect("append spawned")
            .fact
    }

    #[tokio::test]
    async fn runtime_residency_overlay_updates_snapshot_and_emits_team_delta() {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = CanonicalSessionIdentity::open_or_create(dir.path()).expect("identity");
        let child = SessionId::new("0198f1a0-0000-7000-8000-000000000011");
        append_spawned_child(dir.path(), &identity, &child);

        let hub = Arc::new(V2ProjectionHub::new("epoch-residency"));
        let bootstrap = hub.bootstrap(dir.path(), "seed").expect("bootstrap");
        let child_snapshot = bootstrap
            .projections
            .team
            .agents
            .iter()
            .find(|agent| agent.agent_id == child)
            .expect("child snapshot");
        assert_eq!(child_snapshot.residency, TeamAgentResidency::Unloaded);

        let mut subscription = hub
            .subscribe(dir.path(), "seed", Some(&bootstrap.snapshot_cursor))
            .expect("subscribe");
        hub.set_team_residency(dir.path(), "seed", &child, TeamAgentResidency::Loaded)
            .expect("set runtime residency");

        let loaded = hub.bootstrap(dir.path(), "seed").expect("loaded bootstrap");
        assert_eq!(
            loaded
                .projections
                .team
                .agents
                .iter()
                .find(|agent| agent.agent_id == child)
                .expect("loaded child")
                .residency,
            TeamAgentResidency::Loaded
        );
        loop {
            match subscription.next().await {
                V2StreamItem::Event(envelope)
                    if matches!(
                        &envelope.payload,
                        ProjectionPayload::TeamDelta(TeamDelta::AgentResidencyChanged {
                            residency: TeamAgentResidency::Loaded,
                            ..
                        })
                    ) =>
                {
                    assert_eq!(envelope.delivery, RingingV2Delivery::Ephemeral);
                    break;
                }
                V2StreamItem::Event(_) => continue,
                other => panic!("expected ephemeral residency event, got {other:?}"),
            }
        }

        let restarted = Arc::new(V2ProjectionHub::new("epoch-restarted"));
        let rebuilt = restarted
            .bootstrap(dir.path(), "seed")
            .expect("restart bootstrap");
        assert_eq!(
            rebuilt
                .projections
                .team
                .agents
                .iter()
                .find(|agent| agent.agent_id == child)
                .expect("rebuilt child")
                .residency,
            TeamAgentResidency::Unloaded,
            "runtime residency must not survive daemon restart"
        );
    }

    #[tokio::test]
    async fn task_delta_is_published_on_the_session_stream() {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = CanonicalSessionIdentity::open_or_create(dir.path()).expect("identity");
        let _ = append_two_facts(dir.path(), &identity);
        let hub = Arc::new(V2ProjectionHub::new("epoch-task-delta"));
        let bootstrap = hub.bootstrap(dir.path(), "seed").expect("bootstrap");
        let mut subscription = hub
            .subscribe(dir.path(), "seed", Some(&bootstrap.snapshot_cursor))
            .expect("subscribe");
        let task = TeamTaskSnapshot {
            task_id: "task_01J00000000000000000000000".into(),
            title: "ship it".into(),
            state: "open".into(),
            owner: None,
            claim_epoch: 0,
            depends_on: vec![],
            artifacts: vec![],
            acceptance: vec![],
            result_ref: None,
            created_at_ms: 1_789_830_000_000,
            updated_at_ms: 1_789_830_000_000,
        };
        hub.publish_task_delta(dir.path(), "seed", task.clone())
            .expect("publish task delta");
        loop {
            match subscription.next().await {
                V2StreamItem::Event(envelope) => {
                    if let ProjectionPayload::TeamDelta(TeamDelta::TaskChanged {
                        revision,
                        task: received,
                    }) = &envelope.payload
                    {
                        assert_eq!(*revision, 1);
                        assert_eq!(received.task_id, task.task_id);
                        assert_eq!(envelope.delivery, RingingV2Delivery::Ephemeral);
                        break;
                    }
                }
                other => panic!("expected task delta, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn board_delta_is_published_on_the_session_stream() {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = CanonicalSessionIdentity::open_or_create(dir.path()).expect("identity");
        let _ = append_two_facts(dir.path(), &identity);
        let hub = Arc::new(V2ProjectionHub::new("epoch-board-delta"));
        let bootstrap = hub.bootstrap(dir.path(), "seed").expect("bootstrap");
        let mut subscription = hub
            .subscribe(dir.path(), "seed", Some(&bootstrap.snapshot_cursor))
            .expect("subscribe");
        let board = TeamBoardSnapshot {
            board_id: Some(identity.session_id.clone()),
            revision: 1,
            last_fact_seq: 1,
            channels: vec![],
            threads: vec![],
            posts: vec![],
            subscriptions: vec![],
        };
        hub.publish_board_change(dir.path(), "seed", board.clone())
            .expect("publish board delta");
        loop {
            match subscription.next().await {
                V2StreamItem::Event(envelope) => {
                    if let ProjectionPayload::TeamDelta(TeamDelta::BoardChanged {
                        revision,
                        board: received,
                    }) = &envelope.payload
                    {
                        assert_eq!(*revision, 1);
                        assert_eq!(received.board_id, board.board_id);
                        assert_eq!(envelope.delivery, RingingV2Delivery::Ephemeral);
                        break;
                    }
                }
                other => panic!("expected board delta, got {other:?}"),
            }
        }
    }

    #[test]
    fn hub_bootstraps_from_committed_canonical_facts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = CanonicalSessionIdentity::open_or_create(dir.path()).expect("identity");
        let (created, changed) = append_two_facts(dir.path(), &identity);
        let hub = Arc::new(V2ProjectionHub::new("epoch-1"));
        let bootstrap = hub.bootstrap(dir.path(), "seed").expect("bootstrap");
        assert_eq!(bootstrap.last_fact_seq, changed.fact_seq);
        let _subscription = hub
            .subscribe(dir.path(), "seed", Some(&bootstrap.snapshot_cursor))
            .expect("subscribe");

        let replay = replay_after(
            dir.path(),
            &identity.session_id,
            &identity.log_id,
            "epoch-1",
            created.fact_seq,
        )
        .expect("replay");
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].fact_seq, Some(changed.fact_seq));
        // C3：信封携带源 fact 的提交时间（epoch ms）。
        assert_eq!(
            replay[0].ts_ms,
            Some(u64::try_from(changed.ts_ms).expect("ts_ms"))
        );
    }

    /// The three transient compaction deltas must land on the conversation
    /// channel with the same double-tagged shape as the fact-derived deltas.
    #[test]
    fn compact_deltas_publish_on_the_conversation_stream() {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = CanonicalSessionIdentity::open_or_create(dir.path()).expect("identity");
        append_two_facts(dir.path(), &identity);
        let hub = Arc::new(V2ProjectionHub::new("epoch-1"));
        let bootstrap = hub.bootstrap(dir.path(), "seed").expect("bootstrap");
        let mut subscription = hub
            .subscribe(dir.path(), "seed", Some(&bootstrap.snapshot_cursor))
            .expect("subscribe");

        hub.publish_compact_started(dir.path(), "seed", "compact-1", 12, 3)
            .expect("started");
        hub.publish_compact_progress(dir.path(), "seed", "compact-1", "累积摘要".into())
            .expect("progress");
        hub.publish_compact_finished(
            dir.path(),
            "seed",
            "compact-1",
            CompactStatus::Completed,
            Some(4),
            Some(9),
            Some(6),
        )
        .expect("finished");

        let mut payloads = Vec::new();
        while let Some(V2StreamItem::Event(envelope)) = subscription.try_next() {
            assert_eq!(
                envelope.stream_key,
                RingingV2StreamKey::Channel(qaqh_domain::RingingChannel::Conversation)
            );
            assert_eq!(envelope.delivery, RingingV2Delivery::Replaceable);
            assert_eq!(envelope.cursor, None, "transient deltas carry no cursor");
            assert_eq!(envelope.fact_seq, Some(bootstrap.last_fact_seq));
            payloads.push(serde_json::to_value(&envelope.payload).expect("payload json"));
        }
        assert_eq!(payloads.len(), 3);
        assert_eq!(payloads[0]["kind"], "conversation_delta");
        assert_eq!(payloads[0]["data"]["kind"], "compact_started");
        assert_eq!(payloads[0]["data"]["data"]["turns_total"], 12);
        assert_eq!(payloads[1]["data"]["kind"], "compact_progress");
        assert_eq!(payloads[1]["data"]["data"]["delta"], "累积摘要");
        assert_eq!(payloads[2]["data"]["kind"], "compact_finished");
        assert_eq!(payloads[2]["data"]["data"]["status"], "completed");
        // One monotonic revision per publish, carried by both envelope and delta.
        assert_eq!(payloads[0]["data"]["data"]["revision"], 1);
        assert_eq!(payloads[2]["data"]["data"]["revision"], 3);
    }

    /// Reconnecting mid-compaction recovers the in-flight card from the hub's
    /// current-value slot; after the terminal event the slot must be gone, or a
    /// finished compaction would replay as a permanently running card.
    #[test]
    fn compact_slot_replays_while_running_and_clears_on_terminal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = CanonicalSessionIdentity::open_or_create(dir.path()).expect("identity");
        append_two_facts(dir.path(), &identity);
        let hub = Arc::new(V2ProjectionHub::new("epoch-1"));
        let bootstrap = hub.bootstrap(dir.path(), "seed").expect("bootstrap");

        hub.publish_compact_started(dir.path(), "seed", "compact-7", 20, 4)
            .expect("started");
        hub.publish_compact_progress(dir.path(), "seed", "compact-7", "第一段".into())
            .expect("progress");
        hub.publish_compact_progress(dir.path(), "seed", "compact-7", "第一段第二段".into())
            .expect("progress");

        let running = hub
            .subscribe(dir.path(), "seed", Some(&bootstrap.snapshot_cursor))
            .expect("subscribe");
        let slots = compact_slot_kinds(&running);
        assert_eq!(
            slots,
            vec!["compact_progress"],
            "only the latest value replays"
        );

        hub.publish_compact_finished(
            dir.path(),
            "seed",
            "compact-7",
            CompactStatus::Cancelled,
            None,
            None,
            None,
        )
        .expect("finished");
        let settled = hub
            .subscribe(dir.path(), "seed", Some(&bootstrap.snapshot_cursor))
            .expect("subscribe");
        assert!(compact_slot_kinds(&settled).is_empty());
    }

    fn compact_slot_kinds(subscription: &V2Subscription) -> Vec<String> {
        subscription
            .replay
            .iter()
            .filter_map(|envelope| match &envelope.payload {
                ProjectionPayload::ConversationDelta(
                    ConversationDelta::CompactStarted { .. }
                    | ConversationDelta::CompactProgress { .. }
                    | ConversationDelta::CompactFinished { .. },
                ) => Some(
                    serde_json::to_value(&envelope.payload).expect("json")["data"]["kind"]
                        .as_str()?
                        .to_string(),
                ),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn subscribe_with_foreign_log_cursor_requires_log_id_reset() {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = CanonicalSessionIdentity::open_or_create(dir.path()).expect("identity");
        let (_, changed) = append_two_facts(dir.path(), &identity);
        let hub = Arc::new(V2ProjectionHub::new("epoch-1"));
        let foreign =
            CanonicalCursor::snapshot("0198f1a0-0000-7000-8000-0000000000ff", changed.fact_seq);
        let token = CursorToken::encode_snapshot(&foreign).expect("token");
        let mut subscription = hub
            .subscribe(dir.path(), "seed", Some(&token))
            .expect("subscribe");
        match subscription.next().await {
            V2StreamItem::Reset(reset) => {
                assert_eq!(reset.reason, RingingV2ResetReason::LogIdMismatch);
                assert_eq!(reset.session_id, "seed");
            }
            other => panic!("expected log_id reset, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn subscribe_beyond_last_fact_requires_unknown_fact_reset() {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = CanonicalSessionIdentity::open_or_create(dir.path()).expect("identity");
        let (_, changed) = append_two_facts(dir.path(), &identity);
        let hub = Arc::new(V2ProjectionHub::new("epoch-1"));
        let ahead = CanonicalCursor::snapshot(identity.log_id.as_str(), changed.fact_seq + 10);
        let token = CursorToken::encode_snapshot(&ahead).expect("token");
        let mut subscription = hub
            .subscribe(dir.path(), "seed", Some(&token))
            .expect("subscribe");
        match subscription.next().await {
            V2StreamItem::Reset(reset) => {
                assert_eq!(reset.reason, RingingV2ResetReason::UnknownFact);
                assert!(reset.snapshot_cursor.is_some());
            }
            other => panic!("expected unknown_fact reset, got {other:?}"),
        }
    }
}
