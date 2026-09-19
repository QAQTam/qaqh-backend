//! TypedTool：新工具优先实现的 typed 接口（base spec §6.1）。
//!
//! schema 由类型生成（schemars 派生）——参数/输出与 schema 单一事实源；
//! 迁移期 legacy `ToolHandler` 经 `LegacyToolAdapter` 接入（plan P2），
//! 新工具只能实现本 trait。

use schemars::JsonSchema;
use serde::de::DeserializeOwned;

use super::context::ToolCallContext;
use super::descriptor::ToolDescriptor;
use super::error::ToolExecutionError;
use super::output::ToolProjection;

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
    fn run(
        &self,
        ctx: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError>;
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
        AgentMode, CancellationToken, OutputBudget, ToolCapabilities, ToolExposure, ToolName,
        ToolSource,
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
