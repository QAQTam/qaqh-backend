//! Ringing envelope：命令信封、ack、命令状态。
//!
//! 阶段 3d（hub-fact-bus）：v1 事件信封 `RingingEventEnvelope` 与事件批次
//! `RingingEventBatch` 已随 v1 事件总线删除——事件面由 v2 typed projection
//! envelope（`v2` 模块）承担，本文件只保留命令 wire 契约。

use qaqh_domain::RingingChannel;
use serde::{Deserialize, Serialize};
#[cfg(feature = "ts")]
use ts_rs::TS;

use crate::command::RingingCommand;
use crate::protocol::{RINGING_SCHEMA, RINGING_VERSION, is_safe_integer};

/// 命令信封（PLAN 固定字段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingCommandEnvelope {
    pub schema: String,
    pub version: u32,
    pub channel: RingingChannel,
    /// 命令幂等 id：accepted 前断线可安全重试；accepted 后不得重复执行。
    pub command_id: String,
    /// 发起客户端实例 id（lease 绑定该身份）。
    pub client_instance_id: String,
    /// open 成功后由 daemon 签发的连接级身份。
    pub client_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// 乐观并发修订（可选）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(as = "u32"))]
    pub expected_revision: Option<u64>,
    pub command: RingingCommand,
}

impl RingingCommandEnvelope {
    pub fn new(
        command_id: impl Into<String>,
        client_instance_id: impl Into<String>,
        command: RingingCommand,
    ) -> Self {
        let channel = command.channel();
        Self {
            schema: RINGING_SCHEMA.to_string(),
            version: RINGING_VERSION,
            channel,
            command_id: command_id.into(),
            client_instance_id: client_instance_id.into(),
            client_session_id: String::new(),
            session_id: None,
            expected_revision: None,
            command,
        }
    }

    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn with_client_session_id(mut self, client_session_id: impl Into<String>) -> Self {
        self.client_session_id = client_session_id.into();
        self
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema != RINGING_SCHEMA || self.version != RINGING_VERSION {
            return Err("unsupported_version");
        }
        if self.command.channel() != self.channel {
            return Err("invalid_envelope");
        }
        if self.command_id.is_empty() || self.client_instance_id.is_empty() {
            return Err("invalid_envelope");
        }
        if self.client_session_id.is_empty() {
            return Err("lease_required");
        }
        if self.session_id.as_deref().is_some_and(str::is_empty) {
            return Err("invalid_envelope");
        }
        if self.session_id.is_none()
            && !matches!(
                self.command,
                RingingCommand::Control(qaqh_domain::ControlCommand::SessionCreate { .. })
            )
        {
            return Err("missing_session_id");
        }
        if self.expected_revision.is_some_and(|v| !is_safe_integer(v)) {
            return Err("invalid_envelope");
        }
        Ok(())
    }
}

/// 命令确认：accepted 仅代表校验通过并进入正确 actor/worker，
/// 业务完成必须通过 `causation_id = command_id` 的可靠事件返回。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum RingingCommandAckStatus {
    Accepted,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingCommandAck {
    pub command_id: String,
    pub status: RingingCommandAckStatus,
    /// 稳定错误码（rejected 时）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// 限流/退避提示（rejected 时）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(as = "u32"))]
    pub retry_after_ms: Option<u64>,
}

/// 可持久化的命令执行状态。ACK 丢失时客户端用原 command_id 查询它。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum RingingCommandState {
    Accepted,
    Running,
    Succeeded,
    Failed,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingCommandStatus {
    pub command_id: String,
    pub state: RingingCommandState,
    pub payload_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_envelope_round_trip() {
        use qaqh_domain::{ConversationCommand, ConversationMode};
        let cmd = RingingCommand::Conversation(ConversationCommand::ConversationSetMode {
            mode: ConversationMode::Plan,
        });
        let env = RingingCommandEnvelope::new("cmd-1", "client-a", cmd).with_session_id("s1");
        let json = serde_json::to_string(&env).expect("serialize");
        assert!(json.contains("\"command_id\":\"cmd-1\""));
        let back: RingingCommandEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.channel, RingingChannel::Conversation);
    }

    #[test]
    fn ack_round_trip() {
        let ack = RingingCommandAck {
            command_id: "cmd-1".into(),
            status: RingingCommandAckStatus::Accepted,
            code: None,
            message: None,
            retry_after_ms: None,
        };
        let json = serde_json::to_string(&ack).expect("serialize");
        assert!(json.contains("\"status\":\"accepted\""));
        let back: RingingCommandAck = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.status, RingingCommandAckStatus::Accepted);
    }
}
