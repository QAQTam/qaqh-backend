//! TypedTool：新工具优先实现的 typed 接口（base spec §6.1）。
//!
//! schema 由类型生成（schemars 派生）——参数/输出与 schema 单一事实源；
//! 迁移期 legacy `ToolHandler` 经 `LegacyToolAdapter` 接入（plan P2），
//! 新工具只能实现本 trait。

use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::de::DeserializeOwned;

use super::context::{ToolCallContext, ToolCallSource};
use super::descriptor::ToolDescriptor;
use super::erased::ErasedTool;
use super::error::{FatalToolError, ToolError, ToolExecutionError};
use super::output::{
    ToolContentBlock, ToolExecutionMetrics, ToolModelProjection, ToolOutcome, ToolOutputValue,
    ToolProjection,
};

/// 类型化工具（v1 同步；async 变体见 base spec §6.1 备注，v1 不引入）。
pub trait TypedTool: Send + Sync {
    /// 参数类型（派生 `JsonSchema`；反序列化失败映射为 `InvalidArguments`）。
    type Args: DeserializeOwned + JsonSchema + Send + 'static;
    /// 输出类型（实现 [`ToolProjection`] 提供模型/展示投影）。
    type Output: ToolProjection + JsonSchema + 'static;

    /// 工具描述符（唯一描述源）。
    fn descriptor(&self) -> ToolDescriptor;

    /// 执行一次调用。
    ///
    /// - 可恢复失败 → [`ToolExecutionError::Recoverable`]；
    /// - 内部 fatal → [`ToolExecutionError::Fatal`]（不得伪装为可恢复）。
    #[allow(clippy::result_large_err)] // ToolExecutionError is the frozen public boundary.
    fn run(
        &self,
        ctx: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError>;
}

/// [`TypedTool`] 到 runtime 统一执行面的类型擦除桥。
///
/// 负责把 JSON 参数反序列化为 typed args、把可恢复错误映射为
/// [`ToolOutcome`]、把 fatal 原样上抛，并把 typed output 投影为
/// model/display/canonical 三面。
pub struct TypedToolAdapter<T> {
    tool: T,
}

impl<T> TypedToolAdapter<T> {
    pub fn new(tool: T) -> Self {
        Self { tool }
    }
}

impl<T> ErasedTool for TypedToolAdapter<T>
where
    T: TypedTool + 'static,
{
    fn descriptor(&self) -> ToolDescriptor {
        self.tool.descriptor()
    }

    fn execute(
        &self,
        ctx: ToolCallContext,
        args: serde_json::Value,
    ) -> Result<ToolOutcome, FatalToolError> {
        let started = Instant::now();
        let raw_args = args.clone();
        let args = match serde_json::from_value::<T::Args>(args) {
            Ok(args) => args,
            Err(error) => {
                return Ok(recoverable_outcome(
                    ToolError::invalid_arguments(format!("invalid arguments: {error}")),
                    started.elapsed(),
                ));
            }
        };

        match self.tool.run(&ctx, args) {
            Ok(output) => Ok(project_output(
                &output,
                &raw_args,
                ctx.source,
                started.elapsed(),
            )),
            Err(ToolExecutionError::Recoverable(error)) => {
                Ok(recoverable_outcome(error, started.elapsed()))
            }
            Err(ToolExecutionError::Fatal(error)) => Err(error),
        }
    }
}

fn project_output<O: ToolProjection>(
    output: &O,
    args: &serde_json::Value,
    source: ToolCallSource,
    elapsed: Duration,
) -> ToolOutcome {
    let blocks = output.model_blocks();
    let model_text = model_text_for(output, &blocks);
    let output_value = serde_json::to_value(output).unwrap_or(serde_json::Value::Null);
    let mut display = output.display(args);
    if display.summary.is_none() {
        display.summary = output.summary();
    }
    let output_bytes = model_text.len() as u64;
    let status = output.status();
    let error = output.error();

    ToolOutcome {
        status,
        output: ToolOutputValue::Json(output_value),
        error,
        model: ToolModelProjection {
            text: model_text,
            truncated: false,
        },
        display,
        images: Vec::new(),
        metrics: ToolExecutionMetrics {
            elapsed,
            output_bytes,
            retry_count: 0,
            effective_tool_name: None,
            user_initiated: matches!(source, ToolCallSource::User),
        },
        effects: output.effects(),
    }
}

fn recoverable_outcome(error: ToolError, elapsed: Duration) -> ToolOutcome {
    let mut text = error.detail.clone();
    if let Some(hint) = error.hint.as_ref()
        && !hint.is_empty()
    {
        text.push_str("\nHint: ");
        text.push_str(hint);
    }
    ToolOutcome {
        status: qaqh_types::ToolStatus::Error,
        output: ToolOutputValue::Empty,
        error: Some(error),
        model: ToolModelProjection {
            text: text.clone(),
            truncated: false,
        },
        display: Default::default(),
        images: Vec::new(),
        metrics: ToolExecutionMetrics {
            elapsed,
            output_bytes: text.len() as u64,
            retry_count: 0,
            effective_tool_name: None,
            user_initiated: false,
        },
        effects: Vec::new(),
    }
}

fn model_text_for<O: ToolProjection>(output: &O, blocks: &[ToolContentBlock]) -> String {
    if blocks.is_empty() {
        return serde_json::to_string(output).unwrap_or_default();
    }
    blocks
        .iter()
        .map(|block| match block {
            ToolContentBlock::Text { text } => text.clone(),
            ToolContentBlock::Json { value } => {
                serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
            }
            ToolContentBlock::Image(image) => {
                serde_json::to_string(image).unwrap_or_else(|_| "[image]".to_string())
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::ToolRisk;
    use crate::permission::ToolCategory;
    use crate::tool_api::{
        AgentMode, CancellationToken, OutputBudget, SandboxMode, ToolCapabilities, ToolExposure,
        ToolName, ToolSource,
    };

    #[derive(Debug, Deserialize, JsonSchema)]
    struct EchoArgs {
        text: String,
    }

    #[derive(Debug, Serialize, JsonSchema)]
    struct EchoOutput {
        text: String,
    }

    impl ToolProjection for EchoOutput {
        fn summary(&self) -> Option<String> {
            Some(self.text.clone())
        }
    }

    /// 编译级验证：typed 工具可完整实现（schema 由类型生成）。
    struct EchoTool;

    impl TypedTool for EchoTool {
        type Args = EchoArgs;
        type Output = EchoOutput;

        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: ToolName::new("echo").expect("valid"),
                display_name: None,
                description: "回显输入".to_owned(),
                input_schema: serde_json::to_value(schemars::schema_for!(EchoArgs))
                    .expect("schema 可序列化"),
                output_schema: serde_json::to_value(schemars::schema_for!(EchoOutput))
                    .expect("schema 可序列化"),
                category: ToolCategory::Read,
                risk: ToolRisk::ReadOnly,
                default_timeout: Duration::from_secs(10),
                exposure: ToolExposure::Direct,
                source: ToolSource::Builtin,
                output_budget: OutputBudget::default(),
                capabilities: ToolCapabilities::default(),
            }
        }

        fn run(
            &self,
            _ctx: &ToolCallContext,
            args: Self::Args,
        ) -> Result<Self::Output, ToolExecutionError> {
            Ok(EchoOutput { text: args.text })
        }
    }

    fn ctx() -> ToolCallContext {
        ToolCallContext {
            call_id: "c1".to_owned(),
            session_id: "s1".to_owned(),
            workspace_root: std::path::PathBuf::from("/tmp/ws"),
            mode: AgentMode::Code,
            permission_level: crate::permission::PermissionLevel::ReadFree,
            sandbox: SandboxMode::Main,
            timeout: Duration::from_secs(10),
            cancellation: CancellationToken::new(),
            progress: None,
            source: crate::tool_api::ToolCallSource::Model,
        }
    }

    #[test]
    fn typed_tool_descriptor_is_generated_from_types_and_valid() {
        let tool = EchoTool;
        let descriptor = tool.descriptor();
        assert_eq!(descriptor.validate(), Ok(()));
        assert_eq!(descriptor.input_schema["type"], "object");
        assert!(
            descriptor.input_schema["properties"]["text"].is_object(),
            "schema 由 Args 类型生成"
        );
    }

    #[test]
    fn typed_tool_run_roundtrips() {
        let tool = EchoTool;
        let output = tool
            .run(&ctx(), EchoArgs { text: "hi".into() })
            .expect("run ok");
        assert_eq!(output.summary().as_deref(), Some("hi"));
    }
}
