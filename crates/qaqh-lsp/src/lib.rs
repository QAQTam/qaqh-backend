//! qaqh-lsp：QAQ-Harness 的 LSP 客户端支持。
//! 与 qaqh-mcp 的关系：MCP 是"工具总线"（远端能力接进来），LSP 是
//! "工作区语义面"（本地精确理解）——独立 crate，依赖方向单向
//! （`qaqh-runtime → qaqh-lsp → {qaqh-workspace, qaqh-config, async-lsp}`），
//! 不反向依赖 runtime/msgloop（mcp 设计 §4 同约束）。

pub mod adapter;
pub mod bridge;
pub mod connection;
pub mod error;
pub mod manager;
pub mod projection;
pub mod tool;

pub use bridge::{dispatch, prime_all_async, shutdown_global, take_projection_batch};
pub use connection::{ConnStatus, LifecycleSettings, ServerConnection};
pub use error::{LspError, LspErrorKind};
pub use manager::{ApplyReport, LspManager};

/// M0 起保留的 crate 用途标识。
pub const CRATE_PURPOSE: &str = "QAQ-Harness LSP client support";

use std::sync::{Arc, OnceLock, RwLock};

fn manager_slot_raw() -> &'static RwLock<Arc<LspManager>> {
    static MANAGER_SLOT: OnceLock<RwLock<Arc<LspManager>>> = OnceLock::new();
    // 槽位模式范本：qaqh-mcp/src/lib.rs（OnceLock + 读写锁中毒 into_inner）。
    MANAGER_SLOT.get_or_init(|| RwLock::new(LspManager::disabled()))
}

/// 当前全局 [`LspManager`]（未装配时为 disabled 占位：所有调用报 `LSP_DISABLED`）。
pub fn manager_slot() -> Arc<LspManager> {
    manager_slot_raw()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// 装配全局 manager，返回上一个（QaqhService 装配点一次性调用）。
pub fn install_manager(manager: Arc<LspManager>) -> Arc<LspManager> {
    let mut slot = manager_slot_raw()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::mem::replace(&mut *slot, manager)
}

