//! Native Ringing client contracts.
//!
//! The daemon, transport client, and native shells share the canonical domain
//! and wire types. JSON is a serialization detail at the HTTP/SSE boundary;
//! it is not an application-facing event model.

// `RINGING_SCHEMA` 由下面的 `pub use` 一并带入本模块作用域。
use serde::{Deserialize, Serialize};

// 再导出的完整性是**有意的**：壳层只认 `qaqh-client` 一个入口，不该越过它去直接
// 依赖 `qaqh-domain`/`qaqh-ringing`/`qaqh-types`。缺一个名字，壳层就只能再抄一份
// 镜像——而抄镜像正是漂移的来源（TUI 曾照抄 2419 行协议镜像，实测已漂移出四处
// 缺陷：两个 timeline 反序列化缺口、config 的两处字段缺失）。
//
// 少数类型带 `Domain` 前缀：它们与 `qaqh-client` 自身的类型**同名而语义不同**。
// 最典型的是 `SessionState`——`qaqh-client::SessionState` 是协商出来的租约状态，
// `qaqh_domain::SessionState` 是会话生命周期状态，两者毫无关系。
pub use qaqh_domain::{
    ActivityState as DomainActivityState, AgentLifecycleState, AskAnswer, AskMode,
    AskQuestion as DomainAskQuestion, AskResolution, CompactStatus, ContentRef, ControlCommand,
    ControlEvent, ConversationCommand, ConversationEvent, ConversationInputPurpose,
    ConversationMode, DashboardDocument, DashboardSnapshot as DomainDashboardSnapshot,
    DashboardTask, Delivery, DomainError, ErrorScope, ImageBlock, NoticeLevel, PermissionCategory,
    PermissionRisk, PlanReviewItem, ProviderToolState, RingingChannel as Channel, RoundDeltaKind,
    SessionActivity, SessionState as DomainSessionState, SkillInfo, SkillRuntimeInfo, SkillsStatus,
    TimelineBlock, TimelineBlockKind, TimelineBlockState, TimelineEntry, TimelineEvent,
    TimelineFailure, TimelinePathOp, TimelineRound, TimelineSnapshot, TimelineTool,
    TimelineToolBody, TimelineToolDisplay, TimelineToolHeader, TimelineToolMetrics,
    TimelineToolPermission, TimelineToolState, TimelineTurn, TimelineTurnState, ToolCommand,
    ToolEvent,
};
pub use qaqh_ringing::{
    CLIENT_SESSION_HEADER, ClientOpenRequest as OpenRequest, ClientOpenResponse as OpenResponse,
    MAX_SAFE_INTEGER, RINGING_SCHEMA, RINGING_V2_VERSION, RingingCommand, RingingCommandAckStatus,
    RingingCommandState, is_safe_integer,
};
pub use qaqh_types::{
    SessionListEntry, SessionMeta, ToolContinuation, ToolError, ToolImage, ToolModelPayload,
    ToolResult, ToolStatus, UsageInfo,
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

/// Per-session timeline connection state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TimelineStatus {
    Connecting {
        #[serde(rename = "session_id")]
        session_id: String,
    },
    Open {
        #[serde(rename = "session_id")]
        session_id: String,
        server_epoch: String,
        cursor: u64,
    },
    Reconnecting {
        #[serde(rename = "session_id")]
        session_id: String,
        retry_ms: u64,
        cursor: u64,
        reason: Option<ReconnectReason>,
    },
    Closed {
        #[serde(rename = "session_id")]
        session_id: String,
        reason: String,
    },
}

/// Versioned response from `GET /ringing/v2/sessions/{session_id}/timeline`.
///
/// `snapshot` is the authoritative materialized transcript. Pagination
/// metadata remains outside it because it describes the current HTTP page,
/// not transcript state.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TimelinePage {
    pub schema: String,
    pub version: u32,
    pub server_epoch: String,
    #[serde(rename = "session_id")]
    pub session_id: String,
    pub snapshot: TimelineSnapshot,
    /// 本页之前（游标方向）**仍有可交付的回合**，即在已物化的 timeline 里还能
    /// 再往前翻一页。
    ///
    /// 不变式：`has_more == true ⇒ 本页非空`（BUG-2026-09-13-18——违反它会让
    /// 按 has_more 驱动的翻页客户端拿到零行却永不终止）。
    pub has_more: bool,
    /// 会话持久化的**真实回合总数**，与物化窗口无关。
    ///
    /// 与「已交付回合数」的差即未交付的历史；见 [`Self::truncated_before`]。
    pub total_turns: usize,
    /// 物化窗口**未覆盖到历史开头**：更早的回合存在（在 daemon 归档里），但
    /// 本次交付不到，且当前**没有**深翻页接口能取到它们。
    ///
    /// T-08：重建路径只物化最近几十轮。此前这种截断对客户端完全不可见——既无
    /// 提示、也无从知道历史更长。它**不能**用 `has_more` 表达，因为那会让客户端
    /// 反复请求一个永远拿不到内容的页（见上）。
    ///
    /// 无 `#[serde(default)]`：daemon 侧恒发此键（`timeline_api.rs` 的 body 里写死）。
    /// 原先的 default 是为「旧 daemon 不发此键」加的，2026-09-15 按兼容政策删除
    /// （§0b）。缺键=形状不对，应当**响亮失败**而不是静默当成未截断。
    pub truncated_before: bool,
}

impl TimelinePage {
    pub fn validate_for(&self, session_id: &str) -> Result<(), String> {
        if self.schema != RINGING_SCHEMA
            || self.version != RINGING_V2_VERSION
            || self.session_id != session_id
            || self.server_epoch.is_empty()
        {
            return Err("invalid timeline page".into());
        }
        Ok(())
    }
}

/// Timeline SSE frame.
#[derive(Debug, Clone, Deserialize)]
pub struct TimelineSseFrame {
    pub schema: String,
    pub version: u32,
    pub server_epoch: String,
    #[serde(rename = "session_id")]
    pub session_id: String,
    pub entry: TimelineEntry,
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
    pub expected_revision: Option<u64>,
    /// Driver seat epoch the caller holds. `None` opts out of the v2
    /// `stale_driver_epoch` guard (required for interaction answers, which are
    /// not driver-gated).
    pub driver_epoch: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeline_page_validates_version_and_session() {
        let page = TimelinePage {
            schema: RINGING_SCHEMA.into(),
            version: RINGING_V2_VERSION,
            server_epoch: "epoch-1".into(),
            session_id: "seed-1".into(),
            snapshot: TimelineSnapshot {
                watermark: 0,
                turns: vec![],
            },
            has_more: false,
            total_turns: 0,
            truncated_before: false,
        };
        page.validate_for("seed-1").expect("valid page");
        assert!(page.validate_for("seed-2").is_err());
    }

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

    #[test]
    fn reconnecting_status_keeps_none_and_structured_reasons_distinct() {
        let lagged = TimelineStatus::Reconnecting {
            session_id: "seed-1".into(),
            retry_ms: 2_000,
            cursor: 9,
            reason: Some(ReconnectReason::Lagged { skipped: 3 }),
        };
        assert_eq!(
            serde_json::to_value(&lagged).expect("serialize lagged reconnect"),
            serde_json::json!({
                "status": "reconnecting",
                "session_id": "seed-1",
                "retry_ms": 2_000,
                "cursor": 9,
                "reason": {"kind": "lagged", "skipped": 3}
            })
        );
    }
}
