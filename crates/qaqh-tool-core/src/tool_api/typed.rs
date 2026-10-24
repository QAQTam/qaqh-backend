//! TypedTool：新工具优先实现的 typed 接口（base spec §6.1）。
//!
//! schema 由类型生成（schemars 派生）——参数/输出与 schema 单一事实源；
//! 生产工具一律经 `register_typed` 接入；本模块是 typed 契约的落点。
//! 新工具只能实现本 trait。

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::de::DeserializeOwned;

use super::context::{ToolCallContext, ToolCallSource};
use super::descriptor::{ToolDescriptor, ToolMeta};
use super::erased::ErasedTool;
use super::error::{FatalToolError, ToolError, ToolExecutionError};
use super::output::{
    ToolExecutionMetrics, ToolModelProjection, ToolOutcome, ToolOutputValue, ToolProjection,
};
use super::schema::schema_of;

/// 类型化工具（SDK v2；async 变体见 base spec §6.1 备注，v1 不引入）。
///
/// v2 契约：工具作者只声明 [`ToolMeta`]（名称/描述/权限/档位/超时），schema
/// 由 `Args`/`Output` 类型经 [`schema_of`] 生成——手写 JSON schema 从机制上
/// 不可能，参数/输出与 wire 描述保持单一事实源。
pub trait TypedTool: Send + Sync {
    /// 参数类型（派生 `JsonSchema`；反序列化失败映射为 `InvalidArguments`）。
    type Args: DeserializeOwned + JsonSchema + Send + 'static;
    /// 输出类型（实现 [`ToolProjection`] 提供模型/展示投影）。
    type Output: ToolProjection + JsonSchema + 'static;

    /// 工具元数据（schema 之外的描述符字段）。
    fn meta(&self) -> ToolMeta;

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
/// model/display/canonical 三面；描述符由 meta + 类型生成 schema 组装。
pub struct TypedToolAdapter<T> {
    tool: T,
    /// `T::Args` 的生成 schema（首次用到时算一次，之后复用）。
    ///
    /// 注册面（`descriptor()`）与执行面（参数归一、失败
    /// hint）必须看同一份 schema，否则「发给模型的类型」与「我们据以容错
    /// 的类型」会漂移。
    input_schema: OnceLock<serde_json::Value>,
}

impl<T> TypedToolAdapter<T> {
    pub fn new(tool: T) -> Self {
        Self {
            tool,
            input_schema: OnceLock::new(),
        }
    }
}

impl<T: TypedTool> TypedToolAdapter<T> {
    /// 缓存版 `schema_of::<T::Args>()`（见 `input_schema` 字段注释）。
    fn cached_input_schema(&self) -> serde_json::Value {
        self.input_schema
            .get_or_init(|| schema_of::<T::Args>())
            .clone()
    }
}

impl<T> ErasedTool for TypedToolAdapter<T>
where
    T: TypedTool + 'static,
{
    fn descriptor(&self) -> ToolDescriptor {
        let mut descriptor = ToolDescriptor::from(self.tool.meta());
        descriptor.input_schema = self.cached_input_schema();
        descriptor.output_schema = schema_of::<T::Output>();
        descriptor
    }

    fn execute(
        &self,
        ctx: ToolCallContext,
        args: serde_json::Value,
    ) -> Result<ToolOutcome, FatalToolError> {
        let started = Instant::now();
        let raw_args = args.clone();
        // JSON types are part of the tool contract; never repair model values.
        let args = match serde_json::from_value::<T::Args>(args) {
            Ok(args) => args,
            Err(error) => {
                return Ok(recoverable_outcome(
                    ToolError::invalid_arguments(format!("invalid arguments: {error}"))
                        .with_retryable(true)
                        .with_hint(super::coerce::expected_shape_hint(
                            &self.cached_input_schema(),
                        )),
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
    let model_text = if blocks.is_empty() {
        serde_json::to_string(output).unwrap_or_default()
    } else {
        super::output::joined_model_text(&blocks)
    };
    let output_value = serde_json::to_value(output).unwrap_or(serde_json::Value::Null);
    let display = output.display(args);
    let output_bytes = model_text.len() as u64;
    let status = output.status();
    let error = output.error();
    let images = output.images();

    ToolOutcome {
        status,
        output: ToolOutputValue::Json(output_value),
        error,
        model: ToolModelProjection {
            text: model_text,
            truncated: false,
        },
        display,
        images,
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::ToolRisk;
    use crate::permission::ToolCategory;
    use crate::tool_api::{AgentMode, CancellationToken, SandboxMode, ToolContentBlock};

    #[derive(Debug, Deserialize, JsonSchema)]
    /// 回显参数。
    struct EchoArgs {
        /// 待回显文本。
        text: String,
    }

    #[derive(Debug, Serialize, JsonSchema)]
    /// 回显输出。
    struct EchoOutput {
        /// 回显文本。
        text: String,
    }

    impl ToolProjection for EchoOutput {
        fn model_blocks(&self) -> Vec<ToolContentBlock> {
            vec![ToolContentBlock::Text {
                text: self.text.clone(),
            }]
        }
    }

    /// 编译级验证：typed 工具可完整实现（schema 由类型生成）。
    struct EchoTool;

    impl TypedTool for EchoTool {
        type Args = EchoArgs;
        type Output = EchoOutput;

        fn meta(&self) -> ToolMeta {
            ToolMeta::new(
                "echo",
                "回显输入",
                ToolCategory::Read,
                ToolRisk::ReadOnly,
                Duration::from_secs(10),
            )
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
            permission_level: crate::permission::PermissionLevel::ReadOnly,
            sandbox: SandboxMode::Main,
            sandbox_spec: crate::tool_api::SandboxSpec::workspace_write(std::path::PathBuf::from(
                "/tmp/ws",
            )),
            exec_default_shell: None,
            timeout: Duration::from_secs(10),
            cancellation: CancellationToken::new(),
            progress: None,
            exec_progress: None,
            official_web: None,
            source: crate::tool_api::ToolCallSource::Model,
        }
    }

    #[test]
    fn typed_tool_descriptor_is_generated_from_types_and_valid() {
        let adapter = TypedToolAdapter::new(EchoTool);
        let descriptor = adapter.descriptor();
        assert_eq!(descriptor.validate(), Ok(()));
        assert_eq!(descriptor.input_schema["type"], "object");
        assert!(
            descriptor.input_schema["properties"]["text"].is_object(),
            "schema 由 Args 类型生成"
        );
        assert_eq!(descriptor.description, "回显输入");
    }

    #[test]
    fn typed_tool_run_roundtrips() {
        let tool = EchoTool;
        let output = tool
            .run(&ctx(), EchoArgs { text: "hi".into() })
            .expect("run ok");
        assert_eq!(output.text, "hi");
        // summary 由模型面首行派生（v2）。
        let outcome = project_output(
            &output,
            &serde_json::json!({}),
            crate::tool_api::ToolCallSource::Model,
            Duration::ZERO,
        );
        assert_eq!(outcome.model.text, "hi");
        let wire = outcome.to_tool_result();
        assert_eq!(wire.summary(), "hi");
    }

    #[test]
    fn typed_display_summary_is_not_backfilled_from_model_output() {
        let output = EchoOutput { text: "hi".into() };
        let outcome = project_output(
            &output,
            &serde_json::json!({}),
            crate::tool_api::ToolCallSource::Model,
            Duration::ZERO,
        );
        assert_eq!(outcome.model.text, "hi");
        assert_eq!(
            outcome.display.summary, None,
            "display.summary must be explicitly constructed by the tool projector"
        );
    }

    mod coercion {
        use super::*;
        use crate::tool_api::ToolErrorKind;

        #[derive(Debug, Deserialize, JsonSchema)]
        /// 需要类型归一的参数（模型爱把这三个可选字段写成字符串）。
        #[allow(dead_code)] // 仅用于 execute 路径断言，字段值由 run 拼回文本。
        struct CoercionArgs {
            /// 文本。
            text: String,
            /// 上限。
            #[serde(default, skip_serializing_if = "Option::is_none")]
            max: Option<u64>,
            /// 过滤模式。
            #[serde(default, skip_serializing_if = "Option::is_none")]
            globs: Option<Vec<String>>,
            /// 干跑。
            #[serde(default, skip_serializing_if = "Option::is_none")]
            dry: Option<bool>,
        }

        struct CoercionTool;

        impl TypedTool for CoercionTool {
            type Args = CoercionArgs;
            type Output = EchoOutput;

            fn meta(&self) -> ToolMeta {
                ToolMeta::new(
                    "coercion_probe",
                    "参数归一探针",
                    ToolCategory::Read,
                    ToolRisk::ReadOnly,
                    Duration::from_secs(5),
                )
            }

            fn run(
                &self,
                _ctx: &ToolCallContext,
                args: Self::Args,
            ) -> Result<Self::Output, ToolExecutionError> {
                Ok(EchoOutput {
                    text: format!(
                        "{}|{}|{:?}|{}",
                        args.text,
                        args.max.unwrap_or_default(),
                        args.globs.unwrap_or_default(),
                        args.dry.unwrap_or(false),
                    ),
                })
            }
        }

        #[test]
        fn native_numbers_arrays_bools_are_accepted() {
            let adapter = TypedToolAdapter::new(CoercionTool);
            let outcome = adapter
                .execute(
                    ctx(),
                    serde_json::json!({
                        "text": "hi",
                        "max": 3,
                        "globs": ["*.rs"],
                        "dry": true
                    }),
                )
                .expect("execute ok");
            assert!(
                outcome.error.is_none(),
                "归一后不该再报参数错: {:?}",
                outcome.error
            );
            assert_eq!(outcome.model.text, "hi|3|[\"*.rs\"]|true");
        }

        #[test]
        fn stringified_numbers_arrays_bools_are_rejected() {
            let adapter = TypedToolAdapter::new(CoercionTool);
            for (field, value) in [("max", "3"), ("globs", "[\"*.rs\"]"), ("dry", "true")] {
                let mut args = serde_json::json!({"text":"hi"});
                args[field] = serde_json::json!(value);
                let outcome = adapter.execute(ctx(), args).unwrap();
                assert!(
                    outcome.error.is_some(),
                    "stringified {field} must not execute"
                );
            }
        }

        #[test]
        fn uncoercible_args_fail_with_field_typed_hint() {
            let adapter = TypedToolAdapter::new(CoercionTool);
            let outcome = adapter
                .execute(ctx(), serde_json::json!({ "text": "hi", "max": "abc" }))
                .expect("execute ok");
            let error = outcome.error.expect("坏参数必须报错");
            assert!(matches!(error.kind, ToolErrorKind::InvalidArguments));
            assert!(error.retryable, "类型错模型可自行纠正 → 允许重试");
            let hint = error.hint.expect("必须带字段级 hint");
            assert!(hint.contains("max?=integer"), "{hint}");
            assert!(hint.contains("globs?=array<string>"), "{hint}");
            assert!(hint.contains("text=string"), "{hint}");
            assert!(
                outcome.model.text.contains("Hint:"),
                "hint 要渲染进模型面: {}",
                outcome.model.text
            );
        }

        #[test]
        fn registered_and_coercion_schemas_are_the_same_object() {
            // descriptor 与归一面共用缓存 schema，否则「发给模型的类型」与
            // 「据以容错的类型」会漂移。
            let adapter = TypedToolAdapter::new(CoercionTool);
            let registered = adapter.descriptor().input_schema;
            assert_eq!(registered["properties"]["max"]["type"], "integer");
            assert_eq!(registered["properties"]["globs"]["type"], "array");
        }
    }
}
