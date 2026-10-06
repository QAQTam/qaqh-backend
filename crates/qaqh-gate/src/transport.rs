//! gate::transport — SDK 桥接层共享的传输零件。
//!
//! HTTP/SSE/重试已整体移交 mutil-ai（`anthropic_sdk` / `openai_sdk` /
//! `responses_sdk` + `sdk_common`）；本模块只保留仍被复用的同步门面
//! （crate 级 current-thread runtime 的 `block_on`）、可取消睡眠、
//! `RetryPolicy`（gate 侧旋钮，编译成 SDK 策略）、错误描述表、
//! skill envelope 归一与 `SseTrace` 诊断。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use qaqh_types::{ContentBlock, Message};

use super::types::{ProviderConfig, StreamEvent};

// 与官方客户端会话重试策略对齐（opencode session/retry.ts：2s 初始、翻倍；
// 本常量 5 = 1 次原始请求 + 4 次重试），吸收网关瞬时 5xx  burst。
pub(crate) const MAX_RETRIES: u32 = 5;
// 对齐 codex（200ms）与 opencode（2s）的折中：整请求重发的协议适配层成本下取 1s。
pub(crate) const BASE_DELAY_SECS: u64 = 1;
/// 空闲看门狗：流连续无新字节超过此时长视为半开连接（对齐 opencode
/// wrapSSE / codex stream_idle_timeout 的 300s；需大于最长 thinking 静默期）。
pub(crate) const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Crate-global tokio runtime for reqwest I/O.
/// Uses current-thread scheduler — all async I/O serialises on the
/// calling thread via Runtime::block_on.
static FALLBACK_RT: std::sync::LazyLock<tokio::runtime::Runtime> = std::sync::LazyLock::new(|| {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to create qaqh-gate shared tokio runtime")
});

pub(crate) fn block_on<F: std::future::Future>(f: F) -> F::Output {
    FALLBACK_RT.block_on(f)
}

pub(crate) fn is_cancelled(cancel: Option<&Arc<AtomicBool>>) -> bool {
    cancel.map(|c| c.load(Ordering::SeqCst)).unwrap_or(false)
}

pub(crate) fn sleep_with_cancel(delay: Duration, cancel: Option<&Arc<AtomicBool>>) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < delay {
        if is_cancelled(cancel) {
            return true;
        }
        // `while` 守卫与本次减法之间 elapsed 可能已越过 delay（纳秒级竞争窗口，
        // 毫秒轮询每迭代都有一次机会）。Duration 的 `Sub` 是无条件 panic（debug
        // 与 release 皆然，见 BUG-2026-09-13-21）→ 用 checked_sub 饱和到 ZERO：
        // 剩余时间为 0 时立即进入下一轮守卫并正常退出。
        let remaining = delay.checked_sub(start.elapsed()).unwrap_or(Duration::ZERO);
        std::thread::sleep(remaining.min(Duration::from_millis(100)));
    }
    false
}

/// 每端点重试策略（对齐 codex `ModelProviderInfo` 的 request_max_retries /
/// stream_idle_timeout 三元组）。由端点的 `EndpointCompat.retry`（TOML）编译
/// 而来；缺省值即现行全局常量，行为零变化。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    /// 最大尝试次数（含首次；`5` = 1 次原始请求 + 4 次重试，与既有
    /// `MAX_RETRIES` 语义一致）。
    pub max_retries: u32,
    /// 首次重试的基础退避（指数翻倍 + ±10% jitter，封顶 `max_delay`）。
    pub base_delay: Duration,
    /// 单次等待上限。
    pub max_delay: Duration,
    /// 流空闲看门狗：连续无新字节超过此时长视为半开连接。
    pub idle_timeout: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: MAX_RETRIES,
            base_delay: Duration::from_secs(BASE_DELAY_SECS),
            max_delay: Duration::from_secs(30),
            idle_timeout: STREAM_IDLE_TIMEOUT,
        }
    }
}

impl RetryPolicy {
    /// T10 贯通终点：把 TOML 来的 `RetrySpec`（零值 = 未设置）编译为策略，
    /// 未设置字段回落内置缺省。
    pub fn from_spec(spec: Option<&qaqh_types::RetrySpec>) -> Self {
        let mut policy = Self::default();
        if let Some(spec) = spec {
            if spec.max_retries > 0 {
                // BUG-2026-09-13-02：无上限的 max_retries 会放大退避溢出面；
                // 32 次已是荒谬级的重试预算，超出按上限截断。
                policy.max_retries = spec.max_retries.min(32);
            }
            if spec.base_delay_secs > 0 {
                policy.base_delay = Duration::from_secs(spec.base_delay_secs);
            }
            if spec.max_delay_secs > 0 {
                policy.max_delay = Duration::from_secs(spec.max_delay_secs);
            }
            if spec.idle_timeout_secs > 0 {
                policy.idle_timeout = Duration::from_secs(spec.idle_timeout_secs);
            }
        }
        policy
    }

    /// 第 `attempt` 次失败后的等待时长（attempt 从 1 计）：
    /// `base_delay * 2^(attempt-1)`，±10% jitter，封顶 `max_delay`。
    pub fn delay_for(&self, attempt: u32) -> Duration {
        let base_ms = self.base_delay.as_millis() as u64;
        // BUG-2026-09-13-02：attempt 无上限时 2u64.pow 会溢出——debug panic /
        // release 回绕为 0（退避塌缩 → 重试风暴）。checked_pow 饱和到 u64::MAX，
        // 下游 saturating_mul + min(max_delay) 正确封顶。
        let mult = 2u64
            .checked_pow(attempt.saturating_sub(1))
            .unwrap_or(u64::MAX);
        let jittered = jitter_ms(base_ms.saturating_mul(mult));
        Duration::from_millis(jittered.min(self.max_delay.as_millis() as u64))
    }
}

#[cfg(test)]
fn backoff_delay(attempt: u32) -> Duration {
    RetryPolicy::default().delay_for(attempt)
}

/// 返回 `base_ms * U(0.9, 1.1)`，不引入 rand 依赖（纳秒时钟低位已足够打散）。
fn jitter_ms(base_ms: u64) -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    // factor ∈ [0.900, 1.100]
    let factor = ((nanos % 201) as f64) / 1000.0 + 0.9;
    ((base_ms as f64) * factor) as u64
}

pub(crate) fn http_error_description(status: u16) -> &'static str {
    match status {
        400 => "Bad Request — 格式错误",
        401 => "Unauthorized — API key 无效",
        402 => "Payment Required — 余额不足",
        422 => "Unprocessable — 参数错误",
        429 => "Rate Limit — 请求速率超限",
        500 => "Internal Error — 服务器故障",
        503 => "Service Unavailable — 服务器繁忙",
        _ => "Unknown",
    }
}

/// 空请求保护（流式收口）：投影后无任何可发送内容（如 responses 的 `input: []`）时
/// 发一个空 assistant 的 Done，让 runtime 正常完成本回合（而不是把 400 当 Fatal）。
/// 与 [`empty_request_sync_error`] 同族语义——本地短路，**零 HTTP 请求**。
pub(crate) fn empty_request_noop_done_event() -> StreamEvent {
    StreamEvent::Done {
        raw_message: Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: Vec::new(),
        },
        usage: None,
        // 非掐流：`Some` 让 runtime 的"不完整回合续写"判定放行。
        stop_reason: Some("stop".into()),
    }
}

/// 空请求保护（sync 路径）：sync 没有流式收口可用，返回可诊断错误而不是
/// 拿空数组去换上游 400。
pub(crate) fn empty_request_sync_error() -> String {
    "empty_request: 本次请求无任何可投影内容（input 为空），已本地短路；未向上游发送空请求"
        .to_string()
}

pub(crate) fn normalize_skill_envelope(
    provider: &ProviderConfig,
    mut messages: Vec<Message>,
) -> Result<Vec<Message>, String> {
    let is_envelope = messages.last().is_some_and(|message| {
        message.role == "system" && message.content.iter().any(|block| {
            matches!(block, ContentBlock::Text { text } if text.starts_with("<skill_context_envelope"))
        })
    });
    if !is_envelope || provider.supports_tail_system {
        return Ok(messages);
    }
    let envelope = messages.pop().expect("checked last message");
    let dynamic_slot = messages
        .iter()
        .take_while(|message| message.role == "system")
        .count();
    messages.insert(dynamic_slot, envelope);
    log::warn!("skill context moved to head dynamic system slot; prompt-prefix cache degraded");
    Ok(messages)
}

/// `QAQH_SSE_TRACE=<path>`：将 gate 派生的每个流式事件按到达序追加写入文件
/// （`<seq>\t<自流起始的毫秒>\t<类型>\t<长度>`），用于核对 reasoning/content/tool
/// 在链路的忠实流转与先后顺序，并诊断高速流下的吞吐塌陷/排流模式
/// （相邻行间隔突增 = 上游断供或本地背压）。不设该变量时零开销。
pub(crate) struct SseTrace {
    pub(crate) file: Option<std::fs::File>,
    pub(crate) seq: u64,
    /// 流起始时刻：逐事件记录耗时，诊断高速流掐流前的吞吐模式。
    start: std::time::Instant,
}

impl SseTrace {
    pub(crate) fn from_env() -> Self {
        let file = std::env::var_os("QAQH_SSE_TRACE").and_then(|path| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
        });
        Self {
            file,
            seq: 0,
            start: std::time::Instant::now(),
        }
    }
    pub(crate) fn record(&mut self, event: &StreamEvent) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        use std::io::Write;
        let tag = match event {
            StreamEvent::ReasoningDelta(d) => format!("reasoning\t{}", d.chars().count()),
            StreamEvent::ContentDelta(d) => format!("content\t{}", d.chars().count()),
            StreamEvent::ToolCallProgress { .. } => "tool_call_progress".to_string(),
            StreamEvent::Done { .. } => "done".to_string(),
            StreamEvent::UsageUpdate(_) => "usage".to_string(),
            StreamEvent::WebSearchStatus(_) => "web_search_status".to_string(),
            _ => "other".to_string(),
        };
        let _ = writeln!(
            file,
            "{}\t{}\t{}",
            self.seq,
            self.start.elapsed().as_millis(),
            tag
        );
        self.seq += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_exponentially_with_cap() {
        let d1 = backoff_delay(1);
        let d2 = backoff_delay(2);
        let d3 = backoff_delay(3);
        // 基数 1s + jitter：期望值区间 [0.9, 1.1] × 2^(n-1) s
        assert!(d1 >= Duration::from_millis(900) && d1 <= Duration::from_millis(1100));
        assert!(d2 >= Duration::from_millis(1800) && d2 <= Duration::from_millis(2200));
        assert!(d3 >= Duration::from_millis(3600) && d3 <= Duration::from_millis(4400));
        // 高次 attempt 封顶 30s
        let dmax = backoff_delay(20);
        assert!(dmax <= Duration::from_secs(30));
    }

    #[test]
    fn retry_policy_delay_doubles_and_caps() {
        let policy = RetryPolicy {
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(5),
            ..Default::default()
        };
        let d1 = policy.delay_for(1);
        let d2 = policy.delay_for(2);
        assert!(d1 >= Duration::from_millis(900) && d1 <= Duration::from_millis(1100));
        assert!(d2 >= Duration::from_millis(1800) && d2 <= Duration::from_millis(2200));
        // 封顶：8s 基数被 max_delay=5s 截断（含 jitter 上界）。
        let d3 = policy.delay_for(4);
        assert!(d3 <= Duration::from_millis(5500));
    }

    // BUG-2026-09-13-02 回归：attempt 极大时不得 panic（debug）也不得塌缩到
    // 0（release 回绕），必须被 max_delay 正确封顶。
    #[test]
    fn delay_for_overflow_attempt_saturates_instead_of_panic_or_collapse() {
        let policy = RetryPolicy {
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            ..Default::default()
        };
        // 2^64 起全部溢出（首爆点 attempt=65）；u64::pow 在 debug 下 panic、
        // release 下回绕——修复后此处必须返回封顶值。
        let d65 = policy.delay_for(65);
        assert!(d65 <= Duration::from_secs(30));
        assert!(d65 >= Duration::from_secs(27));
        let d = policy.delay_for(66);
        assert!(d <= Duration::from_secs(30));
        assert!(d >= Duration::from_secs(27));
    }

    #[test]
    fn from_spec_caps_max_retries() {
        let spec = qaqh_types::RetrySpec {
            max_retries: 9999,
            ..Default::default()
        };
        let policy = RetryPolicy::from_spec(Some(&spec));
        assert_eq!(policy.max_retries, 32);
    }

    // ---- BUG-2026-09-13-21：Duration 减法下溢防护回归 ----

    /// 复刻竞争窗口：`while start.elapsed() < delay` 守卫与 `delay - elapsed`
    /// 是两次独立采样，守卫通过后 elapsed 越过 delay 时旧的裸减法会 panic
    /// （`overflow when subtracting durations`）。用「两次采样」测试模型直接
    /// 命中该结构：守卫采样恰好通过，减法采样被推过 delay。
    #[test]
    fn sleep_with_cancel_tolerates_elapsed_past_delay() {
        fn guard_passes(guard_sample: Duration, delay: Duration) -> bool {
            guard_sample < delay
        }
        fn remaining_checked(sub_sample: Duration, delay: Duration) -> Duration {
            delay.checked_sub(sub_sample).unwrap_or(Duration::ZERO)
        }

        let delay = Duration::from_millis(1);
        let guard_sample = Duration::from_micros(999);
        // 守卫通过（第一次采样）
        assert!(guard_passes(guard_sample, delay));
        // 竞争夺取：减法用的第二次采样已越过 delay
        let sub_sample = Duration::from_millis(2);
        // 修复前 `delay - sub_sample` 在此 panic，修复后饱和为 ZERO
        assert_eq!(remaining_checked(sub_sample, delay), Duration::ZERO);

        // 端到端：真实函数在 delay 极小、必然越界的场景下不 panic 且正常返回
        let cancel = Arc::new(AtomicBool::new(false));
        assert!(!sleep_with_cancel(Duration::from_nanos(1), Some(&cancel)));
    }

    /// 过期 / 零值 delay 防御：不 panic，且不误报取消。
    #[test]
    fn sleep_with_cancel_tolerates_expired_delay() {
        let cancel = Arc::new(AtomicBool::new(false));
        assert!(!sleep_with_cancel(Duration::ZERO, Some(&cancel)));
        assert!(!sleep_with_cancel(Duration::ZERO, None));
    }

    /// 高频防抖：极小 delay 反复调用，暴露任何残留的窗口下溢。
    #[test]
    fn sleep_with_cancel_repeatedly_does_not_panic() {
        let cancel = Arc::new(AtomicBool::new(false));
        for _ in 0..64 {
            assert!(!sleep_with_cancel(Duration::from_nanos(1), Some(&cancel)));
        }
    }
}
