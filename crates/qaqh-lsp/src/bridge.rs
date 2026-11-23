//! 同步工具环 ↔ LSP 连接的桥接（mcp bridge.rs 同款：宿主 runtime + 投影批次）。
//!
//! - E-5 单一 dispatcher：[`dispatch`] 是 `lsp` 聚合工具的 fn 指针；
//! - runtime（v4.4 P1-1 收敛）：本 crate 不再自建常驻 runtime。async-lsp 的
//!   mainloop/driver 任务落在 [`runtime_handle`] 选出的 runtime 上——daemon 里
//!   就是宿主 ambient runtime，CLI/裸线程才用进程级唯一兜底；同步工具线程经
//!   [`qaqh_tool_core::async_rt::block_on_host`] 等有界结果，取代旧的两处
//!   `dedicated_runtime().block_on(...)` sandwich；
//! - 投影批次：enabled 即在场（`lsp list_servers` 零配置可答"无配置"，mcp 钉底
//!   同款）；无连接维度——聚合工具不随 server 增减。

use std::sync::Arc;
use std::time::Duration;

use qaqh_tool_core::tool_api::{FatalToolError, ToolCallContext, ToolOutcome};
use qaqh_workspace::DynamicTool;

use crate::manager::LspManager;
use crate::manager_slot;

/// ctx 超时缺省（LSP 请求轻量，30s；mcp 为 60s）。
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// LSP 会话 actor 所在 runtime 句柄（P1-1：宿主 ambient 优先，共享兜底其次）。
pub fn runtime_handle() -> tokio::runtime::Handle {
    qaqh_tool_core::async_rt::runtime_handle()
}

/// daemon 退出路径的优雅关闭入口。
///
/// **必须在宿主 runtime 仍被驱动时 `await`**（daemon main 在
/// `runtime.block_on(...)` 的 async 块内收尾）：P1-1 之后 async-lsp 的
/// transport/driver 任务就挂在这个 runtime 上，runtime 停摆后再向它要收尾
/// 等于自己等自己。逐连接 `Shutdown` → 限时硬顶（L-09）→ driver abort →
/// 进程组清扫。
pub async fn shutdown_global() {
    manager_slot().shutdown_all().await;
}

/// E-5 单一 dispatcher：`lsp` 聚合工具的 fn 指针。
pub fn dispatch(
    name: &str,
    ctx: &ToolCallContext,
    args: serde_json::Value,
) -> Result<ToolOutcome, FatalToolError> {
    crate::tool::aggregate_dispatch(name, ctx, args)
}

/// 投影批次：enabled 即 Some（含聚合工具钉底），disabled 即 None。
pub fn take_projection_batch() -> Option<Vec<(String, DynamicTool)>> {
    projection_batch_with(&manager_slot())
}

/// [`take_projection_batch`] 的可测形态：manager 显式注入。
#[doc(hidden)]
pub fn projection_batch_with(manager: &Arc<LspManager>) -> Option<Vec<(String, DynamicTool)>> {
    if !manager.config().enabled {
        return None;
    }
    Some(vec![crate::tool::aggregate_entry(Duration::from_secs(
        DEFAULT_TIMEOUT_SECS,
    ))])
}
