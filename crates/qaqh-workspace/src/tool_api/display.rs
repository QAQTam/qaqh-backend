//! 展示投影类型（09-18 跨仓展示契约 §3.2 唯一事实源）。
//!
//! 注意：这些是 **SDK 内部类型**，不得直接序列化进 wire；wire 映射由
//! `qaqh-runtime::timeline::wire_display` 唯一负责（契约 §3.1/§3.4）。
//!
//! （原 `tool_api.rs` 于 2026-09-19 拆分为 `tool_api/` 目录；本文件内容原样
//! 迁入，类型与语义零变化。）

/// 工具作者声明的展示投影函数。
///
/// 入参是**执行期已校验的 args** 与工具的 canonical 输出文本（exec 为
/// `ExecOutput` JSON）；投影由工具作者实现并注册，timeline 不得自行解析
/// 工具输出（H13）。
pub type ToolDisplayFn = fn(&serde_json::Value, &str) -> ToolDisplay;

/// 按模型侧同一上限截断 display body，返回 `(text, truncated)`。
///
/// canonical display 会被 `ToolResult` 带进 `messages.jsonl`，所以大内容工具的
/// display body 不能直接放完整正文——否则每次调用都会把正文多持久化一份。模型侧
/// 已经按 [`CONTENT_BEARING_CHAR_LIMIT`](crate::tool_side_fold::CONTENT_BEARING_CHAR_LIMIT)
/// 折叠，display 侧对齐即可，`truncated` 如实标记。
pub(crate) fn clamp_display_body(text: &str) -> (String, bool) {
    let limit = crate::tool_side_fold::CONTENT_BEARING_CHAR_LIMIT;
    if text.chars().count() <= limit {
        return (text.to_owned(), false);
    }
    (text.chars().take(limit).collect(), true)
}

#[cfg(test)]
mod tests {
    use super::clamp_display_body;

    #[test]
    fn clamp_display_body_caps_content_bearing_text() {
        assert_eq!(clamp_display_body("abc"), ("abc".to_owned(), false));

        let limit = crate::tool_side_fold::CONTENT_BEARING_CHAR_LIMIT;
        let long = "字".repeat(limit + 5);
        let (text, truncated) = clamp_display_body(&long);
        assert!(truncated, "over-limit body must be marked truncated");
        assert_eq!(text.chars().count(), limit, "cap is counted in chars");
    }
}

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
