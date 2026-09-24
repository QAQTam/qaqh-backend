//! Canonical Ringing v2 projection hub.
//!
//! The hub is installed as a [`ProjectionSink`]. It keeps a rebuildable
//! projection state per canonical session, publishes committed projection
//! events to live subscribers, and replays from the committed canonical log.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use qaqh_domain::RingingChannel;
use qaqh_ringing::{
    CanonicalCursor, CursorToken, RingingV2Delivery, RingingV2EventEnvelope, RingingV2ResetReason,
    RingingV2ResetRequired, RingingV2StreamKey,
};
use qaqh_session::canonical::{
    CANONICAL_IDENTITY_FILE, CanonicalSessionIdentity, CommittedFactReader, EVENTS_FILE,
};
use qaqh_session::projection::{
    ControlInteractionState, Projection, ProjectionSet, ProjectionSetSnapshot, ProjectionSink,
    projection_events_for_fact,
};
use qaqh_session::session_fact_v2::{
    Delivery, LogId, ProjectionEvent, ProjectionPayload, SessionFact, SessionId, StreamKey,
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
            Self::SessionMissing(seed) => write!(formatter, "session {seed} has no canonical log"),
            Self::SnapshotMissing(seed) => {
                write!(formatter, "session {seed} has no committed snapshot cursor")
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
    pub seed: String,
    pub log_id: LogId,
    pub snapshot_cursor: CursorToken,
    pub last_fact_seq: u64,
    pub projections: ProjectionSetSnapshot,
}

pub struct V2ProjectionHub {
    epoch: String,
    sessions: RwLock<HashMap<SessionId, Arc<V2Session>>>,
}

struct V2Session {
    state: Mutex<V2SessionState>,
}

struct V2SessionState {
    last_fact_seq: u64,
    projections: ProjectionSet,
    live_tx: broadcast::Sender<V2Envelope>,
}

pub struct V2Subscription {
    channel: RingingChannel,
    replay: VecDeque<V2Envelope>,
    live_rx: broadcast::Receiver<V2Envelope>,
    server_epoch: String,
    seed: String,
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
            sessions: RwLock::new(HashMap::new()),
        }
    }

    pub fn server_epoch(&self) -> &str {
        &self.epoch
    }

    /// Install this hub as the process-wide canonical projection sink.
    pub fn install(self: &Arc<Self>) -> Result<(), Arc<dyn ProjectionSink>> {
        qaqh_session::projection::install_projection_sink(self.clone())
    }

    pub fn bootstrap(
        &self,
        session_dir: impl AsRef<Path>,
        seed: &str,
    ) -> Result<V2BootstrapSnapshot, V2HubError> {
        let session_dir = session_dir.as_ref();
        let (session_id, log_id) = resolve_identity(session_dir, seed)?;
        let session = self.session_for(session_dir, session_id, log_id.clone())?;
        let state = session
            .state
            .lock()
            .map_err(|_| V2HubError::Canonical("v2 session lock poisoned".into()))?;
        if state.last_fact_seq == 0 {
            return Err(V2HubError::SnapshotMissing(seed.to_string()));
        }
        let cursor = CanonicalCursor::snapshot(log_id.as_str(), state.last_fact_seq);
        let snapshot_cursor = CursorToken::encode_snapshot(&cursor)
            .map_err(|error| V2HubError::InvalidCursor(error.to_string()))?;
        Ok(V2BootstrapSnapshot {
            server_epoch: self.epoch.clone(),
            seed: seed.to_string(),
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
        seed: &str,
    ) -> Result<Vec<ControlInteractionState>, V2HubError> {
        let session_dir = session_dir.as_ref();
        let (session_id, log_id) = resolve_identity(session_dir, seed)?;
        let session = self.session_for(session_dir, session_id, log_id)?;
        let state = session
            .state
            .lock()
            .map_err(|_| V2HubError::Canonical("v2 session lock poisoned".into()))?;
        Ok(state.projections.control.snapshot().interactions)
    }

    pub fn subscribe(
        &self,
        session_dir: impl AsRef<Path>,
        seed: &str,
        channel: RingingChannel,
        since_cursor: Option<&CursorToken>,
    ) -> Result<V2Subscription, V2HubError> {
        let session_dir = session_dir.as_ref();
        let (session_id, log_id) = resolve_identity(session_dir, seed)?;
        let session = self.session_for(session_dir, session_id.clone(), log_id.clone())?;
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
                            seed: seed.to_string(),
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
                            seed: seed.to_string(),
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

        let replay = if initial_reset.is_some() {
            VecDeque::new()
        } else {
            replay_after(
                session_dir,
                &session_id,
                &log_id,
                &self.epoch,
                since_fact_seq,
            )?
            .into_iter()
            .filter(|event| event_channel(event).is_none_or(|candidate| candidate == channel))
            .collect()
        };
        drop(state);

        Ok(V2Subscription {
            channel,
            replay,
            live_rx,
            server_epoch: self.epoch.clone(),
            seed: seed.to_string(),
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

        let state = load_session_state(session_dir, session_id.clone(), log_id)?;
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
        let seed = session_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let Ok(session) =
            self.session_for(session_dir, fact.session_id.clone(), fact.log_id.clone())
        else {
            log::error!("[ringing-v2] failed to load session {seed} for projection publish");
            return;
        };
        let Ok(mut state) = session.state.lock() else {
            log::error!("[ringing-v2] projection state lock poisoned for {seed}");
            return;
        };
        if state.last_fact_seq < fact.fact_seq {
            let _ = state.projections.apply(fact);
            state.last_fact_seq = fact.fact_seq;
        }
        for event in events {
            let Some(envelope) = event_to_envelope(&self.epoch, seed, event) else {
                continue;
            };
            let _ = state.live_tx.send(envelope);
        }
    }
}

impl V2Subscription {
    pub async fn next(&mut self) -> V2StreamItem {
        if let Some(reset) = self.initial_reset.take() {
            return V2StreamItem::Reset(reset);
        }
        if let Some(event) = self.replay.pop_front() {
            return V2StreamItem::Event(Box::new(event));
        }
        loop {
            match self.live_rx.recv().await {
                Ok(event) => {
                    if event_channel(&event).is_none_or(|candidate| candidate == self.channel) {
                        return V2StreamItem::Event(Box::new(event));
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    return V2StreamItem::Reset(RingingV2ResetRequired {
                        schema: qaqh_ringing::RINGING_SCHEMA.into(),
                        version: qaqh_ringing::RINGING_V2_VERSION,
                        server_epoch: self.server_epoch.clone(),
                        seed: self.seed.clone(),
                        log_id: Some(self.log_id.as_str().to_string()),
                        snapshot_cursor: None,
                        reason: RingingV2ResetReason::ReplayOverflow,
                    });
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return V2StreamItem::Reset(RingingV2ResetRequired {
                        schema: qaqh_ringing::RINGING_SCHEMA.into(),
                        version: qaqh_ringing::RINGING_V2_VERSION,
                        server_epoch: self.server_epoch.clone(),
                        seed: self.seed.clone(),
                        log_id: Some(self.log_id.as_str().to_string()),
                        snapshot_cursor: None,
                        reason: RingingV2ResetReason::PerConnectionOverflow,
                    });
                }
            }
        }
    }
}

fn resolve_identity(session_dir: &Path, seed: &str) -> Result<(SessionId, LogId), V2HubError> {
    if !session_dir.join(CANONICAL_IDENTITY_FILE).exists()
        && !session_dir.join(EVENTS_FILE).exists()
    {
        return Err(V2HubError::SessionMissing(seed.to_string()));
    }
    let identity = CanonicalSessionIdentity::open_or_create(session_dir)
        .map_err(|error| V2HubError::Canonical(error.to_string()))?;
    Ok((identity.session_id, identity.log_id))
}

fn load_session_state(
    session_dir: &Path,
    session_id: SessionId,
    log_id: LogId,
) -> Result<V2SessionState, V2HubError> {
    let facts = CommittedFactReader::open(session_dir, session_id, log_id.clone())
        .map_err(|error| V2HubError::Canonical(error.to_string()))?
        .read_all()
        .map_err(|error| V2HubError::Canonical(error.to_string()))?;
    let last_fact_seq = facts.last().map(|fact| fact.fact_seq).unwrap_or(0);
    let projections = ProjectionSet::rebuild(facts.into_iter());
    let (live_tx, _) = broadcast::channel(LIVE_CAPACITY);
    Ok(V2SessionState {
        last_fact_seq,
        projections,
        live_tx,
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

fn event_to_envelope(
    server_epoch: &str,
    seed: &str,
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
        seed: seed.to_string(),
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
        payload: event.payload.clone(),
    })
}

fn event_channel(event: &V2Envelope) -> Option<RingingChannel> {
    match &event.stream_key {
        RingingV2StreamKey::Channel(channel) => Some(*channel),
        RingingV2StreamKey::Resource { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_session::canonical::{
        CanonicalSessionIdentity, CanonicalSessionStore, WriterId, generate_ulid,
    };
    use qaqh_session::session_fact_v2::{
        EventId, FactPayload, FactSchema, MetadataSource, SessionCreated, SessionFact,
        SessionMetadataChanged, SessionMetadataPatch,
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

    #[test]
    fn hub_bootstraps_from_committed_canonical_facts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = CanonicalSessionIdentity::open_or_create(dir.path()).expect("identity");
        let (created, changed) = append_two_facts(dir.path(), &identity);
        let hub = Arc::new(V2ProjectionHub::new("epoch-1"));
        let bootstrap = hub.bootstrap(dir.path(), "seed").expect("bootstrap");
        assert_eq!(bootstrap.last_fact_seq, changed.fact_seq);
        let _subscription = hub
            .subscribe(
                dir.path(),
                "seed",
                RingingChannel::Control,
                Some(&bootstrap.snapshot_cursor),
            )
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
            .subscribe(dir.path(), "seed", RingingChannel::Control, Some(&token))
            .expect("subscribe");
        match subscription.next().await {
            V2StreamItem::Reset(reset) => {
                assert_eq!(reset.reason, RingingV2ResetReason::LogIdMismatch);
                assert_eq!(reset.seed, "seed");
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
            .subscribe(dir.path(), "seed", RingingChannel::Control, Some(&token))
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
