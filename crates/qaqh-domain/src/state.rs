//! 三频道快照 `state` 的**类型化视图**（前端契约 G1）。
//!
//! `RingingChannelSnapshot.state` 是 `serde_json::Value`——中立 JSON，形状此前**没有
//! 任何 Rust 类型承载**。每个前端都得手解，且必然各解各的：TUI 的手解至今漏了
//! `active_turn` / `last_round` / `compact_status` / `compact_id` / `cancelled` /
//! `last_finished` 六个字段而无人察觉。本模块把形状固定下来，三端（winui / web /
//! TUI）共用，web 端可经 `ts` feature 直接生成 TS 类型。
//!
//! # 字段来源（**产出方全量审计**，2026-09-15）
//!
//! 三段 `state` 只有三处写入方，逐字段对应如下（`grep -rnE 'state\["[a-z_]+"\]\s*=[^=]'
//! crates/qaqh-runtime/src/ringing/` 实测：30 处写入**全部**在 `projection.rs`）：
//!
//! 1. `projection.rs::SnapshotProjector::snapshot_for` 的初值 —— 仅 `seed` / `channel` / `revision`
//!    （三者均由快照信封承载，故**不在**本模块的类型里重复）；
//! 2. `projection.rs::fold` —— 事件折叠，下面每个字段的文档注明了它属于哪个频道的
//!    哪个事件分支；
//! 3. `hub.rs::merge_persisted_conversation_state` —— 把持久化投影
//!    （`conversation_snapshot.rs::persisted_conversation_state`）的 9 个键合入
//!    conversation 频道，即本模块 `ConversationState` 的前 9 个字段。
//!
//! # 兼容策略
//!
//! 全部字段带 `#[serde(default)]`：缺字段 = 保持缺省，**解析失败一律降级而不是崩**
//! ——这是各端一致的行为契约。代价是「形状漂移会静默变成缺省」，故**漂移由测试兜**
//! （`qaqh-runtime` 的产出方往返测试直接拿真实快照断言字段非缺省），不靠运行期报错。

use serde::{Deserialize, Serialize};

use crate::{
    ActivityState, AgentLifecycleState, DashboardSnapshot, SessionState, TurnData,
};

#[cfg(feature = "ts")]
use ts_rs::TS;

/// conversation 频道 `state`。
///
/// 不派生 `PartialEq`：含 `TurnData` / `UsageInfo`，二者未派生该 trait；本类型是
/// DTO，消费侧不需要相等比较，故不为它去改动共享类型。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ConversationState {
    // ── 持久化投影（`merge_persisted_conversation_state` 写入）────────────
    /// 完整对话回合投影（与 `RoundData` 逐字段同构）。
    pub turns: Vec<TurnData>,
    /// 会话**持久化的真实回合数**（与 `turns.len()` 未必相等：快照窗口可能被裁剪）。
    pub total_turns: usize,
    /// 快照窗口是否还有更早的回合未交付。
    pub has_more: bool,
    pub usage: Option<qaqh_types::UsageInfo>,
    pub usage_totals: Option<qaqh_types::UsageInfo>,
    pub usage_requests: Option<u64>,
    pub cache_reported_requests: Option<u64>,
    pub model: Option<String>,
    pub context_limit: Option<u64>,

    // ── 事件折叠（`ConversationEvent` 分支）──────────────────────────────
    /// 当前进行中的回合 id；`None` = 无进行中回合。
    /// `TurnStarted` 置位，`TurnCompleted` / `TurnFailed` / `ConversationCancelled` 清空。
    pub active_turn: Option<String>,
    pub last_completed_turn: Option<String>,
    pub last_failed_turn: Option<String>,
    /// 最近完成的 round 摘要。
    pub last_round: Option<LastRound>,
    /// 压缩状态（`CompactStarted` → `"running"`，`CompactFinished` → 终态字符串）。
    pub compact_status: Option<String>,
    pub compact_id: Option<String>,
    /// 最近一次取消标记；**新回合开始时清空**（R6：否则快照在会话余生持续误报）。
    pub cancelled: Option<bool>,
}

/// conversation 频道 `last_round` 的载荷（`RoundCompleted` 写入）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct LastRound {
    pub turn_id: String,
    pub round_num: u32,
    /// 线上键名是 `final`（Rust 关键字），故此处改名。
    #[serde(rename = "final")]
    pub is_final: bool,
}

/// control 频道 `state`（同样不派生 `PartialEq`，理由见 [`ConversationState`]）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ControlState {
    pub session_state: Option<SessionState>,
    pub activity: Option<ActivityState>,
    pub agent_lifecycle: Option<AgentLifecycleState>,
    /// 配置版本号（每次 `config.save` 自增）。**值本身不在此**——消费者据此重拉
    /// `config.load`。
    pub config_rev: Option<u64>,
    /// 会话元数据（标题）**已变更**的事实；具体值由前端重拉 `session.list`（全量权威）。
    pub meta_changed: Option<String>,
    pub pending_interaction: Option<PendingInteraction>,
    pub last_failure: Option<LastFailure>,
    pub last_notice: Option<String>,
    pub dashboard_snapshot: Option<DashboardSnapshot>,
}

/// 挂起交互（`InteractionRequested` / `PlanReviewRequested` 写入）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct PendingInteraction {
    pub id: String,
    pub kind: InteractionKind,
}

/// 挂起交互的类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum InteractionKind {
    Ask,
    Plan,
    /// 前向兼容：daemon 新增类别时旧客户端仍能解析（不因未知取值丢掉整个字段）。
    #[serde(other)]
    Unknown,
}

/// 最近一次操作失败标记（`OperationFailed` 置位，`OperationCompleted` 清空）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct LastFailure {
    pub occurred: bool,
}

/// tool 频道 `state`。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ToolState {
    /// 等待用户授权的 tool_call_id；`None` = 无挂起授权。
    pub pending_permission: Option<String>,
    pub last_finished: Option<String>,
    /// 进行中的工具；`None` = 无（线协议用 `null` 表达，而非空数组）。
    pub running: Option<Vec<RunningTool>>,
}

/// 进行中的工具（`ToolStarted` 写入，`ToolFinished` 清空）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RunningTool {
    pub tool_call_id: String,
    pub turn_id: String,
    pub round_num: u32,
}
