//! edit — 精确字符串替换（str_replace 语义）。
//!
//! 模型面字段：`path` / `old_str` / `new_str`（+ 可选 `replace_all`）。
//!
//! - `old_str` 必须精确命中文件中的**唯一**位置：
//!   - 0 处命中 → `NOT_FOUND`，返回一份「old_str → 文件实际内容」的 diff 供模型自纠；
//!   - 多处命中 → `AMBIGUOUS_MATCH`，列出各命中位置与上下文，提示扩展 `old_str`；
//!   - 恰好 1 处 → 替换并返回紧凑回执（pass）。
//!   - 多处命中且 `replace_all=true` → 全部替换（非重叠，`str::replace` 语义）。
//! - 匹配在 LF 规范视图上做，写回按原文件 CRLF 还原；成功写盘走 `atomic_write`，
//!   并维护 `file_state` 行号偏移链与 session journal。
//!
//! 已移除（v2 遗留）：`hunks`/kind、`context_before`/`context_after`、`hint_line`、
//! `expected_hash`、`dry_run`、Tier2/Tier3 模糊采纳、reindent、
//! read/overwrite 模式。多文件/大范围重排仍走 `apply_patch`。

pub mod core;
pub mod handler;

pub use handler::exec_edit;
pub use handler::register;

#[cfg(test)]
mod tests;
