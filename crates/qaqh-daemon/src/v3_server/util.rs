//! V3 骨架的极小事工具（两件：锁与时钟）。
//!
//! 锁中毒的处理立场：**Fail-Closed**。持锁线程 panic 说明状态可能停在半更新处，
//! 把这种状态当正常状态继续服务，正是宪法 #3 判死的那种"看起来能跑"。
//! 宁可让这条路径当场炸掉，也不静默 `into_inner()` 假装没事。

use std::sync::{Mutex, MutexGuard};

/// 加锁（中毒即 panic，见模块文档）。
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => panic!("v3_server 状态锁中毒（持锁线程 panic）：{poisoned}"),
    }
}

/// 当前 Unix 毫秒。服务端时钟是 V3 的**唯一时间权威**（`FrameMeta::server_ts_ms`）。
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
