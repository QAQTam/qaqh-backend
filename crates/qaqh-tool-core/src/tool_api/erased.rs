//! ErasedTool：类型擦除适配器接口（base spec §6.2）。
//!
//! 运行时通过本接口统一调用所有工具。适配器职责（对 [`super::typed::TypedTool`]）：
//! 1. 按 typed `Args` 反序列化参数；
//! 2. 参数错误转换为可恢复 `ToolError`；
//! 3. 调用 `TypedTool::run`；
//! 4. 把 typed output 投影为 [`ToolOutcome`]；
//! 5. fatal 原样上抛。

use super::context::ToolCallContext;
use super::descriptor::ToolDescriptor;
use super::error::FatalToolError;
use super::output::ToolOutcome;

/// 运行时统一调用面（类型擦除）。
pub trait ErasedTool: Send + Sync {
    /// 工具描述符。
    fn descriptor(&self) -> ToolDescriptor;

    /// 执行单次调用。
    ///
    /// 可恢复错误写入 `ToolOutcome.error`（返回 `Ok`）；仅内部 fatal 走
    /// `Err`（base spec §7.3）。
    fn execute(
        &self,
        ctx: ToolCallContext,
        args: serde_json::Value,
    ) -> Result<ToolOutcome, FatalToolError>;
}
