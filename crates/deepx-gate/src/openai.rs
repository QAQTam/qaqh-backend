//! OpenAI Chat Completions API streaming client — sync (ureq).
//! Includes retry with exponential backoff for transient errors (429, 500, 503, transport).

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use deepx_types::{CacheTokenField, ThinkingParamMode};
use deepx_types::{ContentBlock, Message, ToolDef, UsageInfo};

use super::types::{ProviderConfig, StreamEvent};

/// Per-read timeout for SSE streaming. When no data arrives within this
/// interval, `read()` returns a `TimedOut` error so we can check the cancel
/// flag and retry. This makes cancel responsive even during the "thinking"
/// delay before the first token arrives.
const SSE_READ_TIMEOUT: Duration = Duration::from_millis(50);

/// Check whether the cancel flag is set.
fn is_cancelled(cancel: Option<&Arc<AtomicBool>>) -> bool {
    cancel.map(|c| c.load(Ordering::SeqCst)).unwrap_or(false)
}

/// Sleep for `delay` but wake up every 100ms to check the cancel flag.
/// Returns `true` if cancelled during the sleep.
fn sleep_with_cancel(delay: Duration, cancel: Option<&Arc<AtomicBool>>) -> bool {
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

/// Check whether an `io::Error` from `reader.read()` is a recoverable timeout.
///
/// ureq v3 wraps its internal `Error::Timeout` as `io::ErrorKind::Other` via
/// `into_io()`, so we must also inspect the inner error to detect timeouts.
/// Without this, every SSE read timeout is treated as a fatal error.
fn is_read_timeout(e: &std::io::Error) -> bool {
    if e.kind() == std::io::ErrorKind::TimedOut
        || e.kind() == std::io::ErrorKind::WouldBlock
        || e.kind() == std::io::ErrorKind::Interrupted
    {
        return true;
    }
    // ureq v3: io::Error::other(Error::Timeout(_))
    e.get_ref()
        .and_then(|inner| inner.downcast_ref::<ureq::Error>())
        .map(|err| matches!(err, ureq::Error::Timeout(_)))
        .unwrap_or(false)
}

const MAX_RETRIES: u32 = 3;
const BASE_DELAY_SECS: u64 = 1;

fn is_retryable(status: u16) -> bool {
    matches!(status, 429 | 500 | 503)
}

/// Providers use several OpenAI-compatible names for the same hidden
/// reasoning stream. Keep that data out of `content`, which is user-visible.
fn reasoning_delta<'a>(delta: &'a serde_json::Value) -> Option<&'a str> {
    [
        "reasoning_content",
        "reasoning",
        "thinking",
        "analysis_content",
    ]
    .into_iter()
    .find_map(|key| delta.get(key).and_then(|value| value.as_str()))
}

/// Some compatible endpoints put reasoning inside `content` using think tags.
/// Split complete tags before events reach the frontend. The normal provider
/// fields above remain the authoritative path; this is a compatibility guard.
fn split_inline_thinking(text: &str, in_thinking: &mut bool) -> Vec<(bool, String)> {
    let mut result = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let marker = if *in_thinking { "</think>" } else { "<think>" };
        match rest.find(marker) {
            Some(index) => {
                if index > 0 {
                    result.push((*in_thinking, rest[..index].to_string()));
                }
                *in_thinking = !*in_thinking;
                rest = &rest[index + marker.len()..];
            }
            None => {
                result.push((*in_thinking, rest.to_string()));
                break;
            }
        }
    }
    result
}

fn backoff_delay(attempt: u32) -> Duration {
    let secs = BASE_DELAY_SECS * 2u64.pow(attempt.saturating_sub(1));
    Duration::from_secs(secs.min(30))
}

/// Send a chat completion request and stream SSE events via `on_event`.
///
/// `cancel` is an optional `Arc<AtomicBool>` that, when set to `true`, causes
/// the streaming to abort as soon as the next read times out (within
/// `SSE_READ_TIMEOUT`). This makes cancel responsive even while the HTTP
/// response is still being streamed.
#[allow(clippy::string_slice)]
pub fn chat_stream_openai(
    provider: &ProviderConfig,
    model: &str,
    messages: Vec<Message>,
    tools: Option<Vec<ToolDef>>,
    max_tokens: u32,
    effort: Option<String>,
    user_id: Option<String>,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut dyn FnMut(StreamEvent),
) -> anyhow::Result<()> {
    let messages = normalize_skill_envelope(provider, messages).map_err(anyhow::Error::msg)?;
    // Stateful 模式：只发增量消息（最后一条 user + 其后的 tool 结果）
    let messages = if provider.stateful {
        filter_stateful_messages(messages)
    } else {
        messages
    };

    let api_msgs = convert_messages(messages, None);

    let openai_tools: Option<Vec<serde_json::Value>> = tools.map(|tds| {
        tds.into_iter()
            .map(|td| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": td.function.name,
                        "description": td.function.description,
                        "parameters": td.function.parameters,
                    }
                })
            })
            .collect()
    });

    let mut body_map = serde_json::Map::new();
    body_map.insert("model".into(), serde_json::json!(model));
    body_map.insert("messages".into(), serde_json::Value::Array(api_msgs));
    body_map.insert("stream".into(), serde_json::json!(true));
    if provider.supports_thinking {
        match provider.thinking_mode {
            ThinkingParamMode::OpenAi => {
                body_map.insert("thinking".into(), serde_json::json!({"type": "enabled"}));
            }
            ThinkingParamMode::QwenEnableThinking => {
                body_map.insert("enable_thinking".into(), serde_json::json!(true));
            }
            ThinkingParamMode::MiniMaxAdaptive => {
                body_map.insert("thinking".into(), serde_json::json!({"type": "adaptive"}));
                body_map.insert("reasoning_split".into(), serde_json::json!(true));
            }
        }
    }
    body_map.insert("max_tokens".into(), serde_json::json!(max_tokens));

    if let Some(ref e) = effort {
        body_map.insert("reasoning_effort".into(), serde_json::json!(e));
    }
    if let Some(sample) = provider.do_sample {
        body_map.insert("do_sample".into(), serde_json::json!(sample));
    }
    if let Some(ref t) = openai_tools {
        body_map.insert("tools".into(), serde_json::Value::Array(t.clone()));
    }
    if let Some(ref uid) = user_id {
        if provider.user_id_mode.is_some() {
            body_map.insert("user_id".into(), serde_json::json!(uid));
        }
    }

    let body = serde_json::Value::Object(body_map);
    let url = build_chat_url(&provider.base_url, provider.chat_path.as_deref());

    let mut attempt = 0u32;
    // Reuse a global Agent with a short per-read timeout so that stream_sse
    // can check the cancel flag between reads. Connection pool and DNS cache
    // are preserved across requests.
    // http_status_as_error(false) so we can read error bodies for retry logic.
    static GLOBAL_AGENT: std::sync::LazyLock<ureq::Agent> = std::sync::LazyLock::new(|| {
        ureq::Agent::config_builder()
            .timeout_recv_body(Some(SSE_READ_TIMEOUT))
            .timeout_send_body(Some(Duration::from_secs(30)))
            .http_status_as_error(false)
            .build()
            .into()
    });
    let agent = &*GLOBAL_AGENT;

    loop {
        attempt += 1;

        // Check cancel before sending the request
        if is_cancelled(cancel) {
            return Err(anyhow::anyhow!("cancelled by user"));
        }

        let resp = agent
            .post(&url)
            .header("Authorization", &format!("Bearer {}", provider.api_key))
            .header("Content-Type", "application/json")
            .send_json(&body);

        match resp {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if status >= 200 && status < 300 {
                    return stream_sse(resp, provider, user_id.as_deref(), cancel, on_event);
                }
                // HTTP error — read body for details
                let text = resp.into_body().read_to_string().unwrap_or_default();
                let code_desc = http_error_description(status);
                if attempt >= MAX_RETRIES || !is_retryable(status) {
                    let msg = format!("OpenAI API HTTP {} ({})", status, code_desc);
                    on_event(StreamEvent::Error(format!("{}: {}", msg, text)));
                    return Err(anyhow::anyhow!("{}", msg));
                }

                let delay = backoff_delay(attempt);
                on_event(StreamEvent::Retrying {
                    attempt,
                    max_retries: MAX_RETRIES,
                    delay_secs: delay.as_secs(),
                    error: format!("HTTP {} ({})", status, code_desc),
                });
                if sleep_with_cancel(delay, cancel) {
                    return Err(anyhow::anyhow!("cancelled by user"));
                }
            }
            Err(e) => {
                // Transport / timeout / connection errors
                if attempt >= MAX_RETRIES {
                    let msg = format!("HTTP transport error: {e}");
                    on_event(StreamEvent::Error(msg.clone()));
                    return Err(anyhow::anyhow!("{}", msg));
                }

                let delay = backoff_delay(attempt);
                on_event(StreamEvent::Retrying {
                    attempt,
                    max_retries: MAX_RETRIES,
                    delay_secs: delay.as_secs(),
                    error: format!("{e}"),
                });
                if sleep_with_cancel(delay, cancel) {
                    return Err(anyhow::anyhow!("cancelled by user"));
                }
            }
        }
    }
}

fn stream_sse(
    resp: ureq::http::Response<ureq::Body>,
    provider: &ProviderConfig,
    _user_id: Option<&str>,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut dyn FnMut(StreamEvent),
) -> anyhow::Result<()> {
    let mut reader = resp.into_body().into_reader();
    let mut sse_buf = String::new();
    let mut byte_buf = vec![0u8; 512];

    let mut text_buf = String::new();
    let mut reasoning_buf = String::new();
    let mut tool_acc: HashMap<usize, (String, String, String)> = HashMap::new();
    let mut dsml_buf = String::new();
    let mut dsml_seen: HashSet<String> = HashSet::new();
    let mut usage_info: Option<UsageInfo> = None;
    let mut stop_reason: Option<String> = None;
    let mut inline_thinking = false;

    loop {
        // Check cancel before each read attempt
        if is_cancelled(cancel) {
            return Err(anyhow::anyhow!("cancelled by user"));
        }

        let n = match reader.read(&mut byte_buf) {
            Ok(n) => n,
            Err(e) if is_read_timeout(&e) => {
                // Read timeout (SSE_READ_TIMEOUT elapsed with no data).
                // Loop back to check cancel, then retry the read.
                continue;
            }
            Err(e) => {
                let msg = format!("SSE read error: {e}");
                on_event(StreamEvent::Error(msg.clone()));
                return Err(anyhow::anyhow!("{}", msg));
            }
        };

        if n == 0 {
            // EOF — stream ended without Done
            break;
        }

        sse_buf.push_str(&String::from_utf8_lossy(&byte_buf[..n]));

        while let Some(pos) = sse_buf.find("\n\n") {
            let raw = sse_buf[..pos].to_string();
            sse_buf.drain(..pos + 2); // drain in-place, no reallocation of tail

            let mut data_str = String::new();
            for line in raw.lines() {
                let trimmed = line.trim();
                if let Some(dt) = trimmed.strip_prefix("data: ") {
                    data_str = dt.to_string();
                }
            }
            if data_str.is_empty() || data_str == "[DONE]" {
                continue;
            }

            let ev: serde_json::Value = match serde_json::from_str(&data_str) {
                Ok(e) => e,
                Err(e) => {
                    log::warn!("OpenAI SSE: deserialize fail: {} — data: {}", e, data_str);
                    continue;
                }
            };

            // Parse choices
            if let Some(choices) = ev.get("choices").and_then(|c| c.as_array()) {
                if let Some(choice) = choices.first() {
                    let finish = choice.get("finish_reason").and_then(|v| v.as_str());
                    if let Some(fr) = finish {
                        if !fr.is_empty() && fr != "null" {
                            stop_reason = Some(fr.to_string());
                        }
                    }

                    if let Some(delta) = choice.get("delta") {
                        // Text content
                        if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
                            for (is_reasoning, t) in
                                split_inline_thinking(text, &mut inline_thinking)
                            {
                                if is_reasoning {
                                    reasoning_buf.push_str(&t);
                                    on_event(StreamEvent::ReasoningDelta(t));
                                } else {
                                    text_buf.push_str(&t);
                                    on_event(StreamEvent::ContentDelta(t.clone()));

                                    // DSML tool call detection in content stream
                                    dsml_buf.push_str(&t);
                                    let mut search_from = 0usize;
                                    while let Some(start) =
                                        dsml_buf[search_from..].find("<｜DSML｜invoke name=\"")
                                    {
                                        let abs_start = search_from + start;
                                        let after_tag = abs_start + "<｜DSML｜invoke name=\"".len();
                                        if let Some(rest) = dsml_buf.get(after_tag..) {
                                            if let Some(quote_end) = rest.find('"') {
                                                let name = rest[..quote_end].to_string();
                                                if dsml_seen.insert(name.clone()) {
                                                    let idx = dsml_seen.len() - 1;
                                                    on_event(StreamEvent::ToolCallProgress {
                                                        index: idx,
                                                        id: format!("dsml_tc_{}", idx),
                                                        name,
                                                        args_so_far: String::new(),
                                                    });
                                                }
                                                search_from = after_tag + quote_end + 1;
                                                continue;
                                            }
                                        }
                                        break;
                                    }
                                }
                            }
                        }

                        // Reasoning content
                        if let Some(rc) = reasoning_delta(delta) {
                            let r = rc.to_string();
                            reasoning_buf.push_str(&r);
                            on_event(StreamEvent::ReasoningDelta(r));
                        }

                        // Tool calls (native OpenAI format)
                        if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                            for tc in tcs {
                                let idx =
                                    tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                                let entry = tool_acc.entry(idx).or_insert_with(|| {
                                    let tid = tc
                                        .get("id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let tname = tc
                                        .get("function")
                                        .and_then(|f| f.get("name"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    (tid, tname, String::new())
                                });
                                if let Some(args) = tc
                                    .get("function")
                                    .and_then(|f| f.get("arguments"))
                                    .and_then(|v| v.as_str())
                                {
                                    entry.2.push_str(args);
                                    log::info!(
                                        "[GATE] ToolCallProgress idx={idx} id={} name={} args_len={}",
                                        entry.0,
                                        entry.1,
                                        entry.2.len()
                                    );
                                    on_event(StreamEvent::ToolCallProgress {
                                        index: idx,
                                        id: entry.0.clone(),
                                        name: entry.1.clone(),
                                        args_so_far: entry.2.clone(),
                                    });
                                }
                            }
                        }
                    }
                }
            }

            // Usage info (may appear in any chunk)
            if let Some(u) = ev.get("usage") {
                let pt = u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let ct = u
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                let (hit, miss) = match provider.cache_field {
                    CacheTokenField::PromptCacheHitTokens => (
                        u.get("prompt_cache_hit_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as u32,
                        u.get("prompt_cache_miss_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as u32,
                    ),
                    CacheTokenField::PromptDetailsCached => {
                        let cached = u
                            .get("prompt_tokens_details")
                            .and_then(|d| d.get("cached_tokens"))
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as u32;
                        (cached, 0)
                    }
                    CacheTokenField::UsageCachedTokens => {
                        let cached =
                            u.get("cached_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                        (cached, 0)
                    }
                    CacheTokenField::None => (0, 0),
                };
                let rt = u
                    .get("completion_tokens_details")
                    .and_then(|d| d.get("reasoning_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                usage_info = Some(UsageInfo {
                    prompt_tokens: pt,
                    completion_tokens: ct,
                    total_tokens: pt + ct,
                    prompt_cache_hit_tokens: hit,
                    prompt_cache_miss_tokens: miss,
                    reasoning_tokens: rt,
                });
                // Emit real-time usage update so InfoBar can show live cache-hit stats
                on_event(StreamEvent::UsageUpdate(usage_info.clone().unwrap()));
            }
        }
    }

    // Build final message from accumulated content
    let mut blocks: Vec<ContentBlock> = Vec::new();

    if !reasoning_buf.is_empty() {
        blocks.push(ContentBlock::Reasoning {
            reasoning: reasoning_buf,
        });
    }

    // ── DSML integration: extract tool calls from text content ──
    let _final_text = if crate::tool_parser::has_dsml(&text_buf) {
        let (cleaned, dsml_tcs) = crate::tool_parser::parse_dsml_tool_calls(&text_buf, &[]);
        // Merge DSML tool calls into tool_acc (with unique ids to avoid collision)
        let base_idx = tool_acc.len();
        for (i, tc) in dsml_tcs.iter().enumerate() {
            let idx = base_idx + i;
            tool_acc.insert(
                idx,
                (
                    tc.id.clone(),
                    tc.function.name.clone(),
                    tc.function.arguments.to_string(),
                ),
            );
        }
        if !cleaned.is_empty() {
            blocks.push(ContentBlock::text(&cleaned));
        }
        cleaned
    } else {
        if !text_buf.is_empty() {
            blocks.push(ContentBlock::text(&text_buf));
        }
        text_buf.clone()
    };

    let mut sorted: Vec<(usize, String, String, String)> = tool_acc
        .into_iter()
        .map(|(idx, (id, name, args))| (idx, id, name, args))
        .collect();
    sorted.sort_by_key(|(idx, _, _, _)| *idx);
    for (_idx, id, name, args_json) in sorted {
        let input: serde_json::Value =
            serde_json::from_str(&args_json).unwrap_or(serde_json::Value::Null);
        blocks.push(ContentBlock::ToolUse { id, name, input });
    }

    let raw_message = Message {
        msg_id: None,
        role: "assistant".into(),
        name: None,
        content: blocks,
    };

    on_event(StreamEvent::Done {
        raw_message,
        usage: usage_info,
        stop_reason,
    });

    Ok(())
}

// ── Message conversion ──

/// Stateful 模式：只保留增量消息。
/// Web 代理端已记住完整上下文。
/// 规则：
///   - 首次请求（无 assistant 历史）：发 system + 所有消息
///   - 后续请求：只发最后一条 assistant 之后的消息
fn filter_stateful_messages(messages: Vec<Message>) -> Vec<Message> {
    if messages.is_empty() {
        return messages;
    }

    let last_asst_idx = messages.iter().rposition(|m| m.role == "assistant");
    let start = last_asst_idx.map(|i| i + 1).unwrap_or(0);
    let is_first = start == 0;

    // Debug: 打印过滤前的消息角色序列
    #[cfg(debug_assertions)]
    {
        let roles: Vec<&str> = messages.iter().map(|m| m.role.as_str()).collect();
        eprintln!(
            "[filter] 输入: {:?} | last_asst={:?} start={}",
            roles, last_asst_idx, start
        );
    }

    if is_first {
        return messages;
    }

    let mut out: Vec<Message> = Vec::new();

    // 保留 start 之后的新消息
    for msg in &messages[start..] {
        out.push(msg.clone());
    }

    // 兜底：如果没有任何新消息，且最后一条是 user/tool（非 assistant），保留它
    if out.is_empty() {
        if let Some(last) = messages.last() {
            if last.role != "assistant" {
                out.push(last.clone());
            }
        }
    }

    let out_roles: Vec<&str> = out.iter().map(|m| m.role.as_str()).collect();
    eprintln!("[filter] 输出: {:?} (is_first={})", out_roles, is_first);

    out
}

fn normalize_skill_envelope(
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

fn convert_messages(messages: Vec<Message>, system: Option<String>) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::new();
    if let Some(sys) = system {
        if !sys.is_empty() {
            out.push(serde_json::json!({"role": "system", "content": sys}));
        }
    }

    for msg in messages {
        let name = &msg.name;
        match msg.role.as_str() {
            "system" => {
                if let Some(tb) = msg.content.iter().find_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                }) {
                    let mut obj = serde_json::json!({"role": "system", "content": tb});
                    if let Some(n) = name {
                        obj["name"] = serde_json::json!(n);
                    }
                    out.push(obj);
                }
            }
            "user" => {
                let mut text = String::new();
                for block in &msg.content {
                    if let ContentBlock::Text { text: t } = block {
                        text.push_str(t);
                    }
                }
                let mut obj = serde_json::json!({"role": "user", "content": text});
                if let Some(n) = name {
                    obj["name"] = serde_json::json!(n);
                }
                out.push(obj);
            }
            "assistant" => {
                let mut content = String::new();
                let mut reasoning = String::new();
                let mut tool_calls: Vec<serde_json::Value> = Vec::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => content.push_str(text),
                        ContentBlock::Reasoning { reasoning: r } => reasoning.push_str(r),
                        ContentBlock::ToolUse { id, name, input } => {
                            tool_calls.push(serde_json::json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": serde_json::to_string(input).unwrap_or_default(),
                                }
                            }));
                        }
                        _ => {}
                    }
                }
                let mut obj = serde_json::json!({"role": "assistant"});
                if !content.is_empty() {
                    obj["content"] = serde_json::json!(content);
                } else if tool_calls.is_empty() && !reasoning.is_empty() {
                    obj["content"] = serde_json::json!("[Thinking complete]");
                }
                if !reasoning.is_empty() {
                    obj["reasoning_content"] = serde_json::json!(reasoning);
                }
                if !tool_calls.is_empty() {
                    obj["tool_calls"] = serde_json::json!(tool_calls);
                }
                if obj.as_object().map_or(false, |m| m.len() > 1) {
                    out.push(obj);
                }
            }
            "tool" => {
                for block in &msg.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = block
                    {
                        out.push(serde_json::json!({
                            "role": "tool",
                            "tool_call_id": tool_use_id,
                            "content": content,
                        }));
                    }
                }
            }
            _ => {}
        }
    }

    out
}

// ── Synchronous (non-streaming) chat ──

pub fn chat_sync_openai(
    provider: &ProviderConfig,
    model: &str,
    messages: Vec<Message>,
    max_tokens: u32,
) -> Result<String, String> {
    let messages = normalize_skill_envelope(provider, messages)?;
    let messages = if provider.stateful {
        filter_stateful_messages(messages)
    } else {
        messages
    };
    let api_msgs = convert_messages(messages, None);
    let url = build_chat_url(&provider.base_url, provider.chat_path.as_deref());

    let mut body = serde_json::json!({
        "model": model,
        "messages": api_msgs,
        "max_tokens": max_tokens,
        "stream": false,
    });
    if provider.supports_thinking {
        let thinking = match provider.thinking_mode {
            ThinkingParamMode::OpenAi => serde_json::json!({"type": "enabled"}),
            ThinkingParamMode::QwenEnableThinking => serde_json::json!(true),
            ThinkingParamMode::MiniMaxAdaptive => serde_json::json!({"type": "adaptive"}),
        };
        body["thinking"] = thinking;
    }

    let resp = ureq::post(&url)
        .header("Authorization", &format!("Bearer {}", provider.api_key))
        .header("Content-Type", "application/json")
        .send_json(&body)
        .map_err(|e| format!("compact request failed: {e}"))?;

    let json: serde_json::Value = resp
        .into_body()
        .read_json()
        .map_err(|e| format!("compact parse failed: {e}"))?;

    json["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "compact: no content in response".to_string())
}

// ── URL builder ──

fn build_chat_url(base_url: &str, chat_path: Option<&str>) -> String {
    if let Some(path) = chat_path {
        if path.starts_with("http") {
            return path.to_string();
        }
        let base = base_url.trim_end_matches('/');
        return format!("{}{}", base, path);
    }
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/chat/completions") {
        base.to_string()
    } else {
        format!("{}/chat/completions", base)
    }
}

// ── Error descriptions ──

fn http_error_description(status: u16) -> &'static str {
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

#[cfg(test)]
mod skill_envelope_tests {
    use super::*;

    #[test]
    fn stateful_first_request_does_not_duplicate_system_slots() {
        let messages = vec![
            Message::system("base"),
            Message::system("catalog"),
            Message::user("hi"),
            Message::system("envelope"),
        ];
        let filtered = filter_stateful_messages(messages.clone());
        assert_eq!(filtered.len(), messages.len());
    }

    #[test]
    fn stateful_increment_always_keeps_authoritative_tail_envelope() {
        let messages = vec![
            Message::system("base"),
            Message::user("old"),
            Message {
                msg_id: None,
                role: "assistant".into(),
                name: None,
                content: vec![ContentBlock::text("done")],
            },
            Message::user("next"),
            Message::system("<skill_context_envelope />"),
        ];
        let filtered = filter_stateful_messages(messages);
        assert_eq!(
            filtered
                .iter()
                .map(|message| message.role.as_str())
                .collect::<Vec<_>>(),
            vec!["user", "system"]
        );
        assert!(
            matches!(&filtered[1].content[0], ContentBlock::Text { text } if text.contains("skill_context_envelope"))
        );
    }

    fn provider() -> ProviderConfig {
        ProviderConfig::openai(
            "http://test",
            "",
            "m",
            None,
            None,
            ThinkingParamMode::OpenAi,
            CacheTokenField::None,
            false,
        )
    }

    #[test]
    fn normalizes_reasoning_aliases_and_think_tags() {
        for key in [
            "reasoning_content",
            "reasoning",
            "thinking",
            "analysis_content",
        ] {
            let mut map = serde_json::Map::new();
            map.insert(
                key.to_string(),
                serde_json::Value::String("hidden".to_string()),
            );
            let delta = serde_json::Value::Object(map);
            assert_eq!(reasoning_delta(&delta), Some("hidden"));
        }

        let mut in_thinking = false;
        assert_eq!(
            split_inline_thinking("visible<think>hidden</think>done", &mut in_thinking),
            vec![
                (false, "visible".to_string()),
                (true, "hidden".to_string()),
                (false, "done".to_string()),
            ],
        );
        assert!(!in_thinking);
    }

    #[test]
    fn stateless_provider_can_explicitly_degrade_to_head_dynamic_slot() {
        let provider = provider().with_tail_system_support(false);
        let messages = vec![
            Message::system("base"),
            Message::user("hi"),
            Message::system("<skill_context_envelope />"),
        ];
        let normalized = normalize_skill_envelope(&provider, messages).unwrap();
        assert_eq!(
            normalized
                .iter()
                .map(|message| message.role.as_str())
                .collect::<Vec<_>>(),
            vec!["system", "system", "user"]
        );
    }

    #[test]
    fn stateful_provider_refuses_silent_head_fallback() {
        let provider = provider()
            .with_stateful(true)
            .with_tail_system_support(false);
        let error = normalize_skill_envelope(
            &provider,
            vec![
                Message::user("hi"),
                Message::system("<skill_context_envelope />"),
            ],
        )
        .unwrap_err();
        assert!(error.contains("SKILL_CONTEXT_SYNC_UNSUPPORTED"));
    }
}
