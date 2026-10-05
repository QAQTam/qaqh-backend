//! Ringing envelope：命令 ack 与命令状态。
//!
//! 阶段 3d（hub-fact-bus）：v1 事件信封 `RingingEventEnvelope`、事件批次
//! `RingingEventBatch` 与 v1 命令信封 `RingingCommandEnvelope` 已删除——事件面
//! 由 v2 typed projection envelope（`v2` 模块）承担，本文件只保留命令 ack/状态契约。

use serde::{Deserialize, Serialize};
#[cfg(feature = "ts")]
use ts_rs::TS;

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
