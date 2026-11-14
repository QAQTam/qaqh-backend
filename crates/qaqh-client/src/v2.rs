//! Ringing v2 typed client surface.
//! The client exposes the RingingV2 protocol types directly and handles its
//! transport lease, command requests, bootstrap, and session event stream.

use std::fmt;

use bytes::Bytes;
use futures_util::stream::{BoxStream, StreamExt};
use qaqh_ringing::{
    CommandTarget, RingingV2ApprovalRespondRequest, RingingV2Capabilities, RingingV2CommandAck,
    RingingV2CommandEnvelope, RingingV2CommandStatus, RingingV2DeviceChallenges,
    RingingV2LeaseRenewResponse, RingingV2OpenRequest, RingingV2OpenResponse,
    RingingV2ResyncRequired,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::client::Client;
use crate::error::{ClientError, Result};
use crate::sse_decoder::SseDecoder;
use crate::types::{CommandOptions, RingingCommand, SseFrame};

pub use qaqh_ringing::{RINGING_V2_BASE_PATH, RINGING_V2_VERSION};

pub use qaqh_ringing as ringing;
pub use qaqh_ringing::v2 as ringing_v2;
pub use qaqh_ringing_contract::events as contract_events;
pub use qaqh_ringing_contract::fact;
pub use qaqh_ringing_contract::state as contract_state;

pub use qaqh_ringing_contract::events::SessionEventFrame;
pub use qaqh_ringing_contract::team as contract_team;
pub use qaqh_ringing_contract::{SessionBootstrap, SessionState, TeamResponse};

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
    Event(Box<SessionEventFrame>),
    Reset(RingingV2ResyncRequired),
    /// 字节到达但凑不出完整帧（daemon 的 keepalive 注释行 / 跨 chunk 半帧）。
    /// 把「到达」本身呈现给调用方，v2 流的空闲计时器才能在**字节层**复位：
    /// 会话安静期里 daemon 的 15s keepalive 是唯一字节来源，只按事件复位
    /// 会让健康流每 45s 被误判 `v2 SSE idle timeout` 重连（真机实测 11 次，
    /// 每次还触发 app 层全量 re-bootstrap）。字节层探活
    /// （原 timeline 流的 BUG-2026-09-12-10 教训）。
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
        // 解码器中遗留的完整帧（上次 push 后未消费）直接返回。
        // 注：所有分支必然 return,原 `loop` 被 clippy::never_loop 拒绝(1.98)。
        if let Some(frame) = self.decoder.next_frame() {
            return Self::decode_frame(frame).map(Some);
        }

        match self.stream.next().await {
            Some(Ok(bytes)) => {
                self.decoder.push(&bytes);
                match self.decoder.next_frame() {
                    // 这批字节凑出了完整帧：照常解码返回。
                    Some(frame) => Self::decode_frame(frame).map(Some),
                    // 字节到了但凑不出帧：以 KeepAlive 把「到达」本身报给
                    // 调用方（见枚举上的 KeepAlive 文档）。
                    None => Ok(Some(ClientV2SubscriptionEvent::KeepAlive)),
                }
            }
            Some(Err(error)) => Err(error.into()),
            None => Ok(None),
        }
    }

    /// 一帧 SSE → 订阅事件。`Err(())` 是解码器的 UTF-8 失败标记。
    fn decode_frame(frame: std::result::Result<SseFrame, ()>) -> Result<ClientV2SubscriptionEvent> {
        let frame =
            frame.map_err(|()| ClientError::Protocol("invalid UTF-8 in v2 SSE frame".into()))?;
        if frame.event_type == "ringing.resync_required" {
            let reset: RingingV2ResyncRequired = serde_json::from_str(&frame.data)?;
            return Ok(ClientV2SubscriptionEvent::Reset(reset));
        }
        // SSE 事件名即信封外层，只有 `ringing.event`（空名 = 旧形态兼容）承载
        // SessionEventFrame。旁路帧不注册会话事件词表、data 也不是帧形状
        // （`diagnostics.memory` 载原始快照 JSON，无 seq；`ringing.stream_terminated`
        // 载 {code,session_id,message}）——拿 SessionEventFrame 硬解它们必然 Err，
        // 而 v2 流把任何 Err 当断流退避重连：内存探测开着时 daemon 每秒推一帧
        // `diagnostics.memory`，重连风暴与探测同生命周期、停表即愈（2026-11-12 真机
        // 事故：右下角「网络波动(第 N 次)」无限递增）。字节到了 = 链路活着，按
        // KeepAlive 上报复位空闲计时器即符合此处字节层探活的既有口径。
        // 未来新控制帧要改变这一默认语义的，必须在本函数显式匹配事件名。
        if !frame.event_type.is_empty() && frame.event_type != "ringing.event" {
            return Ok(ClientV2SubscriptionEvent::KeepAlive);
        }
        let event: SessionEventFrame = serde_json::from_str(&frame.data)?;
        Ok(ClientV2SubscriptionEvent::Event(Box::new(event)))
    }
}

impl Client {
    /// `POST /ringing/v2/clients/open`.
    pub async fn open_v2(&self) -> Result<RingingV2OpenResponse> {
        let client_instance_id = self.inner.session.client_instance_id();
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
        self.inner.session.adopt(state).await;
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

    /// `GET /ringing/v2/sessions/{seed}/bootstrap`。
    /// `timeline_tail` 为 Some(n) 时追加 `?timeline_tail=n`:响应 timeline 只含
    /// 最近 n 块(回合边界回退,`state.timeline_from_ord` 标记边界),历史回合
    /// 由 `session.turn` 按需取回。None = 全量快照,旧语义不变。
    pub async fn bootstrap_v2(
        &self,
        session_id: &str,
        timeline_tail: Option<u32>,
    ) -> Result<SessionBootstrap> {
        let state = self.require_v2_session().await?;
        let path = format!("{RINGING_V2_BASE_PATH}/sessions/{session_id}/bootstrap");
        let url = match timeline_tail {
            Some(tail) => format!("{}{path}?timeline_tail={tail}", self.credentials().base_url),
            None => format!("{}{path}", self.credentials().base_url),
        };
        let response = self
            .inner
            .http
            .get(url)
            .bearer_auth(&self.credentials().token)
            .header("X-QAQH-Client-Session-Id", &state.client_session_id)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(api_error(response, &path).await);
        }
        let bootstrap: SessionBootstrap = response.json().await?;
        if bootstrap.session_contract_revision != qaqh_ringing::SESSION_CONTRACT_REVISION {
            return Err(ClientError::Protocol(format!(
                "unsupported session contract revision: {}",
                bootstrap.session_contract_revision
            )));
        }
        Ok(bootstrap)
    }

    /// `GET /ringing/v2/sessions/{seed}/approvals` — 本地壳层审批投影。
    /// 返回 daemon 的原始 JSON(`{ "pending_permission": …|null,
    /// "pending_interaction": …|null }`),id 为 canonical 形态(`call_<ULID>` /
    /// `int_<ULID>`)。壳层(桌面宿主)自行做不透明 challenge 映射;返回裸
    /// `Value` 而非 typed 结构,与 daemon 注释契约「形状与旧 v1 端点保持一致」
    /// 解耦,避免投影字段演进被迫改 client。
    pub async fn pending_approvals(&self, session_id: &str) -> Result<serde_json::Value> {
        let state = self.require_v2_session().await?;
        let path = format!("{RINGING_V2_BASE_PATH}/sessions/{session_id}/approvals");
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

    /// device 身份 `GET /ringing/v2/sessions/{seed}/approvals`：不透明
    /// challenge 列表（一次性 `challenge_id`，canonical id 不出 daemon）。
    ///
    /// 与 [`Self::pending_approvals`]（admin 面，裸 JSON）刻意不同形：
    /// device 面是一手 DTO（`RingingV2DeviceChallenges`），移动端直接消费。
    pub async fn device_challenges(&self, session_id: &str) -> Result<RingingV2DeviceChallenges> {
        let state = self.require_v2_session().await?;
        let path = format!("{RINGING_V2_BASE_PATH}/sessions/{session_id}/approvals");
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

    /// device 侧审批应答：`POST /ringing/v2/sessions/{seed}/approvals/respond`。
    ///
    /// daemon 一次性消费 challenge → 映射回 canonical 命令 → 执行，返回命令
    /// ack（Accepted / Rejected）。`challenge_id` 过期或不符会被 403 拒绝
    /// （`Api{code}`），调用方应重新拉 [`Self::device_challenges`]。
    pub async fn respond_device_approval(
        &self,
        session_id: &str,
        request: RingingV2ApprovalRespondRequest,
    ) -> Result<RingingV2CommandAck> {
        let state = self.require_v2_session().await?;
        let path =
            format!("{RINGING_V2_BASE_PATH}/sessions/{session_id}/approvals/respond");
        let response = self
            .inner
            .http
            .post(format!("{}{path}", self.credentials().base_url))
            .bearer_auth(&self.credentials().token)
            .header("X-QAQH-Client-Session-Id", &state.client_session_id)
            .json(&request)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(api_error(response, &path).await);
        }
        Ok(response.json().await?)
    }

    /// Open the typed v2 per-session SSE subscription. Each frame carries one
    /// monotonically increasing sequence and a typed session event.
    pub async fn subscribe_v2(
        &self,
        session_id: &str,
        since_cursor: Option<&str>,
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
            request = request.query(&[("since_cursor", cursor)]);
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
        target: CommandTarget,
        command: RingingCommand,
        options: CommandOptions,
    ) -> Result<RingingV2CommandAck> {
        let state = self.require_v2_session().await?;
        let command_id = options
            .command_id
            .unwrap_or_else(qaqh_ringing_contract::ulid::generate_ulid);
        let client_session_id = state.client_session_id.clone();
        let mut payload = RingingV2CommandEnvelope::new(command_id.clone(), target, command)
            .with_client_identity(state.client_instance_id, client_session_id.clone());
        payload.preconditions.extend(options.preconditions);
        payload
            .validate()
            .map_err(|code| ClientError::Protocol(format!("invalid RingingV2 command: {code}")))?;
        let path = format!("{RINGING_V2_BASE_PATH}/commands");
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
                "RingingV2 command ack id does not match submission".into(),
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

    /// `GET /ringing/v2/sessions/{seed}/team`。
    /// Team projection 快照：roster（以 `AgentPath` 为主）+ inbox + task board +
    /// message board。deltas 在 per-seed 单流上以
    /// [`fact::ProjectionPayload::TeamDelta`] 到达；壳层应当 **先拉一次快照、再应用
    /// delta**，不要只靠 delta 增量拼状态。
    pub async fn team_v2(&self, session_id: &str) -> Result<TeamResponse> {
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

pub(crate) async fn api_error(response: reqwest::Response, path: &str) -> ClientError {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Team projection 的 wire 形状必须和 daemon `/team` 端点逐字对齐。
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
                        "author": {"kind": "path", "data": "/root/writer"},
                        "recipient": {"kind": "path", "data": "/root"},
                        "task_id": "task-1",
                        "delivery": "queue",
                        "created_at_ms": 1_759_000_000_000i64
                    },
                    {
                        "message_id": "msg-2",
                        "author": {"kind": "session", "data": "0199a0f0-0000-7000-8000-000000000009"},
                        "recipient": {"kind": "path", "data": "/root"},
                        "delivery": "trigger",
                        "created_at_ms": 1_759_000_000_001i64
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

        let response: TeamResponse =
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
        assert_eq!(
            child.residency,
            qaqh_ringing_contract::team::TeamAgentResidency::Unloaded
        );
        assert_eq!(
            child.status,
            qaqh_ringing_contract::team::TeamAgentStatus::Completed
        );

        // inbox 必须带 author / recipient / task / delivery 四要素；
        // 署名是 InterAgentActor union（ADR-0019）：同根 path、跨根 session id。
        let inbox = &response.team.unread_messages[0];
        assert_eq!(
            inbox.author,
            qaqh_ringing_contract::fact::InterAgentActor::Path(
                qaqh_ringing_contract::fact::AgentPath::parse_absolute("/root/writer").unwrap()
            )
        );
        assert_eq!(inbox.task_id.as_deref(), Some("task-1"));
        assert_eq!(
            inbox.delivery,
            qaqh_ringing_contract::fact::InterAgentDelivery::Queue
        );
        assert_eq!(
            response.team.unread_messages[1].author,
            qaqh_ringing_contract::fact::InterAgentActor::Session(
                qaqh_ringing_contract::fact::SessionId::new(
                    "0199a0f0-0000-7000-8000-000000000009"
                )
            )
        );
    }

    #[test]
    fn public_v2_surface_is_typed() {
        assert_eq!(RINGING_V2_VERSION, 2);
        assert_eq!(RINGING_V2_BASE_PATH, "/ringing/v2");
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
            "seq": 7,
            "type": "timeline_appended",
            "entry": {"kind": "input", "data": {
                "block_id": "block_1",
                "purpose": "trigger_turn",
                "content": {"kind": "inline", "data": {"text": "hi"}},
                "attachments": [],
                "actor": {"kind": "user", "id": "user"}}}
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
