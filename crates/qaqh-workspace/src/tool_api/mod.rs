//! Tool SDK shim — SDK 主体已拆至 [`qaqh_tool_core`]（P2 crate 拆分，研究文档 §4）。
//!
//! 本模块只做 re-export：`qaqh_workspace::tool_api::*` 既有引用（runtime / mcp /
//! lsp / subagent / 工具实现）保持不变。
//!
//! 原 `boundary` 子模块（`ExecuteBatch` / `BatchOutcome` / `PendingInteraction` …）
//! 已于 2026-10-30 删除：模块自述「尚未接线生产路径」，且 8 个类型在本 crate 外
//! 0 引用——loop ↔ runtime 的实际边界由 `qaqh-agent-runtime` 自己的
//! `PendingInteraction` 承担。

pub use qaqh_tool_core::tool_api::*;
