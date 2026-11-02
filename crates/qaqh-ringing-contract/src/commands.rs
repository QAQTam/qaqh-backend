//! 会话命令目录:一个类型化枚举 + 一个信封,替代三套命令机制。
//!
//! 目标由 URL 表达(`POST /sessions/{session_id}/commands`);信封不再携带
//! channel/session_id/expected_revision。driver 席位 CAS 与 client 身份保留。

use serde::{Deserialize, Serialize};
#[cfg(feature = "ts")]
use ts_rs::TS;

use crate::fact::{
    ContentRef, InputId, InputPurpose, InteractionId, InterAgentDelivery, TurnId, TurnMode,
};

/// ask_user 表单中的单个答案。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct AskAnswer {
    pub question_id: String,
    pub answer: String,
}

/// 用户消息中的图片附件(multimodal)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ImageBlock {
    /// MIME type(如 "image/png")。
    pub mime_type: String,
    /// Base64 编码的图像数据(不含 data URI 前缀)。
    pub data: String,
}

/// 代理间通信元数据;命令的 `text` 即 canonical 正文。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct InterAgentEnvelope {
    pub message_id: String,
    pub root_session_id: String,
    pub author: String,
    pub recipient: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub other_recipients: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<String>,
    #[serde(default)]
    pub delivery: InterAgentDelivery,
    pub created_at_ms: i64,
}

/// 子代理终态种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "snake_case")]
pub enum SubagentTerminalKind {
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

/// 随子代理结果注入携带的结构化终态通知。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct SubagentTerminalNotification {
    pub child_session_id: String,
    pub parent_call_id: String,
    pub terminal: SubagentTerminalKind,
}

/// 统一交互应答:permission、ask、plan 按同一 interaction_id 回答,
/// 答案按交互类型区分。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum InteractionAnswer {
    Permission {
        approved: bool,
        trust_folder: bool,
    },
    PlanReview {
        approved: bool,
        message: Option<String>,
        autonomous: bool,
    },
    Ask {
        answers: Vec<AskAnswer>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct SessionCommandEnvelope {
    pub command_id: String,
    pub client_instance_id: String,
    pub client_session_id: String,
    /// 调用方认为持有的 driver 席位 epoch;不匹配拒绝为 stale_driver_epoch。
    /// None 跳过席位守卫(交互应答不受席位约束)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver_epoch: Option<u64>,
    #[serde(flatten)]
    pub payload: SessionCommand,
}

/// 会话操作目录。发送输入、取消、交互应答、配置与 driver 席位管理。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[allow(clippy::large_enum_variant)]
pub enum SessionCommand {
    /// 发送输入。accepted 仅代表已被 owner 接收;TurnStarted 才是执行权威。
    /// inter-agent 业务语义(QueueOnly/Steer/Interject、inter_agent 元数据、
    /// 子代理结果注入)完整保留,不在内部转换中丢弃。
    SendInput {
        /// 客户端生成的稳定输入身份(幂等/去重键)。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_id: Option<InputId>,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageBlock>,
        /// Electron main 上传后的附件引用;命令中不允许出现本地路径。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<ContentRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
        /// 子代理如何应用本消息:触发回合 / 仅入队 / 运行中安全点合并。
        #[serde(default)]
        input_purpose: InputPurpose,
        /// 系统级注入:以 system 角色进入 transcript,跳过用户输入专属处理。
        #[serde(default)]
        as_system: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        inter_agent: Option<InterAgentEnvelope>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subagent_terminal: Option<SubagentTerminalNotification>,
    },
    /// 取消当前回合(可指定 turn_id 校验)。
    CancelTurn {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<TurnId>,
    },
    /// 移除指定回合。
    UndoTurn { turn_id: TurnId },
    /// 触发上下文压缩;CompactFinished 才是终态。
    Compact {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<TurnId>,
    },
    SetMode { mode: TurnMode },
    RespondInteraction {
        interaction_id: InteractionId,
        answer: InteractionAnswer,
    },
    /// 关闭 ask 交互而不作答(中止被挂起的回合)。
    DismissInteraction { interaction_id: InteractionId },
    SetToolMode {
        tool_mode: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        custom_tools: Vec<String>,
    },
    SkillsActivate { name: String },
    SkillsReload,
    SkillsOperation {
        operation_id: String,
        action: String,
        name: String,
    },
    /// 为非 actor 线程写入的 workspace 资源补一条变化记录(todo 等)。
    PublishResourceChanged { resource_kind: String },
    /// 优雅关闭本会话 agent。
    Shutdown,
    /// 重载配置(provider、model、permission 等)。
    ReloadConfig,
    /// 认领 driver 席位;client_session_id 由 daemon 从已认证 lease 覆写。
    DriverClaim {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stale_holder: Option<String>,
    },
    /// 释放 driver 席位;expected_epoch 为可选 CAS。
    DriverRelease {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_epoch: Option<u64>,
    },
}
