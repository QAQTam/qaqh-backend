//! Ringing v2 typed client surface.
//!
//! The transport methods are additive to the v1 client. They use the same
//! discovery credentials, but negotiate and carry a separate v2 lease so v1
//! and v2 identities never alias during the compatibility window.

use std::fmt;

use bytes::Bytes;
use futures_util::stream::{BoxStream, StreamExt};
use qaqh_ringing::{
    CursorToken, RingingCommandAck, RingingCommandStatus, RingingV2Bootstrap,
    RingingV2Capabilities, RingingV2CommandAck, RingingV2CommandEnvelope, RingingV2CommandStatus,
    RingingV2DriverClaimResponse, RingingV2DriverReleaseResponse, RingingV2EventEnvelope,
    RingingV2LeaseRenewResponse, RingingV2OpenRequest, RingingV2OpenResponse,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::client::{Client, ClientOptions};
use crate::error::{ClientError, Result};
use crate::sse_decoder::SseDecoder;
use crate::types::{CommandOptions, RingingCommand, TimelinePage};

pub use qaqh_ringing::{
    CanonicalCursor as ClientV2Cursor, CursorToken as ClientV2CursorToken,
    END_OF_FACT as CLIENT_V2_END_OF_FACT, RINGING_V2_BASE_PATH, RINGING_V2_VERSION,
    RingingV2AskOutcome as ClientV2AskOutcome, RingingV2CommandAck as ClientV2CommandAck,
    RingingV2CommandResult as ClientV2CommandResult,
    RingingV2CommandStatus as ClientV2CommandStatus, RingingV2DriverState as ClientV2DriverState,
    RingingV2ExistingResult as ClientV2ExistingResult,
    RingingV2InteractionKind as ClientV2InteractionKind,
    RingingV2PendingInteraction as ClientV2PendingInteraction,
    RingingV2PendingSet as ClientV2PendingSet, RingingV2ResetReason as ClientV2ResetReason,
    RingingV2ResetRequired as ClientV2Reset,
};

/// Typed canonical projection payload exposed to shells through qaqh-client.
pub type ClientV2Payload = qaqh_session::session_fact_v2::ProjectionPayload;

/// Typed v2 SSE envelope.
pub type ClientV2Event = RingingV2EventEnvelope<ClientV2Payload>;

/// Alias kept explicit for shells that prefer the full contract name.
pub type ClientV2EventEnvelope = ClientV2Event;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClientV2ControlState {
    pub session_id: Option<qaqh_session::session_fact_v2::SessionId>,
    pub activity: qaqh_session::session_fact_v2::ActivityState,
    pub current_turn_id: Option<qaqh_session::session_fact_v2::TurnId>,
    pub current_call_id: Option<qaqh_session::session_fact_v2::ToolCallId>,
    pub round: Option<qaqh_session::projection::ControlRoundState>,
    pub tools: Vec<qaqh_session::projection::ControlToolState>,
    pub subagents: Vec<qaqh_session::projection::ControlSubagentState>,
    pub last_recovery: Option<qaqh_session::session_fact_v2::RecoveryOutcome>,
    pub revision: u64,
    pub last_fact_seq: u64,
    pub interactions: Vec<ClientV2PendingInteraction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<ClientV2DriverState>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClientV2ToolState {
    pub tools: Vec<qaqh_session::projection::ControlToolState>,
}

pub type ClientV2ConversationState = qaqh_session::projection::ConversationSnapshot;

/// Typed authoritative v2 bootstrap.
pub type ClientV2Bootstrap =
    RingingV2Bootstrap<ClientV2ControlState, ClientV2ConversationState, ClientV2ToolState>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientV2SessionState {
    pub client_instance_id: String,
    pub client_session_id: String,
    pub server_epoch: String,
    pub lease_ttl_ms: u64,
    pub renew_interval_ms: u64,
    pub capabilities: RingingV2Capabilities,
}

impl ClientV2SessionState {
    fn from_open(client_instance_id: String, response: RingingV2OpenResponse) -> Result<Self> {
        response
            .validate()
            .map_err(|code| ClientError::Protocol(format!("invalid v2 open response: {code}")))?;
        if !response.accepted {
            return Err(ClientError::Negotiation(
                "daemon rejected v2 open request".into(),
            ));
        }
        Ok(Self {
            client_instance_id,
            client_session_id: response.client_session_id,
            server_epoch: response.server_epoch,
            lease_ttl_ms: response.lease_ttl_ms,
            renew_interval_ms: response.renew_interval_ms,
            capabilities: response.capabilities,
        })
    }
}

#[derive(Debug, Clone)]
pub enum ClientV2SubscriptionEvent {
    Event(Box<ClientV2Event>),
    Reset(ClientV2Reset),
}

pub struct ClientV2Subscription {
    stream: BoxStream<'static, std::result::Result<Bytes, reqwest::Error>>,
    decoder: SseDecoder,
}

impl fmt::Debug for ClientV2Subscription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientV2Subscription")
            .finish_non_exhaustive()
    }
}

impl ClientV2Subscription {
    pub async fn next(&mut self) -> Result<Option<ClientV2SubscriptionEvent>> {
        loop {
            if let Some(frame) = self.decoder.next_frame() {
                let frame = frame
                    .map_err(|()| ClientError::Protocol("invalid UTF-8 in v2 SSE frame".into()))?;
                if frame.event_type == "ringing.reset_required" {
                    let reset: ClientV2Reset = serde_json::from_str(&frame.data)?;
                    reset.validate().map_err(|code| {
                        ClientError::Protocol(format!("invalid v2 reset: {code}"))
                    })?;
                    return Ok(Some(ClientV2SubscriptionEvent::Reset(reset)));
                }
                let event: ClientV2Event = serde_json::from_str(&frame.data)?;
                event
                    .validate()
                    .map_err(|code| ClientError::Protocol(format!("invalid v2 event: {code}")))?;
                return Ok(Some(ClientV2SubscriptionEvent::Event(Box::new(event))));
            }

            match self.stream.next().await {
                Some(Ok(bytes)) => self.decoder.push(&bytes),
                Some(Err(error)) => return Err(error.into()),
                None => return Ok(None),
            }
        }
    }
}

impl Client {
    /// Connect through the existing discovery path and negotiate a v2 lease.
    pub async fn connect_v2_async(options: ClientOptions) -> Result<Client> {
        let client = Self::connect_async(options).await?;
        client.open_v2().await?;
        Ok(client)
    }

    /// `POST /ringing/v2/clients/open`.
    pub async fn open_v2(&self) -> Result<RingingV2OpenResponse> {
        self.open_v2_with_instance_id(uuid::Uuid::new_v4().to_string())
            .await
    }

    pub async fn open_v2_with_instance_id(
        &self,
        client_instance_id: impl Into<String>,
    ) -> Result<RingingV2OpenResponse> {
        let client_instance_id = client_instance_id.into();
        let request = RingingV2OpenRequest::new(client_instance_id.clone());
        let path = format!("{RINGING_V2_BASE_PATH}/clients/open");
        let response = self
            .inner
            .http
            .post(format!("{}{path}", self.credentials().base_url))
            .bearer_auth(&self.credentials().token)
            .json(&request)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(api_error(response, &path).await);
        }
        let response: RingingV2OpenResponse = response.json().await?;
        let state = ClientV2SessionState::from_open(client_instance_id, response.clone())?;
        *self.inner.v2_session.lock().await = Some(state);
        Ok(response)
    }

    pub async fn v2_session_state(&self) -> Option<ClientV2SessionState> {
        self.inner.v2_session.lock().await.clone()
    }

    /// `POST /ringing/v2/leases/renew`.
    pub async fn renew_lease_v2(&self) -> Result<RingingV2LeaseRenewResponse> {
        let state = self.require_v2_session().await?;
        let path = format!("{RINGING_V2_BASE_PATH}/leases/renew");
        let response = self
            .inner
            .http
            .post(format!("{}{path}", self.credentials().base_url))
            .bearer_auth(&self.credentials().token)
            .header("X-QAQH-Client-Session-Id", &state.client_session_id)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(api_error(response, &path).await);
        }
        Ok(response.json().await?)
    }

    /// `GET /ringing/v2/sessions/{seed}/bootstrap`.
    pub async fn bootstrap_v2(&self, seed: &str) -> Result<ClientV2Bootstrap> {
        let state = self.require_v2_session().await?;
        let path = format!("{RINGING_V2_BASE_PATH}/sessions/{seed}/bootstrap");
        let response = self
            .inner
            .http
            .get(format!("{}{path}", self.credentials().base_url))
            .bearer_auth(&self.credentials().token)
            .header("X-QAQH-Client-Session-Id", &state.client_session_id)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(api_error(response, &path).await);
        }
        let bootstrap: ClientV2Bootstrap = response.json().await?;
        bootstrap
            .validate()
            .map_err(|code| ClientError::Protocol(format!("invalid v2 bootstrap: {code}")))?;
        Ok(bootstrap)
    }

    /// Open the typed v2 per-seed SSE subscription.
    ///
    /// 2026-09-24 冻结修订（硬切）：**每 seed 一条流**，不再按 channel 分订阅。
    /// 事件带 `stream_key`，调用方自行 demux。旧的三条 `subscribe_v2(seed,
    /// channel, …)` 形态已删除；调用方必须在 `open` 响应里断言
    /// `capabilities.single_stream == true`。
    pub async fn subscribe_v2(
        &self,
        seed: &str,
        since_cursor: Option<&CursorToken>,
    ) -> Result<ClientV2Subscription> {
        let state = self.require_v2_session().await?;
        let path = qaqh_ringing::events_path(seed);
        let mut request = self
            .inner
            .http
            .get(format!("{}{path}", self.credentials().base_url))
            .bearer_auth(&self.credentials().token)
            .header("X-QAQH-Client-Session-Id", &state.client_session_id)
            .header("Accept", "text/event-stream");
        if let Some(cursor) = since_cursor {
            request = request.query(&[("since_cursor", cursor.as_str())]);
        }
        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(api_error(response, &path).await);
        }
        Ok(ClientV2Subscription {
            stream: response.bytes_stream().boxed(),
            decoder: SseDecoder::new(),
        })
    }

    /// Submit a v2 command with an explicit v2 lease identity.
    pub async fn send_command_v2(
        &self,
        seed: Option<&str>,
        command: RingingCommand,
        options: CommandOptions,
    ) -> Result<RingingCommandAck> {
        Ok(self
            .send_command_v2_typed(seed, command, options)
            .await?
            .into_v1())
    }

    /// v2 command submission that keeps the typed `existing` receipt returned
    /// for an idempotent `command_id` replay. `send_command_v2` remains as the
    /// v1-shaped compatibility surface.
    pub async fn send_command_v2_typed(
        &self,
        seed: Option<&str>,
        command: RingingCommand,
        options: CommandOptions,
    ) -> Result<RingingV2CommandAck> {
        let state = self.require_v2_session().await?;
        let command_id = options
            .command_id
            .unwrap_or_else(qaqh_session::canonical::generate_ulid);
        let mut payload =
            RingingV2CommandEnvelope::new(command_id.clone(), state.client_instance_id, command)
                .with_client_session_id(state.client_session_id);
        if let Some(seed) = seed {
            payload = payload.with_seed(seed);
        }
        payload.expected_revision = options.expected_revision;
        payload.driver_epoch = options.driver_epoch;
        payload
            .validate()
            .map_err(|code| ClientError::Protocol(format!("invalid v2 command: {code}")))?;
        let path = format!(
            "{RINGING_V2_BASE_PATH}/commands/{}",
            payload.channel.as_str()
        );
        let response = self
            .inner
            .http
            .post(format!("{}{path}", self.credentials().base_url))
            .bearer_auth(&self.credentials().token)
            .json(&payload)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(api_error(response, &path).await);
        }
        let ack: RingingV2CommandAck = response.json().await?;
        if ack.command_id != command_id {
            return Err(ClientError::Protocol(
                "v2 command ack id does not match submission".into(),
            ));
        }
        Ok(ack)
    }

    /// `GET /ringing/v2/commands/{command_id}`.
    pub async fn command_status_v2(&self, command_id: &str) -> Result<RingingCommandStatus> {
        Ok(self.command_status_v2_typed(command_id).await?.into_v1())
    }

    /// v2 command status that keeps the typed terminal `result` payload.
    pub async fn command_status_v2_typed(
        &self,
        command_id: &str,
    ) -> Result<RingingV2CommandStatus> {
        let state = self.require_v2_session().await?;
        let path = format!("{RINGING_V2_BASE_PATH}/commands/{command_id}");
        let response = self
            .inner
            .http
            .get(format!("{}{path}", self.credentials().base_url))
            .bearer_auth(&self.credentials().token)
            .header("X-QAQH-Client-Session-Id", &state.client_session_id)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(api_error(response, &path).await);
        }
        Ok(response.json().await?)
    }

    /// `POST /ringing/v2/sessions/{seed}/driver/claim`.
    pub async fn claim_driver(&self, seed: &str) -> Result<RingingV2DriverClaimResponse> {
        let state = self.require_v2_session().await?;
        let path = format!("{RINGING_V2_BASE_PATH}/sessions/{seed}/driver/claim");
        let response = self
            .inner
            .http
            .post(format!("{}{path}", self.credentials().base_url))
            .bearer_auth(&self.credentials().token)
            .header("X-QAQH-Client-Session-Id", &state.client_session_id)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(api_error(response, &path).await);
        }
        Ok(response.json().await?)
    }

    /// `POST /ringing/v2/sessions/{seed}/driver/release`.
    pub async fn release_driver(&self, seed: &str) -> Result<RingingV2DriverReleaseResponse> {
        let state = self.require_v2_session().await?;
        let path = format!("{RINGING_V2_BASE_PATH}/sessions/{seed}/driver/release");
        let response = self
            .inner
            .http
            .post(format!("{}{path}", self.credentials().base_url))
            .bearer_auth(&self.credentials().token)
            .header("X-QAQH-Client-Session-Id", &state.client_session_id)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(api_error(response, &path).await);
        }
        Ok(response.json().await?)
    }

    /// `GET /ringing/v2/sessions/{seed}/timeline`.
    pub async fn timeline_v2(
        &self,
        seed: &str,
        before_index: Option<usize>,
        limit: Option<usize>,
    ) -> Result<TimelinePage> {
        let state = self.require_v2_session().await?;
        let path = format!("{RINGING_V2_BASE_PATH}/sessions/{seed}/timeline");
        let mut query = Vec::new();
        if let Some(before_index) = before_index {
            query.push(("before_index", before_index.to_string()));
        }
        if let Some(limit) = limit {
            query.push(("limit", limit.to_string()));
        }
        let response = self
            .inner
            .http
            .get(format!("{}{path}", self.credentials().base_url))
            .bearer_auth(&self.credentials().token)
            .header("X-QAQH-Client-Session-Id", &state.client_session_id)
            .query(&query)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(api_error(response, &path).await);
        }
        Ok(response.json().await?)
    }

    /// Typed v2 service RPC. The caller selects the response type; control
    /// flow never falls back to `serde_json::Value`.
    pub async fn service_v2<P, T>(&self, method: &str, params: P) -> Result<T>
    where
        P: Serialize,
        T: DeserializeOwned,
    {
        let state = self.require_v2_session().await?;
        let path = format!("{RINGING_V2_BASE_PATH}/service/{method}");
        let response = self
            .inner
            .http
            .post(format!("{}{path}", self.credentials().base_url))
            .bearer_auth(&self.credentials().token)
            .header("X-QAQH-Client-Session-Id", &state.client_session_id)
            .json(&params)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(api_error(response, &path).await);
        }
        Ok(response.json().await?)
    }

    /// `GET /ringing/v2/content/{content_id}`.
    pub async fn content_v2(&self, content_id: &str) -> Result<Bytes> {
        let state = self.require_v2_session().await?;
        let path = format!("{RINGING_V2_BASE_PATH}/content/{content_id}");
        let response = self
            .inner
            .http
            .get(format!("{}{path}", self.credentials().base_url))
            .bearer_auth(&self.credentials().token)
            .header("X-QAQH-Client-Session-Id", &state.client_session_id)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(api_error(response, &path).await);
        }
        Ok(response.bytes().await?)
    }

    async fn require_v2_session(&self) -> Result<ClientV2SessionState> {
        self.inner
            .v2_session
            .lock()
            .await
            .clone()
            .ok_or_else(|| ClientError::Negotiation("v2 session not open".into()))
    }
}

#[derive(Debug, Deserialize)]
struct ApiErrorBody {
    code: Option<String>,
    message: Option<String>,
}

async fn api_error(response: reqwest::Response, path: &str) -> ClientError {
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    if let Ok(parsed) = serde_json::from_str::<ApiErrorBody>(&body)
        && let Some(code) = parsed.code
    {
        return ClientError::Api {
            status,
            code,
            message: parsed.message.unwrap_or(body),
        };
    }
    ClientError::Http {
        status,
        path: path.to_string(),
    }
}

/// Compatibility aliases for shells that want to build typed v2 events in
/// reducer tests without importing `qaqh-ringing` directly.
pub use qaqh_ringing::{
    RingingV2Capabilities as ClientV2Capabilities, RingingV2Delivery as ClientV2Delivery,
    RingingV2StreamKey as ClientV2StreamKey,
};

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_ringing::RingingV2Delivery;

    #[test]
    fn public_v2_surface_is_typed() {
        let cursor = ClientV2CursorToken::encode_snapshot(&ClientV2Cursor::snapshot("log-1", 7))
            .expect("cursor");
        assert_eq!(cursor.decode_snapshot().expect("decode").fact_seq, 7);
        assert_eq!(ClientV2Delivery::Reliable, RingingV2Delivery::Reliable);
        assert_eq!(CLIENT_V2_END_OF_FACT, u16::MAX);
        assert_eq!(RINGING_V2_VERSION, 2);
        assert_eq!(RINGING_V2_BASE_PATH, "/ringing/v2");
    }

    #[test]
    fn v2_reset_reason_is_stable() {
        let reason = ClientV2ResetReason::CursorExpired;
        assert_eq!(
            serde_json::to_string(&reason).expect("json"),
            "\"cursor_expired\""
        );
    }

    #[test]
    fn interaction_and_driver_contracts_are_nameable() {
        let pending = ClientV2PendingInteraction {
            interaction_id: "i1".into(),
            call_id: "c1".into(),
            turn_id: "t1".into(),
            kind: ClientV2InteractionKind::PlanReview,
        };
        let set: ClientV2PendingSet = vec![pending];
        let driver = ClientV2DriverState {
            holder: None,
            driver_epoch: 0,
            can_claim: true,
        };
        assert_eq!(set.len(), 1);
        assert!(driver.can_claim);
    }
}
