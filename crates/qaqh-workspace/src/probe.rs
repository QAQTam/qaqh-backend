//! 测试探针工具（`test-harness` 门控）。
//!
//! 存在的理由：runtime 集成测试需要一个"注册任意行为"的工具来观测编排、账本、
//! 取消契约。这一职责历史上由 v1 `ToolHandler` + `LegacyToolAdapter` 承担，并
//! 携带 `ToolCallCtx` 与线程局部兼容视图；[`ProbeTool`] 只用显式
//! [`ToolCallContext`]——生产代码看不到本模块。

use std::time::Duration;

use crate::ToolRisk;
use crate::tool_api::{
    ErasedTool, FatalToolError, OutputBudget, ToolCallContext, ToolCapabilities, ToolDescriptor,
    ToolExposure, ToolName, ToolOutcome, ToolSource,
};

/// 探针执行体：显式上下文 + args → v1 `ToolResult` 信封。
pub type ProbeBody = fn(&ToolCallContext, serde_json::Value) -> crate::ToolResult;

/// 可注册的测试探针：模型面元数据 + 探针体。
pub struct ProbeTool {
    pub key: String,
    pub description: &'static str,
    pub input_schema: serde_json::Value,
    pub handler: ProbeBody,
    pub risk: ToolRisk,
    pub category: crate::permission::ToolCategory,
    pub default_timeout: Duration,
}

impl ErasedTool for ProbeTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::new(&self.key)
                .unwrap_or_else(|error| panic!("invalid probe tool name {}: {error}", self.key)),
            display_name: None,
            description: self.description.to_owned(),
            input_schema: self.input_schema.clone(),
            output_schema: serde_json::json!({"type": "object"}),
            category: self.category,
            risk: self.risk.clone(),
            default_timeout: self.default_timeout,
            exposure: ToolExposure::Direct,
            source: ToolSource::Builtin,
            output_budget: OutputBudget::default(),
            capabilities: ToolCapabilities::default(),
        }
    }

    fn execute(
        &self,
        ctx: ToolCallContext,
        args: serde_json::Value,
    ) -> Result<ToolOutcome, FatalToolError> {
        Ok(crate::tool_api::map_tool_result((self.handler)(&ctx, args)))
    }
}
