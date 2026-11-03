//! Native Ringing client contracts.
//!
//! The daemon, transport client, and native shells share the canonical domain
//! and wire types. JSON is a serialization detail at the HTTP/SSE boundary;
//! it is not an application-facing event model.

// `RINGING_SCHEMA` 由下面的 `pub use` 一并带入本模块作用域。
use serde::{Deserialize, Serialize};

// 再导出的完整性是**有意的**：壳层只认 `qaqh-client` 一个入口，不该越过它去直接
// 依赖 `qaqh-domain`/`qaqh-ringing`/`qaqh-types`。缺一个名字，壳层就只能再抄一份
// 镜像——而抄镜像正是漂移的来源（TUI 曾照抄 2419 行协议镜像，实测已漂移出
// config 的两处字段缺失）。
//
// 少数类型带 `Domain` 前缀：它们与 `qaqh-client` 自身的类型**同名而语义不同**。
// 最典型的是 `SessionState`——`qaqh-client::SessionState` 是协商出来的租约状态，
// `qaqh_domain::SessionState` 是会话生命周期状态，两者毫无关系。
pub use qaqh_domain::{
    ActivityState as DomainActivityState, AgentLifecycleState, AskAnswer, AskMode,
    AskQuestion as DomainAskQuestion, AskResolution, CompactStatus, ContentRef, DashboardDocument,
    DashboardSnapshot as DomainDashboardSnapshot, DashboardTask, DomainError, ErrorScope,
    NoticeLevel, PermissionCategory, PermissionRisk, PlanReviewItem, ProviderToolState,
    RoundDeltaKind, SessionActivity, SessionState as DomainSessionState, SkillInfo,
    SkillRuntimeInfo, SkillsStatus,
};
pub use qaqh_ringing::{
    CLIENT_SESSION_HEADER, CommandPrecondition, CommandTarget, ConversationSendMessage,
    MAX_SAFE_INTEGER, RINGING_SCHEMA, RingingCommand, RingingCommandAckStatus, RingingCommandState,
    RingingV2CommandEnvelope, is_safe_integer,
};
pub use qaqh_ringing_contract::commands::ImageBlock;
pub use qaqh_ringing_contract::fact::InputPurpose;
pub use qaqh_types::{
    SessionListEntry, SessionMeta, SessionRunStatus, ToolContinuation, ToolError, ToolImage,
    ToolModelPayload, ToolResult, ToolStatus, UsageInfo,
};

/// 服务端主动终止流的结构化原因。
///
/// `None` 表示普通网络断开或超时；只有服务端发送了终止帧时才会带原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReconnectReason {
    /// 服务端事件缓冲溢出，`skipped` 为明确丢弃的事件数。
    Lagged { skipped: u64 },
    /// 服务端以其他稳定 code 终止流；未知 code 原样保留。
    StreamTerminated { code: String },
}

/// Parsed SSE frame (a block of `key: value` lines separated by a blank line).
#[derive(Debug, Clone, Default)]
pub struct SseFrame {
    pub id: String,
    pub event_type: String,
    pub data: String,
}

/// Options for one typed command submission. The client creates an id when
/// callers do not need to supply one for durable retry/correlation.
#[derive(Debug, Clone, Default)]
pub struct CommandOptions {
    pub command_id: Option<String>,
    pub preconditions: Vec<CommandPrecondition>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_reason_serializes_without_losing_server_detail() {
        assert_eq!(
            serde_json::to_value(ReconnectReason::Lagged { skipped: 7 }).expect("serialize lagged"),
            serde_json::json!({"kind": "lagged", "skipped": 7})
        );
        assert_eq!(
            serde_json::to_value(ReconnectReason::StreamTerminated {
                code: "protocol_version".into(),
            })
            .expect("serialize terminated"),
            serde_json::json!({"kind": "stream_terminated", "code": "protocol_version"})
        );
    }
}
