//! gate::transport — 三协议共享的传输零件（Phase 3-1 收敛）。
//!
//! `chat_completions_api` / `message_api` / `responses_api` 曾各持一份字节级
//! 相同的实现：3 个独立 current-thread tokio runtime、cancel 轮询、重试退避、
//! 错误描述、skill envelope 归一、stateful 过滤、`SseTrace` 诊断。本模块是其
//! 单一来源；协议本质差异（convert_messages ×3、帧处理 ×3、convert_tools ×3）
//! 保留在各协议文件。
//!
//! 收敛时消除的行为漂移：
//! - 3 个独立 runtime → 1 个共享 runtime（此前三协议各建各的 current-thread RT）；
//! - `responses_api` 的内联 `2u64.pow(attempt)` 无 30s 上限 → 统一走
//!   `backoff_delay`（`BASE_DELAY_SECS * 2^(attempt-1)`，上限 30s）；
//! - `chat_completions_api::filter_stateful_messages` 在 release 也打
//!   `eprintln!("[filter] 输出…")` → 随统一删除（stderr 污染缺陷，见 Phase 4）。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use qaqh_types::{ContentBlock, Message};

use super::types::{ProviderConfig, StreamEvent};

/// SSE 轮询间隔：无数据到达时以外层 Tokio timeout 检查 cancel 标志。
pub(crate) const SSE_POLL_INTERVAL: Duration = Duration::from_millis(50);

// 与官方客户端会话重试策略对齐（opencode session/retry.ts：5 次重试、
// 2s 初始、翻倍），吸收网关瞬时 5xx  burst。
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
        let remaining = delay - start.elapsed();
        std::thread::sleep(remaining.min(Duration::from_millis(100)));
    }
    false
}

pub(crate) fn is_retryable(status: u16) -> bool {
    matches!(status, 429 | 500 | 503)
}

/// 每端点重试策略（对齐 codex `ModelProviderInfo` 的 request_max_retries /
/// stream_idle_timeout 三元组）。T9 起由 `EndpointSpec` 的 `RetrySpec`（TOML）
/// 编译而来；缺省值即现行全局常量，行为零变化。
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
                policy.max_retries = spec.max_retries;
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
        let mult = 2u64.pow(attempt.saturating_sub(1));
        let jittered = jitter_ms(base_ms.saturating_mul(mult));
        Duration::from_millis(jittered.min(self.max_delay.as_millis() as u64))
    }
}

/// 一次尝试的分类结果（[`run_with_retry`] 闭包的返回值）。
pub(crate) enum Attempt<T> {
    /// 成功，终止重试并返回。
    Ok(T),
    /// 可重试失败。`retry_after`（服务端 retry-after 头）优先于本地退避；
    /// `reason` 进入 Retrying 事件；`final_error` 在重试额度耗尽时由执行器
    /// 发出 Error 事件并作为终态错误返回。
    Retry {
        retry_after: Option<Duration>,
        reason: String,
        final_error: String,
    },
    /// 不可重试——立即传播。闭包自行负责（按既有路径）Error 事件发射。
    Fatal(anyhow::Error),
}

/// 统一重试执行器（对齐 codex 的重试循环形态）：闭包只做"一次尝试"并分类
/// 结果；计数、取消检查、退避计算（retry-after 优先）、Retrying 事件与可
/// 取消睡眠全部集中在此。三协议流式循环与 `chat_sync_*` 共用，T9 起策略
/// 可按端点从 TOML 注入。
pub(crate) fn run_with_retry<T, F>(
    policy: &RetryPolicy,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut dyn FnMut(StreamEvent),
    mut attempt_fn: F,
) -> anyhow::Result<T>
where
    F: FnMut(u32, &mut dyn FnMut(StreamEvent)) -> Attempt<T>,
{
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        if is_cancelled(cancel) {
            return Err(anyhow::anyhow!("cancelled by user"));
        }
        match attempt_fn(attempt, &mut *on_event) {
            Attempt::Ok(value) => return Ok(value),
            Attempt::Fatal(e) => return Err(e),
            Attempt::Retry {
                retry_after,
                reason,
                final_error,
            } => {
                if attempt >= policy.max_retries {
                    on_event(StreamEvent::Error(final_error.clone()));
                    return Err(anyhow::anyhow!("{}", final_error));
                }
                let delay = retry_after.unwrap_or_else(|| policy.delay_for(attempt));
                on_event(StreamEvent::Retrying {
                    attempt,
                    max_retries: policy.max_retries,
                    delay_secs: delay.as_secs(),
                    error: reason,
                });
                if sleep_with_cancel(delay, cancel) {
                    return Err(anyhow::anyhow!("cancelled by user"));
                }
            }
        }
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

/// 解析上游限流头（opencode retry.ts 同款）：`retry-after-ms`（毫秒）优先，
/// 其次 `retry-after`（秒或 HTTP-date）。`None` = 无可用头，退回指数退避。
pub(crate) fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    if let Some(v) = headers.get("retry-after-ms")
        && let Ok(s) = v.to_str()
        && let Ok(ms) = s.trim().parse::<u64>()
    {
        return Some(Duration::from_millis(ms));
    }
    let v = headers.get("retry-after")?.to_str().ok()?;
    let s = v.trim();
    if let Ok(secs) = s.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    // HTTP-date 形式：取与当前时间的正差，已过期/时钟回拨则为 0（立即重试）。
    let target = httpdate::parse_http_date(s).ok()?;
    let delta = target
        .duration_since(std::time::SystemTime::now())
        .unwrap_or(Duration::ZERO);
    Some(delta)
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

pub(crate) fn filter_stateful_messages(messages: Vec<Message>) -> (Vec<Message>, usize) {
    if messages.is_empty() {
        return (messages, 0);
    }
    let last_asst_idx = messages.iter().rposition(|m| m.role == "assistant");
    let start = last_asst_idx.map(|i| i + 1).unwrap_or(0);
    let is_first = start == 0;
    if is_first {
        return (messages, 0);
    }
    let dropped_images = messages[..start]
        .iter()
        .flat_map(|m| m.content.iter())
        .filter(|b| {
            matches!(
                b,
                ContentBlock::Image { .. } | ContentBlock::ImageRef { .. }
            )
        })
        .count();
    let mut out: Vec<Message> = Vec::new();
    for msg in &messages[start..] {
        out.push(msg.clone());
    }
    if out.is_empty()
        && let Some(last) = messages.last()
        && last.role != "assistant"
    {
        out.push(last.clone());
    }
    (out, dropped_images)
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
    if provider.stateful {
        return Err("SKILL_CONTEXT_SYNC_UNSUPPORTED: stateful provider cannot accept the authoritative tail system envelope; rebuild the remote session with a compatible provider".into());
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
    fn parse_retry_after_prefers_ms_header() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("retry-after-ms", "250".parse().unwrap());
        h.insert("retry-after", "999".parse().unwrap());
        assert_eq!(parse_retry_after(&h), Some(Duration::from_millis(250)));
    }

    #[test]
    fn parse_retry_after_seconds_header() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("retry-after", "7".parse().unwrap());
        assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(7)));
    }

    #[test]
    fn parse_retry_after_http_date_future_and_past() {
        // 未来 1 小时：应得到正的、不超过 1 小时的时长
        let future = std::time::SystemTime::now() + Duration::from_secs(3600);
        let future_http = httpdate::fmt_http_date(future);
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("retry-after", future_http.parse().unwrap());
        let d = parse_retry_after(&h).expect("future date should parse");
        assert!(d > Duration::from_secs(3500) && d <= Duration::from_secs(3600));

        // 过去时间：立即重试（0）
        let past = std::time::SystemTime::now() - Duration::from_secs(3600);
        let mut h2 = reqwest::header::HeaderMap::new();
        h2.insert("retry-after", httpdate::fmt_http_date(past).parse().unwrap());
        assert_eq!(parse_retry_after(&h2), Some(Duration::ZERO));
    }

    #[test]
    fn parse_retry_after_missing_header_returns_none() {
        let h = reqwest::header::HeaderMap::new();
        assert_eq!(parse_retry_after(&h), None);
    }

    // ── run_with_retry ──

    /// 静默事件收集器（sync 测试共用）。
    fn silent_events() -> impl FnMut(StreamEvent) {
        |_e: StreamEvent| {}
    }

    #[test]
    fn run_with_retry_succeeds_after_transient_failures() {
        let policy = RetryPolicy {
            base_delay: Duration::ZERO,
            ..Default::default()
        };
        let mut on_event = silent_events();
        let mut calls = 0u32;
        let result = run_with_retry(&policy, None, &mut on_event, |_attempt, _ev| {
            calls += 1;
            if calls < 3 {
                Attempt::Retry {
                    retry_after: Some(Duration::ZERO),
                    reason: "transient".into(),
                    final_error: "gave up".into(),
                }
            } else {
                Attempt::Ok(calls)
            }
        });
        assert_eq!(result.unwrap(), 3);
        assert_eq!(calls, 3);
    }

    #[test]
    fn run_with_retry_exhausts_budget_and_returns_final_error() {
        let policy = RetryPolicy {
            max_retries: 2,
            base_delay: Duration::ZERO,
            ..Default::default()
        };
        let mut events: Vec<StreamEvent> = Vec::new();
        let mut collect = |e: StreamEvent| events.push(e);
        let result: anyhow::Result<()> =
            run_with_retry(&policy, None, &mut collect, |attempt, _ev| {
                let _ = attempt;
                Attempt::Retry {
                    retry_after: None,
                    reason: "boom".into(),
                    final_error: "final failure".into(),
                }
            });
        assert!(result.is_err());
        // attempt=1 重试 + attempt=2 重试额度耗尽 → Error 事件两支各一（耗尽支）。
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::Error(m) if m.contains("final failure")))
        );
    }

    #[test]
    fn run_with_retry_fatal_propagates_immediately() {
        let policy = RetryPolicy::default();
        let mut on_event = silent_events();
        let mut calls = 0u32;
        let result: anyhow::Result<u32> =
            run_with_retry(&policy, None, &mut on_event, |_attempt, _ev| {
                calls += 1;
                Attempt::<u32>::Fatal(anyhow::anyhow!("hard stop"))
            });
        assert_eq!(calls, 1, "Fatal must not consume another attempt");
        assert!(result.unwrap_err().to_string().contains("hard stop"));
    }

    #[test]
    fn run_with_retry_respects_cancellation() {
        let policy = RetryPolicy {
            base_delay: Duration::ZERO,
            ..Default::default()
        };
        let cancel = Arc::new(AtomicBool::new(true));
        let mut on_event = silent_events();
        let result: anyhow::Result<()> =
            run_with_retry(&policy, Some(&cancel), &mut on_event, |_, _| {
            Attempt::Retry {
                retry_after: None,
                reason: "x".into(),
                final_error: "x".into(),
            }
        });
        // 已取消状态下首次进入循环即退出。
        assert!(result.unwrap_err().to_string().contains("cancelled"));
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
}
