//! RingingV2 命令共用的业务值对象。

use serde::{Deserialize, Serialize};
#[cfg(feature = "ts")]
use ts_rs::TS;

use crate::fact::InterAgentDelivery;

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

/// widget-directive §4：fragment 的身份引用（marker payload 的 path/title 原样）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct WidgetRef {
    /// daemon 侧绝对路径（`.qaqh/widgets/<session>/…html`）。
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// widget-directive §4：widget→模型的结构化状态回流。user-send 一等字段，
/// daemon 把它作为回合内存在记忆 reminder 注入 provider 上下文——不落时间线、
/// 不拼用户可见文本；缺失/版本不符只少上下文，不失败回合。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct WidgetContext {
    pub session_id: String,
    pub widget: WidgetRef,
    /// 用户确认 follow-up 时 iframe 的**当下最新** modelContent 快照（任意 JSON）。
    pub model_content: serde_json::Value,
}
