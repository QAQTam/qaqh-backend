//! API 反馈的上下文占用（工具结果信封上的"词元钱包"读数）。
//!
//! 为什么需要它：runtime 从不把 usage / 上下文百分比回灌给模型，模型只能靠
//! "这条会话看起来很大"去猜自己的预算。responses 端点尤其糟——那边会把模型
//! 自己的思考摘要原样回放（`responses_echo_reasoning_content`），一次
//! "预算耗尽"的臆测就成了后续回合反复引用的"事实"（实测：UI 报上下文 29%
//! 时模型自称"这条会话的预算已经用到底了"，并据此停工）。
//!
//! 这里只做一件事：把 provider 回报的 `prompt_tokens` 与端点声明的
//! `context_length` 记成**可测量的读数**，由工具结果信封带给模型。不猜、
//! 不估，直接是 API 自己说的那个数。
//!
//! 线程本地而非进程全局：一个 actor 线程只跑一个会话（与
//! `qaqh_workspace::current_workspace()` 同款做法），子会话不会读到父会话
//! 的读数。

use serde::{Deserialize, Serialize};
use std::cell::Cell;

/// 一次 API 反馈的上下文占用快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUsage {
    /// provider 回报的输入 token 数（含命中缓存的部分）。
    pub used_tokens: u32,
    /// 端点声明的上下文窗口（`ContextConfig::context_length`）。
    pub window_tokens: u32,
}

impl ContextUsage {
    /// `window_tokens = 0`（端点没声明窗口）→ `None`：没有分母就不编百分比。
    pub fn new(used_tokens: u32, window_tokens: u32) -> Option<Self> {
        (window_tokens > 0).then_some(Self {
            used_tokens,
            window_tokens,
        })
    }

    /// 已用百分比：四舍五入，夹在 `0..=100`。
    ///
    /// 端点少报窗口（或本地 `context_length` 配小了）时 `used > window`，
    /// 读成 100% 而不是 137%——越界读数只说明"到顶了"，不该变成新噪声。
    pub fn used_pct(self) -> u8 {
        let used = u64::from(self.used_tokens);
        let window = u64::from(self.window_tokens);
        let pct = used.saturating_mul(100).saturating_add(window / 2) / window;
        u8::try_from(pct.min(100)).unwrap_or(100)
    }
}

thread_local! {
    /// 本线程最近一次 API 反馈的上下文占用。
    static LAST_API_CONTEXT: Cell<Option<ContextUsage>> = const { Cell::new(None) };
}

/// 记下本回合 API 反馈的输入量。返回记下的读数（窗口未声明时为 `None`，
/// 此时不覆盖旧值——一次没带分母的响应不该抹掉上一个有效读数）。
pub fn note_api_context_usage(used_tokens: u32, window_tokens: u32) -> Option<ContextUsage> {
    let usage = ContextUsage::new(used_tokens, window_tokens)?;
    LAST_API_CONTEXT.set(Some(usage));
    Some(usage)
}

/// 读本线程最近一次读数（工具结果落盘时盖戳用）。
pub fn current_api_context_usage() -> Option<ContextUsage> {
    LAST_API_CONTEXT.get()
}

/// 清空读数（会话边界 / 测试）。
pub fn clear_api_context_usage() {
    LAST_API_CONTEXT.set(None);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本地顺序执行的测试：thread_local 在同一线程里跨用例可见。
    fn isolated<F: FnOnce()>(body: F) {
        clear_api_context_usage();
        body();
        clear_api_context_usage();
    }

    #[test]
    fn pct_is_rounded_and_clamped() {
        assert_eq!(
            ContextUsage::new(290_000, 1_000_000).unwrap().used_pct(),
            29
        );
        assert_eq!(
            ContextUsage::new(295_000, 1_000_000).unwrap().used_pct(),
            30
        );
        assert_eq!(ContextUsage::new(0, 1_000_000).unwrap().used_pct(), 0);
        assert_eq!(
            ContextUsage::new(1_000_000, 1_000_000).unwrap().used_pct(),
            100
        );
        // 端点少报窗口：读成到顶，不产生 >100 的噪声。
        assert_eq!(
            ContextUsage::new(1_370_000, 1_000_000).unwrap().used_pct(),
            100
        );
    }

    #[test]
    fn a_missing_window_has_no_reading() {
        assert!(ContextUsage::new(1234, 0).is_none());
    }

    #[test]
    fn noting_then_reading_roundtrips() {
        isolated(|| {
            assert!(current_api_context_usage().is_none());
            let noted = note_api_context_usage(290_000, 1_000_000).expect("读数");
            assert_eq!(current_api_context_usage(), Some(noted));
            assert_eq!(noted.used_pct(), 29);
        });
    }

    /// 未声明窗口的响应不得抹掉上一个有效读数（否则信封会突然失去字段）。
    #[test]
    fn a_windowless_response_keeps_the_previous_reading() {
        isolated(|| {
            note_api_context_usage(290_000, 1_000_000);
            assert!(note_api_context_usage(290_001, 0).is_none());
            assert_eq!(current_api_context_usage().map(|u| u.used_pct()), Some(29));
        });
    }
}
