//! Bridge: deepx-client（Ringing 协议）<-> 原生 XAML 视图族。
//!
//! - `BridgeCore`（tokio 侧）：daemon 连接管理 + 三 SSE 频道事件解析
//!   （conversation → ChatView/Composer 直连缓存；control/tool → 交互队列
//!   状态机；control → 侧栏/技能/goalBar 快照）+ 命令/查询直发层。
//! - `Bridge`（UI 线程侧）：`core` 引用 + pump 心跳（失联检测）。
//!
//! WebView 已移除：invoke/emit/outbox 通道整体下线；renderer（SolidJS）
//! 仅供 daemon `/debug/` 浏览器调试入口使用（不经本桥）。
//!
//! Threading: `BridgeCore` is `Send + Sync` and lives on the tokio side;
//! `Bridge` 仅持 `Arc<BridgeCore>`，UI 线程调用均无锁跨线程约束。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use deepx_client::{
    ActionRequest, AskAnswer as DomainAskAnswer, Channel, ChannelStatus, Client, ClientHandlers,
    ClientOptions, CommandOptions, ContentRef, ControlCommand, ControlEvent, ConversationCommand,
    ConversationEvent as DomainConversationEvent, ConversationMode, EventBatch, PermissionCategory,
    PermissionRisk, QueryRequest, RingingCommand, RingingEvent, TimelinePage, TimelineSnapshot,
    TimelineStatus, ToolCommand, ToolEvent,
};
use markdown_winui::PendingOutput;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::chat_adapter;
use crate::shell_store::{
    ActivityState, DashboardSnapshot, SessionDetail, SessionItem, SettingsSnapshot, SkillsSnapshot,
    activity_event, dashboard_event, parse_activities, parse_config_load, parse_conversation_state,
    parse_skills_payload, parse_tools, parse_workspace_status, project_session_meta,
    session_state_event, skills_event,
};

/// 直连模式的发送反馈（替代 Web setComposer 的 submitError/sendAck 投影）。
#[derive(Debug, Clone, Default)]
struct ComposerFeedback {
    /// 最近发送失败原因（空 = 无错误；composer_bar 显示且不清空草稿）。
    submit_error: String,
    /// 发送 accepted 后递增（悲观清空信号；UI 侧已本地清空，保留兼容）。
    send_ack: u64,
}

/// XAML 标题栏状态（headerDirect：Rust 从壳导航/会话列表/conversation
/// 事件组装；Web `shell.setHeader` 投影仅在直连关闭时生效）。
///
/// 字段名对齐 Web 侧 `HeaderState`（camelCase）。`#[serde(default)]` 保证
/// 未来字段扩展向后兼容（P-2 typed struct 预埋，见 WORKFLOW §6.1）。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HeaderState {
    pub view: String,
    pub title: String,
    pub workspace: String,
    /// 当前会话 seed（chat 视图；apply_header 同步 active_seed）。
    pub seed: String,
    pub info_open: bool,
    pub stats_open: bool,
    pub compacting: bool,
    pub compact_disabled: bool,
    pub undo_disabled: bool,
    pub pet_enabled: bool,
}

/// 标题栏本地开关（headerDirect：壳本地翻转，不回传 Web）。
#[derive(Debug, Clone, Copy)]
pub enum HeaderFlag {
    /// Info 面板开合。
    Info,
    /// Stats 面板开合。
    Stats,
}

/// XAML 设置页 Web 侧初始投影（`shell.setSettings` 载荷）。
///
/// theme/lang/permissionLevel 的状态单一数据源在 Web（App.tsx：localStorage
/// + config.load 派生）；壳侧设置页改动后经 `shell.settingsAction` 回传校正
/// （对齐 D2 执行权原则：壳只渲染，不持有状态）。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SettingsProjection {
    /// system | light | dark | dark-gray（三态进协议，P-5）。
    pub theme: String,
    /// en | zh。
    pub lang: String,
    pub permission_level: u64,
    /// local | wsl | remote（workspace 运行环境）。
    pub workspace_mode: String,
}

/// XAML 交互模态状态投影（Web `shell.setInteraction` 载荷）。
///
/// 字段名对齐 Web 侧 `PendingInteraction`（camelCase，`kind` 直通）。
/// `kind` = "none" 表示当前无活动交互（壳关闭覆盖层面板）；
/// "permission" / "ask" / "plan" 三种用户介入模板（统一交互弹窗体系，
/// 见 ELECTRON-MIGRATION.md Phase 5）。`#[serde(default)]` 保证字段扩展
/// 向后兼容（P-2 typed struct 预埋）。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InteractionState {
    /// "none" | "permission" | "ask" | "plan"。
    pub kind: String,
    /// 交互 id（permission = tool_call_id；ask = ask id）。
    pub id: String,
    /// 所属会话 seed（回传时定位 activeEntry）。
    pub seed: String,
    // ── permission 字段 ────────────────────────────────
    pub tool_name: String,
    pub reason: String,
    pub paths: Vec<String>,
    pub category: String,
    pub level: u64,
    /// low | medium | high。
    pub risk: String,
    pub consequence: String,
    // ── ask 字段 ───────────────────────────────────────
    pub questions: Vec<AskQuestion>,
    // ── plan 字段 ───────────────────────────────────────
    pub plan_content: String,
    /// todo_activation | 其他（计划审核）。
    pub review_type: String,
    pub todo_items: Vec<PlanTodoItem>,
}

/// plan 审批的任务项（对齐 renderer `TodoActivationItem`）。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PlanTodoItem {
    pub id: String,
    pub title: String,
    pub description: String,
    /// small | medium | large。
    pub complexity: String,
}

/// `ask_user` 中的单个问题（对齐 renderer `AskQuestion`，ts-rs 生成）。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AskQuestion {
    pub id: String,
    pub question: String,
    pub options: Vec<String>,
    pub allow_custom: bool,
}

// ── Rust 直连交互队列状态机（读路径直连，不经 WebView）──────────────────
//
// 等价移植 Web `sessionPresentation.pendingInteractions` 组装：
//   permission（tool 频道 pendingPermission 卡片）优先于 ask/plan
//   （control 频道 activeAskPlan），取 [0] 为活动交互。
// daemon 事件 → `parse_interaction_event` / `parse_tool_permission_event`
// → `InteractionMachine::apply` → `snapshot` 组装 InteractionState。
// 幂等：事件重放（SSE 重连续传）经 PartialEq 比对不产生多余 rev。

/// per-seed 交互队列状态机。
#[derive(Debug, Clone, Default)]
struct InteractionMachine {
    /// tool 频道挂起的权限请求（等价 Web tool.cards 中 pendingPermission=true）。
    pending_permissions: Vec<PendingPermission>,
    /// control 频道活动 ask/plan（等价 Web control.activeAskPlan）。
    active_ask_plan: Option<ActiveAskPlan>,
}

/// 挂起的权限请求（`tool_permission_requested` 完整字段；turn_id 仅在事件
/// 层消费，快照形状不含——对齐 Web `PendingInteraction` 投影）。
#[derive(Debug, Clone)]
struct PendingPermission {
    tool_call_id: String,
    tool_name: String,
    reason: String,
    paths: Vec<String>,
    category: String,
    level: u64,
    risk: String,
    consequence: String,
}

/// control 频道活动 ask/plan（`activeAskPlan` 等价形状；turn_id 不投影）。
#[derive(Debug, Clone)]
enum ActiveAskPlan {
    Ask {
        id: String,
        questions: Vec<AskQuestion>,
    },
    Plan {
        id: String,
        plan_content: String,
        review_type: String,
        todo_items: Vec<PlanTodoItem>,
    },
}

/// control 频道交互事件（`parse_interaction_event` 解析产物，对齐 Web
/// `controlReducer` 的 interaction_requested / interaction_resolved /
/// plan_review_requested / plan_review_resolved / operation_failed 分支）。
enum InteractionEvent {
    AskRequested {
        id: String,
        questions: Vec<AskQuestion>,
    },
    AskResolved {
        id: String,
    },
    PlanRequested {
        id: String,
        plan_content: String,
        review_type: String,
        todo_items: Vec<PlanTodoItem>,
    },
    PlanResolved {
        id: String,
    },
    /// operation_failed（ask_rejected / interaction_not_found）→ 幽灵交互
    /// 自愈：worker 重启后挂起态丢失，SSE 重放的历史 interaction_requested
    /// 无终态时清除活动面板，让 UI 回到可操作状态（对齐 Web reducer）。
    GhostCleanup,
}

/// tool 频道权限事件（`parse_tool_permission_event` 解析产物，对齐 Web
/// `toolReducer` 的 tool_permission_requested / tool_finished 分支）。
enum ToolPermissionEvent {
    Requested {
        tool_call_id: String,
        tool_name: String,
        reason: String,
        paths: Vec<String>,
        category: String,
        level: u64,
        risk: String,
        consequence: String,
    },
    /// tool_finished：权限已响应（Web 侧置 pendingPermission=false，此处
    /// 直接移除——组装只消费 pendingPermission 卡片，语义等价）。
    Resolved { tool_call_id: String },
}

impl InteractionMachine {
    fn apply(&mut self, ev: InteractionEvent) {
        match ev {
            InteractionEvent::AskRequested { id, questions } => {
                self.active_ask_plan = Some(ActiveAskPlan::Ask { id, questions });
            }
            InteractionEvent::AskResolved { id } => {
                if matches!(&self.active_ask_plan, Some(ActiveAskPlan::Ask { id: cur, .. }) if cur == &id)
                {
                    self.active_ask_plan = None;
                }
            }
            InteractionEvent::PlanRequested {
                id,
                plan_content,
                review_type,
                todo_items,
            } => {
                self.active_ask_plan = Some(ActiveAskPlan::Plan {
                    id,
                    plan_content,
                    review_type,
                    todo_items,
                });
            }
            InteractionEvent::PlanResolved { id } => {
                if matches!(&self.active_ask_plan, Some(ActiveAskPlan::Plan { id: cur, .. }) if cur == &id)
                {
                    self.active_ask_plan = None;
                }
            }
            InteractionEvent::GhostCleanup => {
                self.active_ask_plan = None;
            }
        }
    }

    /// 应用 tool 频道权限事件（独立于 control 的 ask/plan 状态机）。
    fn apply_tool(&mut self, ev: ToolPermissionEvent) {
        match ev {
            ToolPermissionEvent::Requested {
                tool_call_id,
                tool_name,
                reason,
                paths,
                category,
                level,
                risk,
                consequence,
            } => {
                // upsert：同 tool_call_id 覆盖（对齐 Web 卡片 patch），
                // 移除后 push 末尾 → 最新请求排最后，first 仍为最旧。
                self.pending_permissions
                    .retain(|p| p.tool_call_id != tool_call_id);
                self.pending_permissions.push(PendingPermission {
                    tool_call_id,
                    tool_name,
                    reason,
                    paths,
                    category,
                    level,
                    risk,
                    consequence,
                });
            }
            ToolPermissionEvent::Resolved { tool_call_id } => {
                self.pending_permissions
                    .retain(|p| p.tool_call_id != tool_call_id);
            }
        }
    }

    /// 组装活动交互（permission 优先，等价 Web `pendingInteractions[0]`）。
    /// 无活动交互时返回 default（kind=""，XAML 覆盖层判空关闭）。
    fn snapshot(&self, seed: &str) -> InteractionState {
        if let Some(p) = self.pending_permissions.first() {
            return InteractionState {
                kind: "permission".into(),
                id: p.tool_call_id.clone(),
                seed: seed.to_string(),
                tool_name: p.tool_name.clone(),
                reason: p.reason.clone(),
                paths: p.paths.clone(),
                category: p.category.clone(),
                level: p.level,
                risk: p.risk.clone(),
                consequence: p.consequence.clone(),
                ..InteractionState::default()
            };
        }
        match &self.active_ask_plan {
            Some(ActiveAskPlan::Plan {
                id,
                plan_content,
                review_type,
                todo_items,
                ..
            }) => InteractionState {
                kind: "plan".into(),
                id: id.clone(),
                seed: seed.to_string(),
                plan_content: plan_content.clone(),
                review_type: review_type.clone(),
                todo_items: todo_items.clone(),
                ..InteractionState::default()
            },
            Some(ActiveAskPlan::Ask { id, questions, .. }) => InteractionState {
                kind: "ask".into(),
                id: id.clone(),
                seed: seed.to_string(),
                questions: questions.clone(),
                ..InteractionState::default()
            },
            None => InteractionState::default(),
        }
    }

    /// 是否存在挂起交互（composer `hasPendingGate` 直连来源，
    /// 等价 Web `activeInteraction(session()) !== null`）。
    fn has_pending(&self) -> bool {
        !self.pending_permissions.is_empty() || self.active_ask_plan.is_some()
    }
}

/// 从 control 频道事件提取交互队列更新。
///
/// 事件形状（deepx-domain `ControlEvent`，`tag="type"`）：
/// `interaction_requested { interaction_id, turn_id, mode, questions[] }`、
/// `interaction_resolved { interaction_id, resolution }`、
/// `plan_review_requested { interaction_id, turn_id, plan_content, review_type,
/// todo_items?[] }`、`plan_review_resolved { interaction_id, approved }`、
/// `operation_failed { error: { code } }`（幽灵自愈）。`type` 不符返回 None。
fn interaction_event(event: &ControlEvent) -> Option<InteractionEvent> {
    match event {
        ControlEvent::InteractionRequested {
            interaction_id,
            questions,
            ..
        } => Some(InteractionEvent::AskRequested {
            id: interaction_id.clone(),
            questions: questions
                .iter()
                .map(|question| AskQuestion {
                    id: question.id.clone(),
                    question: question.question.clone(),
                    options: question.options.clone(),
                    allow_custom: question.allow_custom,
                })
                .collect(),
        }),
        ControlEvent::InteractionResolved { interaction_id, .. } => {
            Some(InteractionEvent::AskResolved {
                id: interaction_id.clone(),
            })
        }
        ControlEvent::PlanReviewRequested {
            interaction_id,
            plan_content,
            review_type,
            todo_items,
            ..
        } => Some(InteractionEvent::PlanRequested {
            id: interaction_id.clone(),
            plan_content: plan_content.clone(),
            review_type: review_type.clone(),
            todo_items: todo_items
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|item| PlanTodoItem {
                    id: item.id.clone(),
                    title: item.title.clone(),
                    description: item.description.clone(),
                    complexity: item.complexity.clone(),
                })
                .collect(),
        }),
        ControlEvent::PlanReviewResolved { interaction_id, .. } => {
            Some(InteractionEvent::PlanResolved {
                id: interaction_id.clone(),
            })
        }
        ControlEvent::OperationFailed { error, .. }
            if matches!(
                error.code.as_str(),
                "ask_rejected" | "interaction_not_found"
            ) =>
        {
            Some(InteractionEvent::GhostCleanup)
        }
        _ => None,
    }
}

/// 从 tool 频道事件提取权限队列更新。
///
/// 事件形状（deepx-domain `ToolEvent`）：`tool_permission_requested
/// { tool_call_id, turn_id, round_num, tool_name, reason, paths[],
/// category, level, risk, consequence }`、`tool_finished { tool_call_id, ... }`。
/// 注意 daemon 字段为 snake_case（`allow_custom` 等），与壳投影
/// （camelCase `allowCustom`）不同——解析时手动取 snake_case 键。
fn tool_permission_event(event: &ToolEvent) -> Option<ToolPermissionEvent> {
    match event {
        ToolEvent::ToolPermissionRequested {
            tool_call_id,
            tool_name,
            reason,
            paths,
            category,
            level,
            risk,
            consequence,
            ..
        } => Some(ToolPermissionEvent::Requested {
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            reason: reason.clone(),
            paths: paths.clone(),
            category: match category {
                PermissionCategory::Read => "read",
                PermissionCategory::Write => "write",
                PermissionCategory::Exec => "exec",
                PermissionCategory::Net => "net",
            }
            .to_string(),
            level: u64::from(*level),
            risk: match risk {
                PermissionRisk::Low => "low",
                PermissionRisk::Medium => "medium",
                PermissionRisk::High => "high",
            }
            .to_string(),
            consequence: consequence.clone(),
        }),
        ToolEvent::ToolFinished { tool_call_id, .. } => Some(ToolPermissionEvent::Resolved {
            tool_call_id: tool_call_id.clone(),
        }),
        _ => None,
    }
}

/// 解析 daemon `questions` 数组（snake_case 键 → 壳投影 camelCase 形状）。
#[cfg(test)]
fn parse_questions(v: &Value) -> Vec<AskQuestion> {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|q| {
                    Some(AskQuestion {
                        id: q.get("id")?.as_str()?.to_string(),
                        question: q.get("question")?.as_str()?.to_string(),
                        options: q
                            .get("options")
                            .and_then(|o| o.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|x| x.as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        allow_custom: q
                            .get("allow_custom")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 解析 daemon `todo_items`（可为 null；字段无 camelCase 转换需求）。
#[cfg(test)]
fn parse_todo_items(v: Option<&Value>) -> Vec<PlanTodoItem> {
    v.and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    Some(PlanTodoItem {
                        id: t.get("id")?.as_str()?.to_string(),
                        title: t.get("title")?.as_str()?.to_string(),
                        description: t
                            .get("description")
                            .and_then(|d| d.as_str())
                            .unwrap_or("")
                            .to_string(),
                        complexity: t
                            .get("complexity")
                            .and_then(|c| c.as_str())
                            .unwrap_or("")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

// ── Native composer activity tracking ────────────────────────────────
//
// Canonical conversation events own streaming/usage state. Mode and send
// feedback are local UI state; permission comes from the typed settings cache.

/// 卡死阈值（对齐 Web `SESSION_STALL_TIMEOUT_MS`）：超时视为流式中断。
const COMPOSER_STALL_TIMEOUT_MS: u64 = 4 * 60 * 1000;

/// per-seed composer 活动追踪（isStreaming 判定 + usage 缓存）。
#[derive(Debug, Clone, Default)]
struct ComposerActivity {
    /// activeTurn 是否存在（turn_started 置 true；终态置 false）。
    active_turn: bool,
    /// 最近领域事件时间（epoch ms；0 = 未知 → 保守视为流式中）。
    last_activity_at: u64,
    /// `usage_updated` 缓存（contextTokens = usage.prompt_tokens，对齐 Web）。
    prompt_tokens: u64,
    context_limit: u64,
    model: String,
}

/// conversation 频道活动事件（`parse_conversation_activity_event` 解析产物）。
enum ConversationActivityEvent {
    /// turn_started：活动开始。
    Started,
    /// turn_completed / turn_failed / conversation_cancelled：活动结束。
    Ended,
    /// round_delta / block_checkpoint / round_completed / provider_retrying /
    /// provider_tool_status：活动（刷新时间戳）。
    Touched,
    /// usage_updated：活动 + model/context_limit/prompt_tokens 缓存。
    Usage {
        prompt_tokens: u64,
        context_limit: u64,
        model: String,
    },
}

impl ComposerActivity {
    /// 等价 Web `isSessionStreaming`：activeTurn 存在且最近活动未超时；
    /// 时间戳未知（旧数据/恢复间隙）保守按流式中处理。
    fn is_streaming(&self, now: u64) -> bool {
        if !self.active_turn {
            return false;
        }
        if self.last_activity_at == 0 {
            return true;
        }
        now.saturating_sub(self.last_activity_at) < COMPOSER_STALL_TIMEOUT_MS
    }

    fn apply(&mut self, ev: ConversationActivityEvent, now: u64) {
        match ev {
            ConversationActivityEvent::Started => {
                self.active_turn = true;
                self.last_activity_at = now;
            }
            ConversationActivityEvent::Ended => {
                self.active_turn = false;
            }
            ConversationActivityEvent::Touched => {
                self.last_activity_at = now;
            }
            ConversationActivityEvent::Usage {
                prompt_tokens,
                context_limit,
                model,
            } => {
                self.prompt_tokens = prompt_tokens;
                self.context_limit = context_limit;
                self.model = model;
                self.last_activity_at = now;
            }
        }
    }
}

/// 从 conversation 频道事件提取活动更新（对齐 Web `applyConversationEventToDraft`
/// 的活动刷新语义：除 compact_* 外所有领域事件都视为活动）。
///
/// 事件形状（deepx-domain `ConversationEvent`）：`turn_started { turn_id,
/// user_text }`、`turn_completed { turn_id, stop_reason?, usage? }`、
/// `turn_failed { turn_id, error }`、`conversation_cancelled { turn_id? }`、
/// `usage_updated { turn_id, round_num, usage, context_limit, model }`、
/// `round_delta / block_checkpoint / round_completed / provider_retrying /
/// provider_tool_status`。`type` 不符返回 None。
fn conversation_activity_event(
    event: &DomainConversationEvent,
) -> Option<ConversationActivityEvent> {
    match event {
        DomainConversationEvent::TurnStarted { .. } => Some(ConversationActivityEvent::Started),
        DomainConversationEvent::TurnCompleted { .. }
        | DomainConversationEvent::TurnFailed { .. }
        | DomainConversationEvent::ConversationCancelled { .. } => {
            Some(ConversationActivityEvent::Ended)
        }
        DomainConversationEvent::UsageUpdated {
            usage,
            context_limit,
            model,
            ..
        } => Some(ConversationActivityEvent::Usage {
            prompt_tokens: u64::from(usage.prompt_tokens),
            context_limit: u64::from(*context_limit),
            model: model.clone(),
        }),
        DomainConversationEvent::RoundDelta { .. }
        | DomainConversationEvent::BlockCheckpoint { .. }
        | DomainConversationEvent::RoundCompleted { .. }
        | DomainConversationEvent::ProviderRetrying { .. }
        | DomainConversationEvent::ProviderToolStatus { .. } => {
            Some(ConversationActivityEvent::Touched)
        }
        DomainConversationEvent::CompactStarted { .. }
        | DomainConversationEvent::CompactProgress { .. }
        | DomainConversationEvent::CompactFinished { .. } => None,
    }
}

/// 当前 unix 时间（epoch ms；系统时钟异常时回退 0——streaming 保守判定）。
fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `ask_user` 表单中的单个答案（对齐 renderer `AskAnswer`：question_id）。
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct AskAnswer {
    pub question_id: String,
    pub answer: String,
}

/// View model consumed directly by the native XAML composer.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ComposerState {
    /// 当前会话 seed：壳据此重置草稿（会话切换即清空输入框，同 Web 行为）。
    pub seed: String,
    pub is_streaming: bool,
    pub has_pending_gate: bool,
    /// plan | code。
    pub mode: String,
    pub model: String,
    pub context_tokens: u64,
    pub context_limit: u64,
    /// 1..=4（对齐 config.permission_level）。
    pub permission_level: u64,
    pub queue_count: u64,
    pub queue_items: Vec<ComposerQueueItem>,
    /// Native send failure shown without clearing the draft.
    pub submit_error: String,
    /// Incremented after a command is accepted so the shell can clear its draft.
    pub send_ack: u64,
}

/// followUpQueue 排队项（壳显示列表 + 删除）。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ComposerQueueItem {
    pub id: String,
    pub text: String,
}

/// 图片附件（壳选文件后传路径；Web 侧复用 desktop.readFileBase64 读 base64）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposerAttachment {
    pub file_name: String,
    pub mime_type: String,
    pub path: String,
}

/// 文本附件（壳选文件后传路径；Web 侧复用 desktop.readTextFile 读内容）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposerTextFile {
    pub file_name: String,
    pub path: String,
}

/// Result of resolving one `RoundCompleted.output_ref` through the Ringing
/// content service. The UI applies it back to the Transcript model.
#[derive(Debug, Clone)]
pub struct ChatOutputResolution {
    pub turn_id: String,
    pub round_num: u32,
    pub result: Result<String, String>,
}

/// `Send + Sync` half of the bridge: client, lease bookkeeping, outbox sender.
/// Lives on the tokio side.
pub struct BridgeCore {
    client: Mutex<Option<Client>>,
    attached: Mutex<HashSet<String>>,
    /// Latest native transport state for each Ringing channel.
    channel_status: Mutex<HashMap<Channel, ChannelStatus>>,
    /// XAML 侧栏数据源：会话列表投影（`session.list` + `session.activity`）。
    sessions: Mutex<Vec<SessionItem>>,
    /// 实时活动状态（control `session_activity_changed` 事件增量更新）。
    activities: Mutex<HashMap<String, ActivityState>>,
    /// 侧栏数据版本：refresh / activity 事件后递增，UI 侧 timer 比对后刷新。
    session_rev: AtomicU64,
    /// XAML 侧栏当前选中的会话 seed。
    active_seed: Mutex<String>,
    /// XAML 标题栏数据源，由壳导航、会话列表和 conversation 事件组装。
    header_state: Mutex<HeaderState>,
    /// 标题栏状态版本：组装/投影后递增，UI 侧 timer 比对后刷新（同 session_rev）。
    header_rev: AtomicU64,
    /// per-seed turns 计数（undo_disabled 判定源）：timeline 快照写入点缓存
    /// （不随 chat_view consume 清空），增量 turn 事件不改变计数。
    header_turns: Mutex<HashMap<String, usize>>,
    /// per-seed 最近回合 id（undo 命令用）：turn_started 事件/快照写入点
    /// 更新；无缓存时撤销按钮直发层拒绝发送。
    last_turn_ids: Mutex<HashMap<String, String>>,
    /// daemon 失联检测（A 方案，WORKFLOW §7）：timeline 流非 Open 的起始时刻。
    timeline_stall_since: Mutex<Option<Instant>>,
    /// 三 ringing 通道无一 Open 的起始时刻。
    channels_stall_since: Mutex<Option<Instant>>,
    /// 重建进行中（防 ensure_client 重入）。
    rebuilding: AtomicBool,
    /// 连接进行中（防并发 invoke 各自 connect_async → 各自 spawn daemon）。
    /// 首个调用者置位并真正发起连接，其余调用者轮询等待其结果。
    connecting: AtomicBool,
    /// 最近一次重建时刻（冷却防抖，避免网络抖动时反复重建）。
    last_rebuild_at: Mutex<Instant>,
    /// 最近一次"无 client 自动重连"时刻（独立冷却，见 AUTO_RECONNECT_COOLDOWN）。
    last_auto_reconnect_at: Mutex<Instant>,
    /// 连续 rebuild 失败计数（指数退避冷却用；成功清零）。
    rebuild_failures: AtomicU32,
    /// 最近一次 timeline.activate 的 seed（重建后恢复前端 transcript 流）。
    last_timeline_seed: Mutex<String>,
    /// timeline 连接状态缓存（检测用；ringing 状态走 channel_status）。
    timeline_status: Mutex<Option<TimelineStatus>>,
    /// XAML 技能页数据源：最近 `skills_updated` 事件完整载荷（WORKFLOW §8）。
    skills: Mutex<Option<SkillsSnapshot>>,
    /// 技能数据版本：事件/拉取后递增，UI 侧 timer 比对后刷新（同 session_rev）。
    skills_rev: AtomicU64,
    /// 壳主导的当前视图（`navigate` 同步；XAML 视图族接管 skills 的判定源）。
    current_view: Mutex<String>,
    /// XAML 设置页数据源：`config.load` + `skills.list_tools` 合并投影。
    settings: Mutex<Option<SettingsSnapshot>>,
    /// 设置数据版本：config.load / tools 拉取后递增，UI 侧 timer 比对后刷新。
    settings_rev: AtomicU64,
    /// XAML-local appearance and workspace preferences.
    settings_proj: Mutex<SettingsProjection>,
    /// Local preference version used by the UI refresh loop.
    settings_proj_rev: AtomicU64,
    /// XAML Info 面板数据源：bootstrap `conversation.state` 投影。
    info: Mutex<Option<SessionDetail>>,
    /// Info 数据版本：refresh 后递增，UI 侧 timer 比对后刷新（同 session_rev）。
    info_rev: AtomicU64,
    /// XAML interaction modal view model.
    interaction: Mutex<InteractionState>,
    /// Interaction version used by the UI refresh loop.
    interaction_rev: AtomicU64,
    /// daemon control/tool 事件直接组装的 per-seed 交互状态机。
    interactions: Mutex<HashMap<String, InteractionMachine>>,
    /// Composer version used by the UI refresh loop.
    composer_rev: AtomicU64,
    /// Rust 直连 composer 活动追踪（per seed）：conversation 频道事件
    /// 直连解析 isStreaming（卡死检测）/model/context（usage_updated 缓存）
    /// ——读路径直连，不经 WebView（终局数据源）。`hasPendingGate` 复用
    /// 交互队列状态机（interactions）。
    composer_activity: Mutex<HashMap<String, ComposerActivity>>,
    /// 直连模式的 mode 本地缓存（Web 单例语义：会话共享，默认 "plan"）。
    composer_mode: Mutex<String>,
    /// 直连模式的发送反馈（submitError 显示 / sendAck 清空信号）。
    composer_feedback: Mutex<ComposerFeedback>,
    /// 原生 ChatView 事件队列：conversation 频道渲染相关事件（turn/round/
    /// delta/checkpoint）直连缓存，UI 线程 timer drain 喂 Transcript。
    /// Queue entries are canonical typed Ringing events; the adapter only maps
    /// domain variants into presentation models.
    ///
    /// **seed 标记（2026-08-08 修复）**：队列元素为 `(seed, event)`——
    /// daemon 的 SSE 流按 lease 推送**所有**会话的事件（batch.seed 区分），
    /// 此前入队忽略 seed、drain 全量返回，后台会话增量会污染活动会话的
    /// Transcript（切换瞬间残留事件串台）。现入队带 seed、`chat_drain`
    /// 按 active_seed 过滤（非活动事件丢弃，切回时由权威快照 + 切回后的
    /// 增量补齐）。
    chat_events: Mutex<std::collections::VecDeque<(String, RingingEvent)>>,
    /// 最近一次 typed timeline 快照（`TimelineSnapshot` + 所属 seed：
    /// 权威 turns 历史，resume 旧对话的数据源；chat_view 泵消费 restore）。
    /// seed 标记防竞态：快速切会话时旧快照晚到不会被灌进新会话。
    chat_timeline: Mutex<Option<(String, TimelineSnapshot)>>,
    /// 分页元数据：seed → 服务端是否还有更早回合（快照缓存时同步更新）。
    /// ChatView 上滚到窗口顶部且 `expand_window` 已全量放行时据此翻页。
    timeline_has_more: Mutex<std::collections::HashMap<String, bool>>,
    /// 更早回合分页页（`(seed, TimelineSnapshot)`）：`spawn_fetch_earlier`
    /// 异步拉取后入队，chat_view 泵 drain 后 `Transcript::prepend_turns`
    /// 前插（与 `chat_timeline` 的整包替换语义区分）。
    chat_prepend: Mutex<std::collections::VecDeque<(String, TimelineSnapshot)>>,
    /// 分页在途标记（seed 集合）：防止滚动抖动时重复发起同一翻页请求。
    timeline_fetching: Mutex<std::collections::HashSet<String>>,
    /// External answer bodies resolved asynchronously for the native ChatView.
    chat_outputs: Mutex<std::collections::VecDeque<(String, ChatOutputResolution)>>,
    /// `(seed, turn_id, round_num, content_id)` requests currently in flight.
    content_fetching: Mutex<std::collections::HashSet<(String, String, u32, String)>>,
    /// ChatView 数据版本：事件入队后递增，UI 侧 timer 比对后 drain。
    chat_rev: AtomicU64,
    /// 快照重拉节流：seed 不匹配时主动 `activate_timeline` 重拉（daemon
    /// 幂等重推快照）；16ms 泵每 tick 都会看到不匹配快照，须限频。
    timeline_refresh_at: Mutex<Instant>,
    /// XAML goalBar 数据源：control 频道 `dashboard_snapshot` 事件投影。
    dashboard: Mutex<Option<DashboardSnapshot>>,
    /// dashboard 数据版本：事件到达后递增，UI 侧 timer 比对后刷新。
    dashboard_rev: AtomicU64,
}

/// 失联阈值：backoff 1+2+4+8=15s 内 4 次重试仍失败视为失联（daemon 重启/关闭）。
const STALL_THRESHOLD: Duration = Duration::from_secs(15);
/// 重建冷却：网络抖动时避免每 15s 重建一次。
const REBUILD_COOLDOWN: Duration = Duration::from_secs(60);
/// 无 client 自动重连冷却：首次 connect 失败（daemon 初始化窗口）后
/// 尽快恢复，比 stall 重建的 60s 冷却短。
const AUTO_RECONNECT_COOLDOWN: Duration = Duration::from_secs(5);
/// 等待并发连接完成的上限：覆盖 discovery 等待（8s）+ open 协商（10s）+
/// 余量。超过即视为连接失败（调用方重试机制兜底）。
const CONNECT_WAIT_TIMEOUT: Duration = Duration::from_secs(25);
/// ChatView 快照重拉节流：seed 不匹配时主动 activate_timeline 重拉，
/// 16ms 泵每 tick 都会看到不匹配快照，1s 限频防 activate 风暴。
const REFRESH_THROTTLE: Duration = Duration::from_secs(1);
/// 连续失败后 rebuild 冷却指数退避封顶（60s → 120s → 240s → 480s → 960s）。
const REBUILD_BACKOFF_CAP: u32 = 4;

/// rebuild 冷却：连续失败后指数拉长（60s→960s 封顶），防止 rebuild
/// 风暴把 daemon 连接数打爆（32 连接信号量 → 静默 drop）。
fn rebuild_cooldown_for(failures: u32) -> Duration {
    REBUILD_COOLDOWN.saturating_mul(1u32 << failures.min(REBUILD_BACKOFF_CAP))
}

/// 无 client 自动重连冷却：同样受失败计数退避保护（5s→320s 封顶）。
fn auto_reconnect_cooldown_for(failures: u32) -> Duration {
    AUTO_RECONNECT_COOLDOWN.saturating_mul(1u32 << failures.min(REBUILD_BACKOFF_CAP + 2))
}

impl BridgeCore {
    /// Arc to self: `BridgeCore` is stored in an `Arc` by the UI-side Bridge.
    fn self_arc(&self) -> Arc<BridgeCore> {
        SHARED_CORE
            .get()
            .expect("bridge core not initialized")
            .clone()
    }

    // ── XAML 侧栏（shell_store 投影）──────────────────────────────

    /// (items, rev) 快照：UI 侧 timer 比对 rev 决定是否刷新列表。
    pub fn session_snapshot(&self) -> (Vec<SessionItem>, u64) {
        let items = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let rev = self.session_rev.load(Ordering::Relaxed);
        (items, rev)
    }

    pub fn active_seed(&self) -> String {
        self.active_seed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_active_seed(&self, seed: &str) {
        *self.active_seed.lock().unwrap_or_else(|e| e.into_inner()) = seed.to_string();
        // 交互缓存跟随活动会话：
        // 只显示当前会话的交互，后台会话请求保持挂起直至切回）。
        self.refresh_interaction_snapshot();
        // 标题栏直连：seed/view/title 随活动会话刷新。
        self.refresh_header();
    }

    // ── XAML 标题栏（header 投影，同 sessions 模式）────────────────

    /// (state, rev) 快照：UI 侧 timer 比对 rev 决定是否刷新 TitleBar。
    pub fn header_snapshot(&self) -> (HeaderState, u64) {
        let state = self
            .header_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let rev = self.header_rev.load(Ordering::Relaxed);
        (state, rev)
    }

    /// 壳侧组装标题栏状态：view/seed 来自壳导航与会话
    /// 切换，title 查会话列表，undo/compact disabled 由 conversation 事件
    /// 推断（对齐 Web：`turns.length === 0 || streaming` / `streaming`）。
    /// info_open/stats_open/compacting/workspace 保留现值（本地状态，
    /// 不经 Web）。每次调用递增 rev（调用方在状态实际变化时触发）。
    pub fn refresh_header(&self) {
        let view = self
            .current_view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let seed = self.active_seed();
        let sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let title = sessions
            .iter()
            .find(|s| s.seed == seed)
            .map(|s| s.title.clone())
            .unwrap_or_default();
        let now = unix_ms();
        let streaming = self
            .composer_activity
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&seed)
            .map(|a| a.is_streaming(now))
            .unwrap_or(false);
        let turns = self
            .header_turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&seed)
            .copied()
            .unwrap_or(0);
        let mut h = self.header_state.lock().unwrap_or_else(|e| e.into_inner());
        h.view = view;
        h.seed = seed;
        h.title = title;
        h.undo_disabled = turns == 0 || streaming;
        h.compact_disabled = streaming;
        drop(h);
        self.header_rev.fetch_add(1, Ordering::Relaxed);
    }

    /// 翻转标题栏本地开关（info_open / stats_open）并递增 rev——壳本地
    /// 状态，不再回传 Web（headerAction::Info/Stats 通道随 WebView 移除
    /// 而淘汰）。
    pub fn toggle_header_flag(&self, flag: HeaderFlag) {
        let mut h = self.header_state.lock().unwrap_or_else(|e| e.into_inner());
        match flag {
            HeaderFlag::Info => h.info_open = !h.info_open,
            HeaderFlag::Stats => h.stats_open = !h.stats_open,
        }
        drop(h);
        self.header_rev.fetch_add(1, Ordering::Relaxed);
    }

    // ── XAML 交互模态（interaction 投影，同 header 模式）────────────

    /// (state, rev) 快照：UI 侧 timer 比对 rev 决定是否刷新覆盖层面板。
    pub fn interaction_snapshot(&self) -> (InteractionState, u64) {
        let state = self
            .interaction
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let rev = self.interaction_rev.load(Ordering::Relaxed);
        (state, rev)
    }

    /// 应用 daemon control 事件到交互队列状态机；活动会话快照变化时递增 rev。
    /// 幂等：SSE 重连续传重放事件经 PartialEq 比对不产生多余 rev。
    /// 注意：机器按**事件 seed** 更新（后台会话交互保持挂起），缓存只投影
    /// **active_seed** 的机器，后台会话不会覆盖当前 UI。
    fn apply_interaction_event(&self, seed: &str, ev: InteractionEvent) {
        let mut machines = self.interactions.lock().unwrap_or_else(|e| e.into_inner());
        machines.entry(seed.to_string()).or_default().apply(ev);
        drop(machines);
        self.refresh_interaction_snapshot();
    }

    /// tool 频道变体（permission 队列独立于 ask/plan 状态机）。
    fn apply_tool_permission_event(&self, seed: &str, ev: ToolPermissionEvent) {
        let mut machines = self.interactions.lock().unwrap_or_else(|e| e.into_inner());
        machines.entry(seed.to_string()).or_default().apply_tool(ev);
        drop(machines);
        self.refresh_interaction_snapshot();
    }

    /// 将 active_seed 对应机器的快照写入缓存（无该 seed 机器 → 空交互）。
    /// 快照未变化（PartialEq）不递增 rev——重放/无关会话事件零开销。
    fn refresh_interaction_snapshot(&self) {
        let active = self.active_seed();
        let machines = self.interactions.lock().unwrap_or_else(|e| e.into_inner());
        let next = machines
            .get(&active)
            .map(|m| m.snapshot(&active))
            .unwrap_or_default();
        drop(machines);
        let mut cur = self.interaction.lock().unwrap_or_else(|e| e.into_inner());
        if *cur != next {
            *cur = next;
            self.interaction_rev.fetch_add(1, Ordering::Relaxed);
        }
    }

    // ── XAML Composer native view model ──────────────────────────────

    /// (state, rev) 快照：UI 侧 timer 比对 rev 决定是否刷新底部栏。
    /// Combines typed conversation activity, interaction gates, settings, and
    /// UI-local command feedback.
    pub fn composer_snapshot(&self) -> (ComposerState, u64) {
        let rev = self.composer_rev.load(Ordering::Relaxed);
        let active = self.active_seed();
        let now = unix_ms();
        let activity = self
            .composer_activity
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&active)
            .cloned();
        // hasPendingGate 复用交互队列状态机（permission/ask/plan 任一挂起）。
        let gate = self
            .interactions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&active)
            .map(|m| m.has_pending())
            .unwrap_or(false);
        let is_streaming = activity
            .as_ref()
            .map(|a| a.is_streaming(now))
            .unwrap_or(false);
        let model = activity
            .as_ref()
            .map(|a| a.model.clone())
            .unwrap_or_default();
        let context_tokens = activity.as_ref().map(|a| a.prompt_tokens).unwrap_or(0);
        let context_limit = activity.as_ref().map(|a| a.context_limit).unwrap_or(0);
        let mut state = ComposerState::default();
        state.seed = active;
        state.is_streaming = is_streaming;
        state.has_pending_gate = gate;
        state.model = model;
        state.context_tokens = context_tokens;
        state.context_limit = context_limit;
        // Mode and feedback are UI-local; permission comes from config.load.
        state.mode = self
            .composer_mode
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        state.permission_level = self
            .settings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|s| s.permission_level)
            .unwrap_or(1);
        let fb = self
            .composer_feedback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        state.submit_error = fb.submit_error;
        state.send_ack = fb.send_ack;
        (state, rev)
    }

    // ── 原生 ChatView（conversation 事件直连）──────────────────────

    /// (事件队列快照, rev)：UI 线程 timer 比对 rev 后 drain 喂 Transcript。
    /// Events stay typed through the queue and are mapped once to view models.
    ///
    /// **按活动会话隔离**：只返回 `seed == active_seed` 的事件；非活动
    /// 会话的事件在此丢弃（切换瞬间的残留事件不会污染新会话的
    /// Transcript；切回时由权威快照 + 切回后的增量补齐）。
    pub fn chat_drain(&self) -> (Vec<RingingEvent>, u64) {
        let rev = self.chat_rev.load(Ordering::Relaxed);
        let active = self.active_seed();
        let events = self
            .chat_events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .filter(|(seed, _)| *seed == active)
            .map(|(_, ev)| ev)
            .collect();
        (events, rev)
    }

    /// Drain resolved external answer bodies for the active session.
    pub fn chat_output_drain(&self) -> Vec<ChatOutputResolution> {
        let active = self.active_seed();
        self.chat_outputs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .filter(|(seed, _)| *seed == active)
            .map(|(_, resolution)| resolution)
            .collect()
    }

    /// Resolve a model-owned `output_ref` without routing transport details
    /// through the XAML renderer. Duplicate/replayed requests are coalesced.
    pub fn spawn_resolve_chat_output(&self, seed: &str, pending: PendingOutput) {
        let reference = match serde_json::from_value::<ContentRef>(pending.reference) {
            Ok(reference) => reference,
            Err(error) => {
                self.chat_outputs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back((
                        seed.to_string(),
                        ChatOutputResolution {
                            turn_id: pending.turn_id,
                            round_num: pending.round_num,
                            result: Err(format!("invalid output_ref: {error}")),
                        },
                    ));
                self.chat_rev.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        let key = (
            seed.to_string(),
            pending.turn_id.clone(),
            pending.round_num,
            reference.content_id.clone(),
        );
        {
            let mut fetching = self
                .content_fetching
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !fetching.insert(key.clone()) {
                return;
            }
        }
        let core = self.self_arc();
        let seed = seed.to_string();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = core
                .client
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let result = match client {
                Some(client) => match client.download_content(&seed, &reference).await {
                    Ok(bytes) => String::from_utf8(bytes)
                        .map_err(|error| format!("external answer is not UTF-8: {error}")),
                    Err(error) => Err(error.to_string()),
                },
                None => Err("Ringing client is not connected".to_string()),
            };
            core.content_fetching
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            core.chat_outputs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back((
                    seed,
                    ChatOutputResolution {
                        turn_id: pending.turn_id,
                        round_num: pending.round_num,
                        result,
                    },
                ));
            core.chat_rev.fetch_add(1, Ordering::Relaxed);
        });
    }

    /// 查看最近一次 timeline 快照（`(seed, TimelineSnapshot)`；resume
    /// 历史数据源）。**peek 语义**：不消费——seed 校验失败的快照保留在缓存，
    /// 等新快照覆盖或调用方主动重拉，避免"take 即弃"导致快照永久丢失后
    /// ChatView 永远停在"加载会话…"。
    pub fn chat_timeline_peek(&self) -> Option<(String, TimelineSnapshot)> {
        self.chat_timeline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 消费当前快照（仅调用方确认 `seed == active_seed` 后调用）。
    pub fn chat_timeline_consume(&self) {
        *self.chat_timeline.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// 分页：drain 更早回合页（`(seed, TimelineSnapshot JSON)` 队列，按
    /// active_seed 过滤——与 `chat_drain` 同隔离语义）。
    pub fn chat_prepend_drain(&self) -> Vec<(String, TimelineSnapshot)> {
        let active = self.active_seed();
        self.chat_prepend
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .filter(|(seed, _)| *seed == active)
            .collect()
    }

    /// 服务端是否还有更早回合（上滚翻页判定）。
    pub fn timeline_has_more(&self, seed: &str) -> bool {
        self.timeline_has_more
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(seed)
            .copied()
            .unwrap_or(false)
    }

    /// 翻页加载更早回合：`fetch_timeline_page(seed, before_turn)`（纯读，
    /// 不重建 timeline SSE）→ 页入 `chat_prepend` 队列 + chat_rev++，
    /// chat_view 泵 drain 后 `Transcript::prepend_turns` 前插。
    /// 在途防重入（滚动抖动只发一次）；失败保留 has_more（下次滚动重试）。
    pub fn spawn_fetch_earlier(&self, seed: &str, before_turn: &str) {
        let seed = seed.to_string();
        let before_turn = before_turn.to_string();
        {
            let mut fetching = self
                .timeline_fetching
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !fetching.insert(seed.clone()) {
                return; // 已在途
            }
        }
        let core = self.self_arc();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("fetch_earlier {seed}: connect failed: {err}"));
                    core.timeline_fetching
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&seed);
                    return;
                }
            };
            match client.fetch_timeline_page(&seed, Some(&before_turn), None).await {
                Ok(page) => {
                    let has_more = page.has_more;
                    core.timeline_has_more
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(seed.clone(), has_more);
                    let turns = chat_adapter::restored_turns(&page.snapshot);
                    if turns.is_empty() {
                        // 防御：空页（会话已删/竞态）——视为到底，不再翻页。
                        core.timeline_has_more
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(seed.clone(), false);
                    } else {
                        core.chat_prepend
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push_back((seed.clone(), page.snapshot));
                        core.chat_rev.fetch_add(1, Ordering::Relaxed);
                        log_diag(&format!(
                            "fetch_earlier {seed}: page before {before_turn} ({} turns, has_more={has_more})",
                            turns.len()
                        ));
                    }
                }
                Err(err) => log_diag(&format!("fetch_earlier {seed}: failed: {err}")),
            }
            core.timeline_fetching
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&seed);
        });
    }

    /// 缓存 timeline 快照（`on_timeline_snapshot` 回调主体；独立方法便于单测）。
    ///
    /// - **seed 标记**：优先从快照 body 顶层读取（daemon 写回请求 seed，
    ///   权威来源）；缺失才回退 `last_timeline_seed`。不能依赖后者——
    ///   `spawn_timeline_refresh` 重拉时不更新它，且并发 resume 交错时它
    ///   会被后设值覆盖，快照被错误标记 → ChatView 泵永远判 stale →
    ///   无限 deferred 循环 → 历史永不恢复（日志风暴实证）。
    /// - **层级解包**：turns 在 `snapshot` 子对象（TimelineSnapshot：
    ///   `{"watermark", "turns"}`）。缓存子对象——消费方
    ///   `chat_adapter::timeline_turns` 直接读顶层 `turns`；缓存完整 body
    ///   则解析恒空 → restore 空历史 → ChatView 恢复后仍空白。
    fn cache_timeline_snapshot(&self, page: TimelinePage) {
        let seed = page.seed.clone();
        let seed = if seed.is_empty() {
            // 防御：client 已校验 seed 字段存在，缺失时回退旧标记。
            self.last_timeline_seed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        } else {
            seed
        };
        // 分页元数据：完整响应 body 顶层 has_more（true = 还有更早回合，
        // ChatView 上滚翻页依据）。快照缓存整体替换时同步更新。必须在
        // inner 解包**之前**读取——unwrap_or 会 move snapshot。
        let has_more = page.has_more;
        let snapshot = page.snapshot;
        self.timeline_has_more
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(seed.clone(), has_more);
        *self.chat_timeline.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((seed.clone(), snapshot.clone()));
        self.chat_rev.fetch_add(1, Ordering::Relaxed);
        // 标题栏直连：turns 计数在此缓存（不随 chat_view consume 清空）
        // ——undo_disabled 判定源。
        let turns = chat_adapter::restored_turns(&snapshot).len();
        self.header_turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(seed.clone(), turns);
        // 撤销直发：快照恢复的历史会话缓存最近回合 id。
        if let Some(tid) = chat_adapter::restored_turns(&snapshot)
            .last()
            .map(|t| t.turn_id.clone())
        {
            self.last_turn_ids
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(seed.clone(), tid);
        }
        self.refresh_header();
    }

    /// 主动重拉指定 seed 的 timeline 快照（快照 seed 不匹配时的恢复路径）。
    /// daemon 对重复 activate 幂等（重推快照，无害）；节流 1s 防 16ms 泵
    /// 每 tick 触发。失败静默（快照保留在缓存，下一轮节流到期再试）。
    pub fn spawn_timeline_refresh(&self, seed: &str) {
        let mut last = self
            .timeline_refresh_at
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if last.elapsed() < REFRESH_THROTTLE {
            return;
        }
        *last = Instant::now();
        drop(last);
        let core = self.self_arc();
        let seed = seed.to_string();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("timeline refresh {seed}: connect failed: {err}"));
                    return;
                }
            };
            if let Err(err) = client.activate_timeline(&seed).await {
                log_diag(&format!("timeline refresh {seed}: activate failed: {err}"));
            }
        });
    }

    // ── XAML goalBar（dashboard 投影，control 事件驱动）─────────────

    /// (snapshot, rev) 快照：UI 侧 timer 比对 rev 决定是否刷新 goalBar。
    pub fn dashboard_snapshot(&self) -> (Option<DashboardSnapshot>, u64) {
        let snap = self
            .dashboard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let rev = self.dashboard_rev.load(Ordering::Relaxed);
        (snap, rev)
    }

    /// control 频道 `dashboard_snapshot` 事件落缓存并递增 rev。
    pub fn apply_dashboard(&self, snap: DashboardSnapshot) {
        *self.dashboard.lock().unwrap_or_else(|e| e.into_inner()) = Some(snap);
        self.dashboard_rev.fetch_add(1, Ordering::Relaxed);
    }

    fn seed_set(&self) -> HashSet<String> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|s| s.seed.clone())
            .collect()
    }

    /// XAML 侧生成 command_id（无 uuid 依赖；幂等键只需进程内唯一 + 单调）。
    fn next_command_id(&self) -> String {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        format!("xaml-{ms}-{n}")
    }

    /// 后台刷新 `session.list` + `session.activity` → 投影进缓存 → rev++。
    /// UI 侧（sidebar timer）读取快照即可，无需跨线程回调。
    pub fn spawn_refresh_sessions(&self) {
        let core = self.self_arc();
        let _ = deepx_client::runtime_handle().spawn(async move {
            core.refresh_sessions_inner().await;
        });
    }

    async fn refresh_sessions_inner(&self) {
        let client = match self.ensure_client().await {
            Ok(client) => client,
            Err(err) => {
                log_diag(&format!("refresh_sessions: connect failed: {err}"));
                return;
            }
        };
        let list = match client.query(QueryRequest::SessionList).await {
            Ok(v) => v,
            Err(err) => {
                log_diag(&format!("refresh_sessions: session.list failed: {err}"));
                return;
            }
        };
        let acts = match client.query(QueryRequest::SessionActivity).await {
            Ok(v) => v,
            Err(err) => {
                log_diag(&format!("refresh_sessions: session.activity failed: {err}"));
                return;
            }
        };
        let activities: HashMap<String, ActivityState> =
            parse_activities(&acts).into_iter().collect();
        let mut items = Vec::new();
        if let Some(arr) = list.as_array() {
            items.reserve(arr.len());
            for v in arr {
                let seed = v.get("seed").and_then(|s| s.as_str()).unwrap_or("");
                let running = v.get("running").and_then(|r| r.as_bool()).unwrap_or(false);
                if let Some(item) = project_session_meta(v, activities.get(seed).copied(), running)
                {
                    items.push(item);
                }
            }
        }
        *self.sessions.lock().unwrap_or_else(|e| e.into_inner()) = items;
        *self.activities.lock().unwrap_or_else(|e| e.into_inner()) = activities;
        self.session_rev.fetch_add(1, Ordering::Relaxed);
        log_diag(&format!(
            "refresh_sessions: {} sessions",
            self.sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len()
        ));
        // 标题栏直连：会话列表刷新后 title 可能变化（重命名/首轮摘要）。
        self.refresh_header();
    }

    // ── XAML Info 面板（bootstrap conversation.state 投影）─────────────

    /// (detail, rev) 快照：UI 侧 timer 比对 rev 决定是否刷新面板。
    pub fn info_snapshot(&self) -> (Option<SessionDetail>, u64) {
        let detail = self.info.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let rev = self.info_rev.load(Ordering::Relaxed);
        (detail, rev)
    }

    /// 后台拉取指定会话的用量详情：`client.bootstrap` → `conversation.state`
    /// 投影 → 缓存 + rev++（对齐 conversation_snapshot.rs:29-39 形状）。
    /// 快照为 None（会话无持久状态）时保留旧缓存。
    pub fn spawn_refresh_info(&self, seed: String) {
        log_diag(&format!("spawn_refresh_info({seed})"));
        let core = self.self_arc();
        let _ = deepx_client::runtime_handle().spawn(async move {
            core.refresh_info_inner(&seed).await;
        });
    }

    async fn refresh_info_inner(&self, seed: &str) {
        if seed.is_empty() {
            return;
        }
        let client = match self.ensure_client().await {
            Ok(client) => client,
            Err(err) => {
                log_diag(&format!("refresh_info: connect failed: {err}"));
                return;
            }
        };
        let bootstrap = match client.bootstrap(seed).await {
            Ok(v) => v,
            Err(err) => {
                log_diag(&format!("refresh_info: bootstrap failed: {err}"));
                return;
            }
        };
        let state = &bootstrap.conversation.state;
        *self.info.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(parse_conversation_state(state));
        self.info_rev.fetch_add(1, Ordering::Relaxed);
        log_diag(&format!("refresh_info: {seed} refreshed"));
    }

    /// 新建会话：`session_create`（control）+ 轮询发现新 seed（对齐前端
    /// `waitForSessionCreated` 的 15s 超时）→ navigate chat。
    pub fn spawn_new_session(&self) {
        let core = self.self_arc();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("new_session: connect failed: {err}"));
                    return;
                }
            };
            // 先刷新拿基线，避免"空列表时把旧会话当新会话"。
            core.refresh_sessions_inner().await;
            let before = core.seed_set();
            match client
                .send_command(
                    None,
                    RingingCommand::Control(ControlCommand::SessionCreate {
                        close_current: false,
                    }),
                    CommandOptions::default(),
                )
                .await
            {
                Ok(_) => {
                    for _ in 0..30 {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        core.refresh_sessions_inner().await;
                        let now = core.seed_set();
                        if let Some(new_seed) = now.iter().find(|s| !before.contains(*s)) {
                            let new_seed = new_seed.clone();
                            core.set_active_seed(&new_seed);
                            log_diag(&format!("new_session: created {new_seed}"));
                            core.navigate("chat", Some(&new_seed));
                            return;
                        }
                    }
                    log_diag("new_session: no new seed within 15s");
                }
                Err(err) => log_diag(&format!("new_session: command failed: {err}")),
            }
        });
    }

    /// 恢复会话：`attach(seed)`（session_resume 语义）+ 显式激活 timeline 流
    /// （快照 restore 历史）+ navigate chat。
    ///
    /// 幂等：seed 已是 active 时跳过 attach（挡重复 attach 的网络往返），
    /// 但仍 navigate 回 chat——壳的 current_view 可能已离开 chat（用户点过
    /// 技能/设置，或 resume 失败回 home），否则"点同一会话无反应"。
    pub fn spawn_resume(&self, seed: &str) {
        if self.active_seed() == seed {
            log_diag(&format!("resume {seed}: already active, re-navigate only"));
            self.navigate("chat", Some(seed));
            return;
        }
        let core = self.self_arc();
        let seed = seed.to_string();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("resume {seed}: connect failed: {err}"));
                    return;
                }
            };
            if let Err(err) = client.attach(&seed).await {
                log_diag(&format!("resume {seed}: attach failed: {err}"));
                return;
            }
            // 原生 ChatView 数据源：显式激活 timeline 流，daemon 推送
            // `TimelineSnapshot`（权威 turns 历史）→ bridge 缓存 → restore。
            // 先记录 seed 再 activate：快照可能瞬时到达，缓存标记须就绪。
            *core
                .last_timeline_seed
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = seed.clone();
            // 先同步 active_seed 再 activate：快照瞬时到达时 seed 校验已
            // 就绪（原顺序 activate 在前，快照早到会在 chat_view 泵里被
            // 当作 stale 丢弃——即使没有 Web 竞争也存在竞态窗口）。
            core.set_active_seed(&seed);
            if let Err(err) = client.activate_timeline(&seed).await {
                log_diag(&format!("resume {seed}: activate_timeline failed: {err}"));
            }
            // rev++ 让侧栏 timer 同步 active 高亮（selected_tag 受控刷新）。
            core.session_rev.fetch_add(1, Ordering::Relaxed);
            log_diag(&format!("resume: attached {seed}"));
            core.navigate("chat", Some(&seed));
        });
    }

    /// 归档会话（标签 ×）：Ringing `session_archive`——daemon 侧关实例 +
    /// meta `archived=true`（磁盘保留，左侧列表归档组可见可恢复）。
    ///
    /// 归档的是活动会话时自动切邻居：列表首个非归档会话（updated_at 序），
    /// 无则清空活动态回 home（空态 + 加号引导）。
    pub fn spawn_archive(&self, seed: &str) {
        let is_active = self.active_seed() == seed;
        let neighbor = if is_active {
            self.sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .find(|s| !s.archived && s.seed != seed)
                .map(|s| s.seed.clone())
        } else {
            None
        };
        let core = self.self_arc();
        let seed = seed.to_string();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("archive {seed}: connect failed: {err}"));
                    return;
                }
            };
            match client
                .send_command(
                    Some(&seed),
                    RingingCommand::Control(ControlCommand::SessionArchive { seed: seed.clone() }),
                    CommandOptions::default(),
                )
                .await
            {
                Ok(_) => {
                    core.refresh_sessions_inner().await;
                    if let Some(neighbor) = neighbor {
                        core.spawn_resume(&neighbor);
                    } else {
                        core.set_active_seed("");
                        core.navigate("home", None);
                    }
                }
                Err(err) => log_diag(&format!("archive {seed}: command failed: {err}")),
            }
        });
    }

    /// 恢复归档会话：Ringing `session_unarchive`（meta `archived=false` +
    /// 重新拉起实例），成功后走 resume 链路（attach + timeline 快照）。
    pub fn spawn_unarchive(&self, seed: &str) {
        let core = self.self_arc();
        let seed = seed.to_string();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("unarchive {seed}: connect failed: {err}"));
                    return;
                }
            };
            match client
                .send_command(
                    Some(&seed),
                    RingingCommand::Control(ControlCommand::SessionUnarchive {
                        seed: seed.clone(),
                    }),
                    CommandOptions::default(),
                )
                .await
            {
                Ok(_) => {
                    core.refresh_sessions_inner().await;
                    core.spawn_resume(&seed);
                }
                Err(err) => log_diag(&format!("unarchive {seed}: command failed: {err}")),
            }
        });
    }

    /// 彻底删除会话：Ringing `session_delete`（daemon 侧先关实例再删磁盘
    /// 目录与索引——区别于归档；原 `session_close` 只关实例不删文件）。
    /// 删除的是活动会话时清空活动态回 home。
    pub fn spawn_delete(&self, seed: &str) {
        let core = self.self_arc();
        let seed = seed.to_string();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("delete {seed}: connect failed: {err}"));
                    return;
                }
            };
            match client
                .send_command(
                    Some(&seed),
                    RingingCommand::Control(ControlCommand::SessionDelete { seed: seed.clone() }),
                    CommandOptions::default(),
                )
                .await
            {
                Ok(_) => {
                    if core.active_seed() == seed {
                        core.set_active_seed("");
                        core.navigate("home", None);
                    }
                    core.refresh_sessions_inner().await;
                }
                Err(err) => log_diag(&format!("delete {seed}: command failed: {err}")),
            }
        });
    }

    // ── XAML 技能页（skills_updated 投影，WORKFLOW §8）────────────

    /// (snapshot, rev) 快照：UI 侧 timer 比对 rev 决定是否刷新。
    pub fn skills_snapshot(&self) -> (Option<SkillsSnapshot>, u64) {
        let snap = self
            .skills
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let rev = self.skills_rev.load(Ordering::Relaxed);
        (snap, rev)
    }

    /// 壳主导的当前视图（main.rs 内容区视图切换判定）。
    pub fn current_view(&self) -> String {
        self.current_view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 后端是否已连接（daemon 就绪且 client 建立）。开屏覆盖层显隐依据。
    pub fn backend_connected(&self) -> bool {
        self.client
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// 无缓存时向 daemon 拉一次权威快照（进入技能页首次渲染兜底）。
    ///
    /// 正常路径下 `skills_updated` 事件持续推送（事件即完整快照），无需
    /// 主动拉取；兜底覆盖“事件在页面挂载前已推送”的窗口。
    pub fn ensure_skills(&self) {
        if self
            .skills
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
        {
            return;
        }
        let core = self.self_arc();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("ensure_skills: connect failed: {err}"));
                    return;
                }
            };
            let seed = core.active_seed();
            if seed.is_empty() {
                return;
            }
            match client.bootstrap(&seed).await {
                Ok(snapshot) => {
                    if let Some(skills) = snapshot.control.state.get("skills") {
                        let mut snap = parse_skills_payload(skills);
                        snap.seed = seed;
                        core.skills
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .replace(snap);
                        core.skills_rev.fetch_add(1, Ordering::Relaxed);
                        log_diag("ensure_skills: bootstrap snapshot cached");
                    } else {
                        log_diag("ensure_skills: no control.skills in bootstrap snapshot");
                    }
                }
                Err(err) => log_diag(&format!("ensure_skills: bootstrap failed: {err}")),
            }
        });
    }

    /// 技能动作（对齐 renderer `skills.operation`：request/release/retain）。
    ///
    /// seed 取当前激活会话；operation_id 用壳内序号（daemon 无 UUID 强校验，
    /// 仅透传去重）；expected_revision 取快照 operation_revision（幂等）。
    pub fn spawn_skill_operation(&self, action: &str, name: &str) {
        let core = self.self_arc();
        let action = action.to_string();
        let name = name.to_string();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("skill {action} {name}: connect failed: {err}"));
                    return;
                }
            };
            let seed = core.active_seed();
            if seed.is_empty() {
                log_diag(&format!("skill {action} {name}: no active session"));
                return;
            }
            let revision = core
                .skills
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|s| s.operation_revision)
                .unwrap_or(0);
            match client
                .action(ActionRequest::SkillsOperation {
                    seed,
                    operation_id: core.next_command_id(),
                    action: action.clone(),
                    name: name.clone(),
                    expected_revision: revision,
                })
                .await
            {
                Ok(_) => log_diag(&format!("skill operation {action} {name}: ok")),
                Err(err) => log_diag(&format!("skill operation {action} {name}: failed: {err}")),
            }
        });
    }

    /// 技能目录重载（对齐 renderer `skills.reload`）。
    pub fn spawn_skill_reload(&self) {
        let core = self.self_arc();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("skill reload: connect failed: {err}"));
                    return;
                }
            };
            let seed = core.active_seed();
            if seed.is_empty() {
                log_diag("skill reload: no active session");
                return;
            }
            match client.action(ActionRequest::SkillsReload { seed }).await {
                Ok(_) => log_diag("skill reload: ok"),
                Err(err) => log_diag(&format!("skill reload: failed: {err}")),
            }
        });
    }

    // ── XAML 设置页（config.load 投影 + 壳直连命令，D-2 原则）───────

    /// (snapshot, rev) 快照：UI 侧 timer 比对 rev 决定是否刷新。
    pub fn settings_snapshot(&self) -> (Option<SettingsSnapshot>, u64) {
        let snap = self
            .settings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let rev = self.settings_rev.load(Ordering::Relaxed);
        (snap, rev)
    }

    /// (projection, rev)：Web `shell.setSettings` 初始投影（theme/lang/…）。
    pub fn settings_projection(&self) -> (SettingsProjection, u64) {
        let proj = self
            .settings_proj
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let rev = self.settings_proj_rev.load(Ordering::Relaxed);
        (proj, rev)
    }

    /// 拉取 `config.load` + `skills.list_tools` → 投影进缓存 → rev++。
    /// 幂等：仅缓存为空或 `force` 时执行（进入设置页首次渲染兜底）。
    pub fn spawn_config_load(&self, force: bool) {
        if !force
            && self
                .settings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some()
        {
            return;
        }
        let core = self.self_arc();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("config_load: connect failed: {err}"));
                    return;
                }
            };
            let config = match client.query(QueryRequest::ConfigLoad).await {
                Ok(v) => v,
                Err(err) => {
                    log_diag(&format!("config.load failed: {err}"));
                    return;
                }
            };
            let mut snap = parse_config_load(&config);
            // workspace.status 与 config.load 并行（独立查询，失败不阻塞）。
            if let Ok(status) = client.query(QueryRequest::WorkspaceStatus).await {
                let (cfg, active, endpoint) = parse_workspace_status(&status);
                snap.workspace_configured_mode = cfg;
                snap.workspace_active_mode = active;
                snap.workspace_endpoint = endpoint;
            }
            // 工具列表（subagent 勾选项）；失败不阻塞（页面显示空列表）。
            if let Ok(tools) = client.query(QueryRequest::SkillsListTools).await {
                snap.tools = parse_tools(&tools);
            }
            *core.settings.lock().unwrap_or_else(|e| e.into_inner()) = Some(snap);
            core.settings_rev.fetch_add(1, Ordering::Relaxed);
            log_diag("config_load: settings snapshot cached");
        });
    }

    /// 保存设置：`config.save`（camelCase 全字段，对齐 Web `save()`）。
    pub fn spawn_config_save(&self, fields: Value) {
        let core = self.self_arc();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("config.save: connect failed: {err}"));
                    return;
                }
            };
            match client.action(ActionRequest::ConfigSave { fields }).await {
                Ok(_) => log_diag("config.save: ok"),
                Err(err) => log_diag(&format!("config.save failed: {err}")),
            }
        });
    }

    /// 切换预设：`profile.apply`（daemon 应用后下次 config.load 拿到新值）。
    pub fn spawn_apply_profile(&self, name: String) {
        let core = self.self_arc();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("profile.apply: connect failed: {err}"));
                    return;
                }
            };
            match client.action(ActionRequest::ProfileApply { name }).await {
                Ok(_) => log_diag("profile.apply: ok"),
                Err(err) => log_diag(&format!("profile.apply failed: {err}")),
            }
        });
    }

    /// 把当前编辑的草稿保存为新预设：`profile.save_current`。
    pub fn spawn_save_profile(&self, name: String) {
        let core = self.self_arc();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("profile.save_current: connect failed: {err}"));
                    return;
                }
            };
            match client
                .action(ActionRequest::ProfileSaveCurrent { name })
                .await
            {
                Ok(_) => log_diag("profile.save_current: ok"),
                Err(err) => log_diag(&format!("profile.save_current failed: {err}")),
            }
        });
    }

    /// 删除预设：`profile.delete`（default 不可删，daemon 会返回 Err）。
    pub fn spawn_delete_profile(&self, name: String) {
        let core = self.self_arc();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("profile.delete: connect failed: {err}"));
                    return;
                }
            };
            match client.action(ActionRequest::ProfileDelete { name }).await {
                Ok(_) => log_diag("profile.delete: ok"),
                Err(err) => log_diag(&format!("profile.delete failed: {err}")),
            }
        });
    }

    /// 权限等级：`config.set_permission_level`（对齐 Web changePermissionLevel）。
    pub fn spawn_set_permission(&self, level: u64) {
        let core = self.self_arc();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("set_permission: connect failed: {err}"));
                    return;
                }
            };
            match client
                .action(ActionRequest::ConfigSetPermissionLevel {
                    level: level.clone(),
                })
                .await
            {
                Ok(_) => log_diag(&format!("set_permission {level}: ok")),
                Err(err) => log_diag(&format!("set_permission {level}: failed: {err}")),
            }
        });
    }

    // ── 直连动作（WebView 移除：协议请求 Rust 直发，不再经 Web 中转）──

    /// conversation 频道命令直发（cancel/compact/set_mode 等）。
    /// ack 仅表示 accepted；业务结果经事件流（causation_id）返回。
    /// 失败只记日志（对齐 Web：错误 toast 由调用方本地判定，不阻塞 UI）。
    pub fn spawn_conversation_command(&self, command: ConversationCommand) {
        let core = self.self_arc();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("cmd: connect failed: {err}"));
                    return;
                }
            };
            let seed = core.active_seed();
            match client
                .send_command(
                    Some(&seed),
                    RingingCommand::Conversation(command),
                    CommandOptions::default(),
                )
                .await
            {
                Ok(_) => log_diag("conversation command accepted"),
                Err(err) => log_diag(&format!("conversation command failed: {err}")),
            }
        });
    }

    /// 发送消息：附件统一上传为 ContentRef（图片也走上传——命令中不允许
    /// base64 或本地路径，对齐 daemon 约束与 Electron main 语义）。
    pub fn spawn_send_message(
        &self,
        text: String,
        image_paths: Vec<ComposerAttachment>,
        text_files: Vec<ComposerTextFile>,
    ) {
        let core = self.self_arc();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("send: connect failed: {err}"));
                    return;
                }
            };
            let seed = core.active_seed();
            let mut attachments = Vec::new();
            for att in &image_paths {
                match std::fs::read(&att.path) {
                    Ok(bytes) => match client.upload_content(&seed, &att.mime_type, bytes).await {
                        Ok(content_ref) => attachments.push(content_ref),
                        Err(err) => {
                            log_diag(&format!("send: upload {} failed: {err}", att.file_name))
                        }
                    },
                    Err(err) => log_diag(&format!("send: read {} failed: {err}", att.path)),
                }
            }
            for tf in &text_files {
                match std::fs::read(&tf.path) {
                    Ok(bytes) => match client.upload_content(&seed, "text/plain", bytes).await {
                        Ok(content_ref) => attachments.push(content_ref),
                        Err(err) => {
                            log_diag(&format!("send: upload {} failed: {err}", tf.file_name))
                        }
                    },
                    Err(err) => log_diag(&format!("send: read {} failed: {err}", tf.path)),
                }
            }
            match client
                .send_command(
                    Some(&seed),
                    RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
                        text,
                        images: vec![],
                        attachments: (!attachments.is_empty()).then_some(attachments),
                    }),
                    CommandOptions::default(),
                )
                .await
            {
                Ok(_) => {
                    log_diag("send_message accepted");
                    // B 组反馈本地写入：ack 递增（清空信号）+ 清除错误。
                    let mut fb = core
                        .composer_feedback
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    fb.send_ack = fb.send_ack.wrapping_add(1);
                    fb.submit_error.clear();
                    drop(fb);
                    core.composer_rev.fetch_add(1, Ordering::Relaxed);
                }
                Err(err) => {
                    log_diag(&format!("send_message failed: {err}"));
                    let mut fb = core
                        .composer_feedback
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    fb.submit_error = err.to_string();
                    drop(fb);
                    core.composer_rev.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
    }

    /// 交互响应直发（permission/ask/plan）：Ringing command envelope
    /// （`POST /commands/{control|tool}`，对齐 composer send_message 模式）。
    ///
    /// 2026-08-08 修复：此前误用 query 通道（`/queries/` 白名单不含
    /// `interaction.*` → daemon 404），弹窗按钮全部无效、回合永久挂起；
    /// 现按 method 映射到 deepx-domain `ControlCommand`/`ToolCommand`
    /// 的 serde tag（snake_case）与频道。
    pub fn spawn_interaction_response(&self, method: &str, params: Value) {
        let core = self.self_arc();
        let method = method.to_string();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("{method}: connect failed: {err}"));
                    return;
                }
            };
            let seed = params
                .get("seed")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let gs = |k: &str| {
                params
                    .get(k)
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string()
            };
            let gb = |k: &str| params.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
            let command = match method.as_str() {
                "interaction.permission" => {
                    RingingCommand::Tool(ToolCommand::ToolPermissionRespond {
                        tool_call_id: gs("toolCallId"),
                        approved: gb("approved"),
                        trust_folder: gb("trustFolder"),
                    })
                }
                "interaction.ask_response" => {
                    let answers = params
                        .get("answers")
                        .cloned()
                        .map(serde_json::from_value::<Vec<DomainAskAnswer>>)
                        .transpose();
                    let answers = match answers {
                        Ok(Some(answers)) => answers,
                        Ok(None) => Vec::new(),
                        Err(error) => {
                            log_diag(&format!("{method}: invalid typed answers: {error}"));
                            return;
                        }
                    };
                    RingingCommand::Control(ControlCommand::InteractionAskRespond {
                        interaction_id: gs("askId"),
                        answers,
                    })
                }
                "interaction.ask_dismiss" => {
                    RingingCommand::Control(ControlCommand::InteractionAskDismiss {
                        interaction_id: gs("askId"),
                    })
                }
                "interaction.plan_review" => {
                    RingingCommand::Control(ControlCommand::PlanReviewRespond {
                        interaction_id: gs("callId"),
                        approved: gb("approved"),
                        message: params
                            .get("message")
                            .and_then(|v| v.as_str())
                            .filter(|message| !message.is_empty())
                            .map(str::to_string),
                        autonomous: gb("autonomous"),
                    })
                }
                _ => {
                    log_diag(&format!("{method}: unknown interaction method"));
                    return;
                }
            };
            match client
                .send_command(seed.as_deref(), command, CommandOptions::default())
                .await
            {
                Ok(_) => log_diag(&format!("{method}: accepted")),
                Err(err) => log_diag(&format!("{method}: failed: {err}")),
            }
        });
    }

    /// 工作区切换：`workspace.set`（headerAction::Workspace 直发）。
    pub fn spawn_workspace_set(&self, path: String) {
        let core = self.self_arc();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("workspace.set: connect failed: {err}"));
                    return;
                }
            };
            let seed = core.active_seed();
            match client
                .action(ActionRequest::WorkspaceSet { seed, path })
                .await
            {
                Ok(_) => log_diag("workspace.set: ok"),
                Err(err) => log_diag(&format!("workspace.set failed: {err}")),
            }
        });
    }

    /// 会话工作模式切换：`conversation_set_mode` 命令 + 本地 mode 缓存
    /// （乐观更新——daemon 无 mode 领域事件，对齐 Web 单例 mode 语义）。
    pub fn spawn_set_mode(&self, mode: &str) {
        *self.composer_mode.lock().unwrap_or_else(|e| e.into_inner()) = mode.to_string();
        self.composer_rev.fetch_add(1, Ordering::Relaxed);
        let mode = match mode {
            "plan" => ConversationMode::Plan,
            "code" => ConversationMode::Code,
            _ => ConversationMode::Normal,
        };
        self.spawn_conversation_command(ConversationCommand::ConversationSetMode { mode });
    }

    /// 撤销上一回合：`conversation_undo_turn`（turn_id 来自 per-seed 缓存，
    /// turn_started 事件/快照写入点更新；无缓存则不发送）。
    pub fn spawn_undo_last_turn(&self) {
        let seed = self.active_seed();
        let Some(turn_id) = self
            .last_turn_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&seed)
            .cloned()
        else {
            log_diag("undo: no last turn id cached");
            return;
        };
        self.spawn_conversation_command(ConversationCommand::ConversationUndoTurn { turn_id });
    }

    /// 工作区运行模式切换：`workspace.set_mode`（backend.restart 未实现，
    /// 保存成功后由 UI 提示“下次启动生效”）。
    pub fn spawn_workspace_set_mode(&self, mode: &str) {
        let core = self.self_arc();
        let mode = mode.to_string();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("workspace.set_mode: connect failed: {err}"));
                    return;
                }
            };
            match client
                .action(ActionRequest::WorkspaceSetMode { mode: mode.clone() })
                .await
            {
                Ok(_) => log_diag(&format!("workspace.set_mode {mode}: ok")),
                Err(err) => log_diag(&format!("workspace.set_mode {mode}: failed: {err}")),
            }
        });
    }

    /// 刷新 workspace.status 并合并进 settings 缓存（rev++）。
    pub fn spawn_workspace_status(&self) {
        let core = self.self_arc();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("workspace.status: connect failed: {err}"));
                    return;
                }
            };
            match client.query(QueryRequest::WorkspaceStatus).await {
                Ok(status) => {
                    let (cfg, active, endpoint) = parse_workspace_status(&status);
                    if let Some(snap) = core
                        .settings
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_mut()
                    {
                        snap.workspace_configured_mode = cfg;
                        snap.workspace_active_mode = active;
                        snap.workspace_endpoint = endpoint;
                        core.settings_rev.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(err) => log_diag(&format!("workspace.status failed: {err}")),
            }
        });
    }

    /// WSL 诊断（`workspace.diagnose`，workspace 分类只读展示）。
    pub fn spawn_workspace_diagnose(&self) {
        let core = self.self_arc();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("workspace.diagnose: connect failed: {err}"));
                    return;
                }
            };
            match client.query(QueryRequest::WorkspaceDiagnose).await {
                Ok(v) => log_diag(&format!("workspace.diagnose: {v}")),
                Err(err) => log_diag(&format!("workspace.diagnose failed: {err}")),
            }
        });
    }

    /// WSL 安装（`workspace.install_wsl`）。
    pub fn spawn_workspace_install_wsl(&self) {
        let core = self.self_arc();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("workspace.install_wsl: connect failed: {err}"));
                    return;
                }
            };
            match client.action(ActionRequest::WorkspaceInstallWsl).await {
                Ok(_) => log_diag("workspace.install_wsl: ok"),
                Err(err) => log_diag(&format!("workspace.install_wsl failed: {err}")),
            }
        });
    }

    /// home 视图发送：新建会话 + 首条消息（对齐 Web `startNewSessionAndSend`）。
    ///
    /// session_create（control）→ 轮询发现新 seed（15s 超时）→ attach →
    /// 创建新会话并发送首条消息（`session_create` command + 轮询新 seed +
    /// `conversation_send_message` command → navigate chat）。
    ///
    /// 2026-08-08 修复：首条消息此前误用 `action("session.send_message")`
    /// （action 白名单不含 session.* → daemon 拒绝），改走 command 通道。
    pub fn spawn_send_new_session(&self, text: &str) {
        let core = self.self_arc();
        let text = text.to_string();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("send_new_session: connect failed: {err}"));
                    return;
                }
            };
            core.refresh_sessions_inner().await;
            let before = core.seed_set();
            match client
                .send_command(
                    None,
                    RingingCommand::Control(ControlCommand::SessionCreate {
                        close_current: false,
                    }),
                    CommandOptions::default(),
                )
                .await
            {
                Ok(_) => {
                    let mut seed = String::new();
                    for _ in 0..30 {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        core.refresh_sessions_inner().await;
                        let now = core.seed_set();
                        if let Some(new_seed) = now.iter().find(|s| !before.contains(*s)) {
                            seed = new_seed.clone();
                            break;
                        }
                    }
                    if seed.is_empty() {
                        log_diag("send_new_session: no new seed within 15s");
                        return;
                    }
                    if let Err(err) = client.attach(&seed).await {
                        log_diag(&format!("send_new_session: attach failed: {err}"));
                        return;
                    }
                    core.set_active_seed(&seed);
                    if let Err(err) = client
                        .send_command(
                            Some(&seed),
                            RingingCommand::Conversation(
                                ConversationCommand::ConversationSendMessage {
                                    text,
                                    images: vec![],
                                    attachments: None,
                                },
                            ),
                            CommandOptions::default(),
                        )
                        .await
                    {
                        log_diag(&format!("send_new_session: send_message failed: {err}"));
                        return;
                    }
                    log_diag(&format!("send_new_session: created {seed}, message sent"));
                    core.navigate("chat", Some(&seed));
                }
                Err(err) => log_diag(&format!("send_new_session: command failed: {err}")),
            }
        });
    }

    /// 通知 renderer 切换视图（XAML 侧栏的导航出口）。
    ///
    /// 同步更新壳侧 `current_view`——XAML 视图族据此接管/让出 skills 视图
    /// （main.rs 内容区同 cell 重叠 + opacity 切换，见 WORKFLOW §8）。
    pub fn navigate(&self, view: &str, seed: Option<&str>) {
        *self.current_view.lock().unwrap_or_else(|e| e.into_inner()) = view.to_string();
        // WebView 移除：不再 emit shell.navigate（视图切换壳本地持有）。
        if let Some(seed) = seed {
            self.set_active_seed(seed);
        }
        // 标题栏直连：view 变化立即刷新（不再等 Web setHeader 回推）。
        self.refresh_header();
    }

    /// Lazily connect the deepx-client and register event forwarding.
    /// 外部入口：重建进行中时拒绝（防双 client 竞态），否则委托内部实现。
    async fn ensure_client(&self) -> Result<Client, String> {
        // A 方案：重建进行中时拒绝新连接（rebuild_client 内部持锁协调），
        // 避免双 client 竞态（两个 connect 各建一套 SSE 流）。
        if self.rebuilding.load(Ordering::Relaxed) {
            return Err("client is rebuilding after daemon stall".into());
        }
        self.connect_client().await
    }

    /// 连接主体（无 `rebuilding` 检查）。`rebuild_client` 在
    /// `rebuilding=true` 下调用本方法——若走 `ensure_client` 会自锁：
    /// 重建永远返回 "client is rebuilding" 失败，client 被 close 后无法
    /// 恢复，所有请求（config.load/session.list/attach）连接失败。
    async fn connect_client(&self) -> Result<Client, String> {
        if let Some(client) = self
            .client
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Ok(client);
        }
        // 连接互斥：renderer 秒开后首屏多个 invoke（backend.connect + 会话
        // 列表 + config.load + 侧栏刷新）几乎同时到达，若无互斥则每个调用
        // 各自 connect_async → 各自 wait_for_daemon spawn daemon（双 daemon
        // 并存触发源）。首个调用者置位并发起连接，其余轮询等待其结果。
        if self.connecting.swap(true, Ordering::AcqRel) {
            return self.wait_connect_result().await;
        }
        log_diag("connect_client: connecting...");
        let result = Client::connect_async(ClientOptions {
            handlers: ClientHandlers {
                on_batch: Arc::new({
                    let core = self.self_arc();
                    move |batch: EventBatch| core.emit_batch(batch)
                }),
                on_status: Arc::new({
                    let core = self.self_arc();
                    move |channel: Channel, status: ChannelStatus| core.emit_status(channel, status)
                }),
                on_reset: Some(Arc::new({
                    let core = self.self_arc();
                    move |reset: deepx_client::ResetRequired| core.handle_reset(reset)
                })),
                on_timeline_entry: Arc::new({
                    let _core = self.self_arc();
                    move |_seed: String, _entry: deepx_client::TimelineEntry| {
                        // WebView 移除：timeline.entry 不再转发 Web。
                    }
                }),
                on_timeline_status: Arc::new({
                    let core = self.self_arc();
                    move |status: TimelineStatus| {
                        // A 方案：缓存状态供失联检测（timeline 流死循环判据）。
                        // WebView 移除：不再 emit timeline.status。
                        *core
                            .timeline_status
                            .lock()
                            .unwrap_or_else(|e| e.into_inner()) = Some(status);
                    }
                }),
                on_timeline_snapshot: Arc::new({
                    let core = self.self_arc();
                    move |snapshot: TimelinePage| {
                        // 原生 ChatView：缓存权威 turns 历史（resume 数据源）。
                        // seed 标记与层级解包见 `cache_timeline_snapshot`——
                        // 从快照 body 顶层读权威 seed，缓存 `snapshot` 子对象。
                        core.cache_timeline_snapshot(snapshot);
                        // WebView 移除：不再 emit timeline.snapshot（原生 ChatView
                        // 从 chat_timeline 缓存消费）。
                    }
                }),
            },
            launch_daemon_if_missing: true,
            ..Default::default()
        })
        .await;
        // 无论成败都先复位互斥位，等待者据此退出/复用结果。
        self.connecting.store(false, Ordering::Release);
        let client = result.map_err(|e| {
            log_diag(&format!("connect_client connect failed: {e}"));
            e.to_string()
        })?;
        *self.client.lock().unwrap_or_else(|e| e.into_inner()) = Some(client.clone());
        // WebView 移除：不再 emit backend.status（连接状态由壳本地持有）。
        Ok(client)
    }

    /// 等待并发连接发起者完成：成功 → 复用其 client；失败/超时 → 返回错误
    /// （调用方各自的重试路径——auto-reconnect 冷却 5s 起——负责恢复）。
    async fn wait_connect_result(&self) -> Result<Client, String> {
        let deadline = Instant::now() + CONNECT_WAIT_TIMEOUT;
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if let Some(client) = self
                .client
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                return Ok(client);
            }
            if !self.connecting.load(Ordering::Acquire) {
                // 发起者已结束且失败：直接失败，避免每个等待者再各发起一次。
                return Err("backend connect failed (concurrent attempt)".into());
            }
            if Instant::now() >= deadline {
                return Err("backend connect in progress timed out".into());
            }
        }
    }

    /// `ringing.reset_required`: re-bootstrap the affected session and push a
    /// fresh snapshot to the renderer (mirrors browserBridge `handleReset`).
    pub fn handle_reset(&self, reset: deepx_client::ResetRequired) {
        let core = self.self_arc();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("reset: reconnect failed: {err}"));
                    return;
                }
            };
            match client.bootstrap(&reset.seed).await {
                // WebView 移除：bootstrap 结果不再 emit（壳由事件流自愈）。
                Ok(_snapshot) => {}
                Err(err) => log_diag(&format!("reset: bootstrap {} failed: {err}", reset.seed)),
            }
        });
    }

    fn emit_batch(&self, batch: EventBatch) {
        // XAML 侧栏实时活动状态：control 频道 `session_activity_changed`
        // 增量更新缓存（不触发全量 refresh）。
        if batch.channel == Channel::Control {
            let mut changed = false;
            let mut skills_changed = false;
            let mut list_changed = false;
            for env in &batch.envelopes {
                let RingingEvent::Control(event) = &env.event else {
                    continue;
                };
                if let Some((seed, state)) = activity_event(event) {
                    self.activities
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(seed.clone(), state);
                    let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(item) = sessions.iter_mut().find(|i| i.seed == seed) {
                        item.state = state;
                    }
                    changed = true;
                }
                // XAML 技能页：skills_updated 携带完整 SkillsStatus 载荷，
                // 直接缓存为权威快照（含 seed，batch.seed 兜底）。
                if let Some(mut snap) = skills_event(event) {
                    if snap.seed.is_empty() {
                        snap.seed = batch.seed.clone();
                    }
                    self.skills
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .replace(snap);
                    skills_changed = true;
                }
                // 会话生命周期变更（created/archived/unarchived/deleted）：
                // 归档/删除/新建不再依赖 500ms 轮询，事件到达即全量刷新。
                // （发起方命令成功后的主动 refresh 保留，作为快速路径。）
                if session_state_event(event).is_some() {
                    list_changed = true;
                }
                // XAML composer goalBar：dashboard_snapshot 携带完整
                // DashboardSnapshot 载荷（tasks/recent_edits/current_todo_id），
                // 直接缓存为权威快照（终局架构：Web 移除后 XAML 直消费）。
                if let Some(snap) = dashboard_event(event) {
                    self.apply_dashboard(snap);
                }
                if let Some(ev) = interaction_event(event) {
                    self.apply_interaction_event(&batch.seed, ev);
                }
            }
            if changed {
                self.session_rev.fetch_add(1, Ordering::Relaxed);
                // Info 面板打开过（缓存存在）→ 活动状态变化（回合边界信号）
                // 时顺手刷新用量（低频触发，bootstrap 一次成本可接受）。
                let info_active = self
                    .info
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_some();
                if info_active {
                    self.spawn_refresh_info(self.active_seed());
                }
            }
            if skills_changed {
                self.skills_rev.fetch_add(1, Ordering::Relaxed);
            }
            if list_changed {
                // 异步刷新：session.list + session.activity → 投影 → rev++，
                // 侧栏/标签页 timer 比对 rev 后刷新（各视图同一数据源）。
                self.spawn_refresh_sessions();
            }
        } else if batch.channel == Channel::Tool {
            // Rust 直连交互队列（读路径直连）：tool 频道权限请求
            // （permission 优先于 ask/plan，对齐 Web pendingInteractions 组装）。
            for env in &batch.envelopes {
                let RingingEvent::Tool(event) = &env.event else {
                    continue;
                };
                if let Some(ev) = tool_permission_event(event) {
                    self.apply_tool_permission_event(&batch.seed, ev);
                }
            }
            // 原生 ChatView 直连：Tool 频道渲染事件（tool_call_prepared /
            // tool_started / tool_finished）与 conversation 事件同一队列，
            // 供 Transcript 流式渲染工具卡。与 conversation 频道事件交错
            // 到达无顺序保证——round_renderer 按 turn/round 定位 + 自动建
            // turn 兜底，不依赖频道间顺序。
            let mut queue = self.chat_events.lock().unwrap_or_else(|e| e.into_inner());
            let mut pushed = false;
            for env in &batch.envelopes {
                if chat_adapter::render_event(&env.event).is_some() {
                    // seed 标记：drain 侧按 active_seed 过滤（会话隔离）。
                    queue.push_back((batch.seed.clone(), env.event.clone()));
                    pushed = true;
                }
            }
            if pushed {
                self.chat_rev.fetch_add(1, Ordering::Relaxed);
            }
        } else if batch.channel == Channel::Conversation {
            // Rust 直连 composer（读路径直连）：conversation 事件活动追踪
            // ——isStreaming（卡死检测）+ usage_updated 缓存（model/context）。
            // 无条件挂载：streaming 信号同时驱动标题栏 undo/compact disabled
            // （对齐 Web `streaming()` 判定）。事件高频（流式 delta），处理
            // 为 O(1) 时间戳写入，rev 每 batch 递增一次（XAML 250ms 轮询
            // 稀释，无害）。
            let now = unix_ms();
            let mut turn_boundary = false;
            {
                let mut map = self
                    .composer_activity
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let activity = map.entry(batch.seed.clone()).or_default();
                for env in &batch.envelopes {
                    let RingingEvent::Conversation(event) = &env.event else {
                        continue;
                    };
                    if let Some(ev) = conversation_activity_event(event) {
                        if matches!(
                            &ev,
                            ConversationActivityEvent::Started | ConversationActivityEvent::Ended
                        ) {
                            turn_boundary = true;
                            // 撤销直发：turn 事件带 turn_id，缓存最近回合。
                            let tid = match event {
                                DomainConversationEvent::TurnStarted { turn_id, .. }
                                | DomainConversationEvent::TurnCompleted { turn_id, .. }
                                | DomainConversationEvent::TurnFailed { turn_id, .. } => {
                                    Some(turn_id.as_str())
                                }
                                DomainConversationEvent::ConversationCancelled { turn_id } => {
                                    turn_id.as_deref()
                                }
                                _ => None,
                            };
                            if let Some(tid) = tid {
                                self.last_turn_ids
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .insert(batch.seed.clone(), tid.to_string());
                            }
                        }
                        activity.apply(ev, now);
                    }
                }
            }
            self.composer_rev.fetch_add(1, Ordering::Relaxed);
            // 标题栏直连：turn 边界（streaming 翻转）刷新 undo/compact disabled。
            if turn_boundary {
                self.refresh_header();
            }
            // 原生 ChatView 直连：渲染相关事件（turn/round/delta/checkpoint）
            // 入队，UI 线程 timer drain 喂 Transcript（读路径直连）。
            // seed 标记：drain 侧按 active_seed 过滤（会话隔离）。
            let mut queue = self.chat_events.lock().unwrap_or_else(|e| e.into_inner());
            for env in &batch.envelopes {
                if chat_adapter::render_event(&env.event).is_some() {
                    queue.push_back((batch.seed.clone(), env.event.clone()));
                }
            }
            if !queue.is_empty() {
                self.chat_rev.fetch_add(1, Ordering::Relaxed);
            }
        }
        // WebView 移除：ringing.batch 不再转发 Web（原生直连消费上方各分支）。
    }

    fn emit_status(&self, channel: Channel, status: ChannelStatus) {
        self.channel_status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(channel, status);
    }

    // ── A 方案：daemon 失联检测与 client 重建（WORKFLOW §7）────────────────
    //
    // 背景：daemon 重启后旧 lease（server_epoch/client_session_id）失效。
    // SSE 重连带旧 epoch 的 Last-Event-ID 被 daemon 静默按 0 处理（从头
    // 回放，ringing_http.rs parse_sse_cursor）；ringing 通道回放可恢复，但
    // timeline 客户端对回放的旧 seq 报 Protocol error（deepx-client
    // timeline.rs L257），重连死循环——事件流永久断，表现为"后端在处理
    // 但前端 UI 不更新"。修复：检测失联后重建 client（重新 open 拿新
    // epoch），并恢复已激活 seed 的流（快照驱动，前端零改动自愈）。

    /// 失联检测（pump 每 50ms 调用；纯内存轻量检查，无锁嵌套）。
    pub fn check_daemon_health(&self) {
        if self.rebuilding.load(Ordering::Relaxed) {
            return;
        }
        let now = Instant::now();
        // 无 client（首次 connect 失败/从未建立）时自动重连：renderer 只在
        // 页面加载时发一次 backend.connect，若恰逢 daemon 初始化窗口而失败
        // （open 超时/连接拒绝），原逻辑没有任何机制再触发 connect（health
        // 仅覆盖"已建立后 stall"），页面会永久失败直到手动刷新/重启。
        // 此处以独立冷却自动重试，直到 client 建立。
        let client_missing = self
            .client
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none();
        if client_missing {
            let last = self
                .last_auto_reconnect_at
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let reconnect_cooldown =
                auto_reconnect_cooldown_for(self.rebuild_failures.load(Ordering::Relaxed));
            if now.duration_since(*last) >= reconnect_cooldown {
                *self
                    .last_auto_reconnect_at
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = now;
                log_diag("health: no client; auto-reconnecting");
                self.rebuild_client();
            }
            return;
        }
        // 退避冷却：连续失败后指数拉长重建间隔（60s→960s 封顶），防止
        // rebuild 风暴把 daemon 连接数打爆（32 连接信号量 → 静默 drop）。
        let rebuild_cooldown = rebuild_cooldown_for(self.rebuild_failures.load(Ordering::Relaxed));
        let cooldown_ok = {
            let last = self
                .last_rebuild_at
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            now.duration_since(*last) >= rebuild_cooldown
        };
        if !cooldown_ok {
            return;
        }
        if self.compute_stall(now) {
            self.rebuild_client();
        }
    }

    /// 任一活跃流失联持续超阈值即视为 daemon 失联。
    fn compute_stall(&self, now: Instant) -> bool {
        // 1) timeline 流非 Open/Closed 持续超阈值——daemon 重启后
        //    timeline 回放 Protocol error 死循环的专属判据。
        if let Some(status) = self
            .timeline_status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            let healthy = matches!(
                status,
                TimelineStatus::Open { .. } | TimelineStatus::Closed { .. }
            );
            let mut since = self
                .timeline_stall_since
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if healthy {
                *since = None;
            } else if since.is_none() {
                *since = Some(now);
            } else if now.duration_since(since.unwrap()) >= STALL_THRESHOLD {
                log_diag("health: timeline stream stalled, rebuilding client");
                return true;
            }
        }

        // 2) ringing 三通道无一 Open 持续超阈值——daemon 完全不可达场景。
        let statuses = self
            .channel_status
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let any_open = statuses
            .values()
            .any(|status| matches!(status, ChannelStatus::Open { .. }));
        let any_tracked = !statuses.is_empty();
        let mut since = self
            .channels_stall_since
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if any_open || !any_tracked {
            *since = None;
        } else if since.is_none() {
            *since = Some(now);
        } else if now.duration_since(since.unwrap()) >= STALL_THRESHOLD {
            log_diag("health: all ringing channels stalled, rebuilding client");
            return true;
        }

        false
    }

    /// 重建 client：停旧（close）→ 重新 open（新 epoch）→ 恢复已激活的流。
    fn rebuild_client(&self) {
        self.rebuilding.store(true, Ordering::Relaxed);
        *self
            .last_rebuild_at
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Instant::now();
        log_diag("health: rebuilding client (daemon stall detected)");
        let core = self.self_arc();
        let _ = deepx_client::runtime_handle().spawn(async move {
            // 1) 停旧 client 及其全部任务（renewal + 3 通道 + timeline 流）。
            let old = core.client.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(client) = old {
                client.close();
                log_diag("health: closed stale client");
            }
            // 2) 重新协商（新 server_epoch + client_session_id；
            //    launch_daemon_if_missing 兜底拉起 daemon）。用内部
            //    connect_client：此时 rebuilding=true，走 ensure_client
            //    会自锁失败（历史 bug：A 方案重建从未成功）。
            match core.connect_client().await {
                Ok(_) => {
                    log_diag("health: reconnected with fresh session");
                    core.rebuild_failures.store(0, Ordering::Relaxed);
                }
                Err(err) => {
                    log_diag(&format!("health: reconnect failed: {err}"));
                    core.rebuild_failures.fetch_add(1, Ordering::Relaxed);
                    core.rebuilding.store(false, Ordering::Relaxed);
                    core.reset_stall_timers();
                    return;
                }
            }
            // 3) 恢复已 attach 的 seed（XAML 侧栏）+ Web 最近激活的 seed。
            let seeds: Vec<String> = {
                let mut set = core
                    .attached
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                let tseed = core
                    .last_timeline_seed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                if !tseed.is_empty() {
                    set.insert(tseed);
                }
                set.into_iter().collect()
            };
            for seed in &seeds {
                core.restore_seed(seed).await;
            }
            // 4) 状态复位（WebView 移除：不再 emit backend.status）。
            core.rebuilding.store(false, Ordering::Relaxed);
            core.reset_stall_timers();
            core.spawn_refresh_sessions();
            log_diag("health: rebuild complete");
        });
    }

    fn reset_stall_timers(&self) {
        *self
            .timeline_stall_since
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        *self
            .channels_stall_since
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        *self
            .timeline_status
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// 恢复单个 seed：attach（session_resume 语义）→ 每通道 bootstrap 快照
    /// → timeline 流（快照 watermark 续传）。前端 ringingMonitor /
    /// timelineMonitor 收到快照后全量重建；SSE 回放由 applied event_id
    /// 去重（renderer ringingStores L868），无重复应用。
    async fn restore_seed(&self, seed: &str) {
        let client = match self
            .client
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            Some(c) => c,
            None => {
                log_diag(&format!("health: restore {seed}: no client"));
                return;
            }
        };
        if let Err(err) = client.attach(seed).await {
            log_diag(&format!("health: attach {seed} failed: {err}"));
            return;
        }
        match client.bootstrap(seed).await {
            // WebView 移除：快照不再 emit（壳经事件流/主动快照自愈）。
            Ok(_snapshot) => {}
            Err(err) => log_diag(&format!("health: bootstrap {seed} failed: {err}")),
        }
        match client.activate_timeline(seed).await {
            // WebView 移除：timeline 快照经 on_timeline_snapshot 缓存直连。
            Ok(_snapshot) => {}
            Err(err) => log_diag(&format!("health: timeline activate {seed} failed: {err}")),
        }
    }
}

static SHARED_CORE: OnceLock<Arc<BridgeCore>> = OnceLock::new();

/// UI-thread half of the bridge（WebView 移除后仅持 tokio 侧 core 引用）。
pub struct Bridge {
    core: Arc<BridgeCore>,
}

static SHARED: OnceLock<Arc<Bridge>> = OnceLock::new();

impl Bridge {
    pub fn shared() -> Arc<Bridge> {
        SHARED
            .get_or_init(|| {
                let core = Arc::new(BridgeCore {
                    client: Mutex::new(None),
                    attached: Mutex::new(HashSet::new()),
                    channel_status: Mutex::new(HashMap::new()),
                    sessions: Mutex::new(Vec::new()),
                    activities: Mutex::new(HashMap::new()),
                    session_rev: AtomicU64::new(0),
                    active_seed: Mutex::new(String::new()),
                    header_state: Mutex::new(HeaderState::default()),
                    header_rev: AtomicU64::new(0),
                    header_turns: Mutex::new(HashMap::new()),
                    last_turn_ids: Mutex::new(HashMap::new()),
                    timeline_stall_since: Mutex::new(None),
                    channels_stall_since: Mutex::new(None),
                    rebuilding: AtomicBool::new(false),
                    connecting: AtomicBool::new(false),
                    last_rebuild_at: Mutex::new(Instant::now()),
                    last_auto_reconnect_at: Mutex::new(Instant::now()),
                    rebuild_failures: AtomicU32::new(0),
                    last_timeline_seed: Mutex::new(String::new()),
                    timeline_status: Mutex::new(None),
                    skills: Mutex::new(None),
                    skills_rev: AtomicU64::new(0),
                    current_view: Mutex::new(String::new()),
                    settings: Mutex::new(None),
                    settings_rev: AtomicU64::new(0),
                    settings_proj: Mutex::new(SettingsProjection::default()),
                    settings_proj_rev: AtomicU64::new(0),
                    info: Mutex::new(None),
                    info_rev: AtomicU64::new(0),
                    interaction: Mutex::new(InteractionState::default()),
                    interaction_rev: AtomicU64::new(0),
                    interactions: Mutex::new(HashMap::new()),
                    composer_rev: AtomicU64::new(0),
                    composer_activity: Mutex::new(HashMap::new()),
                    composer_mode: Mutex::new("plan".to_string()),
                    composer_feedback: Mutex::new(ComposerFeedback::default()),
                    // Canonical conversation events are queued for the native
                    // ChatView; no renderer projection participates here.
                    chat_events: Mutex::new(std::collections::VecDeque::new()),
                    chat_timeline: Mutex::new(None),
                    timeline_has_more: Mutex::new(std::collections::HashMap::new()),
                    chat_prepend: Mutex::new(std::collections::VecDeque::new()),
                    timeline_fetching: Mutex::new(std::collections::HashSet::new()),
                    chat_outputs: Mutex::new(std::collections::VecDeque::new()),
                    content_fetching: Mutex::new(std::collections::HashSet::new()),
                    chat_rev: AtomicU64::new(0),
                    // 初始化为远古时刻：首次 refresh 立即放行。
                    timeline_refresh_at: Mutex::new(Instant::now() - Duration::from_secs(3600)),
                    dashboard: Mutex::new(None),
                    dashboard_rev: AtomicU64::new(0),
                });
                let _ = SHARED_CORE.set(core.clone());
                Arc::new(Bridge { core })
            })
            .clone()
    }

    /// XAML 侧栏访问 tokio 侧状态（会话列表 / 命令出口）。
    pub fn core(&self) -> Arc<BridgeCore> {
        self.core.clone()
    }

    // ── XAML 侧栏命令透传（sidebar.rs 只依赖 Bridge）─────────────────

    pub fn spawn_refresh_sessions(&self) {
        self.core.spawn_refresh_sessions();
    }

    pub fn spawn_new_session(&self) {
        self.core.spawn_new_session();
    }

    pub fn spawn_resume(&self, seed: &str) {
        self.core.spawn_resume(seed);
    }

    pub fn spawn_archive(&self, seed: &str) {
        self.core.spawn_archive(seed);
    }

    pub fn spawn_unarchive(&self, seed: &str) {
        self.core.spawn_unarchive(seed);
    }

    pub fn spawn_delete(&self, seed: &str) {
        self.core.spawn_delete(seed);
    }

    pub fn navigate(&self, view: &str, seed: Option<&str>) {
        self.core.navigate(view, seed);
    }

    // ── XAML 标题栏 STA 能力（header.rs 只依赖 Bridge；①②③ 壳直接处理）──

    /// ①workspace：目录选择对话框（STA COM；用户取消返回 Ok(null)）。
    pub fn pick_workspace_directory(&self) -> Result<Value, String> {
        show_open_dialog(true, false, false, None)
    }

    /// settings：文件选择对话框（tokenizer 路径；用户取消返回 Ok(null)）。
    pub fn pick_file(&self) -> Result<Value, String> {
        show_open_dialog(false, false, false, None)
    }

    /// ②location：系统 shell 打开会话目录（bridge.rs `open_external`）。
    pub fn open_path(&self, target: &str) -> Result<(), String> {
        open_external(target)
    }

    /// 标题栏本地开关翻转（headerDirect：info/stats 壳本地维护）。
    pub fn toggle_header_flag(&self, flag: HeaderFlag) {
        self.core.toggle_header_flag(flag);
    }

    // ── 直连动作转发（WebView 移除：协议请求 Rust 直发）──────────────

    /// conversation 频道命令直发（cancel/compact/set_mode 等）。
    pub fn spawn_conversation_command(&self, command: ConversationCommand) {
        self.core.spawn_conversation_command(command);
    }

    /// 会话工作模式切换（命令 + 本地 mode 缓存）。
    pub fn spawn_set_mode(&self, mode: &str) {
        self.core.spawn_set_mode(mode);
    }

    /// 发送消息（附件上传 ContentRef 后直发 send_message）。
    pub fn spawn_send_message(
        &self,
        text: String,
        image_paths: Vec<ComposerAttachment>,
        text_files: Vec<ComposerTextFile>,
    ) {
        self.core.spawn_send_message(text, image_paths, text_files);
    }

    /// 交互响应直发（permission/ask/plan）。
    pub fn spawn_interaction_response(&self, method: &str, params: Value) {
        self.core.spawn_interaction_response(method, params);
    }

    /// 工作区切换直发（workspace.set）。
    pub fn spawn_workspace_set(&self, path: String) {
        self.core.spawn_workspace_set(path);
    }

    /// 撤销上一回合直发（conversation_undo_turn）。
    pub fn spawn_undo_last_turn(&self) {
        self.core.spawn_undo_last_turn();
    }

    /// 附件：图片文件选择对话框（STA COM；用户取消返回 Ok(null)）。
    /// 复用 show_open_dialog 的 image_filter（png/jpg/jpeg/gif/webp/bmp）。
    pub fn pick_image_file(&self) -> Result<Value, String> {
        show_open_dialog(false, false, true, Some("选择图片"))
    }

    /// 附件：文本文件选择对话框（STA COM；用户取消返回 Ok(null)）。
    pub fn pick_text_file(&self) -> Result<Value, String> {
        show_open_dialog(false, false, false, Some("选择文本文件"))
    }

    // ── XAML home / settings 视图透传（home_view.rs / settings_view.rs 只依赖 Bridge）──

    /// home：新建会话 + 首条消息（壳直连，不回传 Web）。
    pub fn spawn_send_new_session(&self, text: &str) {
        self.core.spawn_send_new_session(text);
    }

    /// settings：拉取 config.load + tools（force=true 时忽略缓存）。
    pub fn spawn_config_load(&self, force: bool) {
        self.core.spawn_config_load(force);
    }

    /// settings：保存全字段（camelCase，对齐 Web `save()`）。
    pub fn spawn_config_save(&self, fields: Value) {
        self.core.spawn_config_save(fields);
    }

    /// settings：切换预设（profile.apply；daemon 应用后前端轮询拿到新值）。
    pub fn spawn_apply_profile(&self, name: &str) {
        self.core.spawn_apply_profile(name.to_string());
    }

    /// settings：把当前草稿保存为新预设（profile.save_current）。
    pub fn spawn_save_profile(&self, name: &str) {
        self.core.spawn_save_profile(name.to_string());
    }

    /// settings：删除预设（profile.delete；default 不可删）。
    pub fn spawn_delete_profile(&self, name: &str) {
        self.core.spawn_delete_profile(name.to_string());
    }

    /// settings：权限等级（config.set_permission_level）。
    pub fn spawn_set_permission(&self, level: u64) {
        self.core.spawn_set_permission(level);
    }

    /// settings：工作区运行模式（workspace.set_mode；restart 未实现，提示下次生效）。
    pub fn spawn_workspace_set_mode(&self, mode: &str) {
        self.core.spawn_workspace_set_mode(mode);
    }

    /// settings：刷新 workspace.status 进缓存。
    pub fn spawn_workspace_status(&self) {
        self.core.spawn_workspace_status();
    }

    /// settings：WSL 诊断（日志输出，无 UI 回显）。
    pub fn spawn_workspace_diagnose(&self) {
        self.core.spawn_workspace_diagnose();
    }

    /// settings：WSL 安装（日志输出，无 UI 回显）。
    pub fn spawn_workspace_install_wsl(&self) {
        self.core.spawn_workspace_install_wsl();
    }

    /// 心跳（UI 线程 timer 每 50ms 调用）：daemon 失联检测（轻量内存检查，
    /// 重建在 tokio 侧执行）。WebView 移除后无 outbox 投递。
    pub fn pump(&self) {
        self.core.check_daemon_health();
    }
}

#[cfg(test)]
fn parse_interaction_event(event: &Value) -> Option<InteractionEvent> {
    match event.get("type")?.as_str()? {
        "interaction_requested" => Some(InteractionEvent::AskRequested {
            id: event.get("interaction_id")?.as_str()?.to_string(),
            questions: parse_questions(event.get("questions")?),
        }),
        "interaction_resolved" => Some(InteractionEvent::AskResolved {
            id: event.get("interaction_id")?.as_str()?.to_string(),
        }),
        "plan_review_requested" => Some(InteractionEvent::PlanRequested {
            id: event.get("interaction_id")?.as_str()?.to_string(),
            plan_content: event.get("plan_content")?.as_str()?.to_string(),
            review_type: event
                .get("review_type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            todo_items: parse_todo_items(event.get("todo_items")),
        }),
        "plan_review_resolved" => Some(InteractionEvent::PlanResolved {
            id: event.get("interaction_id")?.as_str()?.to_string(),
        }),
        "operation_failed" => match event
            .get("error")
            .and_then(|error| error.get("code"))
            .and_then(Value::as_str)?
        {
            "ask_rejected" | "interaction_not_found" => Some(InteractionEvent::GhostCleanup),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
fn parse_tool_permission_event(event: &Value) -> Option<ToolPermissionEvent> {
    match event.get("type")?.as_str()? {
        "tool_permission_requested" => Some(ToolPermissionEvent::Requested {
            tool_call_id: event.get("tool_call_id")?.as_str()?.to_string(),
            tool_name: event.get("tool_name")?.as_str()?.to_string(),
            reason: event.get("reason")?.as_str()?.to_string(),
            paths: event
                .get("paths")
                .and_then(Value::as_array)
                .map(|paths| {
                    paths
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            category: event.get("category")?.as_str()?.to_string(),
            level: event
                .get("level")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            risk: event.get("risk")?.as_str()?.to_string(),
            consequence: event.get("consequence")?.as_str()?.to_string(),
        }),
        "tool_finished" => Some(ToolPermissionEvent::Resolved {
            tool_call_id: event.get("tool_call_id")?.as_str()?.to_string(),
        }),
        _ => None,
    }
}

#[cfg(test)]
fn parse_conversation_activity_event(event: &Value) -> Option<ConversationActivityEvent> {
    match event.get("type")?.as_str()? {
        "turn_started" => Some(ConversationActivityEvent::Started),
        "turn_completed" | "turn_failed" | "conversation_cancelled" => {
            Some(ConversationActivityEvent::Ended)
        }
        "usage_updated" => Some(ConversationActivityEvent::Usage {
            prompt_tokens: event
                .get("usage")
                .and_then(|usage| usage.get("prompt_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            context_limit: event
                .get("context_limit")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            model: event
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        "round_delta"
        | "block_checkpoint"
        | "round_completed"
        | "provider_retrying"
        | "provider_tool_status" => Some(ConversationActivityEvent::Touched),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_core() -> BridgeCore {
        BridgeCore {
            client: Mutex::new(None),
            attached: Mutex::new(HashSet::new()),
            channel_status: Mutex::new(HashMap::new()),
            sessions: Mutex::new(Vec::new()),
            activities: Mutex::new(HashMap::new()),
            session_rev: AtomicU64::new(0),
            active_seed: Mutex::new(String::new()),
            header_state: Mutex::new(HeaderState::default()),
            header_rev: AtomicU64::new(0),
            header_turns: Mutex::new(HashMap::new()),
            last_turn_ids: Mutex::new(HashMap::new()),
            timeline_stall_since: Mutex::new(None),
            channels_stall_since: Mutex::new(None),
            rebuilding: AtomicBool::new(false),
            connecting: AtomicBool::new(false),
            last_rebuild_at: Mutex::new(Instant::now()),
            last_auto_reconnect_at: Mutex::new(Instant::now()),
            rebuild_failures: AtomicU32::new(0),
            last_timeline_seed: Mutex::new(String::new()),
            timeline_status: Mutex::new(None),
            skills: Mutex::new(None),
            skills_rev: AtomicU64::new(0),
            current_view: Mutex::new(String::new()),
            settings: Mutex::new(None),
            settings_rev: AtomicU64::new(0),
            settings_proj: Mutex::new(SettingsProjection::default()),
            settings_proj_rev: AtomicU64::new(0),
            info: Mutex::new(None),
            info_rev: AtomicU64::new(0),
            interaction: Mutex::new(InteractionState::default()),
            interaction_rev: AtomicU64::new(0),
            interactions: Mutex::new(HashMap::new()),
            composer_rev: AtomicU64::new(0),
            composer_activity: Mutex::new(HashMap::new()),
            composer_mode: Mutex::new("plan".to_string()),
            composer_feedback: Mutex::new(ComposerFeedback::default()),
            chat_events: Mutex::new(std::collections::VecDeque::new()),
            chat_timeline: Mutex::new(None),
            timeline_has_more: Mutex::new(std::collections::HashMap::new()),
            chat_prepend: Mutex::new(std::collections::VecDeque::new()),
            timeline_fetching: Mutex::new(std::collections::HashSet::new()),
            chat_outputs: Mutex::new(std::collections::VecDeque::new()),
            content_fetching: Mutex::new(std::collections::HashSet::new()),
            chat_rev: AtomicU64::new(0),
            timeline_refresh_at: Mutex::new(Instant::now() - Duration::from_secs(3600)),
            dashboard: Mutex::new(None),
            dashboard_rev: AtomicU64::new(0),
        }
    }

    fn reconnecting() -> TimelineStatus {
        TimelineStatus::Reconnecting {
            seed: "s1".into(),
            retry_ms: 1000,
            cursor: 3,
        }
    }

    // ── ChatView 事件队列（seed 隔离）────────────────────────────────

    /// chat_drain 只返回 active_seed 的事件：后台会话增量不污染活动
    /// 会话的 Transcript（切换瞬间残留事件同样被丢弃）。
    #[test]
    fn chat_drain_filters_by_active_seed() {
        let core = test_core();
        core.set_active_seed("sA");
        {
            let mut q = core.chat_events.lock().unwrap();
            let started = |turn_id: &str| {
                RingingEvent::Conversation(DomainConversationEvent::TurnStarted {
                    turn_id: turn_id.to_string(),
                    user_text: String::new(),
                })
            };
            q.push_back(("sA".into(), started("t1")));
            q.push_back(("sB".into(), started("t2")));
            q.push_back((
                "sA".into(),
                RingingEvent::Conversation(DomainConversationEvent::TurnCompleted {
                    turn_id: "t1".into(),
                    stop_reason: None,
                    usage: None,
                }),
            ));
        }
        let (events, _) = core.chat_drain();
        assert_eq!(events.len(), 2, "只返回活动会话 sA 的事件");
        assert!(events.iter().all(|event| !matches!(
            event,
            RingingEvent::Conversation(DomainConversationEvent::TurnStarted { turn_id, .. })
                if turn_id == "t2"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            RingingEvent::Conversation(DomainConversationEvent::TurnStarted { .. })
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            RingingEvent::Conversation(DomainConversationEvent::TurnCompleted { .. })
        )));

        // 切换后：sA 的残留事件（若有）不再泄漏到 sB。
        core.set_active_seed("sB");
        core.chat_events.lock().unwrap().push_back((
            "sA".into(),
            RingingEvent::Conversation(DomainConversationEvent::RoundDelta {
                turn_id: "t1".into(),
                round_num: 0,
                kind: deepx_client::RoundDeltaKind::Answering,
                delta: String::new(),
            }),
        ));
        let (events, _) = core.chat_drain();
        assert!(events.is_empty(), "切换后 sA 残留事件被丢弃");
    }

    // ── 交互队列状态机（Rust 直连读路径）──────────────────────────────

    /// 真实 daemon control 事件（deepx-domain `ControlEvent` snake_case）。
    fn ask_requested_event(id: &str, turn_id: &str) -> Value {
        json!({
            "type": "interaction_requested",
            "interaction_id": id,
            "turn_id": turn_id,
            "mode": "single",
            "questions": [
                { "id": "q1", "question": "继续？", "options": ["是", "否"], "allow_custom": true }
            ],
        })
    }

    #[test]
    fn parses_ask_requested_with_snake_case_keys() {
        let ev = parse_interaction_event(&ask_requested_event("i1", "t1")).expect("parse");
        let InteractionEvent::AskRequested { id, questions } = ev else {
            panic!("expected AskRequested");
        };
        assert_eq!(id, "i1");
        assert_eq!(questions.len(), 1);
        // daemon snake_case `allow_custom` → 壳 camelCase 形状。
        assert_eq!(questions[0].allow_custom, true);
        assert_eq!(questions[0].options, vec!["是", "否"]);
    }

    #[test]
    fn parses_plan_review_requested_with_nullable_todo() {
        let ev = parse_interaction_event(&json!({
            "type": "plan_review_requested",
            "interaction_id": "p1",
            "turn_id": "t2",
            "plan_content": "1. 修 bug",
            "review_type": "todo_activation",
            "todo_items": [
                { "id": "td1", "title": "修 bug", "description": "", "complexity": "small" }
            ],
        }))
        .expect("parse");
        let InteractionEvent::PlanRequested {
            id,
            plan_content,
            review_type,
            todo_items,
        } = ev
        else {
            panic!("expected PlanRequested");
        };
        assert_eq!(id, "p1");
        assert_eq!(plan_content, "1. 修 bug");
        assert_eq!(review_type, "todo_activation");
        assert_eq!(todo_items.len(), 1);
        assert_eq!(todo_items[0].complexity, "small");
        // todo_items 可为 null → 空 Vec。
        let null_ev = parse_interaction_event(&json!({
            "type": "plan_review_requested",
            "interaction_id": "p2",
            "turn_id": "t3",
            "plan_content": "x",
            "review_type": "",
            "todo_items": null,
        }))
        .expect("parse");
        let InteractionEvent::PlanRequested { todo_items, .. } = null_ev else {
            panic!("expected PlanRequested");
        };
        assert!(todo_items.is_empty());
    }

    #[test]
    fn ghost_cleanup_only_for_rejection_codes() {
        let ev = parse_interaction_event(&json!({
            "type": "operation_failed",
            "occurrence_id": "o1",
            "scope": "session",
            "error": { "code": "ask_rejected", "error_id": "e1", "message": "rejected" },
        }))
        .expect("parse");
        assert!(matches!(ev, InteractionEvent::GhostCleanup));
        // 其他错误码不触发自愈。
        let ev2 = parse_interaction_event(&json!({
            "type": "operation_failed",
            "occurrence_id": "o2",
            "scope": "session",
            "error": { "code": "tool_failed", "error_id": "e2", "message": "boom" },
        }));
        assert!(ev2.is_none());
    }

    #[test]
    fn parses_tool_permission_requested_and_finished() {
        let ev = parse_tool_permission_event(&json!({
            "type": "tool_permission_requested",
            "tool_call_id": "tc1",
            "turn_id": "t9",
            "round_num": 1,
            "tool_name": "shell",
            "reason": "run cmd",
            "paths": ["C:/x"],
            "category": "exec",
            "level": 2,
            "risk": "high",
            "consequence": "执行命令",
        }))
        .expect("parse");
        let ToolPermissionEvent::Requested {
            tool_call_id,
            paths,
            level,
            risk,
            ..
        } = ev
        else {
            panic!("expected Requested");
        };
        assert_eq!(tool_call_id, "tc1");
        assert_eq!(paths, vec!["C:/x"]);
        assert_eq!(level, 2);
        assert_eq!(risk, "high");

        let done = parse_tool_permission_event(&json!({
            "type": "tool_finished",
            "tool_call_id": "tc1",
            "turn_id": "t9",
            "round_num": 1,
            "result": { "exit_code": 0 },
        }))
        .expect("parse");
        assert!(matches!(done, ToolPermissionEvent::Resolved { .. }));
    }

    #[test]
    fn machine_permission_priority_and_resolution() {
        let mut m = InteractionMachine::default();
        // permission 请求先到。
        m.apply_tool(
            parse_tool_permission_event(&json!({
                "type": "tool_permission_requested",
                "tool_call_id": "tc1",
                "turn_id": "t9",
                "round_num": 1,
                "tool_name": "shell",
                "reason": "run",
                "paths": [],
                "category": "exec",
                "level": 2,
                "risk": "high",
                "consequence": "执行",
            }))
            .expect("parse"),
        );
        // ask 后到——permission 仍优先（对齐 Web pendingInteractions[0]）。
        m.apply(parse_interaction_event(&ask_requested_event("i1", "t1")).expect("parse"));
        let snap = m.snapshot("seed1");
        assert_eq!(snap.kind, "permission");
        assert_eq!(snap.id, "tc1");
        assert_eq!(snap.tool_name, "shell");
        assert_eq!(snap.seed, "seed1");

        // tool_finished 释放 permission → ask 上位。
        m.apply_tool(ToolPermissionEvent::Resolved {
            tool_call_id: "tc1".into(),
        });
        let snap = m.snapshot("seed1");
        assert_eq!(snap.kind, "ask");
        assert_eq!(snap.id, "i1");
        assert_eq!(snap.questions.len(), 1);

        // interaction_resolved 清除 ask → 空（kind=""，XAML 判空关闭）。
        m.apply(InteractionEvent::AskResolved { id: "i1".into() });
        let snap = m.snapshot("seed1");
        assert!(snap.kind.is_empty());
        assert!(snap.id.is_empty());
    }

    #[test]
    fn machine_plan_flow_and_ghost_cleanup() {
        let mut m = InteractionMachine::default();
        m.apply(
            parse_interaction_event(&json!({
                "type": "plan_review_requested",
                "interaction_id": "p1",
                "turn_id": "t2",
                "plan_content": "plan",
                "review_type": "todo_activation",
                "todo_items": null,
            }))
            .expect("parse"),
        );
        let snap = m.snapshot("s");
        assert_eq!(snap.kind, "plan");
        assert_eq!(snap.plan_content, "plan");

        // 幽灵自愈：operation_failed 清除挂起面板。
        m.apply(InteractionEvent::GhostCleanup);
        assert!(m.snapshot("s").kind.is_empty());

        // 不匹配 id 的 resolved 不清除（对齐 Web reducer 的 id 匹配）。
        m.apply(
            parse_interaction_event(&json!({
                "type": "plan_review_requested",
                "interaction_id": "p2",
                "turn_id": "t4",
                "plan_content": "p2",
                "review_type": "",
            }))
            .expect("parse"),
        );
        m.apply(InteractionEvent::PlanResolved { id: "p1".into() });
        assert_eq!(m.snapshot("s").kind, "plan");
        m.apply(InteractionEvent::PlanResolved { id: "p2".into() });
        assert!(m.snapshot("s").kind.is_empty());
    }

    #[test]
    fn apply_interaction_event_is_idempotent_for_replay() {
        let core = test_core();
        core.set_active_seed("seed1");
        let ev = ask_requested_event("i1", "t1");
        core.apply_interaction_event("seed1", parse_interaction_event(&ev).expect("parse"));
        let rev1 = core.interaction_rev.load(Ordering::Relaxed);
        assert_eq!(core.interaction_snapshot().0.kind, "ask");
        // SSE 重放同一事件：快照无变化 → rev 不递增（幂等）。
        core.apply_interaction_event("seed1", parse_interaction_event(&ev).expect("parse"));
        let rev2 = core.interaction_rev.load(Ordering::Relaxed);
        assert_eq!(rev1, rev2);
    }

    #[test]
    fn interaction_cache_follows_active_seed() {
        let core = test_core();
        // 会话 A 请求 ask；active 尚未设置 → 缓存为空（后台不打扰当前显示）。
        core.apply_interaction_event(
            "seedA",
            parse_interaction_event(&ask_requested_event("iA", "tA")).expect("parse"),
        );
        assert!(core.interaction_snapshot().0.kind.is_empty());
        // 切到 A → 缓存投影 A 的交互。
        core.set_active_seed("seedA");
        assert_eq!(core.interaction_snapshot().0.kind, "ask");
        assert_eq!(core.interaction_snapshot().0.id, "iA");
        // 会话 B 的交互事件不覆盖当前显示（A 保持）。
        core.apply_interaction_event(
            "seedB",
            parse_interaction_event(&ask_requested_event("iB", "tB")).expect("parse"),
        );
        assert_eq!(core.interaction_snapshot().0.id, "iA");
        // 切到 B → B 的交互上位；切回 A → A 恢复（状态机按 seed 保留）。
        core.set_active_seed("seedB");
        assert_eq!(core.interaction_snapshot().0.id, "iB");
        core.set_active_seed("seedA");
        assert_eq!(core.interaction_snapshot().0.id, "iA");
    }

    // ── native composer state ─────────────────────────────────────────

    #[test]
    fn parses_conversation_activity_events() {
        use ConversationActivityEvent as E;
        // turn_started → Started。
        let ev = parse_conversation_activity_event(&json!({
            "type": "turn_started", "turn_id": "t1", "user_text": "hi"
        }))
        .expect("parse");
        assert!(matches!(ev, E::Started));
        // 终态 → Ended。
        for ty in ["turn_completed", "turn_failed", "conversation_cancelled"] {
            let ev = parse_conversation_activity_event(&json!({ "type": ty, "turn_id": "t1" }))
                .expect("parse");
            assert!(matches!(ev, E::Ended), "{ty}");
        }
        // usage_updated → Usage（snake_case usage 字段）。
        let ev = parse_conversation_activity_event(&json!({
            "type": "usage_updated",
            "turn_id": "t1", "round_num": 1,
            "usage": { "prompt_tokens": 1234, "total_tokens": 2000 },
            "context_limit": 200000,
            "model": "gpt-5",
        }))
        .expect("parse");
        let E::Usage {
            prompt_tokens,
            context_limit,
            model,
        } = ev
        else {
            panic!("expected Usage");
        };
        assert_eq!(prompt_tokens, 1234);
        assert_eq!(context_limit, 200000);
        assert_eq!(model, "gpt-5");
        // 流式事件 → Touched。
        for ty in [
            "round_delta",
            "block_checkpoint",
            "round_completed",
            "provider_retrying",
            "provider_tool_status",
        ] {
            let ev = parse_conversation_activity_event(&json!({ "type": ty, "turn_id": "t1" }))
                .expect("parse");
            assert!(matches!(ev, E::Touched), "{ty}");
        }
        // compact/未知 → None（不视为活动）。
        assert!(parse_conversation_activity_event(&json!({ "type": "compact_started" })).is_none());
        assert!(parse_conversation_activity_event(&json!({ "type": "bogus" })).is_none());
    }

    #[test]
    fn composer_streaming_stall_detection() {
        let mut a = ComposerActivity::default();
        // 无活动 turn → 非流式。
        assert!(!a.is_streaming(1_000));
        // turn_started → 流式（时间戳未知保守 true，随后精确）。
        a.apply(ConversationActivityEvent::Started, 1_000);
        assert!(a.is_streaming(1_000));
        // 4 分钟内 → 流式。
        assert!(a.is_streaming(1_000 + COMPOSER_STALL_TIMEOUT_MS - 1));
        // 超时 → 非流式（卡死）。
        assert!(!a.is_streaming(1_000 + COMPOSER_STALL_TIMEOUT_MS));
        // 活动事件刷新时间戳 → 恢复流式。
        a.apply(ConversationActivityEvent::Touched, 10_000);
        assert!(a.is_streaming(10_001));
        // 终态 → 非流式。
        a.apply(ConversationActivityEvent::Ended, 11_000);
        assert!(!a.is_streaming(11_000));
    }

    #[test]
    fn composer_snapshot_uses_typed_activity_and_local_state() {
        let core = test_core();
        core.set_active_seed("seed1");
        // Canonical conversation events drive activity and usage.
        let now = unix_ms();
        {
            let mut map = core.composer_activity.lock().unwrap();
            let a = map.entry("seed1".into()).or_default();
            a.apply(ConversationActivityEvent::Started, now);
            a.apply(
                ConversationActivityEvent::Usage {
                    prompt_tokens: 42,
                    context_limit: 200_000,
                    model: "gpt-5".into(),
                },
                now,
            );
        }
        let (s, _) = core.composer_snapshot();
        assert!(s.is_streaming);
        assert_eq!(s.model, "gpt-5");
        assert_eq!(s.context_tokens, 42);
        assert_eq!(s.context_limit, 200_000);
        assert_eq!(s.seed, "seed1");
        // UI-local state owns mode and send feedback; config owns permission.
        assert_eq!(s.mode, "plan");
        assert_eq!(s.permission_level, 1);
        assert_eq!(s.queue_count, 0);
        assert_eq!(s.send_ack, 0);
        assert_eq!(s.submit_error, "");
        *core.composer_mode.lock().unwrap() = "code".into();
        core.composer_feedback.lock().unwrap().send_ack = 7;
        let (s2, _) = core.composer_snapshot();
        assert_eq!(s2.mode, "code");
        assert_eq!(s2.send_ack, 7);
        // Pending gates come from the typed interaction machine.
        assert!(!s.has_pending_gate);
        core.apply_interaction_event(
            "seed1",
            parse_interaction_event(&ask_requested_event("i1", "t1")).expect("parse"),
        );
        assert!(core.composer_snapshot().0.has_pending_gate);
    }

    #[test]
    fn timeline_stall_triggers_only_after_threshold() {
        let core = test_core();
        let now = Instant::now();
        // 首次出现非 Open 状态：开始计时，不触发。
        *core.timeline_status.lock().unwrap() = Some(reconnecting());
        assert!(!core.compute_stall(now));
        assert!(core.timeline_stall_since.lock().unwrap().is_some());
        // 未到阈值：仍不触发。
        *core.timeline_stall_since.lock().unwrap() =
            Some(now - STALL_THRESHOLD + Duration::from_secs(1));
        assert!(!core.compute_stall(now));
        // 超过阈值：触发。
        *core.timeline_stall_since.lock().unwrap() =
            Some(now - STALL_THRESHOLD - Duration::from_secs(1));
        assert!(core.compute_stall(now));
    }

    /// 快照缓存回归：seed 以 body 顶层为准（refresh/并发路径 last_timeline_seed
    /// 陈旧时不错标），且缓存解包 `snapshot` 子对象（timeline_turns 可解析）。
    /// 此前的两个根因——last_timeline_seed 错标 → 无限 deferred；缓存完整
    /// body → 解析恒空 → restore 空历史——曾导致 ChatView 历史永不恢复。
    #[test]
    fn timeline_snapshot_caches_authoritative_seed_and_inner() {
        let core = test_core();
        // 模拟陈旧标记（spawn_timeline_refresh 路径不更新 last_timeline_seed）。
        *core.last_timeline_seed.lock().unwrap() = "stale-seed".to_string();
        let body = serde_json::json!({
            "schema": "deepx.Ringing",
            "version": 1,
            "server_epoch": "e1",
            "seed": "s1",
            "snapshot": {
                "watermark": 7,
                "turns": [
                    {"turn_id":"t1","created_seq":1,"user_text":"hi","sealed":true,"state":"completed","rounds":[]},
                    {"turn_id":"t2","created_seq":2,"user_text":"again","sealed":false,"state":"running","rounds":[]}
                ]
            },
            "has_more": false,
            "total_turns": 2
        });
        core.cache_timeline_snapshot(serde_json::from_value(body).expect("typed page"));
        // seed 标记取 body 权威值，不受 last_timeline_seed 陈旧影响。
        let (cached_seed, cached) = core.chat_timeline.lock().unwrap().clone().expect("cached");
        assert_eq!(cached_seed, "s1");
        // 解包 snapshot 子对象：turns 可直接解析（完整 body 会恒空）。
        assert_eq!(cached.turns.len(), 2, "must cache typed snapshot inner");
        let turns = chat_adapter::restored_turns(&cached);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].turn_id, "t1");
        // 连带投影：header_turns / last_turn_ids 以权威 seed 写入。
        assert_eq!(core.header_turns.lock().unwrap().get("s1"), Some(&2));
        assert_eq!(
            core.last_turn_ids
                .lock()
                .unwrap()
                .get("s1")
                .map(String::as_str),
            Some("t2")
        );
        // 消费后缓存清空（consume 语义保持）。
        core.chat_timeline_consume();
        assert!(core.chat_timeline.lock().unwrap().is_none());
    }

    #[test]
    fn open_timeline_status_resets_stall_timer() {
        let core = test_core();
        *core.timeline_stall_since.lock().unwrap() = Some(Instant::now() - Duration::from_secs(60));
        *core.timeline_status.lock().unwrap() = Some(TimelineStatus::Open {
            seed: "s1".into(),
            server_epoch: "e1".into(),
            cursor: 9,
        });
        assert!(!core.compute_stall(Instant::now()));
        assert!(core.timeline_stall_since.lock().unwrap().is_none());
    }

    #[test]
    fn all_channels_stalled_triggers_but_single_open_resets() {
        let core = test_core();
        let now = Instant::now();
        let mut reconnecting_map: HashMap<Channel, ChannelStatus> = HashMap::new();
        for ch in [Channel::Control, Channel::Conversation, Channel::Tool] {
            reconnecting_map.insert(
                ch,
                ChannelStatus::Reconnecting {
                    retry_ms: 1_000,
                    last_cursor: 0,
                },
            );
        }
        *core.channel_status.lock().unwrap() = reconnecting_map;
        // 开始计时，不触发。
        assert!(!core.compute_stall(now));
        assert!(core.channels_stall_since.lock().unwrap().is_some());
        // 超过阈值：触发。
        *core.channels_stall_since.lock().unwrap() =
            Some(now - STALL_THRESHOLD - Duration::from_secs(1));
        assert!(core.compute_stall(now));

        // 任一通道 open → 重置计时。
        let core2 = test_core();
        *core2.channels_stall_since.lock().unwrap() =
            Some(now - STALL_THRESHOLD - Duration::from_secs(1));
        core2.channel_status.lock().unwrap().insert(
            Channel::Conversation,
            ChannelStatus::Open {
                server_epoch: "e1".into(),
                cursor: 0,
            },
        );
        assert!(!core2.compute_stall(now));
        assert!(core2.channels_stall_since.lock().unwrap().is_none());
    }

    #[test]
    fn untracked_or_null_status_never_stalls() {
        // 无 client（状态为 null / 空）：不触发、不残留计时。
        let core = test_core();
        assert!(!core.compute_stall(Instant::now()));
        assert!(core.channels_stall_since.lock().unwrap().is_none());
        assert!(core.timeline_stall_since.lock().unwrap().is_none());
    }

    #[test]
    fn rebuild_cooldown_blocks_repeated_rebuilds() {
        let core = test_core();
        *core.last_rebuild_at.lock().unwrap() = Instant::now();
        // 冷却期内即使 stall 也不触发 rebuild（check 的 cooldown 分支）。
        *core.timeline_status.lock().unwrap() = Some(reconnecting());
        *core.timeline_stall_since.lock().unwrap() =
            Some(Instant::now() - STALL_THRESHOLD - Duration::from_secs(1));
        core.check_daemon_health();
        // rebuild_client 未执行（冷却）：rebuilding 保持 false。
        assert!(!core.rebuilding.load(Ordering::Relaxed));
    }

    #[test]
    fn rebuild_cooldown_backs_off_after_failures() {
        // 无失败：60s；每失败翻倍，封顶 960s。
        assert_eq!(rebuild_cooldown_for(0), Duration::from_secs(60));
        assert_eq!(rebuild_cooldown_for(1), Duration::from_secs(120));
        assert_eq!(rebuild_cooldown_for(2), Duration::from_secs(240));
        assert_eq!(rebuild_cooldown_for(3), Duration::from_secs(480));
        assert_eq!(rebuild_cooldown_for(4), Duration::from_secs(960));
        // 超过封顶不再增长（防溢出/无限退避）。
        assert_eq!(rebuild_cooldown_for(5), Duration::from_secs(960));
        assert_eq!(rebuild_cooldown_for(u32::MAX), Duration::from_secs(960));
        // 自动重连冷却同样退避（5s → 10/20/40/80/160/320 封顶）。
        assert_eq!(auto_reconnect_cooldown_for(0), Duration::from_secs(5));
        assert_eq!(auto_reconnect_cooldown_for(6), Duration::from_secs(320));
        assert_eq!(auto_reconnect_cooldown_for(99), Duration::from_secs(320));
    }
}

/// Minimal file logger (GUI subsystem has no console).
fn log_diag(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::var("DEEPX_WINUI_LOG").unwrap_or_else(|_| ".deepx-winui.log".into()))
    {
        let _ = writeln!(f, "{}", msg);
    }
}

/// Open a path/URL with the system shell (best effort).
fn open_external(target: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("cmd")
            .args(["/c", "start", "", target])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(target).spawn();
        Ok(())
    }
}

/// Show the native file/folder picker (Win32 `IFileOpenDialog`).
///
/// **Must be called on the STA UI thread** — this is enforced by the only
/// call site (`Bridge::handle_message`). Mirrors Electron
/// `dialog.showOpenDialog` semantics consumed by the renderer:
///   - `directory` -> `FOS_PICKFOLDERS` (folder picker)
///   - `multiple`  -> `FOS_ALLOWMULTISELECT` (result becomes a JSON array)
///   - `image_filter` -> picture file types filter
///   - cancel      -> `null`; single -> string; multiple -> array of strings
fn show_open_dialog(
    directory: bool,
    multiple: bool,
    image_filter: bool,
    title: Option<&str>,
) -> Result<Value, String> {
    use windows::Win32::{
        CLSCTX_ALL, COMDLG_FILTERSPEC, CoCreateInstance, ERROR_CANCELLED, FOS_ALLOWMULTISELECT,
        FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog,
    };
    use windows::core::{HSTRING, w};

    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_ALL as u32)
            .map_err(|e| format!("CoCreateInstance(FileOpenDialog): {e}"))?;

        let mut options = FOS_FORCEFILESYSTEM | FOS_FILEMUSTEXIST;
        if directory {
            options |= FOS_PICKFOLDERS;
        }
        if multiple {
            options |= FOS_ALLOWMULTISELECT;
        }
        dialog
            .SetOptions(options)
            .ok()
            .map_err(|e| format!("IFileDialog::SetOptions: {e}"))?;

        if let Some(title) = title.filter(|t| !t.is_empty()) {
            let title = HSTRING::from(title);
            dialog
                .SetTitle(&title)
                .ok()
                .map_err(|e| format!("IFileDialog::SetTitle: {e}"))?;
        }

        if image_filter {
            let filters = [
                COMDLG_FILTERSPEC {
                    pszName: w!("Images"),
                    pszSpec: w!("*.png;*.jpg;*.jpeg;*.gif;*.webp;*.bmp"),
                },
                COMDLG_FILTERSPEC {
                    pszName: w!("All files"),
                    pszSpec: w!("*.*"),
                },
            ];
            dialog
                .SetFileTypes(filters.len() as u32, filters.as_ptr())
                .ok()
                .map_err(|e| format!("IFileDialog::SetFileTypes: {e}"))?;
        }

        // Show() is modal; ERROR_CANCELLED (user pressed Cancel / Esc) is
        // mapped to `null`, matching the preload API's cancel semantics.
        // (0.100 HRESULT has no from_win32 helper; build the code inline.)
        let hr = dialog.Show(None);
        if hr.is_err() && hr.0 == ((ERROR_CANCELLED as u32 | 0x8007_0000) as i32) {
            return Ok(json!(null));
        }
        hr.ok().map_err(|e| format!("IFileDialog::Show: {e}"))?;

        if multiple {
            let items = dialog
                .GetResults()
                .map_err(|e| format!("IFileOpenDialog::GetResults: {e}"))?;
            let count = items
                .GetCount()
                .map_err(|e| format!("IShellItemArray::GetCount: {e}"))?;
            let mut paths = Vec::with_capacity(count as usize);
            for i in 0..count {
                let item = items
                    .GetItemAt(i)
                    .map_err(|e| format!("IShellItemArray::GetItemAt({i}): {e}"))?;
                paths.push(shell_item_path(&item)?);
            }
            Ok(json!(paths))
        } else {
            let item = dialog
                .GetResult()
                .map_err(|e| format!("IFileDialog::GetResult: {e}"))?;
            Ok(json!(shell_item_path(&item)?))
        }
    }
}

/// Resolve an `IShellItem` to its filesystem path (`SIGDN_FILESYSPATH`).
/// The returned `PWSTR` is CoTaskMem-allocated and freed here.
fn shell_item_path(item: &windows::Win32::IShellItem) -> Result<String, String> {
    use windows::Win32::{CoTaskMemFree, SIGDN_FILESYSPATH};
    let pw = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }
        .map_err(|e| format!("IShellItem::GetDisplayName(SIGDN_FILESYSPATH): {e}"))?;
    let path =
        unsafe { pw.to_string() }.map_err(|e| format!("selected path is not valid UTF-16: {e}"))?;
    unsafe { CoTaskMemFree(pw.0 as _) };
    Ok(path)
}
