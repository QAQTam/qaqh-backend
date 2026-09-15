//! 频道快照（wire 视图）。

use qaqh_domain::RingingChannel;
use serde::{Deserialize, Serialize};
#[cfg(feature = "ts")]
use ts_rs::TS;

use crate::protocol::{RINGING_SCHEMA, RINGING_VERSION};

/// 频道领域快照。**必须表达领域状态，禁止用事件数组模拟状态**
/// （PLAN 硬规则）。`state` 为对应频道的领域快照 payload
/// （Conversation/Tool/Control snapshot projection 在 transport 层注入强类型）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingChannelSnapshot {
    pub schema: String,
    pub version: u32,
    pub channel: RingingChannel,
    pub seed: String,
    /// 快照覆盖到的 stream_seq 基线（其后的可靠事件需从 cursor 回放）。
    #[cfg_attr(feature = "ts", ts(as = "u32"))]
    pub baseline_stream_seq: u64,
    #[cfg_attr(feature = "ts", ts(as = "u32"))]
    pub state_revision: u64,
    pub snapshot_version: u32,
    pub state: serde_json::Value,
}

impl RingingChannelSnapshot {
    pub fn new(
        channel: RingingChannel,
        seed: impl Into<String>,
        baseline_stream_seq: u64,
        state_revision: u64,
        state: serde_json::Value,
    ) -> Self {
        Self {
            schema: RINGING_SCHEMA.to_string(),
            version: RINGING_VERSION,
            channel,
            seed: seed.into(),
            baseline_stream_seq,
            state_revision,
            snapshot_version: 1,
            state,
        }
    }
}

/// 原子恢复一个 session 所需的完整三频道快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingSessionBootstrap {
    pub schema: String,
    pub version: u32,
    pub server_epoch: String,
    pub seed: String,
    pub control: RingingChannelSnapshot,
    pub conversation: RingingChannelSnapshot,
    pub tool: RingingChannelSnapshot,
}

impl RingingSessionBootstrap {
    pub fn new(
        server_epoch: impl Into<String>,
        seed: impl Into<String>,
        control: RingingChannelSnapshot,
        conversation: RingingChannelSnapshot,
        tool: RingingChannelSnapshot,
    ) -> Self {
        Self {
            schema: RINGING_SCHEMA.to_string(),
            version: RINGING_VERSION,
            server_epoch: server_epoch.into(),
            seed: seed.into(),
            control,
            conversation,
            tool,
        }
    }

    /// conversation 频道的**类型化** `state` 视图（前端契约 G1）。
    ///
    /// 三端（winui / web / TUI）此前各自手解 `state: Value`，且必然各解各的——
    /// TUI 的手解至今漏了六个字段而无人察觉。用这里取代手解。
    ///
    /// **降级语义**：`qaqh_domain::state` 的字段全部带 `#[serde(default)]`，故形状
    /// 漂移通常表现为**字段变缺省**而非 `Err`。这不是「静默失败」，而是刻意的
    /// 前向兼容——漂移由 `qaqh-runtime` 的产出方往返测试兜住（直接拿真实快照断言
    /// 字段非缺省），不靠运行期报错。`Err` 只在 `state` 根本不是对象等病态情形出现。
    pub fn conversation_state(&self) -> Result<qaqh_domain::state::ConversationState, serde_json::Error> {
        serde_json::from_value(self.conversation.state.clone())
    }

    /// control 频道的类型化 `state` 视图。降级语义同 [`Self::conversation_state`]。
    pub fn control_state(&self) -> Result<qaqh_domain::state::ControlState, serde_json::Error> {
        serde_json::from_value(self.control.state.clone())
    }

    /// tool 频道的类型化 `state` 视图。降级语义同 [`Self::conversation_state`]。
    pub fn tool_state(&self) -> Result<qaqh_domain::state::ToolState, serde_json::Error> {
        serde_json::from_value(self.tool.state.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trip() {
        let snap = RingingChannelSnapshot::new(
            RingingChannel::Tool,
            "s1",
            42,
            3,
            serde_json::json!({ "running": [], "pending_permission": null }),
        );
        let json = serde_json::to_string(&snap).expect("serialize");
        assert!(json.contains("\"schema\":\"qaqh.Ringing\""));
        assert!(json.contains("\"baseline_stream_seq\":42"));
        let back: RingingChannelSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.state_revision, 3);
        assert_eq!(back.snapshot_version, 1);
    }
}
