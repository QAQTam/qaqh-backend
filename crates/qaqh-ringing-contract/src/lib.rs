//! Ringing 公共契约:会话状态、类型化事件、命令目录与共享词表。
//!
//! 本 crate 是客户端与 daemon 的唯一契约来源;不依赖存储(qaqh-session)、
//! 执行(qaqh-agent-runtime)或传输实现。依赖方向:所有实现 crate → 本 crate;
//! 公共契约独立于数据库、投影与执行循环即可被客户端使用和导出。

#![cfg_attr(not(test), forbid(unsafe_code))]
use serde::{Deserialize, Serialize};

pub mod commands;
pub mod events;
pub mod fact;
pub mod state;
pub mod team;
pub mod ulid;

pub use events::{SessionEvent, SessionEventFrame};
pub use state::{CodeDelta, SessionBootstrap, SessionState, ToolEffectInfo};
pub use team::{TaskBoardSnapshot, TeamBoardSnapshot, TeamSnapshot};

/// `/team` 端点 schema 判别值(契约锁定:客户端按字面量校验)。
pub const TEAM_SCHEMA: &str = "qaqh.ringing.team/v1";

/// `GET /ringing/v2/sessions/{seed}/team` 的 typed 响应:roster + inbox
/// (`team`)、task board(`tasks`)、message board(`board`)一次拿全。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct TeamResponse {
    pub schema: String,
    pub session_id: String,
    pub team: TeamSnapshot,
    pub tasks: TaskBoardSnapshot,
    pub board: TeamBoardSnapshot,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// daemon `/team` 的 wire 形状锁:与客户端侧共享同一份样本。少一个字段/
    /// 改一个键名,前端在编译期或运行期直接断。
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
                "team_id": null,
                "revision": 0,
                "last_fact_seq": 0,
                "tasks": []
            },
            "board": {
                "board_id": null,
                "revision": 0,
                "last_fact_seq": 0,
                "channels": [],
                "threads": [],
                "posts": [],
                "subscriptions": []
            }
        });
        let response: TeamResponse =
            serde_json::from_value(payload.clone()).expect("daemon /team 形状必须能反序列化");
        assert_eq!(response.schema, TEAM_SCHEMA);
        assert_eq!(response.team.agents.len(), 2);
        let serialized = serde_json::to_value(&response).expect("serialize");
        // 无损往返:反序列化再序列化不得丢键/改值。
        let reparsed: TeamResponse =
            serde_json::from_value(serialized.clone()).expect("reparse");
        assert_eq!(reparsed, response);
    }

    /// 空会话的 SessionState 必须能零值往返(SDK 首连空会话路径)。
    #[test]
    fn session_state_empty_round_trip() {
        let state = SessionState::default();
        let value = serde_json::to_value(&state).expect("serialize");
        let reparsed: SessionState = serde_json::from_value(value).expect("reparse");
        assert_eq!(reparsed, state);
    }

    #[test]
    fn timeline_input_reads_payloads_without_new_metadata_fields() {
        let payload = serde_json::json!({
            "kind": "input",
            "data": {
                "block_id": "B1",
                "turn_id": null,
                "purpose": "trigger_turn",
                "content": {"kind": "inline", "data": {"text": "hello"}},
                "attachments": [],
                "actor": {"kind": "user", "id": "user", "display_name": null}
            }
        });
        let entry: state::TimelineEntry =
            serde_json::from_value(payload).expect("old timeline input must still deserialize");
        assert!(matches!(
            entry,
            state::TimelineEntry::Input {
                images,
                inter_agent: None,
                subagent_terminal: None,
                ..
            } if images.is_empty()
        ));
    }
}
