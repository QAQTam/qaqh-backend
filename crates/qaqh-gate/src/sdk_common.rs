//! Shared machinery for the `mutil-ai`-powered transports
//! (`anthropic_sdk`, `openai_sdk`). Kept separate from `transport.rs`
//! (the legacy hand-written stack) so the two can coexist during the
//! strangler migration and the legacy half can be deleted wholesale.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mutil_ai::{
    AuditEvent, AuditSink, CancellationToken, Error as SdkError, ErrorKind, RequestOptions,
    ResponseLimits, RetryPolicy as SdkRetryPolicy,
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
    /// 首帧思考的**可展示类别**（合并规则见 [`StreamState::push_reasoning`]）。
    pub reasoning_kind: Option<qaqh_types::ReasoningKind>,
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
            reasoning_kind: None,
            tools: HashMap::new(),
            usage: None,
            stop: None,
            responded: false,
        }
    }

    /// 追加一段思考，并记下它的类别。返回 `false` 表示该类别**不可展示**
    /// （密文 / 被安全策略抹除）：调用方不得把它播给下游，它也不该进历史——
    /// 否则 UI 会把 `encrypted_content` 当模型的思考渲染。
    ///
    /// 合并规则 = **首帧可展示类别胜出**：一条 assistant 消息只落一个 reasoning
    /// 块、块上只标一个类别；provider 实践里一轮只发一种（`reasoning_text.delta`
    /// 与 `reasoning_summary_text.delta` 互斥），真出现混流也宁可标先到的那一种，
    /// 而不是在流中途造第二个块——那会让 timeline 上的块与这里落盘的块对不上。
    pub(crate) fn push_reasoning(&mut self, kind: qaqh_types::ReasoningKind, text: &str) -> bool {
        if !kind.is_displayable() {
            return false;
        }
        if self.reasoning_kind.is_none() {
            self.reasoning_kind = Some(kind);
        }
        self.reasoning.push_str(text);
        true
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
                kind: self.reasoning_kind,
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
            // 参数损坏（截断 / delta+快照混流）时保持 `Null` 真值，**不得**
            // 伪造 `{}`：`{}` 会让 typed 工具报「invalid arguments: missing
            // field `x`」假错——模型明明发过参数，却被告知参数缺失，无从
            // 自纠。Null input 会被 runtime 执行界面如实转换为「上游参数流
            // 损坏，未执行」。wire 回放兼容（掐流时 `"arguments": "null"`
            // 会被端点判 400，T-7-1 / BUG-2026-09-13-13）由出站
            // [`wire_tool_input`] 统一收敛，语义层不再掺假。
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

/// 出站 wire 的 tool-call input 收敛：语义层允许 `Null`（= 上游参数不可
/// 还原的真值），但端点不接受 `"arguments": "null"` / `"input": null`
/// （T-7-1 / BUG-2026-09-13-13 会 400）。回放前在此统一折叠成 `{}`——
/// 只动 wire 形态，不动语义层；执行面看到的仍是 Null。
pub(crate) fn wire_tool_input(input: &serde_json::Value) -> serde_json::Value {
    if input.is_null() {
        serde_json::json!({})
    } else {
        input.clone()
    }
}

/// tool-call 身份的单字段收口：非空且槽位还空着才写入（first-writer-wins）。
pub(crate) fn adopt_identity(slot: &mut String, value: Option<&str>) {
    if slot.is_empty() {
        *slot = value.unwrap_or_default().trim().to_string();
    }
}

/// 把「SDK 事件里的 tool-call 身份/参数」收口成 runtime 需要的
/// [`StreamEvent::ToolCallProgress`] 帧序列。
///
/// 为什么需要专门的收口器：SDK 把**块起始身份**（call id + 函数名）和
/// **参数增量**放在不同的事件上，而且不同 wire 的摆放方式不一样——
/// - Anthropic：`ToolCallDelta` 的 `id`/`name` **恒为 `None`**（mutil-ai 只在
///   `content_block_start` 时把身份写进累计器，身份只随
///   `ToolCallProgress` 出来）；
/// - Responses：参数增量帧的 `id` 是 **item id**（`fc_1`），真正的
///   `call_id` 只在块起始帧与 `ToolCallProgress` 里。
///
/// 于是「只盯 `ToolCallDelta`」的写法会得到 `name` 恒空的调用（旧缺陷：
/// runtime 的 `parse_tool_calls` 把无名条目整条丢弃，回合就变成
/// 「模型说了句话、什么都没做、前端也什么都没显示」）；而「谁先到算谁」
/// 又会把 item id 当 call id 播出去，下一轮回放的 `function_call_output`
/// 对不上号。这里统一成两条不变量：
/// 1. **身份齐了才开块**——`id`/`name` 任一为空就不发首帧，runtime 不会
///    建出 `tool:`（空 id）或无名工具的 timeline 块；
/// 2. **参数按字节水位增量发**——同一帧序列里每个字节只投递一次，
///    与累计形态（`arguments_so_far`）混用也不会重复计数。
#[derive(Default)]
pub(crate) struct ToolCallAnnouncer {
    announced: HashSet<usize>,
    watermark: HashMap<usize, usize>,
}

impl ToolCallAnnouncer {
    /// 按当前缓冲通报 `index` 处的 tool call；身份未齐则跳过（等下一帧）。
    /// `tools` 是 `StreamState::tools` / `Resps::tools` 同款三元组缓冲。
    pub(crate) fn announce(
        &mut self,
        tools: &HashMap<usize, (String, String, String)>,
        index: usize,
        on_event: &mut dyn FnMut(StreamEvent),
    ) {
        let Some((id, name, args)) = tools.get(&index) else {
            return;
        };
        let starting = !self.announced.contains(&index);
        if starting && (id.is_empty() || name.is_empty()) {
            return;
        }
        let sent = self.watermark.get(&index).copied().unwrap_or(0);
        // 累计帧可能把缓冲换成更短的串：水位钳到边界，绝不越界切片。
        let sent = sent.min(args.len());
        if !starting && args.len() == sent {
            return;
        }
        let chunk = args[sent..].to_string();
        let (id, name) = (id.clone(), name.clone());
        if starting {
            self.announced.insert(index);
        }
        self.watermark.insert(index, args.len());
        on_event(StreamEvent::ToolCallProgress {
            index,
            id,
            name,
            args_chunk: chunk,
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

/// mutil-ai 0.4.0 matches a tool result to its assistant call by BOTH id and
/// name, and the harness stores the function name only on the assistant
/// `ToolUse` block (`Message::tool_result` leaves `name: None`). Build the
/// `tool_use_id → name` map from history so the projection can fill the result.
pub(crate) fn tool_names_by_id(messages: &[Message]) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for msg in messages {
        if msg.role != "assistant" {
            continue;
        }
        for block in &msg.content {
            if let ContentBlock::ToolUse { id, name, .. } = block {
                names.insert(id.clone(), name.clone());
            }
        }
    }
    names
}

/// Name to attach to a projected tool result: explicit `msg.name`, else the
/// assistant call's function name for this id, else the id itself (the pre-0.4
/// shape, only reached when history has no matching call).
pub(crate) fn tool_result_name(
    names: &HashMap<String, String>,
    msg_name: Option<&str>,
    tool_use_id: &str,
) -> String {
    msg_name
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .or_else(|| names.get(tool_use_id).cloned())
        .unwrap_or_else(|| tool_use_id.to_string())
}

/// Generous decoded-response budgets (mutil-ai 0.4.0 `ResponseLimits`). The SDK
/// defaults are 16 MiB body / 16,384 events, which a long reasoning stream or a
/// large tool-argument payload can hit; overshooting now and tightening per
/// provider later is cheaper than an unexpected `ResponseLimit` (which the SDK
/// surfaces as non-retryable `ErrorKind::Decode`).
pub(crate) fn roomy_response_limits() -> ResponseLimits {
    // `ResponseLimits` is `#[non_exhaustive]`, so it cannot be built with a
    // struct literal from this crate — start from `default()` and set fields.
    let mut limits = ResponseLimits::default();
    limits.max_body_bytes = 64 * 1024 * 1024;
    limits.max_stream_events = 131_072;
    limits.max_tool_argument_bytes = 4 * 1024 * 1024;
    limits
}

/// mutil-ai 0.4.0 redefines `RequestOptions.timeout` as one monotonic absolute
/// deadline spanning header injection, HTTP attempts, retries, and all stream
/// reads/reconnects (previously effectively per-attempt). Keep a generous total
/// cap so long agentic turns aren't cut off, while `idle_timeout` still guards
/// a stalled stream.
pub(crate) const REQUEST_TOTAL_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

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
            } else if status == 429
                && let Some(human) = humanize_usage_limit(e.raw_body())
            {
                human
            } else {
                let fallback = e.to_string();
                let body = e.raw_body().unwrap_or(fallback.as_str());
                safe_provider_error_body(body, api_key)
            };
            (msg.clone(), format!("{msg}: {detail}"))
        }
        None => {
            // 0.4.0 redacts ProviderStream/ResponseLimit payloads from Display;
            // pull the raw body back through the explicit accessor so the
            // harness keeps the same bounded, key-redacted detail it had before.
            let msg = format!("{label} API error: {e}");
            let detail = match e.raw_body() {
                Some(body) => safe_provider_error_body(body, api_key),
                None => return (msg.clone(), msg),
            };
            (msg.clone(), format!("{msg}: {detail}"))
        }
    }
}

/// OAuth 套餐额度（5h/周窗口）的 429 人性化：错误体形如
/// `{"error":{"type":"usage_limit_reached","resets_at":<unix 秒>}}`（或
/// `resets_in_seconds`）。能解析出恢复时间就给用户可读文案；解析不出返回
/// `None`，走原 safe body。对标 codex `usage_limit_reached` 的 resets_at 语义。
fn humanize_usage_limit(body: Option<&str>) -> Option<String> {
    let body = body?;
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let error = value.get("error").unwrap_or(&value);
    let is_usage_limit = ["usage_limit_reached", "rate_limit_exceeded"]
        .iter()
        .any(|code| {
            error.get("type").and_then(|v| v.as_str()) == Some(*code)
                || error.get("code").and_then(|v| v.as_str()) == Some(*code)
        });
    if !is_usage_limit {
        return None;
    }
    let in_secs = match error
        .get("resets_in_seconds")
        .and_then(|v| v.as_i64())
    {
        Some(secs) => Some(secs),
        None => error
            .get("resets_at")
            .and_then(|v| v.as_i64())
            .map(|at| at - now_unix_secs()),
    }?;
    if in_secs <= 0 {
        return Some("套餐额度已用尽：用量窗口即将重置，请稍后重试".to_string());
    }
    let minutes = (in_secs + 59) / 60;
    if minutes >= 60 {
        Some(format!(
            "套餐额度已用尽：用量窗口约 {} 小时后重置",
            (minutes + 59) / 60
        ))
    } else {
        Some(format!("套餐额度已用尽：用量窗口约 {minutes} 分钟后重置"))
    }
}

fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
    fn finalize_keeps_null_truth_for_unrecoverable_args_and_sorts_by_index() {
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
        // [Text, ToolUse(a), ToolUse(b)] — reasoning absent, tools index-sorted.
        // b 的参数缓冲损坏（截断/混流）：保持 Null 真值，**不得**伪造 `{}`——
        // `{}` 会让 typed 工具报「missing field `x`」假错（BUG-2026-10-15）。
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
                assert!(input.is_null(), "损坏参数必须保持 Null 真值: {input:?}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn wire_tool_input_folds_null_for_replay_only() {
        // wire 层折叠：端点不接受 "arguments": "null"（T-7-1）；语义层 Null
        // 只在此处收敛，执行面看到的仍是真值。
        assert_eq!(
            super::wire_tool_input(&serde_json::Value::Null),
            serde_json::json!({})
        );
        assert_eq!(
            super::wire_tool_input(&serde_json::json!({"p": 1})),
            serde_json::json!({"p": 1})
        );
    }

    #[test]
    fn announcer_holds_the_start_frame_until_identity_is_known() {
        // Anthropic wire 的真实时序：先只有参数增量（身份还没到），随后
        // ToolCallProgress 才带上 call id 与函数名。首帧必须等信息齐了再发，
        // 且参数一次性带上——runtime 以 `tool:<id>` 为键开块，空 id 会开出
        // 一个永远对不上的孤儿块。
        let mut tools = HashMap::new();
        tools.insert(
            1usize,
            (String::new(), String::new(), "{\"a\":1}".to_string()),
        );
        let mut announcer = ToolCallAnnouncer::default();
        let mut frames = Vec::new();
        announcer.announce(&tools, 1, &mut |ev| frames.push(ev));
        assert!(frames.is_empty(), "身份未齐不得开块: {frames:?}");

        let entry = tools.get_mut(&1).expect("entry");
        adopt_identity(&mut entry.0, Some("call_9"));
        adopt_identity(&mut entry.1, Some("exec"));
        announcer.announce(&tools, 1, &mut |ev| frames.push(ev));
        let StreamEvent::ToolCallProgress {
            id,
            name,
            args_chunk,
            ..
        } = &frames[0]
        else {
            panic!("expected progress frame, got {frames:?}");
        };
        assert_eq!(
            (id.as_str(), name.as_str(), args_chunk.as_str()),
            ("call_9", "exec", "{\"a\":1}")
        );

        // 齐了之后再来一帧：不得重复投递已发出的字节。
        let entry = tools.get_mut(&1).expect("entry");
        entry.2.push_str(",\"b\":2}");
        announcer.announce(&tools, 1, &mut |ev| frames.push(ev));
        assert_eq!(frames.len(), 2);
        let StreamEvent::ToolCallProgress { args_chunk, .. } = &frames[1] else {
            panic!();
        };
        assert_eq!(args_chunk, ",\"b\":2}", "第二帧只带增量");
    }

    #[test]
    fn adopt_identity_never_clobbers_a_known_field() {
        let mut id = "call_1".to_string();
        adopt_identity(&mut id, Some("fc_item_2"));
        assert_eq!(
            id, "call_1",
            "first-writer-wins：item id 不得覆盖已知的 call id"
        );
        let mut blank = String::new();
        adopt_identity(&mut blank, Some("  "));
        assert_eq!(blank, "", "空白身份等同缺席");
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

    /// OAuth 套餐窗口的 429：`usage_limit_reached` + `resets_in_seconds` /
    /// `resets_at` 必须变成用户可读的重置时间，而不是裸 safe body。
    #[test]
    fn usage_limit_429_is_humanized() {
        let e = SdkError::Api {
            provider: "openai",
            status: 429,
            body: r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":7250}}"#.into(),
            retry_after: None,
            retry_source: None,
            request_id: None,
        };
        let (_, full) = sdk_error_detail("OpenAI", &e, "sk-x");
        assert!(full.contains("套餐额度已用尽"), "{full}");
        assert!(full.contains("小时后重置"), "{full}");

        // 非 usage-limit 的 429 走原 safe body 路径。
        let e = SdkError::Api {
            provider: "openai",
            status: 429,
            body: r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#.into(),
            retry_after: None,
            retry_source: None,
            request_id: None,
        };
        let (_, full) = sdk_error_detail("OpenAI", &e, "sk-x");
        assert!(!full.contains("套餐额度已用尽"), "{full}");
    }

    /// `resets_at` 已过期（服务器时钟差 / 刚跨过重置点）→ 「即将重置」。
    #[test]
    fn usage_limit_resets_at_in_the_past_reads_as_imminent() {
        let e = SdkError::Api {
            provider: "openai",
            status: 429,
            body: r#"{"error":{"type":"usage_limit_reached","resets_at":1}}"#.into(),
            retry_after: None,
            retry_source: None,
            request_id: None,
        };
        let (_, full) = sdk_error_detail("OpenAI", &e, "sk-x");
        assert!(full.contains("即将重置"), "{full}");
    }
}
