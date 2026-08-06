//! 壳组件共享工具（P-4 预埋，WORKFLOW §6.1）。
//!
//! `poll_rev` 封装"DispatcherTimer 轮询快照 rev、变化才 set_state"样板
//! （原型见 sidebar.rs 内联实现；header.rs 首用，未来组件按需迁移）。

use std::time::Duration;

use windows_reactor::{DispatcherTimer, HookRef};

/// 轮询 `snapshot()` 的 `(state, rev)`，rev 变化时调用 `apply(state)`。
///
/// 调用方在 `use_effect` 内持有独立的 `timer`/`last_rev`（多实例安全）；
/// 未来合并为单一 UI timer 时只改本函数，组件代码不动。
pub fn poll_rev<T>(
    timer: HookRef<Option<DispatcherTimer>>,
    last_rev: HookRef<u64>,
    interval: Duration,
    snapshot: impl Fn() -> (T, u64) + 'static,
    apply: impl Fn(T) + 'static,
) where
    T: 'static,
{
    if let Ok(t) = DispatcherTimer::new(interval, {
        let last_rev = last_rev.clone();
        move || {
            let (state, rev) = snapshot();
            if rev != *last_rev.borrow() {
                *last_rev.borrow_mut() = rev;
                apply(state);
            }
        }
    }) {
        *timer.borrow_mut() = Some(t);
    }
}
