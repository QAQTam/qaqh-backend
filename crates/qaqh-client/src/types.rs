//! Native Ringing client contracts.
//!
//! The daemon, transport client, and native shells share the canonical domain
//! and wire types. JSON is a serialization detail at the HTTP/SSE boundary;
//! it is not an application-facing event model.

// `RINGING_SCHEMA`/`RINGING_VERSION` 由下面的 `pub use` 一并带入本模块作用域。
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
    ControlEvent, ConversationCommand, ConversationEvent, ConversationMode, DashboardDocument,
    DashboardSnapshot as DomainDashboardSnapshot, DashboardTask, Delivery, DomainError, ErrorScope,
    ImageBlock, NoticeLevel, PermissionCategory, PermissionRisk, ProviderToolState,
    RingingChannel as Channel, RoundDeltaKind, SessionActivity, SessionState as DomainSessionState,
    SkillInfo, SkillRuntimeInfo, SkillsStatus, TimelineBlock, TimelineBlockKind,
    TimelineBlockState, TimelineEntry, TimelineEvent, TimelineFailure, TimelinePathOp,
    TimelineRound, TimelineSnapshot, TimelineTool, TimelineToolBody, TimelineToolDisplay,
    TimelineToolHeader, TimelineToolMetrics, TimelineToolPermission, TimelineToolState,
    TimelineTurn, TimelineTurnState, TodoItem, ToolCommand, ToolEvent,
};
pub use qaqh_ringing::{
    CLIENT_SESSION_HEADER, ClientOpenRequest as OpenRequest, ClientOpenResponse as OpenResponse,
    MAX_SAFE_INTEGER, RINGING_SCHEMA, RINGING_VERSION, RingingChannelSnapshot, RingingCommand,
    RingingCommandAck, RingingCommandAckStatus, RingingCommandState, RingingCommandStatus,
    RingingEvent, RingingEventBatch as EventBatch, RingingEventEnvelope,
    RingingResetRequired as ResetRequired, RingingSessionBootstrap, is_safe_integer,
};
pub use qaqh_types::{
    SessionListEntry, SessionMeta, ToolContinuation, ToolError, ToolImage, ToolModelPayload,
    ToolResult, ToolStatus, UsageInfo,
};

/// Stable channel order used to start the three independent SSE streams.
pub const CHANNELS: [Channel; 3] = [Channel::Control, Channel::Conversation, Channel::Tool];

/// Per-channel SSE connection state. This is a native transport state rather
/// than a renderer payload; UI shells marshal it onto their dispatcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelStatus {
    Connecting,
    Open { server_epoch: String, cursor: u64 },
    Reconnecting { retry_ms: u64, last_cursor: u64 },
    Closed { reason: String },
}

/// Per-session timeline connection state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineStatus {
    Connecting {
        seed: String,
    },
    Open {
        seed: String,
        server_epoch: String,
        cursor: u64,
    },
    Reconnecting {
        seed: String,
        retry_ms: u64,
        cursor: u64,
    },
    Closed {
        seed: String,
        reason: String,
    },
}

/// Versioned response from `GET /ringing/v1/sessions/{seed}/timeline`.
///
/// `snapshot` is the authoritative materialized transcript. Pagination
/// metadata remains outside it because it describes the current HTTP page,
/// not transcript state.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TimelinePage {
    pub schema: String,
    pub version: u32,
    pub server_epoch: String,
    pub seed: String,
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
    pub fn validate_for(&self, seed: &str) -> Result<(), String> {
        if self.schema != RINGING_SCHEMA
            || self.version != RINGING_VERSION
            || self.seed != seed
            || self.server_epoch.is_empty()
        {
            return Err("invalid Ringing V1 timeline page".into());
        }
        Ok(())
    }
}

/// Ringing V1 timeline SSE frame.
#[derive(Debug, Clone, Deserialize)]
pub struct TimelineSseFrame {
    pub schema: String,
    pub version: u32,
    pub server_epoch: String,
    pub seed: String,
    pub entry: TimelineEntry,
}

/// Parsed SSE frame (a block of `key: value` lines separated by a blank line).
#[derive(Debug, Clone, Default)]
pub struct SseFrame {
    pub id: String,
    pub event_type: String,
    pub data: String,
}

/// Extract the stream sequence from `id: <epoch>:<channel>:<seq>`.
pub fn cursor_from_sse_id(id: &str, channel: Channel) -> Option<u64> {
    let mut parts = id.split(':');
    let epoch = parts.next()?;
    let frame_channel = parts.next()?;
    let seq = parts.next()?;
    if epoch.is_empty() || frame_channel != channel.as_str() || parts.next().is_some() {
        return None;
    }
    seq.parse::<u64>().ok()
}

pub fn validate_envelope(envelope: &RingingEventEnvelope, channel: Channel) -> Result<(), String> {
    envelope.validate().map_err(str::to_string)?;
    if envelope.event.channel() != channel {
        return Err(format!(
            "envelope channel {:?} != connection channel {:?}",
            envelope.event.channel(),
            channel
        ));
    }
    Ok(())
}

/// Wrap one validated SSE envelope in the canonical Ringing batch type.
pub fn envelope_to_batch(
    channel: Channel,
    envelope: RingingEventEnvelope,
    server_epoch: String,
) -> EventBatch {
    let seq = envelope.stream_seq;
    EventBatch {
        schema: RINGING_SCHEMA.to_string(),
        version: RINGING_VERSION,
        channel,
        seed: envelope.seed.clone(),
        server_epoch,
        from_stream_seq: seq,
        to_stream_seq: seq,
        envelopes: vec![envelope],
    }
}

/// Options for one typed command submission. The client creates an id when
/// callers do not need to supply one for durable retry/correlation.
#[derive(Debug, Clone, Default)]
pub struct CommandOptions {
    pub command_id: Option<String>,
    pub expected_revision: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::{ConversationEvent, Delivery};
    use qaqh_ringing::RingingEvent;

    #[test]
    fn canonical_envelope_keeps_domain_event_typed() {
        let envelope = RingingEventEnvelope::new(
            "seed-1",
            7,
            3,
            3,
            "event-1",
            RingingEvent::Conversation(ConversationEvent::TurnStarted {
                turn_id: "t1".into(),
                user_text: "hello".into(),
            }),
        );
        assert_eq!(envelope.delivery, Delivery::Reliable);
        validate_envelope(&envelope, Channel::Conversation).expect("valid envelope");
        let batch = envelope_to_batch(Channel::Conversation, envelope, "epoch-1".into());
        batch.validate().expect("canonical batch");
    }

    /// 帧 id 是游标推进的**唯一**依据（`{epoch}:{channel}:{seq}`）——解析放宽
    /// 一个字符，客户端就会按错误的位置续传。逐条锁住形状。
    #[test]
    fn cursor_from_sse_id_accepts_only_the_exact_shape() {
        let ch = Channel::Conversation;
        assert_eq!(cursor_from_sse_id("epoch-1:conversation:7", ch), Some(7));
        assert_eq!(cursor_from_sse_id("epoch-1:conversation:0", ch), Some(0));

        // 频道不匹配：把 tool 的帧当 conversation 的续传位置，会直接跳过一段。
        assert_eq!(cursor_from_sse_id("epoch-1:tool:7", ch), None);
        // 段数不对（多一段 = 帧 id 形状变了，不能猜）。
        assert_eq!(cursor_from_sse_id("epoch-1:conversation:7:9", ch), None);
        // 少段。
        assert_eq!(cursor_from_sse_id("epoch-1:conversation", ch), None);
        assert_eq!(cursor_from_sse_id("epoch-1", ch), None);
        assert_eq!(cursor_from_sse_id("", ch), None);
        // 空 epoch：daemon 尚未协商，此时没有可续传的位置。
        assert_eq!(cursor_from_sse_id(":conversation:7", ch), None);
        // seq 非数字 / 空。
        assert_eq!(cursor_from_sse_id("epoch-1:conversation:abc", ch), None);
        assert_eq!(cursor_from_sse_id("epoch-1:conversation:", ch), None);
    }

    #[test]
    fn timeline_page_validates_version_and_seed() {
        let page = TimelinePage {
            schema: RINGING_SCHEMA.into(),
            version: RINGING_VERSION,
            server_epoch: "epoch-1".into(),
            seed: "seed-1".into(),
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
}
