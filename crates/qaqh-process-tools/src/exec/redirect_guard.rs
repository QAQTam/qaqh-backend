//! Redirect turn 在途计数（CLEAN-3/T10 契约②）。
//!
//! redirect turn 进行中必须拒绝 `spy restore`：turn 未 merge 前，工作区
//! 可见状态是「真实上位盘 + ProjFS 视图」的拼合态，restore 在拼合态上做
//! 全量重写，merge 时会产生不可归因、可能互相覆盖的净变更。因此 spy
//! restore 前必须查询本计数器，>0 即拒绝（fail closed，让模型重试）。
//!
//! 以进程级计数器为事实源——与 `process_registry` 同级的进程内协调原语，
//! 不触及工具注册表单一事实源约束（I10 针对的是注册表，不是这类无状态
//! 计数器）。

use std::sync::atomic::{AtomicUsize, Ordering};

static REDIRECT_TURNS: AtomicUsize = AtomicUsize::new(0);

/// 当前在途 redirect turn 数（sandboxed exec 并行批可 >1）。
pub fn redirect_turns_in_flight() -> usize {
    REDIRECT_TURNS.load(Ordering::Acquire)
}

/// RAII 守卫：构造即计数 +1，drop 即 -1。sbx redirect turn 从视图建立到
/// merge 完成（`turn.merge()` 同步返回，见 sbx_bypass 契约①）全程持有。
pub struct RedirectTurnGuard {
    _private: (),
}

impl RedirectTurnGuard {
    pub fn acquire() -> Self {
        REDIRECT_TURNS.fetch_add(1, Ordering::AcqRel);
        RedirectTurnGuard { _private: () }
    }
}

impl Drop for RedirectTurnGuard {
    fn drop(&mut self) {
        let before = REDIRECT_TURNS.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(before > 0, "redirect turn guard underflow");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_counts_are_balanced() {
        let base = redirect_turns_in_flight();
        {
            let _g1 = RedirectTurnGuard::acquire();
            assert_eq!(redirect_turns_in_flight(), base + 1);
            {
                let _g2 = RedirectTurnGuard::acquire();
                assert_eq!(redirect_turns_in_flight(), base + 2);
            }
            assert_eq!(redirect_turns_in_flight(), base + 1);
        }
        assert_eq!(redirect_turns_in_flight(), base);
    }
}
