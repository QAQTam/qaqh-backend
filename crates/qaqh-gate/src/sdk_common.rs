//! Shared machinery for the `mutil-ai`-powered transports
//! (`anthropic_sdk`, `openai_sdk`). Kept separate from `transport.rs`
//! (the legacy hand-written stack) so the two can coexist during the
//! strangler migration and the legacy half can be deleted wholesale.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mutil_ai::{
    AuditEvent, AuditSink, CancellationToken, Error as SdkError, ErrorKind, RequestOptions,
    RetryPolicy as SdkRetryPolicy,
};
use reqwest::header::{HeaderName, HeaderValue};

use qaqh_types::{ContentBlock, Message, UsageInfo};

use super::transport::{
    RetryPolicy as GateRetryPolicy, SseTrace, http_error_description, is_cancelled,
    sleep_with_cancel,
};
use super::types::{ProviderConfig, StreamEvent, safe_provider_error_body};

/// Result of one whole-request attempt against the SDK stream.
pub(crate) enum Outcome {
    Completed,
    /// 2xx 但上游在未产出任何内容时掐流（旧 `EmptyStreamEof` 哨兵的同义）：
    /// 整请求重试无损。
    EmptyStream,
    Fatal(anyhow::Error),
}

/// Accumulating buffers behind the unified `StreamEvent` contract. The gate
/// assembles `Done.raw_message` from these buffers (not from the SDK
/// response) so the anthropic/chat paths keep byte-identical semantics,
/// including the partial-keep path on mid-stream interruption.
pub(crate) struct StreamState {
    pub text: String,
    pub reasoning: String,
    pub tools: HashMap<usize, (String, String, String)>,
    pub usage: Option<UsageInfo>,
    pub stop: Option<String>,
    pub responded: bool,
}

impl StreamState {
    pub(crate) fn new() -> Self {
        Self {
            text: String::new(),
            reasoning: String::new(),
            tools: HashMap::new(),
            usage: None,
            stop: None,
            responded: false,
        }
    }

    pub(crate) fn had_output(&self) -> bool {
        !self.text.is_empty() || !self.reasoning.is_empty() || !self.tools.is_empty()
    }

    /// 组装 `Done` 事件。`synth_missing_tool_ids`：anthropic 路径为空 id 合成
    /// `toolu_…`（旧行为）；chat 路径保持原样（旧行为同样不合成）。
    pub(crate) fn finalize(
        self,
        synth_missing_tool_ids: bool,
        on_event: &mut dyn FnMut(StreamEvent),
    ) {
        let mut blocks: Vec<ContentBlock> = Vec::new();
        if !self.reasoning.is_empty() {
            blocks.push(ContentBlock::Reasoning {
                reasoning: self.reasoning,
            });
        }
        if !self.text.is_empty() {
            blocks.push(ContentBlock::text(&self.text));
        }
        let mut sorted: Vec<(usize, String, String, String)> = self
            .tools
            .into_iter()
            .map(|(idx, (id, name, buffer))| (idx, id, name, buffer))
            .collect();
        sorted.sort_by_key(|(idx, _, _, _)| *idx);
        for (_idx, id, name, args_json) in sorted {
            let input: serde_json::Value =
                serde_json::from_str(&args_json).unwrap_or(serde_json::Value::Null);
            // 空/非法参数收敛为 `{}`：掐流时 `"arguments": "null"` 会被端点判 400
            // （T-7-1 / BUG-2026-09-13-13）。
            let input = if input.is_null() {
                serde_json::json!({})
            } else {
                input
            };
            let id = if id.is_empty() && synth_missing_tool_ids {
                format!("toolu_{}", uuid_simple())
            } else {
                id
            };
            blocks.push(ContentBlock::ToolUse { id, name, input });
        }
        on_event(StreamEvent::Done {
            raw_message: Message {
                msg_id: None,
                role: "assistant".into(),
                name: None,
                content: blocks,
            },
            usage: self.usage,
            stop_reason: self.stop,
        });
    }
}

/// 合成缺失的 tool-call id（`toolu_…` 形态）。Anthropic 掐流与 Gemini
/// （协议本身不带 id）两条路径共用。
pub(crate) fn uuid_simple() -> String {
    use std::hash::{Hash, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Split an absolute request URL into (origin, path) for
/// `mutil_ai::EndpointSpec{base_url, path}`.
pub(crate) fn split_url(full_url: &str) -> (String, String) {
    if let Some(after) = full_url.find("://") {
        let authority_start = after + 3;
        let rest = &full_url[authority_start..];
        match rest.find('/') {
            Some(path_start) if path_start > 0 => (
                full_url[..authority_start + path_start].to_string(),
                rest[path_start..].to_string(),
            ),
            _ => (full_url.to_string(), "/".to_string()),
        }
    } else {
        (full_url.to_string(), "/".to_string())
    }
}

/// SDK error → the bounded, log-safe (msg, full-detail) pair the harness
/// displays. HTTP-level retries are the SDK's job by now; this is the
/// terminal shape after the SDK's budget is spent.
pub(crate) fn sdk_error_detail(label: &str, e: &SdkError, api_key: &str) -> (String, String) {
    match e.status() {
        Some(status) => {
            let msg = format!(
                "{label} API HTTP {status} ({})",
                http_error_description(status)
            );
            let detail = if status == 401 {
                "authentication failed".to_string()
            } else {
                let fallback = e.to_string();
                let body = e.raw_body().unwrap_or(fallback.as_str());
                safe_provider_error_body(body, api_key)
            };
            (msg.clone(), format!("{msg}: {detail}"))
        }
        None => {
            let msg = format!("{label} API error: {e}");
            (msg.clone(), msg)
        }
    }
}

/// Bridges the harness `AtomicBool` cancel flag to the SDK
/// `CancellationToken` (which its backoff sleeps and stream reads observe).
/// The watcher only needs to run inside the crate `block_on` runtime; it is
/// aborted when the guard drops.
pub(crate) struct CancelWatch {
    token: CancellationToken,
    done: Arc<AtomicBool>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl CancelWatch {
    pub(crate) fn start(cancel: Option<&Arc<AtomicBool>>) -> Self {
        let token = CancellationToken::new();
        let done = Arc::new(AtomicBool::new(false));
        let handle = cancel.cloned().map(|flag| {
            let token = token.clone();
            let done = done.clone();
            tokio::spawn(async move {
                loop {
                    if done.load(Ordering::Relaxed) {
                        break;
                    }
                    if flag.load(Ordering::Relaxed) {
                        token.cancel();
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
        });
        Self {
            token,
            done,
            handle,
        }
    }

    pub(crate) fn token(&self) -> &CancellationToken {
        &self.token
    }
}

impl Drop for CancelWatch {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }
}

/// Forwards the SDK's pre-connect retry scheduling (429/5xx/transport
/// jitter) into the harness `StreamEvent::Retrying` contract. The SDK's
/// stream carries `Retrying` only for in-stream reconnects; whole-request
/// retries are observable through the audit sink, which is the supported
/// seam for it.
pub(crate) struct RetryHub {
    max_retries: u32,
    pending: Mutex<VecDeque<StreamEvent>>,
}

impl RetryHub {
    pub(crate) fn new(max_retries: u32) -> Self {
        Self {
            max_retries,
            pending: Mutex::new(VecDeque::new()),
        }
    }

    pub(crate) fn drain(&self, on_event: &mut dyn FnMut(StreamEvent)) {
        while let Some(ev) = self
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pop_front()
        {
            on_event(ev);
        }
    }
}

impl AuditSink for RetryHub {
    fn record(&self, event: AuditEvent) {
        if let AuditEvent::RetryScheduled {
            attempt,
            delay,
            error_kind,
            ..
        } = event
        {
            let error = match error_kind {
                Some(kind) => format!("{kind:?}"),
                None => "transient error".to_string(),
            };
            self.pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push_back(StreamEvent::Retrying {
                    attempt,
                    max_retries: self.max_retries,
                    delay_secs: delay.as_secs(),
                    error,
                });
        }
    }
}

/// Gate `RetrySpec`/defaults → SDK retry policy. `require_idempotency_key`
/// must stay off: harness requests carry no idempotency key, and the SDK
/// default would downgrade such requests to a single attempt.
pub(crate) fn sdk_retry_policy(gate: &GateRetryPolicy) -> SdkRetryPolicy {
    SdkRetryPolicy {
        max_attempts: gate.max_retries,
        base_delay: gate.base_delay,
        max_delay: gate.max_delay,
        jitter_ratio: 0.1,
        // 对齐退役前的 retry-after 信任上限：5×max_delay，超限由 SDK 放弃重试。
        max_retry_after: gate.max_delay.saturating_mul(5),
        require_idempotency_key: false,
    }
}

pub(crate) fn insert_header(options: &mut RequestOptions, name: &str, value: &str) {
    let (Some(name), Ok(value)) = (
        HeaderName::from_bytes(name.as_bytes()).ok(),
        HeaderValue::from_str(value),
    ) else {
        log::warn!("gate sdk: header {name} rejected (non-ASCII value or invalid name)");
        return;
    };
    options.headers.insert(name, value);
}

/// OpenCode gateway management headers (`x-opencode-*` + UA override).
/// `session_headers` additionally attaches the bun-gateway `X-Session-Id` /
/// legacy `X-Task-Id` passthrough (anthropic path only).
pub(crate) fn apply_management_headers(
    provider: &ProviderConfig,
    user_id: Option<&str>,
    options: &mut RequestOptions,
    session_headers: bool,
) {
    if session_headers && let Some(sid) = user_id.filter(|s| !s.is_empty()) {
        insert_header(options, "X-Session-Id", sid);
        insert_header(options, "X-Task-Id", sid);
    }
    if let Some(h) = &provider.opencode_headers {
        insert_header(options, "x-opencode-session", &h.session_id);
        insert_header(options, "x-opencode-request", &h.request_id);
        insert_header(
            options,
            "x-opencode-client",
            super::types::OPENCODE_CLIENT_ID,
        );
        options.user_agent = HeaderValue::from_str(&format!(
            "opencode/{}",
            super::types::OPENCODE_CLIENT_VERSION
        ))
        .ok();
    }
}

/// Retry driver for the one error class the SDK does not cover: a 2xx
/// response whose stream dies before any content (busy-shedding endpoints
/// return no error code). Everything else — 429/5xx, transport jitter,
/// Retry-After — is retried inside the SDK, which emits the `Retrying`
/// events through `on_event`.
pub(crate) fn empty_stream_retry(
    policy: &GateRetryPolicy,
    final_error: &str,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut dyn FnMut(StreamEvent),
    mut run_attempt: impl FnMut(&mut dyn FnMut(StreamEvent)) -> Outcome,
) -> anyhow::Result<()> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        if is_cancelled(cancel) {
            return Err(anyhow::anyhow!("cancelled by user"));
        }
        let mut trace = SseTrace::from_env();
        let callback = on_event as *mut dyn FnMut(StreamEvent);
        // SAFETY: trace wrapper borrows callback for duration of this fn
        let mut traced = |event: StreamEvent| {
            trace.record(&event);
            unsafe { (*callback)(event) };
        };
        match run_attempt(&mut traced) {
            Outcome::Completed => return Ok(()),
            Outcome::Fatal(e) => return Err(e),
            Outcome::EmptyStream => {
                if attempt >= policy.max_retries {
                    traced(StreamEvent::Error {
                        // 上游 2xx 后零内容掐流：不是传输抖动（SDK 已重试过那些），
                        // 而是这次流式响应本身没有可用终态。
                        kind: ErrorKind::StreamProtocol,
                        message: final_error.to_string(),
                    });
                    return Err(anyhow::anyhow!("{final_error}"));
                }
                let delay = policy.delay_for(attempt);
                traced(StreamEvent::Retrying {
                    attempt,
                    max_retries: policy.max_retries,
                    delay_secs: delay.as_secs(),
                    error: "stream closed early (no content)".into(),
                });
                if sleep_with_cancel(delay, cancel) {
                    return Err(anyhow::anyhow!("cancelled by user"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_url_keeps_gateway_path_prefixes() {
        assert_eq!(
            split_url("https://api.deepseek.com/v1/chat/completions"),
            (
                "https://api.deepseek.com".to_string(),
                "/v1/chat/completions".to_string()
            )
        );
        assert_eq!(
            split_url("http://localhost:8317/anthropic/v1/messages"),
            (
                "http://localhost:8317".to_string(),
                "/anthropic/v1/messages".to_string()
            )
        );
        assert_eq!(
            split_url("https://host.com"),
            ("https://host.com".to_string(), "/".to_string())
        );
    }

    #[test]
    fn finalize_converges_null_args_to_empty_object_and_sorts_by_index() {
        let mut state = StreamState::new();
        state.text = "hi".into();
        state
            .tools
            .insert(1, ("b".into(), "exec".into(), String::new()));
        state
            .tools
            .insert(0, ("a".into(), "read".into(), "{\"p\":1}".into()));
        let mut events = Vec::new();
        state.finalize(false, &mut |e| events.push(e));
        let StreamEvent::Done { raw_message, .. } = &events[0] else {
            panic!("expected Done");
        };
        // [Text, ToolUse(a), ToolUse(b)] — reasoning absent, tools index-sorted,
        // empty args converged to {}.
        match &raw_message.content[1] {
            ContentBlock::ToolUse { id, input, .. } => {
                assert_eq!(id, "a");
                assert_eq!(input, &serde_json::json!({"p": 1}));
            }
            other => panic!("unexpected {other:?}"),
        }
        match &raw_message.content[2] {
            ContentBlock::ToolUse { id, input, .. } => {
                assert_eq!(id, "b");
                assert_eq!(input, &serde_json::json!({}));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn finalize_synthesizes_ids_only_when_asked() {
        let mut state = StreamState::new();
        state
            .tools
            .insert(0, (String::new(), "exec".into(), "{}".into()));
        let mut events = Vec::new();
        state.finalize(true, &mut |e| events.push(e));
        let StreamEvent::Done { raw_message, .. } = &events[0] else {
            panic!("expected Done");
        };
        let ContentBlock::ToolUse { id, .. } = &raw_message.content[0] else {
            panic!();
        };
        assert!(id.starts_with("toolu_"), "anthropic parity: {id}");

        let mut state = StreamState::new();
        state
            .tools
            .insert(0, (String::new(), "exec".into(), "{}".into()));
        let mut events = Vec::new();
        state.finalize(false, &mut |e| events.push(e));
        let StreamEvent::Done { raw_message, .. } = &events[0] else {
            panic!("expected Done");
        };
        let ContentBlock::ToolUse { id, .. } = &raw_message.content[0] else {
            panic!();
        };
        assert_eq!(id, "", "chat parity: id stays as streamed");
    }

    #[test]
    fn error_detail_redacts_key_and_caps_body() {
        let long_body = format!("boom {}", "x".repeat(400));
        let e = SdkError::Api {
            provider: "openai",
            status: 503,
            body: format!("{} sk-secret-leak", long_body),
            retry_after: None,
            retry_source: None,
            request_id: None,
        };
        let (msg, full) = sdk_error_detail("OpenAI", &e, "sk-secret-leak");
        assert!(msg.contains("HTTP 503"), "{msg}");
        assert!(!full.contains("sk-secret-leak"), "api key must be redacted");
        assert!(
            full.chars().count() < 300,
            "detail must stay bounded: {}",
            full.chars().count()
        );
    }
}
