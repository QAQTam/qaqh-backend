//! 工具输出模型（base spec §8）。
//!
//! 一次执行 → 四个投影：`output`（canonical，宿主/审计）/ `error`（模型 +
//! timeline failure）/ `model`（provider tool result）/ `display`（timeline、
//! TUI、client）/ `metrics`（telemetry、审计）。
//!
//! 展示结构（`ToolDisplay` 等）以 09-18 展示契约为唯一事实源，见
//! [`super::display`]。

use std::time::Duration;

use serde::Serialize;

use super::display::ToolDisplay;
use super::error::ToolError;
use qaqh_types::{ContentRef, ToolImage};

pub use qaqh_types::ToolStatus;

/// 模型面内容块（base spec §8.1/§9.1）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum ToolContentBlock {
    /// 文本块。
    Text {
        /// 文本内容。
        text: String,
    },
    /// 图片块（base64 载荷由 gate 在请求构建期降为 provider 原生格式）。
    Image(ToolImage),
    /// 结构化 JSON 块（序列化为 text 时由适配器决定边界）。
    Json {
        /// JSON 值。
        value: serde_json::Value,
    },
}

/// 工具作者声明的输出投影（base spec §8.1）。
///
/// 09-19 补充稿 Q4：原 trait 名 `ToolOutput` 与 09-18 输出侧契约的 typed
/// 容器 `ToolOutput<O>` 撞名，本 trait 更名为 `ToolProjection`（纯重命名；
/// 若评审否决可直接回退）。
#[doc(alias = "ToolOutput")]
pub trait ToolProjection: Serialize {
    /// 模型投影。为空时由适配器按 base spec §8.1 默认规则生成
    /// （文本序列化 / summary + 有界 JSON）。
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        Vec::new()
    }

    /// 单行人类可读摘要（展示与模型提示共用）。不得是 JSON（H1）。
    fn summary(&self) -> Option<String> {
        None
    }

    /// 展示投影。`args` 是**已通过 typed 校验的原始参数值**，供 header 提取
    /// 真相字段（path / command / query）；实现不得读取线程局部，也不得重新
    /// 解析未经校验的输入（H13）。
    fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
        ToolDisplay::default()
    }

    /// 宿主侧 typed effects。默认无副作用；需要注入 skill activation 等
    /// 可信状态迁移的工具在此显式返回，禁止让 runtime 解析工具文本猜测。
    fn effects(&self) -> Vec<crate::ToolEffect> {
        Vec::new()
    }
}

/// canonical 输出值（base spec §8.2）：宿主 / 审计 / 后续工具可消费，
/// 不保证直接发给模型。
#[derive(Debug, Clone, PartialEq)]
pub enum ToolOutputValue {
    /// 无输出。
    Empty,
    /// 文本输出。
    Text(String),
    /// 结构化输出。
    Json(serde_json::Value),
    /// 外置内容引用（大内容）。
    ContentRef(ContentRef),
}

/// 模型投影（base spec §8.2）。
///
/// 适配层把它映射到 `qaqh_types::ToolModelPayload`（含 token 统计等 wire 字段）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolModelProjection {
    /// 模型可见文本（已按 output_budget 截断）。
    pub text: String,
    /// 是否发生截断。
    pub truncated: bool,
}

/// 执行指标（base spec §8.3）：必须经 outcome 进入 runtime 适配层，
/// 不得生成后丢弃。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolExecutionMetrics {
    /// 执行耗时。
    pub elapsed: Duration,
    /// 本次调用产出的展示文本字节数（UTF-8，截断后）。
    pub output_bytes: u64,
    /// 重试次数。
    pub retry_count: u32,
    /// 别名/MCP 解析后的实际工具名；无别名时 = descriptor.name。
    pub effective_tool_name: Option<String>,
    /// 调用来源是否为用户直接发起（宿主侧来源，非工具自报）。
    pub user_initiated: bool,
}

/// 执行结果（base spec §8.2）。
#[derive(Debug)]
pub struct ToolOutcome {
    /// 权威状态（五态，与 wire `qaqh_types::ToolStatus` 同一词汇）。
    pub status: ToolStatus,
    /// canonical 输出。
    pub output: ToolOutputValue,
    /// 可恢复错误（与 status 的不变式见 [`ToolOutcome::check_invariants`]）。
    pub error: Option<ToolError>,
    /// 模型投影。
    pub model: ToolModelProjection,
    /// 展示投影。
    pub display: ToolDisplay,
    /// 图片附件（read_image 等）。
    ///
    /// 09-19 P2 补充：legacy `ToolResult.images` 的落点（wire 适配器据此重建
    /// tool message 的图片部件）；新工具经 `ToolProjection::model_blocks`
    /// 的 `Image` 块表达同一内容。
    pub images: Vec<ToolImage>,
    /// 执行指标。
    pub metrics: ToolExecutionMetrics,
    /// 宿主侧可信 effects（例如 skill activation）。
    pub effects: Vec<crate::ToolEffect>,
}

impl ToolOutcome {
    /// 检查 base spec §7.3 不变量：
    /// `Ok/Backgrounded ⇒ error.is_none()`；`Partial/Cancelled/Error ⇒ error.is_some()`。
    pub fn check_invariants(&self) -> Result<(), &'static str> {
        let success = matches!(self.status, ToolStatus::Ok | ToolStatus::Backgrounded);
        match (success, self.error.is_some()) {
            (true, true) => Err("成功状态不得携带 error（base spec §7.3）"),
            (false, false) => Err("失败状态必须携带 error（base spec §7.3）"),
            _ => Ok(()),
        }
    }

    /// 投影到迁移期 wire 容器 [`qaqh_types::ToolResult`]。
    ///
    /// 这是 typed runtime 与 v1 message/timeline 之间唯一的兼容出口：
    /// canonical typed payload 进入 `ToolResult.data`，model/display/error
    /// 分别沿用既有 wire 字段，旧 client 不需要理解 `ToolOutcome`。
    pub fn to_tool_result(&self) -> qaqh_types::ToolResult {
        let data = match &self.output {
            ToolOutputValue::Empty => serde_json::json!({}),
            ToolOutputValue::Text(text) => serde_json::json!({ "text": text }),
            ToolOutputValue::Json(value) => value.clone(),
            ToolOutputValue::ContentRef(reference) => {
                serde_json::to_value(reference).unwrap_or_else(|_| serde_json::json!({}))
            }
        };
        let mut result = qaqh_types::ToolResult::text(self.status, self.model.text.clone());
        if let Some(summary) = &self.display.summary {
            result = result.with_summary(summary.clone());
        }
        result.data = data;
        result.images = self.images.clone();
        result.diff = self.display.diff.clone();
        result.error = self.error.as_ref().map(|error| qaqh_types::ToolError {
            code: error.code.as_str().to_owned(),
            message: error.detail.clone(),
            retryable: error.retryable,
            hint: error.hint.clone(),
        });
        result.metrics = qaqh_types::ToolResultMetrics {
            elapsed_ms: Some(self.metrics.elapsed.as_millis() as u64),
            output_bytes: self.metrics.output_bytes,
            retry_count: self.metrics.retry_count,
            effective_tool_name: self.metrics.effective_tool_name.clone(),
            user_initiated: self.metrics.user_initiated,
        };
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(status: ToolStatus, error: Option<ToolError>) -> ToolOutcome {
        ToolOutcome {
            status,
            output: ToolOutputValue::Empty,
            error,
            model: ToolModelProjection::default(),
            display: ToolDisplay::default(),
            images: Vec::new(),
            metrics: ToolExecutionMetrics::default(),
            effects: Vec::new(),
        }
    }

    #[test]
    fn invariants_hold_for_consistent_outcomes() {
        assert!(outcome(ToolStatus::Ok, None).check_invariants().is_ok());
        assert!(
            outcome(ToolStatus::Backgrounded, None)
                .check_invariants()
                .is_ok()
        );
        assert!(
            outcome(ToolStatus::Error, Some(ToolError::not_found("x")))
                .check_invariants()
                .is_ok()
        );
    }

    #[test]
    fn invariants_reject_inconsistent_outcomes() {
        assert!(
            outcome(ToolStatus::Ok, Some(ToolError::cancelled("x")))
                .check_invariants()
                .is_err(),
            "成功状态带 error 违反不变量"
        );
        assert!(
            outcome(ToolStatus::Partial, None)
                .check_invariants()
                .is_err(),
            "失败状态缺 error 违反不变量"
        );
    }

    #[test]
    fn default_projection_is_empty() {
        struct Empty;
        impl Serialize for Empty {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_none()
            }
        }
        impl ToolProjection for Empty {}

        assert!(Empty.model_blocks().is_empty());
        assert_eq!(Empty.summary(), None);
        assert_eq!(
            Empty.display(&serde_json::json!({})),
            ToolDisplay::default()
        );
    }
}
