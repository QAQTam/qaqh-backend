//! 进程级管理器装配（与 `qaqh_mcp::install_manager` / `qaqh_lsp::install_manager` 同范式）。
//!
//! 为什么需要全局槽位：`provider_for(ctx, …)`（每回合重建 provider 的唯一构造器）
//! 的 `RingContext` 里**没有** service 句柄，而取 token 必须触达 SIWC 管理器。
//! 往 `RingContext` 里加字段会波及所有 engine 构造点（面大、收益低），
//! 因此沿用本仓库既有的「基础设施管理器」装配方式：**单点装配、深层取用**。
//!
//! 装配来源唯一：daemon 的 `QaqhService::siwc()`（懒装配）在创建管理器时就地安装。

use std::sync::{Arc, OnceLock};

use crate::manager::SiwcManager;

static MANAGER: OnceLock<Arc<SiwcManager>> = OnceLock::new();

/// 安装进程级管理器。**首次安装胜出**，后续调用被忽略——
/// 装配应来自单一入口，多处装配意味着会有两份凭据视图。
pub fn install_manager(manager: Arc<SiwcManager>) {
    let _ = MANAGER.set(manager);
}

/// 取已装配的管理器。`None` = 该能力从未被使用过（**不是错误**：
/// 纯 BYOK 用户不会装配它，也就不会有任何磁盘副作用）。
pub fn manager() -> Option<Arc<SiwcManager>> {
    MANAGER.get().map(Arc::clone)
}
