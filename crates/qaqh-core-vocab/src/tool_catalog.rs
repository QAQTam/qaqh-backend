//! 只读工具目录（`ADR-0042` 铁律 1 / "开箱即渲染"）：`ToolCatalog` / `ToolCatalogEntry`。
//!
//! ## 目录是"选择面"，不是"声明面"
//!
//! - **声明面**（模型要什么 schema、什么描述）是 `qaqh_types::ToolDef` + JSON Schema，
//!   由运行期工具注册表产出，模型据此发起调用；
//! - **选择面/展示面**（有哪些工具、属于哪组、风险多高、为什么不可用）是本模块。
//!
//! 两者刻意**不合并**：把 JSON Schema 塞进目录，就是给"协议里塞裸 `Value`"开后门
//! （V2 的老毛病）。目录条目**不带任何 schema**——需要 schema 的读者去读声明面，
//! 需要"能不能用、为什么不能用"的读者读这里，各自只有一份真相。
//!
//! ## 前端只做薄投影
//!
//! 前端拿到 [`ToolCatalog`] 就能直接渲染工具面板：分组、风险色、禁用原因全是
//! 后端算好的强类型字段，前端**不需要**拼接口、猜约定或复刻权限规则
//! （宪法 #5："凡换完协议反而让前端更难写的设计，一律判定失败"）。

use serde::{Deserialize, Serialize};

use crate::tool_mode::ToolMode;

/// 原始 JSON 文本（**不解释、不强类型**）。
///
/// V3 里唯一允许承载"动态 JSON 值"的载体：它是字符串，不参与协议结构，
/// 用完即弃；绝不提供 `Value` 视图，也就没有"顺手改一个字段"的逃生口
/// （`ADR-0042` 铁律 1：依赖级封杀 `serde_json`）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JsonText(String);

impl JsonText {
    /// 由原始文本构造（不校验 JSON 语法：本类型是**搬运工**，校验属于真正
    /// 要解释它的那一层——在那里 Fail-Closed，而不是在搬运途中猜）。
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// 原始文本视图。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 工具分组＝权限类别（后端唯一既有分组事实）。
///
/// 刻意**不发明**第二套职责分类（"文件/网络/代理/…"）：那种分类在 V2 里不存在，
/// 新造一套只会让同一件事有两个名字，UI 与权限判定迟早对不上。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolGroup {
    /// 无副作用读取。
    Read,
    /// 改文件或会话状态。
    Write,
    /// 执行任意代码 / 控制进程。
    Exec,
    /// 出网。
    Net,
}

/// 风险档位（与权限审批强度同源）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolRiskLevel {
    /// 只读。
    ReadOnly,
    /// 写。
    Write,
    /// 破坏性。
    Destructive,
    /// 管理性（守护进程自管理等）。
    Administrative,
}

/// 并发档（批次调度输入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolConcurrency {
    /// 可与同批任意工具并行。
    Parallel,
    /// 与同批串行。
    Serial,
    /// 独占整批。
    Exclusive,
}

/// 工具能力约束（编排输入的只读投影）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCapabilityConstraints {
    /// 并发档。
    pub concurrency: ToolConcurrency,
    /// 是否声明支持进度流。
    pub streaming: bool,
    /// 是否幂等（重放安全）。
    pub idempotent: bool,
    /// 是否 workspace 绑定（cwd 敏感）。
    pub workspace_bound: bool,
    /// 是否阻塞等用户交互。
    pub interactive: bool,
    /// **是否可能产出图片**（模型无视觉输入时该工具不可用）。
    ///
    /// 这是"能力声明"而不是"按名字猜"：`read_image` 会返回图像，模型若不吃图，
    /// 调用它的结果就是不可渲染的垃圾——必须在目录里如实标成不可用。
    pub image_output: bool,
}

/// 工具来源（动态 MCP 必须有显式标识，前端才能分组与降级）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolOrigin {
    /// 内置工具。
    Builtin,
    /// 动态 MCP 工具（`mcp__{server}__{tool}` 命名空间）。
    DynamicMcp { server: String },
    /// 扩展/插件工具。
    DynamicExtension { id: String },
}

/// 可用性（Fail-Closed：没被明确标为可用，就是不可用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolAvailability {
    /// 本次会话可用。
    Available,
    /// 不可用，且**带机器可读的原因**（前端不需要猜为什么灰了）。
    Unavailable { reason: ToolUnavailableReason },
}

/// 不可用原因（全部来自真实存在的判定源，没有"其它"这种垃圾桶）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolUnavailableReason {
    /// 模型不具备工具调用能力（`ModelCapabilities::tool_calls == false`）：
    /// 整个工具面都不可用（协议面前提缺失，不是单个工具的问题）。
    ModelLacksToolCalls,
    /// 模型不吃图（`ModelCapabilities::image_input == false`），而该工具会产图。
    ModelLacksImageInput,
    /// 被会话工具模式（allowlist）排除。
    ExcludedByToolMode,
    /// 被全局工具天花板排除。
    ExcludedByGlobalCeiling,
}

/// 一条目录条目。
///
/// 字段顺序即语义重要性：名字 → 分组 → 风险 → 约束 → 来源 → 可用性。
/// **不带 schema**（见模块文档）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCatalogEntry {
    /// 规范名（唯一键，与声明面同名）。
    pub name: String,
    /// 分组（＝权限类别）。
    pub group: ToolGroup,
    /// 一段式简述（模型可见描述的首段；空描述在此为空串）。
    pub summary: String,
    /// 风险档位。
    pub risk: ToolRiskLevel,
    /// 能力约束。
    pub constraints: ToolCapabilityConstraints,
    /// 来源（内置 / 动态 MCP / 扩展）。
    pub origin: ToolOrigin,
    /// 可用性 + 原因。
    pub availability: ToolAvailability,
}

impl ToolCatalogEntry {
    /// 是否可用（`Available` 以外一律 false）。
    pub fn is_available(&self) -> bool {
        matches!(self.availability, ToolAvailability::Available)
    }

    /// 不可用原因（可用时为 `None`）。
    pub fn unavailable_reason(&self) -> Option<ToolUnavailableReason> {
        match &self.availability {
            ToolAvailability::Available => None,
            ToolAvailability::Unavailable { reason } => Some(*reason),
        }
    }

    /// 是否动态（MCP / 扩展）来源。
    pub fn is_dynamic(&self) -> bool {
        !matches!(self.origin, ToolOrigin::Builtin)
    }
}

/// 只读工具目录快照（前端唯一工具面真相源）。
///
/// 与 `Cursor`/`since_seq` 无关：目录是**幂等快照**，不是事件流。前端拿最新一份
/// 直接整体替换即可——这也是"开箱即渲染"的含义。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCatalog {
    /// 该快照适用的工具模式。
    pub mode: ToolMode,
    /// 条目（按名字升序，保证同一状态产生同一份字节——快照可比对、可缓存）。
    pub entries: Vec<ToolCatalogEntry>,
}

impl ToolCatalog {
    /// 构造快照。**不重排**：调用方必须已按名字升序给出（投影方负责，
    /// 顺序不稳定等于让前端缓存永远失效）。
    pub fn new(mode: ToolMode, entries: Vec<ToolCatalogEntry>) -> Self {
        Self { mode, entries }
    }

    /// 可用条目。
    pub fn available(&self) -> impl Iterator<Item = &ToolCatalogEntry> {
        self.entries.iter().filter(|entry| entry.is_available())
    }

    /// 不可用条目（含原因）。
    pub fn unavailable(&self) -> impl Iterator<Item = &ToolCatalogEntry> {
        self.entries.iter().filter(|entry| !entry.is_available())
    }

    /// 按名字查条目。
    pub fn entry(&self, name: &str) -> Option<&ToolCatalogEntry> {
        self.entries.iter().find(|entry| entry.name == name)
    }

    /// 条目总数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否没有任何工具（合法状态：全量被排除或注册表为空）。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, availability: ToolAvailability) -> ToolCatalogEntry {
        ToolCatalogEntry {
            name: name.to_string(),
            group: ToolGroup::Read,
            summary: "读取文件".into(),
            risk: ToolRiskLevel::ReadOnly,
            constraints: ToolCapabilityConstraints {
                concurrency: ToolConcurrency::Parallel,
                streaming: false,
                idempotent: true,
                workspace_bound: true,
                interactive: false,
                image_output: false,
            },
            origin: ToolOrigin::Builtin,
            availability,
        }
    }

    #[test]
    fn availability_is_fail_closed_and_carries_a_reason() {
        let available = entry("read", ToolAvailability::Available);
        assert!(available.is_available());
        assert_eq!(available.unavailable_reason(), None);

        let denied = entry(
            "read_image",
            ToolAvailability::Unavailable {
                reason: ToolUnavailableReason::ModelLacksImageInput,
            },
        );
        assert!(!denied.is_available());
        assert_eq!(
            denied.unavailable_reason(),
            Some(ToolUnavailableReason::ModelLacksImageInput)
        );
    }

    #[test]
    fn dynamic_origins_are_distinguishable() {
        assert!(!entry("read", ToolAvailability::Available).is_dynamic());
        let mcp = ToolCatalogEntry {
            origin: ToolOrigin::DynamicMcp {
                server: "fs".into(),
            },
            ..entry("mcp__fs__read_file", ToolAvailability::Available)
        };
        assert!(mcp.is_dynamic());
        assert_eq!(mcp.origin, ToolOrigin::DynamicMcp { server: "fs".into() });
    }

    #[test]
    fn catalog_filters_split_available_and_blocked() {
        let catalog = ToolCatalog::new(
            ToolMode::Core,
            vec![
                entry("exec_command", ToolAvailability::Available),
                entry(
                    "read_image",
                    ToolAvailability::Unavailable {
                        reason: ToolUnavailableReason::ExcludedByToolMode,
                    },
                ),
            ],
        );
        assert_eq!(catalog.len(), 2);
        assert_eq!(
            catalog.available().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["exec_command"]
        );
        assert_eq!(
            catalog
                .unavailable()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            vec!["read_image"]
        );
        assert_eq!(catalog.entry("exec_command").map(|e| e.group), Some(ToolGroup::Read));
        assert!(catalog.entry("nope").is_none());
    }

    #[test]
    fn json_text_is_opaque_transport_only() {
        let text = JsonText::new("{\"a\":1}");
        assert_eq!(text.as_str(), "{\"a\":1}");
        assert_eq!(text, JsonText::new("{\"a\":1}"));
    }
}
