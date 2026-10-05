//! 动态工具（MCP/LSP）typed 适配器。
//!
//! 动态工具的模型面/路由元数据由 `qaqh-workspace` 侧的 `DynamicTool` 承载，
//! 执行面在本模块收口：全体 server 工具共用**一个** dispatcher fn 指针
//! （E-5 单一 dispatcher），注册全名由适配器注入——`ToolCallContext` 的
//! 新契约不含工具名（调用身份是 `call_id`）。
//!
//! 与 [`super::legacy::LegacyToolAdapter`] 的差异：本适配器**不经过** v1
//! `ToolCallCtx`/线程局部兼容视图，dispatcher 拿到的是显式 [`ToolCallContext`]
//! ——工作区、取消、超时全部显式传递。

use super::context::ToolCallContext;
use super::descriptor::ToolDescriptor;
use super::erased::ErasedTool;
use super::error::FatalToolError;
use super::output::ToolOutcome;

/// 动态工具 dispatcher（typed 外壳契约）：
/// `(注册全名, 显式上下文, args) → 执行结果`。
///
/// 可恢复错误写入 [`ToolOutcome::error`]（返回 `Ok`）；仅内部 fatal 走 `Err`。
pub type DynamicDispatch =
    fn(&str, &ToolCallContext, serde_json::Value) -> Result<ToolOutcome, FatalToolError>;

/// 把 dispatcher fn 指针包成 [`ErasedTool`]。
pub struct DynamicToolAdapter {
    name: String,
    descriptor: ToolDescriptor,
    dispatch: DynamicDispatch,
}

impl DynamicToolAdapter {
    pub(crate) fn new(descriptor: ToolDescriptor, dispatch: DynamicDispatch) -> Self {
        let name = descriptor.name.as_str().to_owned();
        Self {
            name,
            descriptor,
            dispatch,
        }
    }
}

impl ErasedTool for DynamicToolAdapter {
    fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
    }

    fn execute(
        &self,
        ctx: ToolCallContext,
        args: serde_json::Value,
    ) -> Result<ToolOutcome, FatalToolError> {
        (self.dispatch)(&self.name, &ctx, args)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::ToolRisk;
    use crate::permission::PermissionLevel;
    use crate::permission::ToolCategory;
    use crate::tool_api::{
        AgentMode, CancellationToken, OutputBudget, SandboxMode, SandboxSpec, ToolCallSource,
        ToolCapabilities, ToolExposure, ToolName, ToolSource,
    };

    fn marker(
        _name: &str,
        ctx: &ToolCallContext,
        args: serde_json::Value,
    ) -> Result<ToolOutcome, FatalToolError> {
        Ok(super::super::legacy::map_tool_result(
            crate::ToolResult::ok_data(
                serde_json::json!({
                    "name_is_full": true,
                    "workspace": ctx.workspace_root.to_string_lossy(),
                    "args": args,
                }),
                "marker",
            ),
        ))
    }

    fn descriptor() -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::new("mcp__demo__echo").expect("valid"),
            display_name: Some("echo".to_owned()),
            description: "dynamic".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: serde_json::json!({"type": "object"}),
            category: ToolCategory::Exec,
            risk: ToolRisk::Administrative,
            default_timeout: Duration::from_secs(30),
            exposure: ToolExposure::Direct,
            source: ToolSource::Mcp,
            output_budget: OutputBudget::default(),
            capabilities: ToolCapabilities::default(),
        }
    }

    fn ctx() -> ToolCallContext {
        ToolCallContext {
            call_id: "c1".to_owned(),
            session_id: "s1".to_owned(),
            workspace_root: std::path::PathBuf::from("/tmp/dyn-ws"),
            mode: AgentMode::Code,
            permission_level: PermissionLevel::ReadOnly,
            sandbox: SandboxMode::Main,
            sandbox_spec: SandboxSpec::workspace_write(std::path::PathBuf::from("/tmp/dyn-ws")),
            exec_default_shell: None,
            timeout: Duration::from_secs(30),
            cancellation: CancellationToken::new(),
            progress: None,
            source: ToolCallSource::Model,
        }
    }

    #[test]
    fn adapter_injects_full_name_and_explicit_context() {
        let adapter = DynamicToolAdapter::new(descriptor(), marker);
        assert_eq!(adapter.descriptor().name.as_str(), "mcp__demo__echo");
        let outcome = adapter
            .execute(ctx(), serde_json::json!({"text": "hi"}))
            .expect("execute");
        assert_eq!(outcome.status, qaqh_types::ToolStatus::Ok);
        match outcome.output {
            crate::tool_api::ToolOutputValue::Json(data) => {
                assert_eq!(data["name_is_full"], true);
                assert_eq!(data["workspace"], "/tmp/dyn-ws");
                assert_eq!(data["args"]["text"], "hi");
            }
            other => panic!("期望 Json(data)，得到 {other:?}"),
        }
    }
}
