//! Ringing v2 wire contract.
//!
//! v2 is additive: v1 constants and types remain untouched. Payloads are
//! generic so the wire crate stays independent from session projection types;
//! `qaqh-client` binds the payload to its typed projection model.

use qaqh_domain::RingingChannel;
use qaqh_domain::state::{ControlState, ConversationState, ToolState};
use serde::{Deserialize, Serialize};

use crate::command::RingingCommand;
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
    pub seed: String,
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
            || self.seed.trim().is_empty()
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
    V1EpochMismatch,
    CrossSession,
    SnapshotMissing,
    SnapshotExpired,
    SnapshotHashMismatch,
    StaleWriter,
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
    pub seed: String,
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
            || self.seed.trim().is_empty()
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
    PlanReview,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingingV2PendingInteraction {
    pub interaction_id: String,
    pub call_id: String,
    pub turn_id: String,
    pub kind: RingingV2InteractionKind,
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
pub struct RingingV2Bootstrap {
    pub schema: String,
    pub version: u32,
    pub server_epoch: String,
    pub seed: String,
    pub snapshot_cursor: CursorToken,
    pub control: RingingV2ChannelSnapshot<RingingV2ControlState>,
    pub conversation: RingingV2ChannelSnapshot<ConversationState>,
    pub tool: RingingV2ChannelSnapshot<ToolState>,
}

impl RingingV2Bootstrap {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema != RINGING_SCHEMA
            || self.version != RINGING_V2_VERSION
            || self.server_epoch.trim().is_empty()
            || self.seed.trim().is_empty()
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
    pub seed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
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
            seed: None,
            expected_revision: None,
            command,
        }
    }

    pub fn with_client_session_id(mut self, client_session_id: impl Into<String>) -> Self {
        self.client_session_id = client_session_id.into();
        self
    }

    pub fn with_seed(mut self, seed: impl Into<String>) -> Self {
        self.seed = Some(seed.into());
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
        if self.seed.as_deref().is_some_and(str::is_empty) {
            return Err("invalid_seed");
        }
        if self
            .expected_revision
            .is_some_and(|value| !is_safe_integer(value))
        {
            return Err("invalid_expected_revision");
        }
        if self.seed.is_none()
            && !matches!(
                self.command,
                RingingCommand::Control(qaqh_domain::ControlCommand::SessionCreate { .. })
            )
        {
            return Err("missing_seed");
        }
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::ControlCommand;

    fn reliable_envelope() -> RingingV2EventEnvelope<serde_json::Value> {
        let cursor = CanonicalCursor::new("log-1", 42, 1);
        RingingV2EventEnvelope {
            schema: RINGING_SCHEMA.into(),
            version: RINGING_V2_VERSION,
            server_epoch: "epoch-1".into(),
            seed: "seed-1".into(),
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
            seed: "seed-1".into(),
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
            "seed": "seed-1",
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
                        "kind": "plan_review"
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
        let bootstrap: RingingV2Bootstrap =
            serde_json::from_value(value).expect("frozen bootstrap shape");
        bootstrap.validate().expect("valid bootstrap");
        assert_eq!(bootstrap.control.state.interactions.len(), 1);
        assert_eq!(
            bootstrap.control.state.interactions[0].kind,
            RingingV2InteractionKind::PlanReview
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
    fn v1_constants_remain_unchanged() {
        assert_eq!(crate::RINGING_VERSION, 1);
        assert_eq!(crate::RINGING_BASE_PATH, "/ringing/v1");
        assert_eq!(RINGING_V2_VERSION, 2);
        assert_eq!(RINGING_V2_BASE_PATH, "/ringing/v2");
        assert_eq!(open_path(), "/ringing/v2/clients/open");
    }

    #[test]
    fn command_envelope_requires_v2_identity() {
        let command = RingingCommand::Control(ControlCommand::SessionResume {
            seed: "seed-1".into(),
        });
        let mut envelope = RingingV2CommandEnvelope::new("cmd-1", "instance-1", command);
        envelope = envelope.with_seed("seed-1");
        assert_eq!(envelope.validate(), Err("invalid_v2_command_envelope"));
        envelope = envelope.with_client_session_id("session-1");
        envelope.validate().expect("valid v2 command envelope");
    }
}
