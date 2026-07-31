//! 频道快照（wire 视图）。

use deepx_domain::RingingChannel;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::protocol::{RINGING_SCHEMA, RINGING_VERSION};

/// 频道领域快照。**必须表达领域状态，禁止用事件数组模拟状态**
/// （PLAN 硬规则）。`state` 为对应频道的领域快照 payload
/// （Conversation/Tool/Control snapshot projection 在 transport 层注入强类型）。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct RingingChannelSnapshot {
    pub schema: String,
    pub version: u32,
    pub channel: RingingChannel,
    pub seed: String,
    /// 快照覆盖到的 stream_seq 基线（其后的可靠事件需从 cursor 回放）。
    pub baseline_seq: u64,
    pub state_revision: u64,
    pub snapshot_version: u32,
    pub state: serde_json::Value,
}

impl RingingChannelSnapshot {
    pub fn new(
        channel: RingingChannel,
        seed: impl Into<String>,
        baseline_seq: u64,
        state_revision: u64,
        state: serde_json::Value,
    ) -> Self {
        Self {
            schema: RINGING_SCHEMA.to_string(),
            version: RINGING_VERSION,
            channel,
            seed: seed.into(),
            baseline_seq,
            state_revision,
            snapshot_version: 1,
            state,
        }
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
        assert!(json.contains("\"schema\":\"deepx.Ringing\""));
        assert!(json.contains("\"baseline_seq\":42"));
        let back: RingingChannelSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.state_revision, 3);
        assert_eq!(back.snapshot_version, 1);
    }
}
