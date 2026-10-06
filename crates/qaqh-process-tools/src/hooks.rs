//! 门面注入钩子（fold policy 输出上限 / process 显示投影）。

use std::sync::OnceLock;

use qaqh_tool_core::tool_api::{ToolDisplay, ToolDisplayFn};

static EXEC_MAX_OUTPUT_TOKENS: OnceLock<fn() -> Option<u32>> = OnceLock::new();
static PROJECT_PROCESS: OnceLock<ToolDisplayFn> = OnceLock::new();

/// 门面注册「执行线程 fold policy 的 exec 输出 token 上限」回调（幂等）。
pub fn set_exec_max_output_tokens(provider: fn() -> Option<u32>) {
    let _ = EXEC_MAX_OUTPUT_TOKENS.set(provider);
}

/// 门面注册 `process` 工具的显示投影（幂等；单一事实源留守门面 display）。
pub fn set_project_process(projector: ToolDisplayFn) {
    let _ = PROJECT_PROCESS.set(projector);
}

/// exec 的缺省输出 token 上限（未注册 = None → 调用侧按 u32::MAX 兜底）。
pub fn exec_max_output_tokens() -> Option<u32> {
    EXEC_MAX_OUTPUT_TOKENS
        .get()
        .map(|provider| provider())
        .flatten()
}

/// `process` 工具的显示投影（未注册时按 process 输出 JSON 形状就地投影——
/// `{"status": ..., "content": ...}`；门面版 `display::project_process` 对
/// 该形状语义一致）。
pub fn project_process(args: &serde_json::Value, output: &str) -> ToolDisplay {
    if let Some(projector) = PROJECT_PROCESS.get() {
        return projector(args, output);
    }
    let action = args
        .get("action")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("process");
    let view: Option<serde_json::Value> = serde_json::from_str(output).ok();
    let status = view
        .as_ref()
        .and_then(|view| view.get("status"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let body = view
        .as_ref()
        .and_then(|view| view.get("content"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| output.to_string());
    ToolDisplay::new(
        qaqh_tool_core::tool_api::ToolHeader::Other {
            label: format!("process {action}"),
        },
        qaqh_tool_core::tool_api::ToolBody::Text {
            text: body,
            truncated: false,
        },
    )
    .with_summary(status.unwrap_or_else(|| "process".to_string()))
}
