//! Tool SDK shim — SDK 主体已拆至 [`qaqh_tool_core`]（P2 crate 拆分，研究文档 §4）。
//!
//! 本模块保留宿主侧的 [`boundary`]（依赖 authorization 授权凭证类型，不随
//! SDK 下沉），并 re-export SDK 全部公开路径——`qaqh_workspace::tool_api::*`
//! 既有引用（runtime / mcp / lsp / subagent / 工具实现）保持不变。

pub use qaqh_tool_core::tool_api::*;

mod boundary;
pub use boundary::{
    BatchContext, BatchOutcome, CallOutcome, ExecuteBatch, InteractionDecision, PendingInteraction,
    PendingKind, ResumeInteraction,
};
pub(crate) use qaqh_tool_core::tool_api::display::clamp_display_body;
