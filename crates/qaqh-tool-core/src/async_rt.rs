//! 进程内 Tokio 宿主选择与同步线程等待桥（v4.4 P1-1）。
//!
//! 一个进程只应有**一套被驱动的** runtime：daemon 在 `main` 里建宿主 runtime，
//! `block_on` 进 `server::run()`。工具执行面是同步的（`spawn_blocking` 闭包、
//! CLI、单测），需要把 async 活儿交给 runtime 驱动并原地等结果。此前
//! `qaqh-mcp` / `qaqh-lsp` / `qaqh-agent-runtime` 各自私建常驻 runtime
//! （daemon 进程内最多 4 套，见 LOCK_AUDIT_REPORT §Runtime 计数），本模块是
//! 收敛后的唯一入口：
//!
//! - [`runtime_handle`]：**ambient 优先**。身处 runtime 上下文的线程直接拿宿主
//!   句柄——包括 `tokio::task::spawn_blocking` 线程（tokio 为其 `enter()` 了宿主
//!   上下文，但没有进入 `block_on` 的 entered 态）。只有在完全没有 ambient
//!   runtime 的线程（CLI 子命令、裸 `#[test]` 线程）才惰性构建兜底 runtime，
//!   且全进程共用这一个 cell。
//! - [`block_on_host`]：把 future 交给 [`runtime_handle`] 并阻塞当前线程等结果。
//!   `Handle::block_on` 的既有限制是「不得在已进入 runtime 的线程上调用」：
//!   daemon 的工具执行恒在 `spawn_blocking` 线程（`session_loop.rs`），那里
//!   合法；future 的依赖任务（rmcp / async-lsp 的 transport actor）由同
//!   runtime 的 worker 线程继续驱动。在 worker 线程上调用会当场 panic
//!   （`Cannot start a runtime from within a runtime`）——这是刻意保留的响法：
//!   宁可炸在调用点，也不要在共享 runtime 上悄悄占死一个 worker。测试里模拟
//!   同步工具调用方时同样要走 `spawn_blocking`。
//! - [`try_runtime_handle`]：不触发惰性构建的只读版本（`Drop` 兜底网络用；
//!   L-22 的成因正是「checked 版读的是另一个 cell」，这里只有一处 cell）。
//!
//! 不变量：同一进程内所有 MCP/LSP 连接与回合任务最终都落在同一 runtime 上。
//! 混用只可能发生在「宿主 runtime 停摆后再从裸线程碰 mcp/lsp」这种退出末端
//! 路径上，收尾因此必须在宿主 runtime 还活着时 `await`（见 daemon `main.rs`）。

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::{Builder, Handle, Runtime};

/// 兜底 runtime 的 worker 数：只为「无 ambient runtime」的 CLI/单测撑住
/// rmcp / async-lsp 的 actor 任务，不需要更多。
const FALLBACK_WORKERS: usize = 2;

static FALLBACK: OnceLock<Runtime> = OnceLock::new();

/// 当前线程应当把 async 活儿派到哪个 runtime：ambient 优先，其次进程级唯一
/// 兜底 runtime。
pub fn runtime_handle() -> Handle {
    match Handle::try_current() {
        Ok(handle) => handle,
        Err(_) => fallback_runtime().handle().clone(),
    }
}

/// [`runtime_handle`] 的不构建版：ambient 句柄或**已构建**的兜底句柄，都没有
/// 则 `None`（`Drop` 里的兜底网络据此判断还能不能派收尾任务）。
pub fn try_runtime_handle() -> Option<Handle> {
    match Handle::try_current() {
        Ok(handle) => Some(handle),
        Err(_) => FALLBACK.get().map(|runtime| runtime.handle().clone()),
    }
}

/// 把 future 交给 [`runtime_handle`] 并阻塞当前线程直到出结果。
///
/// 取代旧实现里的 `runtime_handle().spawn(...) + recv_timeout(250ms)` 轮询桥：
/// 取消与超时改由 future 内部的 `tokio::select!` 事件驱动
/// （`CancellationToken::cancelled` / `tokio::time::sleep`），本函数只管等最终值。
///
/// 只能在非 worker 线程调用（见模块注）；future 内部 panic 会原样穿过本线程，
/// 由工具执行面的 `catch_unwind` 结算——不再像旧桥那样被降级成一条
/// `bridge worker terminated` 协议错误。
pub fn block_on_host<F: Future>(future: F) -> F::Output {
    runtime_handle().block_on(future)
}

fn fallback_runtime() -> &'static Runtime {
    FALLBACK.get_or_init(|| {
        // 走到这里说明本线程没有 ambient runtime；若在 runtime 上下文里落进
        // 本分支，就是第二套常驻 runtime 重新长出来的现场，debug 构建当场响。
        debug_assert!(
            Handle::try_current().is_err(),
            "qaqh fallback runtime must never be built while an ambient runtime exists"
        );
        Builder::new_multi_thread()
            .worker_threads(FALLBACK_WORKERS)
            .enable_all()
            .thread_name("qaqh-rt-fallback")
            .build()
            .expect("qaqh fallback tokio runtime must build")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_thread_gets_one_shared_fallback() {
        // 同一裸线程两次取句柄必须是同一个 runtime（cell 只有一个）。
        let first = runtime_handle();
        let second = runtime_handle();
        assert_eq!(first.id(), second.id());
        assert!(
            try_runtime_handle().is_some(),
            "兜底 runtime 已构建后 checked 版必须看得见（L-22 的现场）"
        );
    }

    #[test]
    fn ambient_handle_wins_over_fallback() {
        let ambient = Runtime::new().expect("test runtime");
        let ambient_id = ambient.handle().id();
        let from_task = ambient
            .block_on(ambient.spawn(async { runtime_handle().id() }))
            .expect("task joined");
        assert_eq!(from_task, ambient_id, "ambient 存在时不得另起 runtime");
    }

    #[test]
    fn spawn_blocking_thread_sees_ambient_and_may_block_on_host() {
        // 生产形态：工具执行在 spawn_blocking 里 block_on_host。这条用例是
        // 「宿主 runtime 的 blocking 线程上 block_on 合法且拿的是宿主句柄」
        // 的可执行证明。
        let ambient = Runtime::new().expect("test runtime");
        let ambient_id = ambient.handle().id();
        let (seen_id, value) = ambient
            .block_on(ambient.spawn(async {
                tokio::task::spawn_blocking(|| (runtime_handle().id(), block_on_host(async { 42 })))
                    .await
                    .expect("blocking task joined")
            }))
            .expect("driver task joined");
        assert_eq!((seen_id, value), (ambient_id, 42));
    }
}
