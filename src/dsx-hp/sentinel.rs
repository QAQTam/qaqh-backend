//! Sentinel — API error classification + retry strategy.
//!
//! Watches API responses: classifies HTTP errors, tracks error rates,
//! manages retry backoff. Ported from the retry loop in `api.rs`
//! and `record_api_error`/`record_api_success` in `health/mod.rs`.

/// Classification of an API error response.
#[derive(Debug, Clone, PartialEq)]
pub enum ApiErrorClass {
    /// 400/401/402/403 — client-side error, do not retry.
    ClientFatal(u16),
    /// 500/503 — server-side error, retry with backoff.
    ServerRetryable(u16),
    /// 429 — rate limited, retry after a delay.
    RateLimited,
    /// Connection timeout / DNS failure.
    NetworkError,
}

/// Persistent state for the sentinel.
#[derive(Debug, Clone)]
pub struct SentinelState {
    pub consecutive_api_errors: u32,
    pub last_error: Option<(ApiErrorClass, String)>,
    pub retry_count: u32,
    pub max_retries: u32,
}

impl Default for SentinelState {
    fn default() -> Self {
        Self {
            consecutive_api_errors: 0,
            last_error: None,
            retry_count: 0,
            max_retries: 10,
        }
    }
}

/// Decision returned by `should_retry` / `record_error`.
#[derive(Debug, Clone, PartialEq)]
pub enum SentinelResult {
    /// Proceed — no action needed.
    Pass,
    /// Should retry after the given number of seconds.
    Retry { after_secs: u64, reason: String },
    /// Should not retry — the error is fatal or retries exhausted.
    Block { permanent: bool, reason: String },
}

/// Classify an HTTP status code into an `ApiErrorClass`.
///
/// Ported from the status-code branches in `api.rs` `chat_stream`.
pub fn classify_error(status_code: u16, _body: &str) -> ApiErrorClass {
    match status_code {
        400 | 401 | 402 | 403 => ApiErrorClass::ClientFatal(status_code),
        429 => ApiErrorClass::RateLimited,
        500 | 503 => ApiErrorClass::ServerRetryable(status_code),
        code if code >= 500 => ApiErrorClass::ServerRetryable(code),
        _ => ApiErrorClass::ClientFatal(status_code),
    }
}

/// Exponential backoff based on retry count.
/// Mirrors the timing used in `api.rs` `chat_stream`.
fn backoff_seconds(retry_count: u32) -> u64 {
    match retry_count {
        0..=3 => 0,
        4..=8 => 10,
        _ => 30,
    }
}

/// Decide whether to retry given the current state and error class.
pub fn should_retry(state: &SentinelState, class: &ApiErrorClass) -> SentinelResult {
    if state.retry_count >= state.max_retries {
        return SentinelResult::Block {
            permanent: true,
            reason: format!("Exceeded max retries ({})", state.max_retries),
        };
    }

    match class {
        ApiErrorClass::ClientFatal(code) => SentinelResult::Block {
            permanent: true,
            reason: format!("Client error {} — not retryable", code),
        },
        ApiErrorClass::ServerRetryable(code) => {
            let after = backoff_seconds(state.retry_count);
            SentinelResult::Retry {
                after_secs: after,
                reason: format!("Server error {} — retry in {}s", code, after),
            }
        }
        ApiErrorClass::RateLimited => {
            // Exponential backoff capped at 30s for rate limits
            let after = (2u64.pow(state.retry_count.min(5))).min(30);
            SentinelResult::Retry {
                after_secs: after,
                reason: format!("Rate limited — retry in {}s", after),
            }
        }
        ApiErrorClass::NetworkError => {
            let after = 1u64.max(state.retry_count as u64);
            SentinelResult::Retry {
                after_secs: after,
                reason: format!("Network error — retry in {}s", after),
            }
        }
    }
}

/// Record a successful API call, resetting retry state.
///
/// Ported from `record_api_success` in `health/mod.rs`.
pub fn record_success(state: &mut SentinelState, model: &str) {
    state.consecutive_api_errors = 0;
    state.retry_count = 0;
    state.last_error = None;
    let _ = model; // reserved for future tracking of successful model
}

/// Record an API error, classify it, and return the retry decision.
///
/// Ported from the error-handling branches in `api.rs` `chat_stream`
/// and `record_api_error` in `health/mod.rs`.
pub fn record_error(state: &mut SentinelState, class: ApiErrorClass, detail: String) -> SentinelResult {
    state.consecutive_api_errors += 1;
    state.retry_count += 1;
    state.last_error = Some((class.clone(), detail));
    should_retry(state, &class)
}
