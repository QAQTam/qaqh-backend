//! 中立领域命令（DomainCommand）。
//!
//! legacy ingress（Ui2Agent → DomainCommand）与 Ringing ingress
//! （RingingCommandEnvelope → DomainCommand）分别校验并构造本类型；
//! Agent core 只消费本类型，不感知来源协议。

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::channel::RingingChannel;
use crate::event::ContentRef;

/// 用户消息中的图片附件（multimodal）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ImageBlock {
    /// MIME type（如 "image/png"）。
    pub mime_type: String,
    /// Base64 编码的图像数据（不含 data URI 前缀）。
    pub data: String,
}

/// ask_user 表单中的单个答案。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AskAnswer {
    pub question_id: String,
    pub answer: String,
}

/// 会话工作模式（legacy `SetMode` 语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ConversationMode {
    Normal,
    Plan,
    Code,
}

/// Control 频道命令。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum ControlCommand {
    /// 创建新会话。`close_current = true` 表示先结束当前会话
    /// （合并 legacy `CreateSession` 与 `NewSession` 语义，见决策记录 Q7）。
    SessionCreate {
        #[serde(default)]
        close_current: bool,
    },
    /// 恢复已保存会话。accepted 后由三个频道分别完成 snapshot/cursor 恢复。
    SessionResume { seed: String },
    /// 关闭指定会话。
    SessionClose { seed: String },
    /// 优雅关闭整个 agent 进程。
    SessionShutdown,
    /// 重载配置（provider、model、permission 等）。
    AgentReloadConfig,
    /// 提交 ask_user 交互的答案（对应 InteractionRequested）。
    InteractionAskRespond {
        interaction_id: String,
        answers: Vec<AskAnswer>,
    },
    /// 关闭 ask_user 交互而不作答（中止被挂起的回合）。
    InteractionAskDismiss { interaction_id: String },
    /// 提交 plan review 决策（对应 PlanReviewRequested）。
    PlanReviewRespond {
        interaction_id: String,
        approved: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(default)]
        autonomous: bool,
    },
    /// 显式激活 skill（等价 $skill-name 提及）。
    SkillsActivate { name: String },
    /// 从上下文卸载显式激活的 skill。
    SkillsRelease { name: String },
    /// 从磁盘重载 skill 目录并刷新目录系统消息。
    SkillsReload,
    /// 带 operation id/revision 保护的 skill UI 操作。
    /// revision 本身统一位于 Ringing command envelope。
    SkillsOperation {
        operation_id: String,
        action: String,
        name: String,
    },
}

/// Conversation 频道命令。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum ConversationCommand {
    /// 发送用户消息。accepted 仅代表输入已被 session actor 接收；
    /// `TurnStarted` 是开始执行的权威事件。
    ConversationSendMessage {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageBlock>,
        /// Electron main 上传后的会话附件引用；命令中不允许出现本地路径。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attachments: Option<Vec<ContentRef>>,
    },
    /// 取消当前回合（停止 gate 流式输出与工具执行）。
    ConversationCancel {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
    /// 移除指定回合。
    ConversationUndoTurn { turn_id: String },
    /// 触发上下文压缩。accepted 不代表成功；`CompactFinished` 才是终态。
    ConversationCompact {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
    /// 加载更早（已归档）的回合（只读查询，结果经 HTTP 直接返回）。
    ConversationLoadMore {
        before_turn_id: String,
        #[serde(default = "default_load_count")]
        count: u32,
    },
    /// 设置会话工作模式。
    ConversationSetMode { mode: ConversationMode },
}

fn default_load_count() -> u32 {
    20
}

/// Tool 频道命令。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum ToolCommand {
    /// 前端主动触发工具执行（UI 按钮/内联操作）。
    ToolInvoke {
        tool_call_id: String,
        name: String,
        action: String,
        #[ts(type = "any")]
        args: serde_json::Value,
    },
    /// 权限请求响应。必须携带对应 interaction/tool_call 的 id；
    /// revision-safe 语义统一由 Ringing command envelope 承载。
    ToolPermissionRespond {
        tool_call_id: String,
        approved: bool,
        #[serde(default)]
        trust_folder: bool,
    },
}

/// 统一领域命令入口。`channel()` 决定命令进入哪个 actor/router。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "channel", rename_all = "snake_case")]
#[ts(export)]
pub enum DomainCommand {
    Control(ControlCommand),
    Conversation(ConversationCommand),
    Tool(ToolCommand),
}

impl DomainCommand {
    pub fn channel(&self) -> RingingChannel {
        match self {
            DomainCommand::Control(_) => RingingChannel::Control,
            DomainCommand::Conversation(_) => RingingChannel::Conversation,
            DomainCommand::Tool(_) => RingingChannel::Tool,
        }
    }
}
