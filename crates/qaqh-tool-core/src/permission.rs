//! 权限词汇 re-export（单一事实源在 [`qaqh_policy`]）。
//!
//! SDK 侧只需要类型词汇（category / level / decision），不承载判定逻辑；
//! `qaqh-workspace::permission` 同样 re-export 同源类型，两侧类型恒等。

pub use qaqh_policy::{PermissionDecision, PermissionLevel, PermissionRisk, ToolCategory};
