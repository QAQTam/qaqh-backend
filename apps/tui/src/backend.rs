//! Thin bridge over `qaqh-client` for the TUI.
//!
//! Owns the client handle and the UI-facing event channel. All daemon
//! discovery / launching / Ringing transport semantics stay inside
//! `qaqh-client`; this module only selects the closed variants the TUI
//! is allowed to use. The TUI never exposes a tool-mode switch, and both
//! create and resume paths pin `minimal:dsh` — so the shell is locked to
//! the two-tool minimal preset by construction.

use std::sync::Arc;
use std::time::Duration;

use qaqh_client::endpoint::{ActionRequest, QueryRequest};
use qaqh_client::error::Result as ClientResult;
use qaqh_client::types::{
    CommandOptions, EventBatch, RingingCommand, RingingCommandAck, TimelineEntry, TimelinePage,
    TimelineStatus,
};
use qaqh_client::{Client, ClientHandlers, ClientOptions};
use qaqh_domain::{ControlCommand, ConversationCommand, SessionState};
use tokio::sync::mpsc;

/// The only tool mode this shell supports. `minimal:dsh` = the two-tool
/// minimal preset (bash + str_replace_editor, model-facing projection).
pub const LOCKED_TOOL_MODE: &str = "minimal:dsh";

/// Events forwarded from client background tasks to the UI loop.
pub enum UiEvent {
    TimelineEntry(String, TimelineEntry),
    TimelineStatus(TimelineStatus),
    TimelineSnapshot(TimelinePage),
    /// Control-channel batch (session lifecycle; used to resolve the seed of
    /// a freshly created session).
    ControlBatch(EventBatch),
}

/// One row of the `session.list` payload relevant to the selector.
#[derive(Debug, Clone)]
pub struct SessionItem {
    pub seed: String,
    pub summary: String,
    pub updated_at: u64,
    pub turn_count: usize,
    pub tool_mode: String,
    pub archived: bool,
    pub running: bool,
}

pub struct QaqhBridge {
    pub client: Client,
    pub rx: mpsc::UnboundedReceiver<UiEvent>,
}

impl QaqhBridge {
    /// Connect to (or launch) the daemon and wire timeline callbacks to a
    /// UI event channel. Uses the async entry point so the TUI runtime owns
    /// every transport task.
    pub async fn connect() -> ClientResult<Self> {
        let (tx, rx) = mpsc::unbounded_channel();

        let entry_tx = tx.clone();
        let status_tx = tx.clone();
        let snapshot_tx = tx.clone();
        let batch_tx = tx.clone();
        let handlers = ClientHandlers {
            on_timeline_entry: Arc::new(move |seed, entry| {
                let _ = entry_tx.send(UiEvent::TimelineEntry(seed, entry));
            }),
            on_timeline_status: Arc::new(move |status| {
                let _ = status_tx.send(UiEvent::TimelineStatus(status));
            }),
            on_timeline_snapshot: Arc::new(move |page| {
                let _ = snapshot_tx.send(UiEvent::TimelineSnapshot(page));
            }),
            on_batch: Arc::new(move |batch| {
                let _ = batch_tx.send(UiEvent::ControlBatch(batch));
            }),
            ..ClientHandlers::default()
        };

        let client = Client::connect_async(ClientOptions {
            handlers,
            launch_daemon_if_missing: true,
            daemon_path: None,
            start_timeout: Duration::from_secs(30),
        })
        .await?;

        Ok(Self { client, rx })
    }

    pub async fn list_sessions(&self) -> ClientResult<Vec<SessionItem>> {
        let value = self.client.query(QueryRequest::SessionList).await?;
        let Some(array) = value.as_array() else {
            return Ok(Vec::new());
        };
        let mut items: Vec<SessionItem> = array
            .iter()
            .filter_map(|entry| {
                Some(SessionItem {
                    seed: entry.get("seed")?.as_str()?.to_string(),
                    summary: entry
                        .get("last_summary")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    updated_at: entry.get("updated_at")?.as_u64()?,
                    turn_count: entry.get("turn_count")?.as_u64()? as usize,
                    tool_mode: entry
                        .get("tool_mode")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    archived: entry.get("archived").and_then(|v| v.as_bool()).unwrap_or(false),
                    running: entry.get("running").and_then(|v| v.as_bool()).unwrap_or(false),
                })
            })
            .collect();
        // Newest activity first; the TUI renders the list top-down.
        items.sort_by_key(|item| std::cmp::Reverse(item.updated_at));
        Ok(items)
    }

    /// Create a fresh session via the Ringing control envelope, pre-locked
    /// to `minimal:dsh`. The daemon persists the tool mode before spawning
    /// the worker, so the minimal system prompt is active from turn one.
    /// The new seed arrives on the control channel (`ControlBatch` event);
    /// use [`Self::created_seed`] to extract it.
    pub async fn create_session(&self, cwd: Option<String>) -> ClientResult<RingingCommandAck> {
        self.client
            .send_command(
                None,
                RingingCommand::Control(ControlCommand::SessionCreate {
                    close_current: false,
                    cwd,
                    tool_mode: Some(LOCKED_TOOL_MODE.to_string()),
                    custom_tools: Vec::new(),
                }),
                CommandOptions::default(),
            )
            .await
    }

    /// Extract the seed of a just-created session from a control batch.
    pub fn created_seed(batch: &EventBatch) -> Option<String> {
        use qaqh_client::types::{Channel, RingingEvent};
        if batch.channel != Channel::Control {
            return None;
        }
        for envelope in &batch.envelopes {
            if let RingingEvent::Control(qaqh_domain::ControlEvent::SessionStateChanged {
                state: SessionState::Created,
                ..
            }) = &envelope.event
            {
                return Some(envelope.seed.clone());
            }
        }
        None
    }

    /// Attach a seed to this client lease (`session_resume`).
    pub async fn attach(&self, seed: &str) -> ClientResult<RingingCommandAck> {
        self.client.attach(seed).await
    }

    /// Re-pin an existing session to the locked tool mode. Persisted by the
    /// daemon and applied to the worker (source-of-truth tool filtering).
    pub async fn lock_minimal(&self, seed: &str) -> ClientResult<serde_json::Value> {
        self.client
            .action(ActionRequest::SessionSetToolMode {
                seed: seed.to_string(),
                tool_mode: LOCKED_TOOL_MODE.to_string(),
                custom_tools: Vec::new(),
            })
            .await
    }

    /// Fetch the authoritative tail snapshot and start the per-session
    /// timeline SSE stream (events arrive on `rx`).
    pub async fn activate_timeline(&self, seed: &str) -> ClientResult<TimelinePage> {
        self.client.activate_timeline(seed).await
    }

    /// Send one user message to the attached session.
    pub async fn send_message(&self, seed: &str, text: &str) -> ClientResult<RingingCommandAck> {
        self.client
            .send_command(
                Some(seed),
                RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
                    text: text.to_string(),
                    images: Vec::new(),
                    attachments: None,
                    as_system: false,
                }),
                CommandOptions::default(),
            )
            .await
    }

    /// Cancel the running turn (best effort; the TUI keeps working if it
    /// fails because the turn may already be sealed).
    pub async fn cancel_turn(&self, seed: &str) -> ClientResult<RingingCommandAck> {
        self.client
            .send_command(
                Some(seed),
                RingingCommand::Conversation(ConversationCommand::ConversationCancel {
                    turn_id: None,
                }),
                CommandOptions::default(),
            )
            .await
    }

    /// Stop transport tasks. The daemon itself is left running, mirroring
    /// the desktop shell behaviour.
    pub fn shutdown(&self) {
        self.client.close();
    }
}
