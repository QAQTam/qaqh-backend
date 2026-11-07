//! Trusted in-process command identity and actor context for the session loop.

use serde::{Deserialize, Serialize};

use crate::command::RingingCommand;

/// daemon → agent 命令消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingWorkerCommandEnvelope {
    pub session_id: String,
    pub command_id: String,
    /// 发起该命令的主体（daemon 依鉴权身份填写，供 canonical 归因穿线）。
    /// `None` = 未知（回退现状：记为本地 user）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<WorkerActor>,
    pub command: RingingCommand,
}

/// 命令发起主体的轻量载体。
///
/// 不引 `qaqh-session::ActorRef`（ringing 是独立 wire 层，不依赖 session）；由
/// runtime 侧映射为 canonical `ActorRef`。`kind` 取 `"user" | "api" | "system" |
/// "agent" | "subagent"`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerActor {
    pub kind: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

impl RingingWorkerCommandEnvelope {
    pub fn new(
        session_id: impl Into<String>,
        command_id: impl Into<String>,
        command: RingingCommand,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            command_id: command_id.into(),
            actor: None,
            command,
        }
    }

    pub fn with_actor(mut self, actor: Option<WorkerActor>) -> Self {
        self.actor = actor;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_messages_round_trip() {
        let cmd = RingingCommand::ToolPermissionRespond {
            tool_call_id: "c".into(),
            approved: true,
            trust_folder: false,
        };
        let frame = RingingWorkerCommandEnvelope::new("s1", "cmd-1", cmd);
        let json = serde_json::to_string(&frame).expect("serialize");
        let back: RingingWorkerCommandEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(
            back.command,
            RingingCommand::ToolPermissionRespond { .. }
        ));
        assert_eq!(back.command_id, "cmd-1");
    }
}
