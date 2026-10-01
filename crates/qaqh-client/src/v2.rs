//! Ringing v2 typed client surface.
//!
//! The transport methods are additive to the v1 client. They use the same
//! discovery credentials, but negotiate and carry a separate v2 lease so v1
//! and v2 identities never alias during the compatibility window.

use std::fmt;

use bytes::Bytes;
use futures_util::stream::{BoxStream, StreamExt};
use qaqh_ringing::{
    CursorToken, RingingV2Bootstrap, RingingV2Capabilities, RingingV2CommandAck,
    RingingV2CommandEnvelope, RingingV2CommandStatus, RingingV2DriverClaimResponse,
    RingingV2DriverReleaseResponse, RingingV2EventEnvelope, RingingV2LeaseRenewResponse,
    RingingV2OpenRequest, RingingV2OpenResponse,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::client::Client;
use crate::error::{ClientError, Result};
use crate::sse_decoder::SseDecoder;
use crate::types::{CommandOptions, RingingCommand, SseFrame};

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

/// Typed control delta (#323 缺口 1).
///
/// 壳层必须能 `match ClientV2ControlDelta::InteractionRequested { .. }` —— match
/// 枚举变体**必须写出枚举名**，所以只导出 `ClientV2Payload` 不够。
///
/// 命名注意：这里的 `kind` 字段是 [`ClientV2DeltaInteractionKind`]（wire 值
/// `ask` / `plan` / `permission`），与 bootstrap 的
/// [`ClientV2PendingInteraction`]`.kind`（[`ClientV2InteractionKind`]，wire 值
/// 同样为 `ask` / `plan` / `permission`）**不是同一个 Rust 枚举**。两者 wire
/// 语义已在 2026-09-24 统一，本别名只让壳层能同时命名它们。
pub use qaqh_session::session_fact_v2::ControlDelta as ClientV2ControlDelta;

/// `ClientV2ControlDelta::SubagentFinished.status`。
pub use qaqh_session::session_fact_v2::SubagentTerminalStatus as ClientV2SubagentTerminalStatus;

/// 会话删除原因（`MetaDelta::Deleted.reason`）。
pub use qaqh_session::session_fact_v2::DeleteReason as ClientV2DeleteReason;
/// 会话回合 id（`ConversationDelta::TurnStarted.turn_id` 等）。
pub use qaqh_session::session_fact_v2::TurnId as ClientV2TurnId;
/// 回合终态（`ConversationDelta::TurnFinished.terminal`）。
pub use qaqh_session::session_fact_v2::TurnTerminal as ClientV2TurnTerminal;

/// `ClientV2Payload::ConversationDelta` 的载荷。
pub use qaqh_session::session_fact_v2::ConversationDelta as ClientV2ConversationDelta;
/// `ClientV2Payload::MetaDelta` 的载荷。
pub use qaqh_session::session_fact_v2::MetaDelta as ClientV2MetaDelta;
/// `ClientV2Payload::ResourceDelta` 的载荷。
pub use qaqh_session::session_fact_v2::ResourceDelta as ClientV2ResourceDelta;

/// `ClientV2ControlDelta::InteractionRequested.kind` 的类型。
pub use qaqh_session::session_fact_v2::InteractionKind as ClientV2DeltaInteractionKind;

/// `ClientV2ControlDelta::InteractionRequested/Resolved/Expired.interaction_id`.
pub use qaqh_session::session_fact_v2::InteractionId as ClientV2InteractionId;

/// `ClientV2ControlDelta::InteractionRequested.call_id`.
pub use qaqh_session::session_fact_v2::ToolCallId as ClientV2ToolCallId;

/// `InteractionRequested.request` / `InteractionResolved.decision` 的载荷
/// （**SSE control delta** 面，canonical 类型）。
pub use qaqh_session::session_fact_v2::ContentValue as ClientV2ContentValue;

/// `ClientV2PendingInteraction.request` 的载荷（**bootstrap** 面）。
///
/// 与 [`ClientV2ContentValue`] 的 JSON 形态一致（`kind` / `data` 判别式），但
/// bootstrap 走 wire 层类型（`qaqh-ringing` 不依赖 session crate），所以是两个
/// 类型；两侧 serde 形态由 daemon 的映射函数保持一致。
pub use qaqh_ringing::RingingV2ContentValue as ClientV2PendingContentValue;

/// `InteractionResolved.verdict`（结构化裁决；legacy fact 为 `None`）。
pub use qaqh_session::session_fact_v2::InteractionDecision as ClientV2InteractionDecision;

/// `InteractionResolved.resolved_by`.
pub use qaqh_session::session_fact_v2::ActorRef as ClientV2ActorRef;

/// `InteractionExpired.reason`。
pub use qaqh_session::session_fact_v2::InteractionExpiryReason as ClientV2InteractionExpiryReason;

/// **Team projection**：TUI/WinUI 的 roster / inbox 唯一权威投影。
///
/// 出处：`docs/current/architecture.md` —— `TeamSnapshot/TeamDelta` 是
/// TUI/WinUI 的唯一 roster/inbox 投影。壳层**不得**再从 `spawn_subagent`
/// 工具卡 JSON 推导 agent 身份。
///
/// 快照来自 `GET /ringing/v2/sessions/{seed}/team`（[`Client::team_v2`]），
/// 之后的增量来自 per-seed 单流的 [`ClientV2Payload::TeamDelta`]。
pub use qaqh_session::projection::TeamSnapshot as ClientV2TeamSnapshot;
/// inbox 消息的投递语义（queue / trigger / interrupt / steer / interject）。
pub use qaqh_session::session_fact_v2::InterAgentDelivery as ClientV2TeamDelivery;
/// agent residency（loaded / unloaded）。**`unloaded != completed != closed`**。
pub use qaqh_session::session_fact_v2::TeamAgentResidency as ClientV2TeamAgentResidency;
/// roster 里一个 agent 的快照：`agent_path` 为主键，`nickname` 只是显示辅助。
pub use qaqh_session::session_fact_v2::TeamAgentSnapshot as ClientV2TeamAgentSnapshot;
/// agent 生命周期状态（idle / running / interrupted / completed）。
pub use qaqh_session::session_fact_v2::TeamAgentStatus as ClientV2TeamAgentStatus;
/// message board 快照（`/team` 的 `board` 字段）。
pub use qaqh_session::session_fact_v2::TeamBoardSnapshot as ClientV2TeamBoardSnapshot;
/// 单流上的 Team 增量。
pub use qaqh_session::session_fact_v2::TeamDelta as ClientV2TeamDelta;
/// inbox 里一条待投递消息的摘要：author / recipient / task / delivery。
pub use qaqh_session::session_fact_v2::TeamInboxSummary as ClientV2TeamInboxSummary;
/// 单条 task（`TeamDelta::TaskChanged.task`）。
pub use qaqh_session::session_fact_v2::TeamTaskSnapshot as ClientV2TeamTaskSnapshot;
/// task board 快照（`/team` 的 `tasks` 字段）。
pub use qaqh_session::team::TaskBoardSnapshot as ClientV2TaskBoardSnapshot;

/// `GET /ringing/v2/sessions/{seed}/team` 的 typed 响应。
///
/// 一次拿到三份快照：roster + inbox（`team`）、task board（`tasks`）、
/// message board（`board`）。Phase 4（roster/inbox）、TEAM-01e（task board）与
/// BOARD-01d（message board）消费的都是这一个端点，避免壳层各拉一次。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientV2TeamResponse {
    pub schema: String,
    #[serde(rename = "session_id")]
    pub session_id: String,
    pub team: ClientV2TeamSnapshot,
    pub tasks: ClientV2TaskBoardSnapshot,
    pub board: ClientV2TeamBoardSnapshot,
}

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

/// `conversation.state.context[].kind`（typed 快照里逐条上下文项）。
pub use qaqh_session::projection::ConversationContextKind as ClientV2ConversationContextKind;
/// `control.state.activity` 的 v2 词汇（idle / running / interrupted）。
pub use qaqh_session::session_fact_v2::ActivityState as ClientV2ActivityState;

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
    pub(crate) fn from_open(
        client_instance_id: String,
        response: RingingV2OpenResponse,
    ) -> Result<Self> {
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
    /// 字节到达但凑不出完整帧（daemon 的 keepalive 注释行 / 跨 chunk 半帧）。
    ///
    /// 把「到达」本身呈现给调用方，v2 流的空闲计时器才能在**字节层**复位：
    /// 会话安静期里 daemon 的 15s keepalive 是唯一字节来源，只按事件复位
    /// 会让健康流每 45s 被误判 `v2 SSE idle timeout` 重连（真机实测 11 次，
    /// 每次还触发 app 层全量 re-bootstrap）。对齐 timeline.rs 的
    /// BUG-2026-09-12-10 字节层探活。
    KeepAlive,
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
                return Self::decode_frame(frame).map(Some);
            }

            match self.stream.next().await {
                Some(Ok(bytes)) => {
                    self.decoder.push(&bytes);
                    match self.decoder.next_frame() {
                        // 这批字节凑出了完整帧：照常解码返回。
                        Some(frame) => return Self::decode_frame(frame).map(Some),
                        // 字节到了但凑不出帧：以 KeepAlive 把「到达」本身报给
                        // 调用方（见枚举上的 KeepAlive 文档）。
                        None => return Ok(Some(ClientV2SubscriptionEvent::KeepAlive)),
                    }
                }
                Some(Err(error)) => return Err(error.into()),
                None => return Ok(None),
            }
        }
    }

    /// 一帧 SSE → 订阅事件。`Err(())` 是解码器的 UTF-8 失败标记。
    fn decode_frame(frame: std::result::Result<SseFrame, ()>) -> Result<ClientV2SubscriptionEvent> {
        let frame =
            frame.map_err(|()| ClientError::Protocol("invalid UTF-8 in v2 SSE frame".into()))?;
        if frame.event_type == "ringing.reset_required" {
            let reset: ClientV2Reset = serde_json::from_str(&frame.data)?;
            reset
                .validate()
                .map_err(|code| ClientError::Protocol(format!("invalid v2 reset: {code}")))?;
            return Ok(ClientV2SubscriptionEvent::Reset(reset));
        }
        let event: ClientV2Event = serde_json::from_str(&frame.data)?;
        event
            .validate()
            .map_err(|code| ClientError::Protocol(format!("invalid v2 event: {code}")))?;
        Ok(ClientV2SubscriptionEvent::Event(Box::new(event)))
    }
}

impl Client {
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
        self.inner.session.adopt_v2_state(state).await;
        Ok(response)
    }

    pub async fn v2_session_state(&self) -> Option<ClientV2SessionState> {
        self.inner.session.state().await
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
    pub async fn bootstrap_v2(&self, session_id: &str) -> Result<ClientV2Bootstrap> {
        let state = self.require_v2_session().await?;
        let path = format!("{RINGING_V2_BASE_PATH}/sessions/{session_id}/bootstrap");
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
        session_id: &str,
        since_cursor: Option<&CursorToken>,
    ) -> Result<ClientV2Subscription> {
        let state = self.require_v2_session().await?;
        let path = qaqh_ringing::events_path(session_id);
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

    /// v2 command submission that keeps the typed `existing` receipt returned
    /// for an idempotent `command_id` replay.
    pub async fn send_command_v2_typed(
        &self,
        session_id: Option<&str>,
        command: RingingCommand,
        options: CommandOptions,
    ) -> Result<RingingV2CommandAck> {
        let state = self.require_v2_session().await?;
        let command_id = options
            .command_id
            .unwrap_or_else(qaqh_session::canonical::generate_ulid);
        let client_session_id = state.client_session_id.clone();
        let mut payload =
            RingingV2CommandEnvelope::new(command_id.clone(), state.client_instance_id, command)
                .with_client_session_id(client_session_id.clone());
        if let Some(session_id) = session_id {
            payload = payload.with_session_id(session_id);
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
            // daemon 的 lease 判定只看 header（信封里的 client_session_id 不参与
            // 鉴权）——漏这个头会稳定拿 401 lease_required。
            .header("X-QAQH-Client-Session-Id", &client_session_id)
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
    pub async fn claim_driver(&self, session_id: &str) -> Result<RingingV2DriverClaimResponse> {
        let state = self.require_v2_session().await?;
        let path = format!("{RINGING_V2_BASE_PATH}/sessions/{session_id}/driver/claim");
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
    pub async fn release_driver(&self, session_id: &str) -> Result<RingingV2DriverReleaseResponse> {
        let state = self.require_v2_session().await?;
        let path = format!("{RINGING_V2_BASE_PATH}/sessions/{session_id}/driver/release");
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

    /// `GET /ringing/v2/sessions/{seed}/team`。
    ///
    /// Team projection 快照：roster（以 `AgentPath` 为主）+ inbox + task board +
    /// message board。deltas 在 per-seed 单流上以
    /// [`ClientV2Payload::TeamDelta`] 到达；壳层应当 **先拉一次快照、再应用
    /// delta**，不要只靠 delta 增量拼状态。
    pub async fn team_v2(&self, session_id: &str) -> Result<ClientV2TeamResponse> {
        let state = self.require_v2_session().await?;
        let path = format!("{RINGING_V2_BASE_PATH}/sessions/{session_id}/team");
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
        self.content_v2_range(content_id, None).await
    }

    /// `GET /ringing/v2/content/{content_id}` with an optional RFC 9110
    /// `Range` header (for example `bytes=0-65535`). The server returns
    /// `206 Partial Content`; this method returns the selected bytes.
    pub async fn content_v2_range(&self, content_id: &str, range: Option<&str>) -> Result<Bytes> {
        let state = self.require_v2_session().await?;
        let path = format!("{RINGING_V2_BASE_PATH}/content/{content_id}");
        let mut request = self
            .inner
            .http
            .get(format!("{}{path}", self.credentials().base_url))
            .bearer_auth(&self.credentials().token)
            .header("X-QAQH-Client-Session-Id", &state.client_session_id);
        if let Some(range) = range {
            request = request.header("Range", range);
        }
        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(api_error(response, &path).await);
        }
        Ok(response.bytes().await?)
    }

    async fn require_v2_session(&self) -> Result<ClientV2SessionState> {
        self.inner
            .session
            .state()
            .await
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

    /// Team projection 的 wire 形状必须和 daemon `/team` 端点逐字对齐。
    ///
    /// 这条测试是 TUI/WinUI Phase 4（roster / inbox）的**契约锁**：壳层不能直接
    /// 依赖 `qaqh-session`（静态门禁 G1），只能经 `qaqh-client` 拿类型，所以
    /// 这里少一个字段/改一个键名，前端就会在编译期或运行期直接断。
    #[test]
    fn team_response_matches_daemon_wire_shape() {
        let payload = serde_json::json!({
            "schema": "qaqh.ringing.team/v1",
            "session_id": "0199a0f0-0000-7000-8000-000000000001",
            "team": {
                "root_session_id": "0199a0f0-0000-7000-8000-000000000001",
                "agents": [
                    {
                        "agent_id": "0199a0f0-0000-7000-8000-000000000001",
                        "agent_path": "/root",
                        "nickname": "main",
                        "status": "running",
                        "residency": "loaded"
                    },
                    {
                        "agent_id": "0199a0f0-0000-7000-8000-000000000002",
                        "agent_path": "/root/writer",
                        "nickname": "writer",
                        "parent_agent_path": "/root",
                        "status": "completed",
                        "residency": "unloaded"
                    }
                ],
                "unread_messages": [
                    {
                        "message_id": "msg-1",
                        "author": "/root/writer",
                        "recipient": "/root",
                        "task_id": "task-1",
                        "delivery": "queue",
                        "created_at_ms": 1_759_000_000_000i64
                    }
                ],
                "revision": 7,
                "last_fact_seq": 42
            },
            "tasks": {
                "root_session_id": null,
                "tasks": [],
                "revision": 0,
                "last_fact_seq": 0
            },
            "board": {
                "revision": 0,
                "last_fact_seq": 0,
                "channels": [],
                "threads": [],
                "posts": [],
                "subscriptions": []
            }
        });

        let response: ClientV2TeamResponse =
            serde_json::from_value(payload).expect("daemon /team 形状必须能反序列化");
        assert_eq!(response.schema, "qaqh.ringing.team/v1");
        assert_eq!(response.session_id, "0199a0f0-0000-7000-8000-000000000001");
        assert_eq!(response.team.agents.len(), 2);

        // roster 以 AgentPath 为主、nickname 为辅。
        let child = &response.team.agents[1];
        assert_eq!(child.agent_path.as_str(), "/root/writer");
        assert_eq!(child.nickname.as_deref(), Some("writer"));
        assert_eq!(
            child.parent_agent_path.as_ref().map(|p| p.as_str()),
            Some("/root")
        );

        // unloaded != completed：两者是**两个正交字段**，前端不能把 unloaded 画成 deleted。
        assert_eq!(child.residency, ClientV2TeamAgentResidency::Unloaded);
        assert_eq!(child.status, ClientV2TeamAgentStatus::Completed);

        // inbox 必须带 author / recipient / task / delivery 四要素。
        let inbox = &response.team.unread_messages[0];
        assert_eq!(inbox.author.as_str(), "/root/writer");
        assert_eq!(inbox.recipient.as_str(), "/root");
        assert_eq!(inbox.task_id.as_deref(), Some("task-1"));
        assert_eq!(
            inbox.delivery,
            qaqh_session::session_fact_v2::InterAgentDelivery::Queue
        );
    }

    /// 壳层必须能**命名** TeamDelta 的变体（这是 TUI 之前做不到的事：类型没导出）。
    #[test]
    fn team_delta_variants_are_nameable_from_the_client_surface() {
        let delta = ClientV2TeamDelta::AgentResidencyChanged {
            revision: 3,
            agent_id: qaqh_session::session_fact_v2::SessionId::new(
                "0199a0f0-0000-7000-8000-000000000002",
            ),
            residency: ClientV2TeamAgentResidency::Unloaded,
        };
        let value = serde_json::to_value(&delta).expect("json");
        assert_eq!(value["kind"], "agent_residency_changed");
        assert_eq!(value["data"]["residency"], "unloaded");

        // 同一个 delta 也能从 `ClientV2Payload` 里取出来并匹配。
        let payload: ClientV2Payload = serde_json::from_value(serde_json::json!({
            "kind": "team_delta",
            "data": value,
        }))
        .expect("payload");
        match payload {
            ClientV2Payload::TeamDelta(ClientV2TeamDelta::AgentResidencyChanged {
                residency,
                ..
            }) => assert_eq!(residency, ClientV2TeamAgentResidency::Unloaded),
            other => panic!("expected TeamDelta, got {other:?}"),
        }
    }

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
            request: Some(ClientV2PendingContentValue::Ref {
                content_ref: "sha256:abc".into(),
            }),
        };
        let set: ClientV2PendingSet = vec![pending];
        let driver = ClientV2DriverState {
            holder: None,
            driver_epoch: 0,
            can_claim: true,
        };
        assert_eq!(set.len(), 1);
        assert_eq!(
            serde_json::to_value(ClientV2InteractionKind::PlanReview).expect("json"),
            serde_json::json!("plan")
        );
        assert!(driver.can_claim);
    }

    // ── 字节层探活（KeepAlive）回归 ─────────────────────────────────────
    //
    // 真机故障：daemon 每 15s 发 SSE 注释 keepalive，但 `next()` 只在产出
    // 事件时返回——安静期里这些字节全部"隐身"，健康 v2 流每 45s 被误判
    // `v2 SSE idle timeout` 重连一次（实测 11 次/20 分钟），每次还触发
    // app 层全量 re-bootstrap。以下两条锁住字节层信号语义。

    /// 从字节块序列构造订阅（不碰网络）。
    fn subscription_with_chunks(chunks: Vec<Vec<u8>>) -> ClientV2Subscription {
        ClientV2Subscription {
            stream: futures_util::stream::iter(
                chunks.into_iter().map(|chunk| Ok(Bytes::from(chunk))),
            )
            .boxed(),
            decoder: SseDecoder::new(),
        }
    }

    /// daemon keepalive 的实际形态：注释行 + 空行。
    fn keepalive_chunk() -> Vec<u8> {
        b": keepalive\n\n".to_vec()
    }

    /// 能通过 envelope `validate()` 的最小 ephemeral 事件帧（payload 用
    /// `team_delta_variants_are_nameable_from_the_client_surface` 验证过的
    /// 反序列化形状）。
    fn event_frame_chunk() -> Vec<u8> {
        let event = serde_json::json!({
            "schema": qaqh_ringing::RINGING_SCHEMA,
            "version": RINGING_V2_VERSION,
            "server_epoch": "epoch-1",
            "session_id": "session-1",
            "event_id": "event-1",
            "stream_key": {"kind": "channel", "data": "control"},
            "delivery": "ephemeral",
            "payload": {"kind": "team_delta", "data": {"kind": "agent_residency_changed",
                "data": {"revision": 3,
                         "agent_id": "0199a0f0-0000-7000-8000-000000000002",
                         "residency": "unloaded"}}}
        });
        format!(
            "event: ringing.event\ndata: {}\n\n",
            serde_json::to_string(&event).expect("json")
        )
        .into_bytes()
    }

    #[tokio::test]
    async fn keepalive_bytes_surface_as_keepalive_without_losing_frames() {
        let mut sub = subscription_with_chunks(vec![keepalive_chunk(), event_frame_chunk()]);

        match sub.next().await.expect("next") {
            Some(ClientV2SubscriptionEvent::KeepAlive) => {}
            other => panic!("keepalive 注释必须呈现为 KeepAlive，got {other:?}"),
        }
        match sub.next().await.expect("next") {
            Some(ClientV2SubscriptionEvent::Event(_)) => {}
            other => panic!("完整事件帧必须照常解码，got {other:?}"),
        }
        assert!(
            sub.next().await.expect("next").is_none(),
            "流结束必须照常报 None"
        );
    }

    /// 跨 chunk 的半帧不得因 KeepAlive 路径丢帧：先到的半帧报 KeepAlive，
    /// 补齐后必须照常解码出事件。
    #[tokio::test]
    async fn half_frame_across_chunks_yields_event_after_keepalive() {
        let frame = event_frame_chunk();
        let split = frame.len() / 2;
        let mut sub =
            subscription_with_chunks(vec![frame[..split].to_vec(), frame[split..].to_vec()]);

        match sub.next().await.expect("next") {
            Some(ClientV2SubscriptionEvent::KeepAlive) => {}
            other => panic!("半帧应先报 KeepAlive，got {other:?}"),
        }
        match sub.next().await.expect("next") {
            Some(ClientV2SubscriptionEvent::Event(_)) => {}
            other => panic!("补齐后必须解码出事件（不得丢帧），got {other:?}"),
        }
    }
}
