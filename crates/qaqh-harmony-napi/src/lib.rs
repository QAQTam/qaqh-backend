//! ArkTS bindings for the canonical Ringing V2 remote client.
#![cfg(target_env = "ohos")]

use std::sync::Arc;

use napi_derive_ohos::napi;
use napi_ohos::{Error, Result};
use qaqh_client::types::{CommandOptions, RingingCommand, RingingCommandAckStatus};
use qaqh_client::v2::SessionEventFrame;
use qaqh_client::v2_stream::V2StreamStatus;
use qaqh_client::{
    Client, ClientHandlers, ClientOptions, CommandTarget, QueryRequest, RemoteEndpoint,
    RingingV2ResyncRequired, pair_remote, sanitize_session_list,
};
use tokio::sync::{Mutex, broadcast, watch};

const EVENT_CAPACITY: usize = 256;

#[napi(object)]
pub struct SessionSnapshot {
    pub server_epoch: String,
    /// The next sequence after the events already covered by this snapshot.
    pub next_seq: String,
    pub json: String,
}

#[napi(object)]
pub struct SessionUpdate {
    /// One of event, reset, connecting, open, reconnecting, closed, or lagged.
    pub kind: String,
    /// Absent for lagged, which invalidates all active session views.
    pub session_id: Option<String>,
    pub seq: Option<String>,
    pub server_epoch: Option<String>,
    pub json: Option<String>,
}

#[derive(Clone)]
enum Update {
    Event(String, Arc<SessionEventFrame>),
    Reset(String, RingingV2ResyncRequired),
    Status(String, V2StreamStatus),
}

fn napi_error(error: impl std::fmt::Display) -> Error {
    Error::from_reason(error.to_string())
}

fn encode(value: &impl serde::Serialize) -> Result<String> {
    serde_json::to_string(value).map_err(napi_error)
}

#[napi]
pub async fn pair_device(
    base_url: String,
    pairing_token: String,
    device_name: String,
    tls_fingerprint: Option<String>,
) -> Result<String> {
    let pair = pair_remote(
        &base_url,
        &pairing_token,
        &device_name,
        "harmonyos",
        tls_fingerprint.as_deref(),
    )
    .await
    .map_err(napi_error)?;
    encode(&pair)
}

#[napi]
pub async fn connect_client(
    base_url: String,
    device_token: String,
    tls_fingerprint: Option<String>,
) -> Result<HarmonyClient> {
    let (updates, receiver) = broadcast::channel(EVENT_CAPACITY);
    let handlers = ClientHandlers {
        on_v2_event: {
            let updates = updates.clone();
            Arc::new(move |id, frame| {
                let _ = updates.send(Update::Event(id, Arc::new(frame)));
            })
        },
        on_v2_reset: {
            let updates = updates.clone();
            Arc::new(move |id, reset| {
                let _ = updates.send(Update::Reset(id, reset));
            })
        },
        on_v2_status: Arc::new(move |id, status| {
            let _ = updates.send(Update::Status(id, status));
        }),
        on_liveness: Arc::new(|| {}),
    };
    let inner = Client::connect_async(ClientOptions {
        handlers,
        launch_daemon_if_missing: false,
        remote: Some(RemoteEndpoint {
            base_url,
            token: device_token,
            tls_fp: tls_fingerprint,
        }),
        ..ClientOptions::default()
    })
    .await
    .map_err(napi_error)?;

    Ok(HarmonyClient {
        inner,
        updates: Mutex::new(receiver),
        closed: watch::Sender::new(false),
    })
}

#[napi]
pub struct HarmonyClient {
    inner: Client,
    updates: Mutex<broadcast::Receiver<Update>>,
    closed: watch::Sender<bool>,
}

#[napi]
impl HarmonyClient {
    #[napi]
    pub async fn session_list(&self) -> Result<String> {
        let list = self
            .inner
            .query(QueryRequest::SessionList)
            .await
            .map_err(napi_error)?;
        encode(&sanitize_session_list(list))
    }

    /// Starts the stream only when the daemon accepts the attach command.
    #[napi]
    pub async fn start_session(&self, session_id: String) -> Result<String> {
        let ack = self.inner.attach(&session_id).await.map_err(napi_error)?;
        if ack.status == RingingCommandAckStatus::Accepted {
            self.inner.activate_stream(&session_id).await;
        }
        encode(&ack)
    }

    #[napi]
    pub async fn stop_session(&self, session_id: String) {
        self.inner.deactivate_stream(&session_id).await;
    }

    #[napi]
    pub async fn bootstrap(&self, session_id: String) -> Result<SessionSnapshot> {
        let snapshot = self
            .inner
            .bootstrap(&session_id, None)
            .await
            .map_err(napi_error)?;
        Ok(SessionSnapshot {
            server_epoch: snapshot.server_epoch.clone(),
            next_seq: snapshot.state.seq.to_string(),
            json: encode(&snapshot)?,
        })
    }

    /// Takes the canonical command and target JSON. Rust constructs the envelope.
    #[napi]
    pub async fn send_command(
        &self,
        target_json: String,
        command_json: String,
        command_id: Option<String>,
        preconditions_json: Option<String>,
    ) -> Result<String> {
        let target: CommandTarget = serde_json::from_str(&target_json).map_err(napi_error)?;
        let command: RingingCommand = serde_json::from_str(&command_json).map_err(napi_error)?;
        let preconditions = preconditions_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(napi_error)?
            .unwrap_or_default();
        let options = CommandOptions {
            command_id,
            preconditions,
        };
        let ack = self
            .inner
            .send_command(target, command, options)
            .await
            .map_err(napi_error)?;
        encode(&ack)
    }

    /// Waits for the next buffered update. Returns no update after close.
    #[napi]
    pub async fn next_update(&self) -> Result<Option<SessionUpdate>> {
        let mut receiver = self.updates.lock().await;
        let mut closed = self.closed.subscribe();
        let update = tokio::select! {
            biased;
            _ = closed.wait_for(|closed| *closed) => return Ok(None),
            update = receiver.recv() => update,
        };
        let (kind, session_id, seq, server_epoch, json) = match update {
            Ok(Update::Event(id, frame)) => (
                "event",
                Some(id),
                Some(frame.seq.to_string()),
                None,
                Some(encode(frame.as_ref())?),
            ),
            Ok(Update::Reset(id, reset)) => ("reset", Some(id), None, None, Some(encode(&reset)?)),
            Ok(Update::Status(id, status)) => {
                let (kind, server_epoch) = match status {
                    V2StreamStatus::Connecting => ("connecting", None),
                    V2StreamStatus::Reconnecting { .. } => ("reconnecting", None),
                    V2StreamStatus::Closed { .. } => ("closed", None),
                    V2StreamStatus::Open { server_epoch, .. } => ("open", Some(server_epoch)),
                };
                (kind, Some(id), None, server_epoch, None)
            }
            Err(broadcast::error::RecvError::Lagged(_)) => ("lagged", None, None, None, None),
            Err(broadcast::error::RecvError::Closed) => return Ok(None),
        };
        Ok(Some(SessionUpdate {
            kind: kind.into(),
            session_id,
            seq,
            server_epoch,
            json,
        }))
    }

    #[napi]
    pub fn close(&self) {
        self.closed.send_replace(true);
        self.inner.close();
    }
}

impl Drop for HarmonyClient {
    fn drop(&mut self) {
        self.close();
    }
}
