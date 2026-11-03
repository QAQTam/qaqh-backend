use serde::{Deserialize, Serialize};
#[cfg(feature = "ts")]
use ts_rs::TS;

/// Internal classification used by domain projections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum RingingChannel {
    /// 会话/agent 生命周期、interaction、skills、系统通知与失败终态。
    Control,
    /// 消息回合、provider 流式输出、compact 与 usage。
    Conversation,
    /// 工具执行生命周期、进度、权限与审计。
    Tool,
}

impl RingingChannel {
    /// Stable label for legacy domain projection records.
    pub fn as_str(self) -> &'static str {
        match self {
            RingingChannel::Control => "control",
            RingingChannel::Conversation => "conversation",
            RingingChannel::Tool => "tool",
        }
    }
}

impl std::fmt::Display for RingingChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
