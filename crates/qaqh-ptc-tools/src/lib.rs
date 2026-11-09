//! `run_code`：PTC 模式的唯一传输通道。
//!
//! 这个工具把「程序里的 `tools.x(...)`」桥接回宿主自己的工具漏斗——它**不**
//! 直接执行工具，也不认识工具语义。宿主实现 [`ToolSurface`]，为每次 `run_code`
//! 构造一个子调用器；子调用器负责走完 precheck/审批/审计/配额，并返回无损 JSON。
//!
//! 关键契约：
//!
//! - 子调用 id 形如 `<parent>:ptc:<n>`，按提交顺序编号，对消费者不透明；
//! - 子调用失败 = 程序侧的 rejection（`ToolCallError`），不是 `run_code` 失败；
//! - 程序级失败（异常/超时/取消/输出超限/子进程死亡）是**结果字段**，同时把
//!   外层工具标成失败，让模型的下一步能自纠；
//! - 程序永远不会被自动重放：先前的副作用可能已经提交。
//!
//! 本轮范围：绑定桥 + 结果投影 + 并发声明（`Exclusive`）。子调用的并行池、
//! 审批交互桥与 `tool/ptc-dispatch` 事件留给下一轮（见 crate 文档末尾的待办）。

#![deny(missing_docs)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use qaqh_policy::ToolCategory;
use qaqh_ptc_core::{
    PtcBindingFn, PtcBindingNamespace, PtcCancel, PtcError, PtcRunRequest, PtcRunResult, PtcRuntime,
};
use qaqh_ptc_sdk::ToolSchema;
use qaqh_tool_core::{
    Concurrency, ToolCallContext, ToolCapabilities, ToolContentBlock, ToolError, ToolErrorKind,
    ToolExecutionError, ToolMeta, ToolProjection, ToolRisk, ToolStatus, TypedTool,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 模型面工具名。
pub const RUN_CODE_NAME: &str = "run_code";

/// 程序可见的命名空间全局名。
pub const TOOLS_NAMESPACE: &str = "tools";

/// 程序可见的类型化拒绝构造器名。
pub const TOOL_CALL_ERROR: &str = "ToolCallError";

/// 一次子调用的输入（由 `run_code` 构造，交给宿主执行）。
#[derive(Debug, Clone)]
pub struct PtcSubCall<'a> {
    /// 子调用 id（`<parent>:ptc:<n>`）。
    pub call_id: &'a str,
    /// 工具名（与 SDK 声明一致）。
    pub name: &'a str,
    /// 无损 JSON 参数。
    pub args: Value,
}

/// 子调用执行器：由宿主为**一次** `run_code` 构造，之后的每次调用只携带
/// `(call_id, name, args)`。
///
/// 实现必须在内部固化本次执行的策略（工作区、权限档、沙箱、审批授权、审计
/// 上下文），并且**不得**接受调用方指定的策略——参数里没有任何策略字段，这是
/// 故意的。
pub type SubCallInvoker = dyn Fn(&str, &str, Value) -> Result<Value, String> + Send + Sync;

/// `run_code` 需要的工具面：枚举可见工具 + 为一次执行构造子调用器。
pub trait ToolSurface: Send + Sync {
    /// 本次调用可见的工具（顺序无关；渲染与绑定都会按名字排序）。
    ///
    /// 必须与应用到模型的 SDK 声明来自同一份可见性，否则模型会调用它读到的
    /// 绑定却被拒。
    fn visible(&self, ctx: &ToolCallContext) -> Vec<ToolSchema>;

    /// 为一次执行构造子调用器。
    fn subcall_invoker(&self, ctx: &ToolCallContext) -> Arc<SubCallInvoker>;
}

/// `run_code` 的参数。
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RunCodeArgs {
    /// The program: the body of an async TypeScript function (erasable syntax only; top-level
    /// `await` and `return` work). Only what you print or return becomes program output.
    pub code: String,
    /// Clear, concise description of what this program does in active voice, 5-10 words (shown in
    /// the UI).
    pub description: String,
    /// Positive elapsed-time budget in milliseconds, including nested tool and approval waits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// 程序级失败的可序列化投影。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RunCodeFailure {
    /// 失败分类（`exception` / `timeout` / `abort` / `worker-exit` / `invalid-output` /
    /// `output-limit` / `protocol`）。
    pub kind: String,
    /// 面向模型可读的细节。
    pub message: String,
}

/// `run_code` 的规范输出。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RunCodeOutput {
    /// 捕获的程序输出，保持发出顺序。
    pub logs: Vec<String>,
    /// 程序的完成值（无损 JSON）；无值或失败时缺省。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// 仅在程序失败时存在。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<RunCodeFailure>,
}

impl ToolProjection for RunCodeOutput {
    fn status(&self) -> ToolStatus {
        if self.failure.is_some() {
            ToolStatus::Error
        } else {
            ToolStatus::Ok
        }
    }

    fn error(&self) -> Option<ToolError> {
        self.failure.as_ref().map(|failure| {
            ToolError::new(
                ToolErrorKind::Execution,
                format!("code run failed ({}): {}", failure.kind, failure.message),
            )
            .with_retryable(false)
        })
    }

    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        let mut text = self.logs.join("\n");
        if let Some(result) = &self.result {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&render_value(result));
        }
        if let Some(failure) = &self.failure {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!(
                "code run failed ({}): {}",
                failure.kind, failure.message
            ));
        }
        if text.is_empty() {
            text.push_str("(run_code completed with no output)");
        }
        vec![ToolContentBlock::Text { text }]
    }
}

fn render_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

/// `run_code` 的预算与并发上限。
#[derive(Debug, Clone, Copy)]
pub struct RunCodeLimits {
    /// 未显式给出预算时的默认经过时间。
    pub default_timeout: Duration,
    /// 任何请求都不超过的经过时间。
    pub max_timeout: Duration,
}

impl Default for RunCodeLimits {
    fn default() -> Self {
        Self {
            default_timeout: Duration::from_secs(120),
            max_timeout: Duration::from_secs(600),
        }
    }
}

/// `run_code` 工具。
pub struct RunCodeTool {
    runtime: Arc<dyn PtcRuntime>,
    surface: Arc<dyn ToolSurface>,
    limits: RunCodeLimits,
}

impl RunCodeTool {
    /// 以执行后端与宿主工具面构造。
    pub fn new(
        runtime: Arc<dyn PtcRuntime>,
        surface: Arc<dyn ToolSurface>,
        limits: RunCodeLimits,
    ) -> Self {
        Self {
            runtime,
            surface,
            limits,
        }
    }

    /// 面向模型的完整描述：固定规则 + 后端拥有的执行说明 + 实际预算。
    pub fn description(&self) -> String {
        let instructions = self.runtime.execution_instructions().trim();
        let mut text = format!(
            "Execute a {} program against the available tools. Takes two required arguments: \
             `code`, the program to execute, and `description`, a short summary of what the \
             program does. Call tools as `await tools.name(args)` per the declarations in the \
             system prompt. Default budget {}ms, capped at {}ms.",
            self.runtime.language().as_str(),
            self.limits.default_timeout.as_millis(),
            self.limits.max_timeout.as_millis(),
        );
        if !instructions.is_empty() {
            text.push(' ');
            text.push_str(instructions);
        }
        text
    }

    fn namespace(&self, ctx: &ToolCallContext, schemas: &[ToolSchema]) -> PtcBindingNamespace {
        let invoker = self.surface.subcall_invoker(ctx);
        let parent = ctx.call_id.clone();
        let counter = Arc::new(AtomicU64::new(0));
        let mut namespace =
            PtcBindingNamespace::new(TOOLS_NAMESPACE).with_error_class(TOOL_CALL_ERROR, "toolName");
        for schema in schemas {
            let name = schema.name.clone();
            let bound = name.clone();
            let invoker = Arc::clone(&invoker);
            let parent = parent.clone();
            let counter = Arc::clone(&counter);
            let call: Arc<PtcBindingFn> = Arc::new(move |args: Value| {
                let seq = counter.fetch_add(1, Ordering::SeqCst) + 1;
                let call_id = format!("{parent}:ptc:{seq}");
                invoker(&call_id, &bound, args)
            });
            namespace = namespace.with_member(name, call);
        }
        namespace
    }
}

impl TypedTool for RunCodeTool {
    type Args = RunCodeArgs;
    type Output = RunCodeOutput;

    fn meta(&self) -> ToolMeta {
        let mut meta = ToolMeta::new(
            RUN_CODE_NAME,
            self.description(),
            ToolCategory::Exec,
            ToolRisk::Destructive,
            self.limits.default_timeout,
        );
        // 程序可以调用任意工具（含独占类），因此整个调用独占批次。
        // run_code 不在 tool-core 的 capability 迁移表内，`ToolManager::register`
        // 不会覆盖 meta 自带的 capabilities，这里直接声明即可。
        meta.capabilities = ToolCapabilities {
            concurrency: Concurrency::Exclusive,
            ..ToolCapabilities::default()
        };
        meta
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        if args.description.trim().is_empty() {
            return Err(ToolExecutionError::Recoverable(
                ToolError::invalid_arguments("invalid description: expected a non-empty string"),
            ));
        }
        let schemas = self.surface.visible(ctx);
        let namespace = self.namespace(ctx, &schemas);

        let request = PtcRunRequest {
            program: args.code,
            bindings: vec![namespace],
            cwd: Some(ctx.workspace_root.clone()),
            timeout: args.timeout_ms.map(Duration::from_millis),
            max_output_bytes: None,
            cancel: PtcCancel::from_shared_flag(ctx.cancellation.shared_flag()),
        };
        let spec = self.runtime.resolve(request).map_err(recoverable)?;
        let result = self.runtime.run(spec).map_err(recoverable)?;
        Ok(into_output(result))
    }
}

fn recoverable(error: PtcError) -> ToolExecutionError {
    ToolExecutionError::Recoverable(ToolError::new(ToolErrorKind::Execution, error.to_string()))
}

fn into_output(result: PtcRunResult) -> RunCodeOutput {
    RunCodeOutput {
        logs: result.logs,
        result: result.value,
        failure: result.failure.map(|failure| RunCodeFailure {
            kind: failure.kind.as_str().to_owned(),
            message: failure.message,
        }),
    }
}

/// 供宿主组装 SDK 文本：把工具描述与输出 schema 一并渲染。
pub fn render_sdk(schemas: &[ToolSchema], format: qaqh_ptc_core::PtcProgramFormat) -> String {
    qaqh_ptc_sdk::render_tools_sdk(schemas, format)
}

/// `run_code` 自身的参数 schema（宿主注册与文档共用一份事实源）。
pub fn run_code_parameters(limits: RunCodeLimits) -> Value {
    qaqh_ptc_sdk::run_code_parameters(
        "The program: the body of an async TypeScript function.",
        Some((
            limits.default_timeout.as_millis() as u64,
            limits.max_timeout.as_millis() as u64,
        )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use qaqh_ptc_core::{PtcFailureKind, PtcProgramFormat, PtcRunFailure, PtcRunSpec};
    use serde_json::json;

    /// 记录调用、按需返回成功/失败的假后端；`run` 会真的调用注入的绑定。
    struct FakeRuntime {
        program: Mutex<Option<String>>,
        namespaces: Mutex<Vec<String>>,
        members: Mutex<Vec<String>>,
        failure: Option<PtcRunFailure>,
    }

    impl FakeRuntime {
        fn new() -> Self {
            Self {
                program: Mutex::new(None),
                namespaces: Mutex::new(Vec::new()),
                members: Mutex::new(Vec::new()),
                failure: None,
            }
        }

        fn failing() -> Self {
            Self {
                failure: Some(PtcRunFailure::new(PtcFailureKind::Timeout, "budget spent")),
                ..Self::new()
            }
        }
    }

    impl PtcRuntime for FakeRuntime {
        fn language(&self) -> qaqh_ptc_core::PtcLanguage {
            qaqh_ptc_core::PtcLanguage::TypeScript
        }

        fn program_format(&self) -> PtcProgramFormat {
            PtcProgramFormat::FunctionBody
        }

        fn execution_instructions(&self) -> &str {
            "fake runtime instructions"
        }

        fn resolve(&self, request: PtcRunRequest) -> Result<PtcRunSpec, PtcError> {
            Ok(PtcRunSpec {
                program: request.program,
                bindings: request.bindings,
                cwd: request
                    .cwd
                    .unwrap_or_else(|| std::path::PathBuf::from("/tmp")),
                timeout: request.timeout.unwrap_or(Duration::from_secs(1)),
                max_output_bytes: request.max_output_bytes.unwrap_or(1024),
                cancel: request.cancel,
            })
        }

        fn run(&self, spec: PtcRunSpec) -> Result<PtcRunResult, PtcError> {
            *self.program.lock().unwrap() = Some(spec.program.clone());
            for namespace in &spec.bindings {
                self.namespaces
                    .lock()
                    .unwrap()
                    .push(namespace.global.clone());
                for member in &namespace.functions {
                    self.members.lock().unwrap().push(member.name.clone());
                    // 模拟程序调用：一次成功、一次失败（由 invoker 决定）。
                    let _ = (member.call)(json!({ "path": "a.txt" }));
                }
            }
            Ok(match &self.failure {
                Some(failure) => PtcRunResult::failed(vec!["captured".to_owned()], failure.clone()),
                None => {
                    PtcRunResult::success(vec!["hello".to_owned()], Some(json!({ "ok": true })))
                }
            })
        }
    }

    struct FakeSurface {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl ToolSurface for FakeSurface {
        fn visible(&self, _ctx: &ToolCallContext) -> Vec<ToolSchema> {
            vec![
                ToolSchema::new(
                    "read",
                    "Read a file",
                    json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
                    json!({ "type": "object", "properties": { "content": { "type": "string" } } }),
                ),
                ToolSchema::new(
                    "grep",
                    "Search files",
                    json!({ "type": "object", "properties": { "pattern": { "type": "string" } } }),
                    json!({ "type": "object", "properties": {} }),
                ),
            ]
        }

        fn subcall_invoker(&self, _ctx: &ToolCallContext) -> Arc<SubCallInvoker> {
            let calls = Arc::clone(&self.calls);
            Arc::new(move |call_id, name, _args| {
                calls.lock().unwrap().push(format!("{call_id}:{name}"));
                Ok(json!({ "ok": true }))
            })
        }
    }

    fn context() -> ToolCallContext {
        ToolCallContext {
            call_id: "call_1".to_owned(),
            session_id: "session_1".to_owned(),
            workspace_root: std::path::PathBuf::from("/tmp"),
            permission_level: qaqh_policy::PermissionLevel::WorkspaceWrite,
            sandbox: qaqh_tool_core::SandboxMode::Main,
            sandbox_spec: qaqh_tool_core::SandboxSpec::workspace_write(std::path::PathBuf::from(
                "/tmp",
            )),
            exec_default_shell: None,
            timeout: Duration::from_secs(30),
            cancellation: qaqh_tool_core::tool_api::CancellationToken::new(),
            progress: None,
            exec_progress: None,
            exec_host: None,
            official_web: None,
            source: qaqh_tool_core::ToolCallSource::Model,
        }
    }

    fn args(description: &str) -> RunCodeArgs {
        RunCodeArgs {
            code: "return 1".to_owned(),
            description: description.to_owned(),
            timeout_ms: None,
        }
    }

    #[test]
    fn forwards_program_and_bindings_to_the_runtime() {
        let runtime = Arc::new(FakeRuntime::new());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let tool = RunCodeTool::new(
            runtime.clone(),
            Arc::new(FakeSurface {
                calls: Arc::clone(&calls),
            }),
            RunCodeLimits::default(),
        );
        let output = tool.run(&context(), args("Count files")).expect("run");

        assert_eq!(runtime.program.lock().unwrap().as_deref(), Some("return 1"));
        assert_eq!(runtime.namespaces.lock().unwrap().as_slice(), ["tools"]);
        assert_eq!(runtime.members.lock().unwrap().as_slice(), ["read", "grep"]);
        assert_eq!(output.status(), ToolStatus::Ok);
        assert_eq!(output.logs, vec!["hello"]);
        assert_eq!(output.result, Some(json!({ "ok": true })));
        assert!(output.failure.is_none());
    }

    #[test]
    fn sub_call_ids_are_parent_scoped_and_sequential() {
        let runtime = Arc::new(FakeRuntime::new());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let tool = RunCodeTool::new(
            runtime,
            Arc::new(FakeSurface {
                calls: Arc::clone(&calls),
            }),
            RunCodeLimits::default(),
        );
        tool.run(&context(), args("Read twice")).expect("run");
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            ["call_1:ptc:1:read", "call_1:ptc:2:grep"]
        );
    }

    #[test]
    fn empty_description_is_a_recoverable_invalid_argument() {
        let tool = RunCodeTool::new(
            Arc::new(FakeRuntime::new()),
            Arc::new(FakeSurface {
                calls: Arc::new(Mutex::new(Vec::new())),
            }),
            RunCodeLimits::default(),
        );
        let error = tool.run(&context(), args("   ")).expect_err("must reject");
        assert!(matches!(error, ToolExecutionError::Recoverable(_)));
        assert!(!error.is_fatal());
    }

    #[test]
    fn program_failure_is_reported_as_a_failed_tool_outcome() {
        let tool = RunCodeTool::new(
            Arc::new(FakeRuntime::failing()),
            Arc::new(FakeSurface {
                calls: Arc::new(Mutex::new(Vec::new())),
            }),
            RunCodeLimits::default(),
        );
        let output = tool.run(&context(), args("Spin")).expect("run");
        assert_eq!(output.status(), ToolStatus::Error);
        assert_eq!(
            output.failure.as_ref().map(|failure| failure.kind.as_str()),
            Some("timeout")
        );
        let error = output.error().expect("error mapping");
        assert!(error.detail.contains("budget spent"));
    }

    #[test]
    fn description_states_language_and_budget() {
        let tool = RunCodeTool::new(
            Arc::new(FakeRuntime::new()),
            Arc::new(FakeSurface {
                calls: Arc::new(Mutex::new(Vec::new())),
            }),
            RunCodeLimits::default(),
        );
        let description = tool.description();
        assert!(description.contains("typescript"));
        assert!(description.contains("120000ms"));
        assert!(description.contains("fake runtime instructions"));
        let meta = tool.meta();
        assert_eq!(meta.name.as_str(), RUN_CODE_NAME);
        assert_eq!(meta.capabilities.concurrency, Concurrency::Exclusive);
    }

    #[test]
    fn sdk_rendering_is_available_for_the_model_surface() {
        let surface = FakeSurface {
            calls: Arc::new(Mutex::new(Vec::new())),
        };
        let schemas = surface.visible(&context());
        let rendered = render_sdk(&schemas, PtcProgramFormat::FunctionBody);
        assert!(rendered.contains("declare const tools"));
        assert!(rendered.contains("read: {"));
        assert!(rendered.contains("grep: {"));
    }
}
