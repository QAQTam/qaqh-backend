//! Ringing v2 wire contract.
//!
//! v2 is additive: v1 constants and types remain untouched. Payloads are
//! generic so the wire crate stays independent from session projection types;
//! `qaqh-client` binds the payload to its typed projection model.

use qaqh_domain::RingingChannel;
use qaqh_domain::state::ControlState;
use serde::{Deserialize, Serialize};

use crate::command::RingingCommand;
use crate::envelope::{RingingCommandAckStatus, RingingCommandState};
use crate::protocol::{RINGING_SCHEMA, is_safe_integer};
use crate::v2::cursor::{CanonicalCursor, CursorToken};
use crate::v2::{RINGING_V2_BASE_PATH, RINGING_V2_VERSION};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingingV2OpenRequest {
    pub schema: String,
    pub version: u32,
    pub client_instance_id: String,
}

impl RingingV2OpenRequest {
    pub fn new(client_instance_id: impl Into<String>) -> Self {
        Self {
            schema: RINGING_SCHEMA.to_string(),
            version: RINGING_V2_VERSION,
            client_instance_id: client_instance_id.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingingV2Capabilities {
    pub subscribe: bool,
    pub interact: bool,
    pub drive: bool,
    pub timeline: bool,
    pub service: bool,
    pub content: bool,
    /// 单流订阅能力（2026-09-24 冻结修订，tag
    /// `tui-ringing-v2-frozen-2026-09-24-single-stream`）。
    ///
    /// `true` = `GET /ringing/v2/sessions/{seed}/events` 是**每 seed 一条**流，
    /// 事件带 `stream_key` 由客户端 demux；per-channel 的 `events/{channel}`
    /// 已硬切删除（不再返回 404 兼容视图）。
    ///
    /// `#[serde(default)]`：旧 client 反序列化新响应时该字段为 `false`，
    /// 新 client 必须显式断言 `true` 才能假定单流语义。
    #[serde(default)]
    pub single_stream: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingingV2OpenResponse {
    pub schema: String,
    pub version: u32,
    pub accepted: bool,
    pub client_session_id: String,
    pub server_epoch: String,
    pub lease_ttl_ms: u64,
    pub renew_interval_ms: u64,
    pub capabilities: RingingV2Capabilities,
}

impl RingingV2OpenResponse {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema != RINGING_SCHEMA
            || self.version != RINGING_V2_VERSION
            || self.client_session_id.trim().is_empty()
            || self.server_epoch.trim().is_empty()
            || !is_safe_integer(self.lease_ttl_ms)
            || !is_safe_integer(self.renew_interval_ms)
        {
            return Err("invalid_open_response");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingingV2LeaseRenewResponse {
    pub ok: bool,
    pub lease_ttl_ms: u64,
    pub renew_interval_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RingingV2Delivery {
    Reliable,
    Replaceable,
    Ephemeral,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum RingingV2StreamKey {
    Channel(RingingChannel),
    Resource { kind: String, id: String },
}

/// v2 event envelope. `P` is the typed projection payload selected by the
/// transport/client boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingingV2EventEnvelope<P> {
    pub schema: String,
    pub version: u32,
    pub server_epoch: String,
    #[serde(rename = "session_id")]
    pub session_id: String,
    pub event_id: String,
    pub stream_key: RingingV2StreamKey,
    pub delivery: RingingV2Delivery,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<CursorToken>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fact_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection_index: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    /// Command/event causal link. Interaction completion is recognized only
    /// when this matches the submitted `command_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    pub payload: P,
}

impl<P> RingingV2EventEnvelope<P> {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema != RINGING_SCHEMA
            || self.version != RINGING_V2_VERSION
            || self.server_epoch.trim().is_empty()
            || self.session_id.trim().is_empty()
            || self.event_id.trim().is_empty()
            || self
                .causation_id
                .as_deref()
                .is_some_and(|value| value.trim().is_empty())
            || self
                .correlation_id
                .as_deref()
                .is_some_and(|value| value.trim().is_empty())
        {
            return Err("invalid_v2_envelope");
        }

        match self.delivery {
            RingingV2Delivery::Reliable => {
                let token = self.cursor.as_ref().ok_or("missing_cursor")?;
                let cursor = token
                    .decode_reliable()
                    .map_err(|_| "invalid_reliable_cursor")?;
                if self.log_id.as_deref() != Some(cursor.log_id.as_str())
                    || self.fact_seq != Some(cursor.fact_seq)
                    || self.projection_index != Some(cursor.projection_index)
                    || self.revision.is_none()
                {
                    return Err("reliable_cursor_mismatch");
                }
            }
            RingingV2Delivery::Replaceable => {
                if self.cursor.is_some()
                    || self.projection_index.is_some()
                    || self.revision.is_none()
                {
                    return Err("invalid_replaceable_envelope");
                }
            }
            RingingV2Delivery::Ephemeral => {
                if self.cursor.is_some()
                    || self.log_id.is_some()
                    || self.fact_seq.is_some()
                    || self.projection_index.is_some()
                    || self.revision.is_some()
                {
                    return Err("invalid_ephemeral_envelope");
                }
            }
        }
        Ok(())
    }

    pub fn cursor_value(&self) -> Result<Option<CanonicalCursor>, &'static str> {
        self.validate()?;
        match self.delivery {
            RingingV2Delivery::Reliable => self
                .cursor
                .as_ref()
                .ok_or("missing_cursor")?
                .decode_reliable()
                .map(Some)
                .map_err(|_| "invalid_reliable_cursor"),
            RingingV2Delivery::Replaceable | RingingV2Delivery::Ephemeral => Ok(None),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RingingV2ResetReason {
    CursorExpired,
    LogIdMismatch,
    UnknownFact,
    UpgradeRequired,
    ReplayOverflow,
    EpochMismatch,
    CrossSession,
    SnapshotMissing,
    SnapshotExpired,
    SnapshotHashMismatch,
    StaleWriter,
    /// 会话级内容总量 hard watermark 触发的重置原因（session-fact spec §5.6）。
    ///
    /// 交互正文的 pinned 准入配额是另一条写路径：超限时不入库并让客户端按
    /// 正文 404 降级，不会把这个值当作流 reset reason。
    ContentQuotaExceeded,
    PerConnectionOverflow,
    ProgressBufferOverflow,
    ActorMailboxOverflow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingingV2ResetRequired {
    pub schema: String,
    pub version: u32,
    pub server_epoch: String,
    #[serde(rename = "session_id")]
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_cursor: Option<CursorToken>,
    pub reason: RingingV2ResetReason,
}

impl RingingV2ResetRequired {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema != RINGING_SCHEMA
            || self.version != RINGING_V2_VERSION
            || self.server_epoch.trim().is_empty()
            || self.session_id.trim().is_empty()
        {
            return Err("invalid_reset_required");
        }
        if let Some(token) = &self.snapshot_cursor {
            let cursor = token
                .decode_snapshot()
                .map_err(|_| "invalid_snapshot_cursor")?;
            if self.log_id.as_deref() != Some(cursor.log_id.as_str()) {
                return Err("reset_log_mismatch");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RingingV2InteractionKind {
    Permission,
    Ask,
    /// Rust 侧保留历史变体名，wire 值统一为 `plan`（与 canonical delta 的
    /// `InteractionKind::Plan` 一致）。2026-09-24 修订前 bootstrap 会序列化成
    /// `plan_review`，壳层必须自己映射；现在两条路径使用同一个 wire 值。
    #[serde(rename = "plan")]
    PlanReview,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingingV2PendingInteraction {
    pub interaction_id: String,
    pub call_id: String,
    pub turn_id: String,
    pub kind: RingingV2InteractionKind,
    /// modal 正文载荷（#345 / 2026-09-24 修订）。
    ///
    /// - `Ref { content_ref }`：正文在 content store 里，用
    ///   `GET /ringing/v2/content/{content_ref}` 取（ask / plan / permission）；
    /// - `Inline { text }`：正文随事件内联；
    /// - `None`：历史事实没有可取的正文，或正文入 store 失败；客户端按
    ///   「正文不可用」降级，不得再依赖已删除的 tool 频道快照。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RingingV2ContentValue>,
}

/// pending interaction 的正文载荷（#345）。
///
/// 与 canonical `ContentValue` 的 serde 形态一致（`kind` / `data` 判别式），
/// 但 wire 层不依赖 session crate，因此在这里独立声明；`Unavailable` 的
/// reason 结构不在此重复建模，按不透明 JSON 透传。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum RingingV2ContentValue {
    Inline { text: String },
    Ref { content_ref: String },
    Unavailable(serde_json::Value),
}

pub type RingingV2PendingSet = Vec<RingingV2PendingInteraction>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingingV2DriverState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
    pub driver_epoch: u64,
    pub can_claim: bool,
}

/// v2 control snapshot. Existing control fields are flattened so the type can
/// grow additively while `interactions` and `driver` remain explicitly typed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RingingV2ControlState {
    #[serde(flatten)]
    pub base: ControlState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interactions: RingingV2PendingSet,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<RingingV2DriverState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingV2ChannelSnapshot<S> {
    pub channel: RingingChannel,
    pub state_revision: u64,
    pub snapshot_version: u32,
    pub state: S,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingV2Bootstrap<C, V, T> {
    pub schema: String,
    pub version: u32,
    pub server_epoch: String,
    #[serde(rename = "session_id")]
    pub session_id: String,
    pub snapshot_cursor: CursorToken,
    pub control: RingingV2ChannelSnapshot<C>,
    pub conversation: RingingV2ChannelSnapshot<V>,
    pub tool: RingingV2ChannelSnapshot<T>,
}

impl<C, V, T> RingingV2Bootstrap<C, V, T> {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema != RINGING_SCHEMA
            || self.version != RINGING_V2_VERSION
            || self.server_epoch.trim().is_empty()
            || self.session_id.trim().is_empty()
        {
            return Err("invalid_v2_bootstrap");
        }
        let cursor = self
            .snapshot_cursor
            .decode_snapshot()
            .map_err(|_| "invalid_snapshot_cursor")?;
        if self.control.channel != RingingChannel::Control
            || self.conversation.channel != RingingChannel::Conversation
            || self.tool.channel != RingingChannel::Tool
        {
            return Err("invalid_snapshot_channel");
        }
        let _ = cursor;
        Ok(())
    }

    pub fn log_id(&self) -> Result<String, &'static str> {
        self.snapshot_cursor
            .decode_snapshot()
            .map(|cursor| cursor.log_id)
            .map_err(|_| "invalid_snapshot_cursor")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingV2CommandEnvelope {
    pub schema: String,
    pub version: u32,
    pub channel: RingingChannel,
    pub command_id: String,
    pub client_instance_id: String,
    pub client_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "session_id")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    /// Driver seat epoch the caller believes it holds. A mismatch is rejected
    /// with `stale_driver_epoch`; `None` opts out of the epoch guard (used by
    /// interaction answers, which are not driver-gated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver_epoch: Option<u64>,
    pub command: RingingCommand,
}

impl RingingV2CommandEnvelope {
    pub fn new(
        command_id: impl Into<String>,
        client_instance_id: impl Into<String>,
        command: RingingCommand,
    ) -> Self {
        Self {
            schema: RINGING_SCHEMA.to_string(),
            version: RINGING_V2_VERSION,
            channel: command.channel(),
            command_id: command_id.into(),
            client_instance_id: client_instance_id.into(),
            client_session_id: String::new(),
            session_id: None,
            expected_revision: None,
            driver_epoch: None,
            command,
        }
    }

    pub fn with_client_session_id(mut self, client_session_id: impl Into<String>) -> Self {
        self.client_session_id = client_session_id.into();
        self
    }

    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn with_driver_epoch(mut self, driver_epoch: u64) -> Self {
        self.driver_epoch = Some(driver_epoch);
        self
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema != RINGING_SCHEMA
            || self.version != RINGING_V2_VERSION
            || self.command_id.trim().is_empty()
            || self.client_instance_id.trim().is_empty()
            || self.client_session_id.trim().is_empty()
        {
            return Err("invalid_v2_command_envelope");
        }
        if self.channel != self.command.channel() {
            return Err("channel_mismatch");
        }
        if self.session_id.as_deref().is_some_and(str::is_empty) {
            return Err("invalid_seed");
        }
        if self
            .expected_revision
            .is_some_and(|value| !is_safe_integer(value))
        {
            return Err("invalid_expected_revision");
        }
        if self
            .driver_epoch
            .is_some_and(|value| !is_safe_integer(value))
        {
            return Err("invalid_driver_epoch");
        }
        if self.session_id.is_none()
            && !matches!(
                self.command,
                RingingCommand::Control(qaqh_domain::ControlCommand::SessionCreate { .. })
            )
        {
            return Err("missing_session_id");
        }
        Ok(())
    }
}

/// v2 command acknowledgement.
///
/// `existing` is populated only when the daemon replays a receipt that is
/// still inside the idempotency TTL — that is, when the caller re-submitted a
/// `command_id` it may have lost the ACK for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingingV2CommandAck {
    pub command_id: String,
    pub status: RingingCommandAckStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    /// Already-recorded outcome this submission collided with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub existing: Option<RingingV2ExistingResult>,
}

/// Why a v2 submission collided with an already-recorded outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum RingingV2ExistingResult {
    /// The same `command_id` was submitted again inside the receipt TTL
    /// (typically because the first ACK was lost).
    CommandReceipt {
        state: RingingCommandState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        terminal_event_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_code: Option<String>,
        /// Typed terminal payload, when the command produced one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<RingingV2CommandResult>,
    },
    /// A different command answered an interaction that was already resolved
    /// (first-answer-wins). Carries the winning verdict so the loser can
    /// reconcile without string-matching `code`.
    InteractionResolved { result: RingingV2CommandResult },
}

/// Typed terminal payload of a command.
///
/// Only outcomes a client must be able to reconcile after losing an ACK are
/// represented. The reliable `causation_id = command_id` event remains the
/// source of truth; this payload exists so idempotent replay and status polling
/// do not degrade to string matching on `code`/`message`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RingingV2CommandResult {
    /// ask_user interaction was resolved by the command that owns this receipt.
    AskResolved {
        interaction_id: String,
        outcome: RingingV2AskOutcome,
    },
    /// plan review was resolved by the command that owns this receipt.
    PlanReviewResolved {
        interaction_id: String,
        approved: bool,
    },
    /// tool permission was resolved by the command that owns this receipt.
    PermissionResolved {
        interaction_id: String,
        approved: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RingingV2AskOutcome {
    Answered,
    Dismissed,
}

/// v2 command status. Superset of [`crate::RingingCommandStatus`] with the
/// typed terminal payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingingV2CommandStatus {
    pub command_id: String,
    pub state: RingingCommandState,
    pub payload_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<RingingV2CommandResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingingV2DriverClaimResponse {
    pub accepted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
    pub driver_epoch: u64,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingingV2DriverReleaseResponse {
    pub accepted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
    pub driver_epoch: u64,
    pub reason: String,
}

pub fn open_path() -> String {
    format!("{RINGING_V2_BASE_PATH}/clients/open")
}

/// 单流订阅路径（每 seed 一条 SSE；事件带 `stream_key`）。
///
/// 2026-09-24 冻结修订：per-channel 的 `events/{channel}` 已硬切删除。
pub fn events_path(session_id: &str) -> String {
    format!("{RINGING_V2_BASE_PATH}/sessions/{session_id}/events")
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::ControlCommand;
    use qaqh_domain::state::{ConversationState, ToolState};

    fn reliable_envelope() -> RingingV2EventEnvelope<serde_json::Value> {
        let cursor = CanonicalCursor::new("log-1", 42, 1);
        RingingV2EventEnvelope {
            schema: RINGING_SCHEMA.into(),
            version: RINGING_V2_VERSION,
            server_epoch: "epoch-1".into(),
            session_id: "seed-1".into(),
            event_id: "event-1".into(),
            stream_key: RingingV2StreamKey::Channel(RingingChannel::Control),
            delivery: RingingV2Delivery::Reliable,
            cursor: Some(CursorToken::encode_reliable(&cursor).expect("cursor")),
            log_id: Some(cursor.log_id),
            fact_seq: Some(cursor.fact_seq),
            projection_index: Some(cursor.projection_index),
            revision: Some(7),
            causation_id: Some("cmd-1".into()),
            correlation_id: None,
            payload: serde_json::json!({ "kind": "control_delta" }),
        }
    }

    #[test]
    fn reliable_envelope_round_trips() {
        let envelope = reliable_envelope();
        envelope.validate().expect("valid envelope");
        let json = serde_json::to_string(&envelope).expect("serialize");
        let decoded: RingingV2EventEnvelope<serde_json::Value> =
            serde_json::from_str(&json).expect("deserialize");
        decoded.validate().expect("valid decoded envelope");
    }

    #[test]
    fn delivery_constraints_are_enforced() {
        let mut replaceable = reliable_envelope();
        replaceable.delivery = RingingV2Delivery::Replaceable;
        replaceable.cursor = None;
        replaceable.projection_index = None;
        replaceable.validate().expect("replaceable envelope");

        let mut ephemeral = reliable_envelope();
        ephemeral.delivery = RingingV2Delivery::Ephemeral;
        ephemeral.cursor = None;
        ephemeral.log_id = None;
        ephemeral.fact_seq = None;
        ephemeral.projection_index = None;
        ephemeral.revision = None;
        ephemeral.validate().expect("ephemeral envelope");

        replaceable.revision = None;
        assert_eq!(replaceable.validate(), Err("invalid_replaceable_envelope"));
        ephemeral.fact_seq = Some(1);
        assert_eq!(ephemeral.validate(), Err("invalid_ephemeral_envelope"));
    }

    #[test]
    fn bootstrap_requires_the_three_frozen_channels() {
        let bootstrap = RingingV2Bootstrap {
            schema: RINGING_SCHEMA.into(),
            version: RINGING_V2_VERSION,
            server_epoch: "epoch-1".into(),
            session_id: "seed-1".into(),
            snapshot_cursor: CursorToken::encode_snapshot(&CanonicalCursor::snapshot("log-1", 42))
                .expect("cursor"),
            control: RingingV2ChannelSnapshot {
                channel: RingingChannel::Control,
                state_revision: 1,
                snapshot_version: 1,
                state: RingingV2ControlState::default(),
            },
            conversation: RingingV2ChannelSnapshot {
                channel: RingingChannel::Conversation,
                state_revision: 2,
                snapshot_version: 1,
                state: ConversationState::default(),
            },
            tool: RingingV2ChannelSnapshot {
                channel: RingingChannel::Tool,
                state_revision: 3,
                snapshot_version: 1,
                state: ToolState::default(),
            },
        };
        bootstrap.validate().expect("valid bootstrap");
        assert_eq!(bootstrap.log_id().expect("log id"), "log-1");
    }

    #[test]
    fn bootstrap_json_matches_frozen_shape() {
        let token = CursorToken::encode_snapshot(&CanonicalCursor::snapshot("log-1", 42))
            .expect("snapshot cursor")
            .into_string();
        let value = serde_json::json!({
            "schema": RINGING_SCHEMA,
            "version": RINGING_V2_VERSION,
            "server_epoch": "epoch-1",
            "session_id": "seed-1",
            "snapshot_cursor": token,
            "control": {
                "channel": "control",
                "state_revision": 7,
                "snapshot_version": 1,
                "state": {
                    "interactions": [{
                        "interaction_id": "i1",
                        "call_id": "c1",
                        "turn_id": "t1",
                        "kind": "plan"
                    }],
                    "driver": {
                        "holder": null,
                        "driver_epoch": 3,
                        "can_claim": true
                    }
                }
            },
            "conversation": {
                "channel": "conversation",
                "state_revision": 19,
                "snapshot_version": 1,
                "state": {}
            },
            "tool": {
                "channel": "tool",
                "state_revision": 11,
                "snapshot_version": 1,
                "state": {}
            }
        });
        let bootstrap: RingingV2Bootstrap<RingingV2ControlState, ConversationState, ToolState> =
            serde_json::from_value(value).expect("frozen bootstrap shape");
        bootstrap.validate().expect("valid bootstrap");
        assert_eq!(bootstrap.control.state.interactions.len(), 1);
        assert_eq!(
            bootstrap.control.state.interactions[0].kind,
            RingingV2InteractionKind::PlanReview
        );
        assert_eq!(
            serde_json::to_value(bootstrap.control.state.interactions[0].kind)
                .expect("serialize interaction kind"),
            serde_json::json!("plan")
        );
        assert_eq!(
            bootstrap
                .control
                .state
                .driver
                .as_ref()
                .expect("driver")
                .driver_epoch,
            3
        );
    }

    #[test]
    fn v2_command_ack_carries_typed_existing_result() {
        let ack = RingingV2CommandAck {
            command_id: "cmd-1".into(),
            status: RingingCommandAckStatus::Accepted,
            code: None,
            message: Some("duplicate command_id (already completed)".into()),
            retry_after_ms: None,
            existing: Some(RingingV2ExistingResult::CommandReceipt {
                state: RingingCommandState::Succeeded,
                terminal_event_id: Some("evt-1".into()),
                error_code: None,
                result: Some(RingingV2CommandResult::AskResolved {
                    interaction_id: "i1".into(),
                    outcome: RingingV2AskOutcome::Answered,
                }),
            }),
        };
        let json = serde_json::to_value(&ack).expect("serialize");
        assert_eq!(json["existing"]["source"], "command_receipt");
        assert_eq!(json["existing"]["state"], "succeeded");
        assert_eq!(json["existing"]["result"]["kind"], "ask_resolved");
        assert_eq!(json["existing"]["result"]["outcome"], "answered");
        let back: RingingV2CommandAck = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, ack);
    }

    #[test]
    fn v2_command_ack_carries_winning_interaction_verdict() {
        let ack = RingingV2CommandAck {
            command_id: "cmd-2".into(),
            status: RingingCommandAckStatus::Rejected,
            code: Some("interaction_already_resolved".into()),
            message: Some("interaction was already resolved".into()),
            retry_after_ms: None,
            existing: Some(RingingV2ExistingResult::InteractionResolved {
                result: RingingV2CommandResult::PermissionResolved {
                    interaction_id: "int_1".into(),
                    approved: false,
                },
            }),
        };
        let json = serde_json::to_value(&ack).expect("serialize");
        assert_eq!(json["existing"]["source"], "interaction_resolved");
        assert_eq!(json["existing"]["result"]["kind"], "permission_resolved");
        assert_eq!(json["existing"]["result"]["approved"], false);
        let back: RingingV2CommandAck = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, ack);
    }

    #[test]
    fn v1_ack_body_deserializes_as_v2_ack_without_existing() {
        let v1 = crate::RingingCommandAck {
            command_id: "cmd-1".into(),
            status: RingingCommandAckStatus::Accepted,
            code: None,
            message: None,
            retry_after_ms: None,
        };
        let json = serde_json::to_value(&v1).expect("serialize");
        let v2: RingingV2CommandAck = serde_json::from_value(json).expect("v2 superset");
        assert_eq!(v2.command_id, "cmd-1");
        assert!(v2.existing.is_none());
    }

    #[test]
    fn v2_command_status_round_trips_typed_result() {
        let status = RingingV2CommandStatus {
            command_id: "cmd-2".into(),
            state: RingingCommandState::Failed,
            payload_fingerprint: "fp".into(),
            terminal_event_id: Some("evt-2".into()),
            error_code: Some("interaction_already_resolved".into()),
            result: Some(RingingV2CommandResult::PlanReviewResolved {
                interaction_id: "i2".into(),
                approved: false,
            }),
        };
        let json = serde_json::to_value(&status).expect("serialize");
        assert_eq!(json["result"]["kind"], "plan_review_resolved");
        assert_eq!(json["result"]["approved"], false);
        let back: RingingV2CommandStatus = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, status);
    }

    #[test]
    fn v1_constants_remain_unchanged() {
        assert_eq!(crate::RINGING_VERSION, 1);
        assert_eq!(RINGING_V2_VERSION, 2);
        assert_eq!(RINGING_V2_BASE_PATH, "/ringing/v2");
        assert_eq!(open_path(), "/ringing/v2/clients/open");
    }

    /// 单流修订（2026-09-24）：SSE 路径不再带 channel 段。
    #[test]
    fn events_path_is_per_session_single_stream() {
        assert_eq!(events_path("s1"), "/ringing/v2/sessions/s1/events");
        assert!(
            !events_path("s1").contains("/events/"),
            "不得再出现 per-channel 的 events/{{channel}} 形态"
        );
    }

    /// `single_stream` 是增量 capability：旧 client 反序列化缺该字段 → false。
    #[test]
    fn single_stream_capability_is_additive_and_defaults_false() {
        let legacy = serde_json::json!({
            "subscribe": true,
            "interact": true,
            "drive": true,
            "timeline": true,
            "service": true,
            "content": true
        });
        let decoded: RingingV2Capabilities =
            serde_json::from_value(legacy).expect("legacy capability body");
        assert!(
            !decoded.single_stream,
            "缺字段必须是 false（不得被推测为单流）"
        );

        let current = serde_json::json!({
            "subscribe": true,
            "interact": true,
            "drive": true,
            "timeline": true,
            "service": true,
            "content": true,
            "single_stream": true
        });
        let decoded: RingingV2Capabilities =
            serde_json::from_value(current).expect("current capability body");
        assert!(decoded.single_stream);
    }

    #[test]
    fn command_envelope_requires_v2_identity() {
        let command = RingingCommand::Control(ControlCommand::SessionResume {
            session_id: "seed-1".into(),
        });
        let mut envelope = RingingV2CommandEnvelope::new("cmd-1", "instance-1", command);
        envelope = envelope.with_session_id("seed-1");
        assert_eq!(envelope.validate(), Err("invalid_v2_command_envelope"));
        envelope = envelope.with_client_session_id("session-1");
        envelope.validate().expect("valid v2 command envelope");
    }
}
