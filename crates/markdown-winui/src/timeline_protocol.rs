//! Timeline 协议事件模型 —— 形状对齐 `deepx-domain` `timeline.rs`。
//!
//! 对应上游真实类型（`F:\DeepX\crates\deepx-domain\src\timeline.rs`），字段名与
//! 序列化 tag 完全一致，真实 wire JSON 可直接 `serde_json::from_value`
//! 反序列化为本类型（零映射胶水，编译器暴露协议变更）。
//!
//! 与 [`crate::protocol`]（conversation 模型）的关系：本模块是 **transcript
//! 渲染的唯一权威源**（block 级有序）；conversation 模型退守 control/telemetry
//! 平面（Phase 2.5 收窄目标）。

use serde::{Deserialize, Serialize};

/// 流式块种类（对齐 `deepx-domain::TimelineBlockKind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineBlockKind {
    /// 模型推理流（UI：可折叠）。
    Reasoning,
    /// 答案正文（UI：markdown 活尾 / final）。
    Text,
    /// 工具调用（UI：工具卡）。
    Tool,
    /// 系统/服务端通知（UI：通知文本块）。
    Notice,
}

/// 块生命周期（对齐 `deepx-domain::TimelineBlockState`）。
///
/// Markdown 只在 `Sealed` 后做 final 渲染（设计 Invariant 4）；`Open` 期间
/// 走流式轻量渲染（纯文本/活尾）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineBlockState {
    Open,
    Sealed,
}

/// 工具块状态更新（对齐 `deepx-domain::TimelineToolState`；更新不改变块位置）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineToolState {
    Prepared,
    Running,
    Succeeded,
    Failed,
}

/// turn 终态（对齐 `deepx-domain::TimelineTurnState`；与块 seal 相互独立——
/// 取消/失败的 turn 可以有已封存的合法 markdown 块）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineTurnState {
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// 消毒后的失败信息（对齐 `deepx-domain::TimelineFailure`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineFailure {
    pub code: String,
    pub message: String,
}

/// 工具权限数据（对齐 `deepx-domain::TimelineToolPermission`；交互生命周期
/// 留在 control 平面，transcript 只保留展示数据）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineToolPermission {
    pub reason: String,
    pub paths: Vec<String>,
    pub category: String,
    pub level: u8,
    pub risk: String,
    pub consequence: String,
}

/// 一个工具块的不可变身份 + 可变展示状态（对齐 `deepx-domain::TimelineTool`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineTool {
    pub tool_call_id: String,
    pub name: String,
    pub state: TimelineToolState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// 结构化参数原文（工具 producer 提供）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_json: Option<String>,
    /// 保留的工具输出尾部（大输出走 content store 显式引用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// 展示平面 unified diff（文件修改类工具）；绝不进模型投影，供前端
    /// diff 抽屉 / 工具卡消费。旧快照缺省为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub progress: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<TimelineFailure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<TimelineToolPermission>,
}

/// 快照中保存的完整展示块（对齐 `deepx-domain::TimelineBlock`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineBlock {
    pub block_id: String,
    /// round 内稳定序；更新永不改变它。
    pub block_order: u32,
    pub kind: TimelineBlockKind,
    pub state: TimelineBlockState,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<TimelineTool>,
}

/// 一个模型轮次（对齐 `deepx-domain::TimelineRound`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineRound {
    pub round_num: u32,
    pub sealed: bool,
    pub is_final: bool,
    pub blocks: Vec<TimelineBlock>,
}

/// 一个 transcript turn（对齐 `deepx-domain::TimelineTurn`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineTurn {
    pub turn_id: String,
    /// TurnOpened entry 的 seq —— 跨快照权威时间序；`0` = 旧数据（退化
    /// turn_id 数值序）。
    #[serde(default)]
    pub created_seq: u64,
    pub user_text: String,
    pub sealed: bool,
    pub state: TimelineTurnState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<TimelineFailure>,
    pub rounds: Vec<TimelineRound>,
}

/// 权威恢复态（对齐 `deepx-domain::TimelineSnapshot`；不是事件数组）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineSnapshot {
    /// turns 中包含的最大 timeline seq。
    pub watermark: u64,
    pub turns: Vec<TimelineTurn>,
}

/// 有序 transcript 的一次变更（对齐 `deepx-domain::TimelineEvent`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TimelineEvent {
    TurnOpened {
        user_text: String,
    },
    BlockOpened {
        block: TimelineBlock,
    },
    /// `fragment_seq` 在单个 text/reasoning 块内单调。
    TextDelta {
        block_id: String,
        fragment_seq: u64,
        delta: String,
    },
    /// 周期**完整值**（replaceable 覆盖；自愈丢失/乱序 delta，fragment 计数
    /// 不动，后续 delta 继续校验）。
    BlockCheckpoint {
        block_id: String,
        text: String,
    },
    ToolUpdated {
        block_id: String,
        tool: TimelineTool,
    },
    /// 工具输出增量，追加到当前 progress 缓冲（由单写 reducer 应用）。
    ToolProgress {
        block_id: String,
        chunk: String,
    },
    BlockSealed {
        block_id: String,
    },
    RoundSealed {
        is_final: bool,
    },
    TurnSealed {
        state: TimelineTurnState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure: Option<TimelineFailure>,
    },
}

/// 一个 session seed 的全局有序记录（对齐 `deepx-domain::TimelineEntry`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineEntry {
    /// 同一 (server epoch, seed) 内严格单调。
    pub timeline_seq: u64,
    pub turn_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round_num: Option<u32>,
    pub event: TimelineEvent,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// wire 形状与上游一致（tag = "type"，snake_case）——保证换用真实
    /// 反序列化时零改动。
    #[test]
    fn wire_shape_matches_upstream() {
        let entry = TimelineEntry {
            timeline_seq: 7,
            turn_id: "t1".into(),
            round_num: Some(0),
            event: TimelineEvent::TextDelta {
                block_id: "text:b1".into(),
                fragment_seq: 3,
                delta: "hel".into(),
            },
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"type\":\"text_delta\""), "{json}");
        assert!(json.contains("\"block_id\":\"text:b1\""), "{json}");
        // roundtrip
        let back: TimelineEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entry);
    }

    /// 真实 wire 形状（deepx-domain TimelineEvent）可反序列化。
    #[test]
    fn upstream_shapes_deserialize() {
        // BlockOpened 带完整 TimelineBlock（含 tool 嵌套）
        let json = serde_json::json!({
            "timeline_seq": 1,
            "turn_id": "t1",
            "round_num": 0,
            "event": {
                "type": "block_opened",
                "block": {
                    "block_id": "tool:c1",
                    "block_order": 0,
                    "kind": "tool",
                    "state": "open",
                    "tool": {
                        "tool_call_id": "c1",
                        "name": "exec",
                        "state": "running",
                        "args_json": "{\"cmd\":\"ls\"}",
                        "progress": ""
                    }
                }
            }
        });
        let entry: TimelineEntry = serde_json::from_value(json).unwrap();
        let TimelineEvent::BlockOpened { block } = &entry.event else {
            panic!("expected block_opened");
        };
        assert_eq!(block.kind, TimelineBlockKind::Tool);
        assert_eq!(block.state, TimelineBlockState::Open);
        assert_eq!(block.tool.as_ref().unwrap().name, "exec");
        assert_eq!(entry.turn_id, "t1");
    }

    /// TurnSealed 携带 state + failure（可选字段缺省可解析）。
    #[test]
    fn turn_sealed_without_failure_deserializes() {
        let json = serde_json::json!({
            "timeline_seq": 9,
            "turn_id": "t1",
            "event": { "type": "turn_sealed", "state": "cancelled" }
        });
        let entry: TimelineEntry = serde_json::from_value(json).unwrap();
        assert!(matches!(
            entry.event,
            TimelineEvent::TurnSealed {
                state: TimelineTurnState::Cancelled,
                failure: None
            }
        ));
    }
}
