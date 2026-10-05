//! 中立领域事件（DomainEvent）。
//!
//! - 事件按频道拆分（Control / Conversation / Tool），由统一枚举 `DomainEvent` 聚合。
//! - 本模块不得引用 legacy 类型（`Agent2Ui`）或 wire 类型（`Ringing*Envelope`）。

use serde::{Deserialize, Serialize};
#[cfg(feature = "ts")]
use ts_rs::TS;

use qaqh_types::UsageInfo;
pub use qaqh_types::{ContentRef, ToolResult};


// ─────────────────────────────────────────────────────────────────────────────
// 共享支持类型
// ─────────────────────────────────────────────────────────────────────────────

/// RoundDelta 的流式块种类（决策记录 Q2：保留 kind 作 replaceable 合并键）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundDeltaKind {
    Thinking,
    ToolCalling,
    Answering,
}

/// provider 内建/服务端工具状态（决策记录 Q3 定稿：封闭枚举，禁止自由字符串）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderToolState {
    InProgress,
    Searching,
    Completed,
}

/// compact 终态（PLAN：completed/skipped/failed/cancelled 明确状态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactStatus {
    Completed,
    Skipped,
    Failed,
    Cancelled,
}

/// 通知级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    Info,
    Warn,
    Error,
}

/// 工具权限分类（legacy `category: "read"|"write"|"exec"|"net"`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionCategory {
    Read,
    Write,
    Exec,
    Net,
}

/// 工具动作内在影响等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionRisk {
    Low,
    Medium,
    High,
}

/// 会话生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Created,
    Resumed,
    Closed,
    Archived,
    Unarchived,
    Deleted,
}

/// 会话活动状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum ActivityState {
    Starting,
    Idle,
    Working,
    WaitingUser,
    Disconnected,
    /// 回合失败（TurnFailed）。区别于用户取消（收敛回 Idle）：错误态需要
    /// 在前端以告警色驻留，直到下一回合开始或新的 Ready 收敛。
    Failed,
}

/// 会话活动快照：`session.activity` 方法与 daemon `/activity` 观测端点的
/// 直接序列化载体（原 proto 同名类型，PR-3-2 迁入 domain 后删除）。
/// JSON 形状由 runtime 侧 shape 快照测试逐字段冻结，不得漂移；
/// 刻意不加 ts-rs 导出（维持零前端曝光现状，bindings 数不变）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionActivity {
    /// Session identifier.
    #[serde(rename = "session_id")]
    pub session_id: String,
    /// Current lifecycle state.
    pub state: ActivityState,
    /// Active turn ID, if a turn is in progress or suspended.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// Monotonic event sequence number for this session.
    pub seq: u64,
    /// Unix timestamp of this state change.
    pub updated_at: u64,
}

/// A single code delta record for persistence.
/// （原 proto 同名类型，PR-3-5 溶解时回流迁入；刻意不加 ts-rs 导出。）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeDeltaRecord {
    pub timestamp: u64,
    pub lines_added: usize,
    pub lines_removed: usize,
    pub files_created: usize,
    pub files_deleted: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

/// agent **进程**生命周期（决策记录 Q8：只含进程状态；回合结束走
/// 会话活动变更事件（Idle），transport 状态另由客户端健康判定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentLifecycleState {
    Booting,
    Ready,
    Stopping,
    Stopped,
}

/// A document visible in the dashboard. This intentionally mirrors only the
/// renderer-facing tracking state, not the legacy protocol type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardDocument {
    pub tag: String,
    pub path: String,
    pub turns_since_read: u32,
    pub is_stale: bool,
}

/// One persisted task row for the native dashboard activity snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardTask {
    pub id: String,
    pub subject: String,
    pub description: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

/// Replaceable dashboard/activity payload. It is deliberately separate from
/// the transcript and is sufficient for the Electron dashboard without an
/// `Agent2Ui::Dashboard` projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardSnapshot {
    #[serde(rename = "session_id")]
    pub session_id: String,
    pub documents: Vec<DashboardDocument>,
    pub recent_edits: Vec<String>,
    pub tasks: Vec<DashboardTask>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_todo_id: Option<String>,
}

/// 失败终态的错误域（PLAN：错误带 scope、code、retryable、dedupe_key）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainError {
    /// 唯一错误实例 id，用于 toast 去重与日志关联。
    pub error_id: String,
    /// 稳定错误码（如 "provider_http_500"）。
    pub code: String,
    /// 人类可读消息（脱敏，禁止含 API key / provider 原始响应）。
    pub message: String,
    /// 是否可重试。
    pub retryable: bool,
    /// 去重键；同键错误只产生一个前端 toast。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
}

/// 错误归属域（用于 OperationFailed 的 scope 字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorScope {
    Control,
    Conversation,
    Tool,
    System,
}

/// ask_user 的提问模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskMode {
    Single,
    Batch,
}

/// ask_user 交互如何离队。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskResolution {
    Answered,
    Dismissed,
}

/// ask_user 中的单个问题。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskQuestion {
    /// 本 ask 内唯一（如 "q1"）。
    pub id: String,
    /// 问题文本（支持 Markdown）。
    pub question: String,
    /// 预设选项；空 = 仅自由文本。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// 是否允许自定义输入。
    #[serde(default = "default_true")]
    pub allow_custom: bool,
}

fn default_true() -> bool {
    true
}

/// plan review 评审项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanReviewItem {
    pub id: String,
    pub title: String,
    pub description: String,
    /// "small" | "medium" | "large"
    pub complexity: String,
}

/// skill 目录条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    /// "project" | "user"
    pub scope: String,
    /// 相对 workspace 的展示路径。
    pub source: String,
}

/// skill 运行时条目（catalog/requested/active/unavailable 生命周期状态）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRuntimeInfo {
    pub name: String,
    pub description: String,
    /// 生命周期状态：\"catalog\" | \"requested\" | \"active\" | \"unavailable\"。
    pub state: String,
    /// 展示源路径。
    pub source: String,
    /// skill 正文估算 token 数。
    pub token_count: usize,
    /// 加载失败时的错误信息。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 技能面板全量状态（frontend skills panel 展示）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillsStatus {
    /// 全部可发现技能。
    pub available: Vec<SkillInfo>,
    /// 当前已加载（显式 / $ 提及激活）的技能名。
    pub active: Vec<String>,
    #[serde(default)]
    pub catalog_revision: String,
    #[serde(default)]
    pub context_epoch: u64,
    #[serde(default)]
    pub operation_revision: u64,
    #[serde(default)]
    pub token_budget: usize,
    #[serde(default)]
    pub token_usage: usize,
    #[serde(default)]
    pub runtime: Vec<SkillRuntimeInfo>,
    #[serde(default)]
    pub diagnostics: Vec<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Conversation 频道
// ─────────────────────────────────────────────────────────────────────────────

/// Conversation 频道领域事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConversationEvent {
    /// 新回合开始（`ConversationSendMessage` accepted 后的权威开始事件）。
    TurnStarted { turn_id: String, user_text: String },
    /// 回合完成（成功）。`TurnFailed` 为失败终态。
    TurnCompleted {
        turn_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<UsageInfo>,
    },
    /// 回合失败（新领域事件；provider 最终失败只产生一个可靠失败终态）。
    TurnFailed { turn_id: String, error: DomainError },
    /// 流式增量（reliable：增量是追加语义，覆盖/合并会吞字；journal 在
    /// `RoundCompleted` 到达后按 round 压缩，见 `ReliableJournal::compact_round_deltas`）。
    RoundDelta {
        turn_id: String,
        round_num: u32,
        kind: RoundDeltaKind,
        delta: String,
    },
    /// 流式块的周期**完整值**（replaceable，覆盖语义，治 D1）。
    ///
    /// `RoundDelta` 是追加增量（reliable，前端拼接）；本事件携带该 round
    /// 当前完整文本，乱序/丢 delta 由下一次 checkpoint 自愈，前端直接
    /// 覆盖赋值。`RoundCompleted` 仍是权威终态（到达后本事件可压缩）。
    BlockCheckpoint {
        turn_id: String,
        round_num: u32,
        kind: RoundDeltaKind,
        text: String,
        char_count: u32,
    },
    /// 一轮 API 调用完成的权威终态。正文大时经 `output_ref` 外置。
    RoundCompleted {
        turn_id: String,
        round_num: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answer: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_ref: Option<ContentRef>,
        /// true = 本回合最后一个 round。
        is_final: bool,
    },
    /// compact 开始（携带 compact_id）。
    CompactStarted {
        compact_id: String,
        turns_total: u32,
        turns_keeping: u32,
    },
    /// compact 流式摘要（replaceable，按 compact_id 合并）。
    CompactProgress { compact_id: String, delta: String },
    /// compact 终态。`ConversationCompact` accepted 不代表成功，本事件才是终态。
    CompactFinished {
        compact_id: String,
        status: CompactStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary_chars: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turns_compacted: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turns_removed: Option<u32>,
    },
    /// 回合被用户取消。
    ConversationCancelled {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
}

// ─────────────────────────────────────────────────────────────────────────────
// Tool 频道
// ─────────────────────────────────────────────────────────────────────────────

/// Tool 频道领域事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // 装箱改造属结构塑形，另立项
pub enum ToolEvent {
    /// 工具真正开始执行（决策记录 Q1：permission 通过后，reliable）。
    ToolStarted {
        tool_call_id: String,
        turn_id: String,
        round_num: u32,
        name: String,
    },
    /// 工具执行成功终态（terminal；发送前必须 flush/覆盖同工具 replaceable 进度）。
    ToolFinished {
        tool_call_id: String,
        turn_id: String,
        round_num: u32,
        result: ToolResult,
    },
    /// 权限请求：agent 挂起回合等待用户批准/拒绝。
    ToolPermissionRequested {
        tool_call_id: String,
        turn_id: String,
        round_num: u32,
        tool_name: String,
        /// Bounded, tool-specific action summary for informed approval.
        /// Optional for backward compatibility with older producers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action_summary: Option<String>,
        reason: String,
        paths: Vec<String>,
        category: PermissionCategory,
        level: u8,
        risk: PermissionRisk,
        consequence: String,
    },
}

// ─────────────────────────────────────────────────────────────────────────────
// Control 频道
// ─────────────────────────────────────────────────────────────────────────────

/// Control 频道领域事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlEvent {
    /// 会话生命周期状态变更。
    SessionStateChanged {
        #[serde(rename = "session_id")]
        session_id: String,
        state: SessionState,
    },
    /// 会话元数据变更（标题生成/重命名）——前端收到后重拉 session.list。
    SessionMetaChanged {
        #[serde(rename = "session_id")]
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// agent **进程**生命周期（决策记录 Q8：不含回合状态）。
    AgentLifecycleChanged { state: AgentLifecycleState },
    /// 会话仪表盘（replaceable，覆盖式）。
    DashboardUpdated {
        hp_connected: bool,
        #[serde(rename = "session_id", alias = "session_seed")]
        session_id: String,
        tool_calls_total: u32,
        tool_failures: u32,
        current_phase: String,
        streaming: bool,
    },
    /// Full native dashboard activity state, replaceable by session seed.
    DashboardSnapshot { snapshot: DashboardSnapshot },
    /// ask_user 交互请求（决策记录 Q10：ask/plan 归 Control，permission 归 Tool）。
    InteractionRequested {
        interaction_id: String,
        turn_id: String,
        mode: AskMode,
        questions: Vec<AskQuestion>,
    },
    /// ask_user 交互终结。
    InteractionResolved {
        interaction_id: String,
        resolution: AskResolution,
    },
    /// plan review 请求（plan 或 todo_activation）。
    PlanReviewRequested {
        interaction_id: String,
        turn_id: String,
        plan_content: String,
        #[serde(default)]
        review_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        todo_items: Option<Vec<PlanReviewItem>>,
    },
    /// plan review 已裁决。
    PlanReviewResolved {
        interaction_id: String,
        approved: bool,
    },
    /// v2 driver 席位变更（canonical `DriverChanged` 的 Ringing 双发）。
    DriverChanged {
        /// `None` = 席位已释放。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        holder: Option<String>,
        driver_epoch: u64,
    },
    /// skill 目录/激活状态变更。
    SkillsUpdated {
        available: Vec<SkillInfo>,
        active: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        catalog_revision: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_revision: Option<u64>,
        #[serde(default)]
        context_epoch: usize,
        #[serde(default)]
        token_budget: usize,
        #[serde(default)]
        token_usage: usize,
        #[serde(default)]
        runtime: Vec<SkillRuntimeInfo>,
        #[serde(default)]
        diagnostics: Vec<String>,
    },
    /// 子代理终态推送：注入被回合 lap 边界吸收（无独立注入回合）时，
    /// 前端 tracker 的唯一收敛信号仍缺失——本事件补发轻量终态，不进入
    /// 回合状态机、不进模型上下文。`state` 为注入标签原样
    /// （COMPLETED / ERROR / TIMEOUT / CANCELLED）。
    SubagentStatus {
        #[serde(rename = "session_id")]
        session_id: String,
        name: String,
        state: String,
    },
    /// 无专用领域终态载荷的命令已完成。用于 undo/set-mode/reload 等
    /// 操作的 receipt 收口，不承担 UI 通知语义。
    OperationCompleted {
        occurrence_id: String,
        scope: ErrorScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_id: Option<String>,
    },
    /// 业务失败终态（结构化、可关联、可去重）。
    OperationFailed {
        occurrence_id: String,
        scope: ErrorScope,
        error: DomainError,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_id: Option<String>,
    },
}

// ─────────────────────────────────────────────────────────────────────────────
// 统一领域事件入口
// ─────────────────────────────────────────────────────────────────────────────

/// 统一领域事件。`channel()` 决定进入哪个频道 router。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "channel", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // 装箱改造属结构塑形，另立项
pub enum DomainEvent {
    Control(ControlEvent),
    Conversation(ConversationEvent),
    Tool(ToolEvent),
}

// ─────────────────────────────────────────────────────────────────────────────
// 测试
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_round_trip_keeps_fields() {
        let event = DomainEvent::Conversation(ConversationEvent::TurnStarted {
            turn_id: "t1".into(),
            user_text: "hello".into(),
        });
        let json = serde_json::to_string(&event).expect("serialize");
        assert!(json.contains("\"channel\":\"conversation\""));
        assert!(json.contains("\"type\":\"turn_started\""));
        let back: DomainEvent = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(
            back,
            DomainEvent::Conversation(ConversationEvent::TurnStarted { ref user_text, .. })
                if user_text == "hello"
        ));
    }

    #[test]
    fn dashboard_task_round_trip_keeps_evidence_and_accepts_legacy_rows() {
        let current = DashboardTask {
            id: "T1".into(),
            subject: "Verify".into(),
            description: "Run checks".into(),
            status: "completed".into(),
            evidence: Some("all checks passed".into()),
        };
        let json = serde_json::to_string(&current).expect("serialize dashboard task");
        assert!(json.contains("\"evidence\":\"all checks passed\""));
        let back: DashboardTask = serde_json::from_str(&json).expect("deserialize dashboard task");
        assert_eq!(back.evidence.as_deref(), Some("all checks passed"));

        let legacy: DashboardTask = serde_json::from_value(serde_json::json!({
            "id": "T2",
            "subject": "Legacy",
            "description": "",
            "status": "idle"
        }))
        .expect("legacy dashboard row without evidence remains readable");
        assert!(legacy.evidence.is_none());
    }

    #[test]
    fn operation_failed_error_round_trip() {
        let event = ControlEvent::OperationFailed {
            occurrence_id: "occ-1".into(),
            scope: ErrorScope::Tool,
            error: DomainError {
                error_id: "e-1".into(),
                code: "provider_http_500".into(),
                message: "upstream".into(),
                retryable: true,
                dedupe_key: Some("k".into()),
            },
            operation_id: Some("op-1".into()),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let back: ControlEvent = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(
            back,
            ControlEvent::OperationFailed {
                scope: ErrorScope::Tool,
                ..
            }
        ));
    }

}
