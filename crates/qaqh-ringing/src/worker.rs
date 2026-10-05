//! agent 边界消息类型（进程内 channel 按值传递；测试经 JSON-LP 行往返）。
//!
//! 曾经的 framed OS pipe 线格式（schema/version/`wire` 判别、direction、
//! 16MB 帧帽）已随 `Loop::new_ipc` 的退役删除：生产路径为
//! `Loop::from_channels` 的 typed 传递，此处仅保留地址与因果元数据。
//! 频道不再冗余存储——由载荷 `command`/`event` 自身派生。

use serde::{Deserialize, Serialize};

use crate::command::RingingCommand;
use crate::event::RingingEvent;

/// daemon → agent 命令消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingWorkerCommandEnvelope {
    pub session_id: String,
    pub command_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
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
            expected_revision: None,
            actor: None,
            command,
        }
    }

    pub fn with_actor(mut self, actor: Option<WorkerActor>) -> Self {
        self.actor = actor;
        self
    }

    pub fn with_expected_revision(mut self, revision: Option<u64>) -> Self {
        self.expected_revision = revision;
        self
    }
}

/// agent → daemon 事件消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingWorkerEventEnvelope {
    pub session_id: String,
    pub event_id: String,
    /// 因果来源 command_id（Ringing 命令执行期间产出的事件携带）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<String>,
    pub event: RingingEvent,
}

impl RingingWorkerEventEnvelope {
    pub fn new(
        session_id: impl Into<String>,
        event_id: impl Into<String>,
        event: RingingEvent,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            event_id: event_id.into(),
            causation_id: None,
            event,
        }
    }

    pub fn with_causation(mut self, causation_id: impl Into<String>) -> Self {
        self.causation_id = Some(causation_id.into());
        self
    }
}

/// agent → daemon timeline intent（独立于频道事件：顺序由 daemon 唯一
/// Timeline writer 赋予）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RingingTimelineIntentEnvelope {
    pub session_id: String,
    pub intent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<String>,
    pub intent: qaqh_domain::TimelineIntent,
}

impl RingingTimelineIntentEnvelope {
    pub fn new(
        session_id: impl Into<String>,
        intent_id: impl Into<String>,
        intent: qaqh_domain::TimelineIntent,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            intent_id: intent_id.into(),
            causation_id: None,
            intent,
        }
    }

    pub fn with_causation(mut self, causation_id: impl Into<String>) -> Self {
        self.causation_id = Some(causation_id.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::ToolCommand;

    #[test]
    fn agent_messages_round_trip() {
        let cmd = RingingCommand::Tool(ToolCommand::ToolInvoke {
            tool_call_id: "c".into(),
            name: "exec".into(),
            args: serde_json::json!({ "cmd": "echo hi" }),
        });
        let frame = RingingWorkerCommandEnvelope::new("s1", "cmd-1", cmd);
        let json = serde_json::to_string(&frame).expect("serialize");
        let back: RingingWorkerCommandEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.command.channel(), qaqh_domain::RingingChannel::Tool);
        assert_eq!(back.command_id, "cmd-1");
    }
}
