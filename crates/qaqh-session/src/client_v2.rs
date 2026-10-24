//! Ringing v2 客户端契约组合（bootstrap/team 的具体 payload 所有权）。
//!
//! 泛型 wire 容器（`RingingV2Bootstrap`/`RingingV2ChannelSnapshot`）在
//! `qaqh-ringing`；这里定义它们在当前产品里的**具体** state 组合，daemon
//! handler、Rust 共享客户端（`qaqh-client`）与 TS 契约导出共用同一份定义，
//! 不再各自维护镜像 struct 或 `json!` 拼装。
//!
//! 命名沿用壳层既有词汇（`ClientV2*`）：TUI/WinUI/桌面壳经 `qaqh-client`
//! 的同名 re-export 消费，TS 侧由 exporter 以同名导出。
//!
//! 层级约束：本模块可以引用 `qaqh-ringing`（wire 下层），但 `qaqh-ringing`
//! 永远不反向依赖本 crate。

use qaqh_ringing::{RingingV2DriverState, RingingV2PendingInteraction};
use serde::{Deserialize, Serialize};

use crate::projection::{ControlRoundState, ControlSubagentState, ControlToolState, TeamSnapshot};
use crate::session_fact_v2::{
    ActivityState, RecoveryOutcome, SessionId, TeamBoardSnapshot, ToolCallId, TurnId,
};
use crate::team::TaskBoardSnapshot as TeamTaskBoardSnapshot;

/// bootstrap `control.state`：canonical control 快照 + wire 化的 pending
/// 交互与 driver 席位（canonical→wire 的映射发生在 daemon handler）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct ClientV2ControlState {
    pub session_id: Option<SessionId>,
    pub activity: ActivityState,
    pub current_turn_id: Option<TurnId>,
    pub current_call_id: Option<ToolCallId>,
    pub round: Option<ControlRoundState>,
    pub tools: Vec<ControlToolState>,
    pub subagents: Vec<ControlSubagentState>,
    pub last_recovery: Option<RecoveryOutcome>,
    pub revision: u64,
    pub last_fact_seq: u64,
    pub interactions: Vec<RingingV2PendingInteraction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<RingingV2DriverState>,
}

/// bootstrap `tool.state`：与 control 快照共享 `ControlToolState` 词表。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct ClientV2ToolState {
    pub tools: Vec<ControlToolState>,
}

/// `GET /ringing/v2/sessions/{seed}/team` 的 typed 响应。
///
/// 一次拿到三份快照：roster + inbox（`team`）、task board（`tasks`）、
/// message board（`board`）。schema 常量与 daemon handler 保持一致。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct ClientV2TeamResponse {
    pub schema: String,
    pub session_id: String,
    pub team: TeamSnapshot,
    pub tasks: TeamTaskBoardSnapshot,
    pub board: TeamBoardSnapshot,
}

/// `/team` 端点的 schema 判别值（契约锁定，勿改：客户端按字面量校验）。
pub const TEAM_SCHEMA: &str = "qaqh.ringing.team/v1";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_fact_v2::{
        AgentPath, InterAgentDelivery, TeamAgentResidency, TeamAgentSnapshot, TeamAgentStatus,
    };

    /// daemon `/team` 的 wire 形状锁：与 qaqh-client 侧同名契约测试共享同一
    /// 份样本。少一个字段/改一个键名，前端在编译期或运行期直接断。
    #[test]
    fn team_response_round_trips_the_daemon_wire_shape() {
        let payload = serde_json::json!({
            "schema": TEAM_SCHEMA,
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
            serde_json::from_value(payload.clone()).expect("daemon /team 形状必须能反序列化");
        assert_eq!(response.schema, TEAM_SCHEMA);
        assert_eq!(response.team.agents.len(), 2);
        assert_eq!(
            response.team.unread_messages[0].author,
            AgentPath::parse_absolute("/root/writer").expect("agent path")
        );
        assert_eq!(
            response.team.unread_messages[0].delivery,
            InterAgentDelivery::Queue
        );
        let serialized = serde_json::to_value(&response).expect("serialize");
        // 无损往返：反序列化再序列化不得丢键/改值（skip 字段在两侧一致出现）。
        let reparsed: ClientV2TeamResponse =
            serde_json::from_value(serialized.clone()).expect("reparse");
        assert_eq!(serde_json::to_value(&reparsed).unwrap(), serialized);
    }

    /// 空会话的 bootstrap control 状态必须能零值往返（SDK 首连空会话路径）。
    #[test]
    fn control_state_empty_round_trip() {
        let state = ClientV2ControlState::default();
        let value = serde_json::to_value(&state).expect("serialize");
        let reparsed: ClientV2ControlState = serde_json::from_value(value).expect("reparse");
        assert_eq!(reparsed, state);
    }

    /// 让缺少的快照类型在测试里保持命名引用（防止误删导入）。
    #[test]
    fn team_snapshot_types_are_nameable() {
        let agent = TeamAgentSnapshot {
            agent_id: SessionId::new("0199a0f0-0000-7000-8000-000000000002"),
            agent_path: AgentPath::parse_absolute("/root/writer").expect("agent path"),
            nickname: Some("writer".into()),
            role: None,
            status: TeamAgentStatus::Running,
            residency: TeamAgentResidency::Unloaded,
            parent_agent_path: Some(AgentPath::parse_absolute("/root").expect("agent path")),
            current_task_id: None,
        };
        assert_eq!(agent.residency, TeamAgentResidency::Unloaded);
    }
}
