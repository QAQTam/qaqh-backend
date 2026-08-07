//! 协议事件模型 —— 形状对齐 `deepx-domain` `ConversationEvent` 子集。
//!
//! 对应上游真实类型（`F:\DeepX\crates\deepx-domain\src\event.rs`）：
//! ```text
//! RoundDelta { turn_id, round_num, kind: RoundDeltaKind, delta: String }
//! BlockCheckpoint { turn_id, round_num, kind: RoundDeltaKind, text: String, char_count: u32 }
//! RoundCompleted { turn_id, round_num, thinking: Option<String>, answer: Option<String>, output_ref: Option<ContentRef>, is_final: bool }
//! ```
//!
//! 三语义（决定前端策略）：
//! - `RoundDelta`：**追加增量**（reliable，前端拼接）
//! - `BlockCheckpoint`：**完整值覆盖**（replaceable，乱序/丢 delta 自愈，治 D1）
//! - `RoundCompleted`：**权威终态**（前端以 thinking/answer 全量重建该 round）
//!
//! 本模块只取渲染所需子集（`output_ref` 外置正文的加载路径见
//! [`crate::round_renderer`] 的 `output_ref` 处理），字段名与序列化
//! tag 与上游一致，便于后续直接换用真实反序列化。

use serde::{Deserialize, Serialize};

/// 流式块种类（对齐 `deepx-domain::RoundDeltaKind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundDeltaKind {
    /// 模型思考流（UI：可折叠）。
    Thinking,
    /// 工具调用流（UI：工具卡）。
    ToolCalling,
    /// 答案正文流（UI：markdown 活尾）。
    Answering,
}

/// 会话对话事件（渲染所需子集，tag 与上游一致）。
///
/// 未知变体（`provider_retrying` / `usage_updated` / `compact_*` /
/// `conversation_cancelled` 等）由 `Unknown` 兜底——真实 wire JSON 可
/// 直接 `serde_json::from_value` 反序列化为本类型，零映射胶水。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConversationEvent {
    /// 新回合开始。
    TurnStarted { turn_id: String, user_text: String },
    /// 回合完成（终态；流式事件至此停止）。
    TurnCompleted { turn_id: String },
    /// 回合失败（provider 最终失败；新领域事件，UI 显示失败态）。
    TurnFailed {
        turn_id: String,
        /// DomainError 原始形状（渲染仅用 turn_id；message 提取由应用层）。
        error: serde_json::Value,
    },
    /// 流式增量（追加语义）。
    RoundDelta {
        turn_id: String,
        round_num: u32,
        kind: RoundDeltaKind,
        delta: String,
    },
    /// 流式块周期完整值（覆盖语义，自愈）。
    BlockCheckpoint {
        turn_id: String,
        round_num: u32,
        kind: RoundDeltaKind,
        text: String,
    },
    /// 一轮 API 调用完成的权威终态。
    RoundCompleted {
        turn_id: String,
        round_num: u32,
        #[serde(default)]
        thinking: Option<String>,
        #[serde(default)]
        answer: Option<String>,
        /// 正文大时外置引用（保留任意形状；内容服务加载路径由应用层承担）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_ref: Option<serde_json::Value>,
        /// true = 本回合最后一个 round。
        #[serde(default)]
        is_final: bool,
    },
    /// 渲染不关心的领域事件（上游新增变体时的兜底；忽略）。
    #[serde(other)]
    Unknown,
}

impl ConversationEvent {
    /// 事件所属 turn（TurnCompleted 的 turn_id 即自身）。
    pub fn turn_id(&self) -> &str {
        match self {
            Self::TurnStarted { turn_id, .. }
            | Self::TurnCompleted { turn_id }
            | Self::TurnFailed { turn_id, .. }
            | Self::RoundDelta { turn_id, .. }
            | Self::BlockCheckpoint { turn_id, .. }
            | Self::RoundCompleted { turn_id, .. } => turn_id,
            Self::Unknown => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 协议形状与上游一致（tag = "type"，snake_case kind）——保证后续
    /// 换用真实反序列化时零改动。
    #[test]
    fn wire_shape_matches_upstream() {
        let ev = ConversationEvent::RoundDelta {
            turn_id: "abc12345".into(),
            round_num: 0,
            kind: RoundDeltaKind::Answering,
            delta: "hel".into(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(
            json.contains("\"type\":\"round_delta\"") && json.contains("\"kind\":\"answering\""),
            "wire shape: {json}"
        );
    }
}
