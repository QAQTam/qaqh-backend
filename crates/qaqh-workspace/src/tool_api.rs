//! Tool SDK v1 — 展示投影类型（09-18 跨仓展示契约 §3.2）。
//!
//! 本模块当前只承载展示投影与工具作者声明的投影函数；`TypedTool` /
//! `ToolOutcome` 的完整迁移见 base spec（`tool_api` 是其落点）。
//!
//! 注意：这些是 **SDK 内部类型**，不得直接序列化进 wire；wire 映射由
//! `qaqh-runtime::timeline::wire_display` 唯一负责（契约 §3.1/§3.4）。

/// 工具作者声明的展示投影函数。
///
/// 入参是**执行期已校验的 args** 与工具的 canonical 输出文本（exec 为
/// `ExecOutput` JSON）；投影由工具作者实现并注册，timeline 不得自行解析
/// 工具输出（H13）。
pub type ToolDisplayFn = fn(&serde_json::Value, &str) -> ToolDisplay;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolDisplay {
    /// 单行人类可读摘要。禁止 JSON（H1）。
    pub summary: Option<String>,
    /// 统一 diff（文件变更类工具）。
    pub diff: Option<String>,
    pub header: ToolHeader,
    pub body: ToolBody,
    /// 由框架填充；工具实现不得决定取值（H4）。
    pub metrics: ToolMetrics,
}

impl ToolDisplay {
    pub fn new(header: ToolHeader, body: ToolBody) -> Self {
        Self {
            header,
            body,
            ..Self::default()
        }
    }

    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }

    pub fn with_diff(mut self, diff: impl Into<String>) -> Self {
        self.diff = Some(diff.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ToolHeader {
    #[default]
    None,
    Path {
        path: String,
        op: PathOp,
    },
    Shell {
        command: String,
    },
    Query {
        query: String,
        scope: Option<String>,
    },
    Other {
        label: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathOp {
    Read,
    Write,
    Edit,
    List,
    Patch,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ToolBody {
    #[default]
    None,
    Text {
        text: String,
        truncated: bool,
    },
    Diff {
        unified: String,
        files: Vec<String>,
    },
    Shell {
        output: String,
        /// None：backgrounded / cancelled / 未拿到退出码。
        exit_code: Option<i32>,
        truncated: bool,
    },
    Subagent {
        name: String,
        seed: String,
    },
}

/// 运行信息（契约 §3.2）。`elapsed_ms = None` 表示本期未接线 metrics，
/// wire 侧 `display.metrics` 为 `None`，client 回退旧字段。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolMetrics {
    pub elapsed_ms: Option<u64>,
    pub output_bytes: u64,
    pub retry_count: u32,
    pub effective_tool_name: Option<String>,
    pub user_initiated: bool,
}
