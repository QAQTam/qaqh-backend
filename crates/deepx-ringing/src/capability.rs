//! 客户端 open 与能力协商（`POST /ringing/v2/clients/open` 的 payload 类型）。

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::protocol::{RINGING_SCHEMA, RINGING_VERSION};

/// 能力名称（PLAN 固定命名）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum CapabilityName {
    /// 基础 Ringing v2 协议（HTTP command/query + 三 SSE）。
    RingingV2,
    /// 批量事件（RingingEventBatch）。
    RingingBatchV2,
    /// 全 session typed bootstrap。
    RingingBootstrapV2,
    /// 命令 receipt/status 查询。
    RingingCommandStatusV2,
}

impl CapabilityName {
    pub fn as_str(self) -> &'static str {
        match self {
            CapabilityName::RingingV2 => "Ringing_v2",
            CapabilityName::RingingBatchV2 => "Ringing_batch_v2",
            CapabilityName::RingingBootstrapV2 => "Ringing_bootstrap_v2",
            CapabilityName::RingingCommandStatusV2 => "Ringing_command_status_v2",
        }
    }
}

/// 客户端 open 请求。端点不存在或版本不兼容时才显式选择 legacy；
/// 禁止在同一连接上猜测 frame 类型。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ClientOpenRequest {
    pub schema: String,
    pub version: u32,
    /// 客户端实例 id（后续 lease 绑定该身份）。
    pub client_instance_id: String,
    /// 请求启用的能力名（`CapabilityName::as_str()`）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
}

impl ClientOpenRequest {
    pub fn new(client_instance_id: impl Into<String>, capabilities: Vec<CapabilityName>) -> Self {
        Self {
            schema: RINGING_SCHEMA.to_string(),
            version: RINGING_VERSION,
            client_instance_id: client_instance_id.into(),
            capabilities: capabilities
                .into_iter()
                .map(|c| c.as_str().to_string())
                .collect(),
        }
    }
}

/// 服务端 open 响应。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ClientOpenResponse {
    pub schema: String,
    pub version: u32,
    pub accepted: bool,
    /// 服务端签发的 client session id（lease 与命令绑定该身份）。
    pub client_session_id: String,
    /// 服务端确认启用的能力名。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// 服务端 epoch（SSE stream_seq 基准）。
    pub server_epoch: String,
    /// 逻辑 lease 的 TTL（毫秒）。
    pub lease_ttl_ms: u64,
    /// 建议的 lease renew 间隔（毫秒，< TTL）。
    pub renew_interval_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_request_round_trip() {
        let req = ClientOpenRequest::new(
            "client-a",
            vec![
                CapabilityName::RingingV2,
                CapabilityName::RingingBatchV2,
                CapabilityName::RingingBootstrapV2,
                CapabilityName::RingingCommandStatusV2,
            ],
        );
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(json.contains("\"schema\":\"deepx.Ringing\""));
        assert!(json.contains("Ringing_v2"));
        assert!(json.contains("Ringing_bootstrap_v2"));
        let back: ClientOpenRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.version, RINGING_VERSION);
        assert_eq!(back.capabilities.len(), 4);
    }

    #[test]
    fn capability_names_match_plan() {
        assert_eq!(CapabilityName::RingingV2.as_str(), "Ringing_v2");
        assert_eq!(CapabilityName::RingingBatchV2.as_str(), "Ringing_batch_v2");
        assert_eq!(
            CapabilityName::RingingBootstrapV2.as_str(),
            "Ringing_bootstrap_v2"
        );
        assert_eq!(
            CapabilityName::RingingCommandStatusV2.as_str(),
            "Ringing_command_status_v2"
        );
    }
}
