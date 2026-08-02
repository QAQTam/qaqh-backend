//! OpenAI Responses API streaming client.
//!
//! Sends requests to `POST /responses` and parses SSE events into
//! the gate's unified `StreamEvent` enum.

use futures::StreamExt;
use reqwest::Client;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use deepx_types::{ContentBlock, Message, ToolDef};

use super::types::{ProviderConfig, ResponsesCompat, StreamEvent};

const SSE_POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_RETRIES: u32 = 3;

static FALLBACK_RT: std::sync::LazyLock<tokio::runtime::Runtime> = std::sync::LazyLock::new(|| {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to create deepx-gate responses tokio runtime")
});

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    FALLBACK_RT.block_on(f)
}

fn is_cancelled(cancel: Option<&Arc<AtomicBool>>) -> bool {
    cancel.map(|c| c.load(Ordering::SeqCst)).unwrap_or(false)
}

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

// ── Lazy global reqwest Client ──
static GLOBAL_CLIENT: std::sync::LazyLock<Client> = std::sync::LazyLock::new(|| {
    Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()
        .expect("failed to create deepx-gate responses reqwest client")
});

// ── URL construction ──

fn build_responses_url(base_url: &str, responses_path: Option<&str>) -> String {
    if let Some(path) = responses_path {
        if path.starts_with("http") {
            return path.to_string();
        }
        let base = base_url.trim_end_matches('/');
        return format!("{}{}", base, path);
    }
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/responses") {
        base.to_string()
    } else {
        format!("{}/responses", base)
    }
}

// ── Message conversion: DeepX ContentBlock → Responses input[] items ──
//
// 第一条 system 消息 → 顶层 `instructions`（文档语义：模型上下文中的
// 第一条 system 消息，静态 base prompt 的正确承载位）；其余 system
// （运行时动态注入：skills catalog、上下文 envelope 等）→ `developer` item。

fn convert_messages_to_input(
    messages: &[Message],
    compat: &ResponsesCompat,
) -> (Vec<serde_json::Value>, Option<String>) {
    let mut items: Vec<serde_json::Value> = Vec::new();
    let mut instructions: Option<String> = None;

    for msg in messages {
        match msg.role.as_str() {
            "system" => {
                let text = extract_text(&msg.content);
                if instructions.is_none() {
                    // 第一条 system = base 系统指令 → 顶层 instructions
                    instructions = Some(text);
                } else {
                    // 动态注入的系统指令保持 developer item（追加语义）
                    items.push(serde_json::json!({
                        "type": "message",
                        "role": "developer",
                        "content": [{"type": "input_text", "text": text}],
                    }));
                }
            }
            "user" => {
                let parts = convert_user_content(&msg.content);
                items.push(serde_json::json!({
                    "type": "message",
                    "role": "user",
                    "content": parts,
                }));
            }
            "assistant" => {
                let text_parts: Vec<_> = msg.content.iter().filter_map(|b| {
                    if let ContentBlock::Text { text } = b {
                        if !text.is_empty() { Some(text.clone()) } else { None }
                    } else {
                        None
                    }
                }).collect();

                if !text_parts.is_empty() {
                    let content: Vec<serde_json::Value> = text_parts.iter().map(|t| {
                        serde_json::json!({"type": "output_text", "text": t})
                    }).collect();
                    items.push(serde_json::json!({
                        "type": "message",
                        "role": "assistant",
                        "content": content,
                    }));
                }

                // ToolUse blocks → top-level function_call items
                for block in &msg.content {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        let args = serde_json::to_string(input).unwrap_or_default();
                        items.push(serde_json::json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            "arguments": args,
                            "status": "completed",
                        }));
                    }
                }

                // WebSearchCall blocks → top-level web_search_call items,
                // echoed verbatim so the server restores its search results.
                if compat.echo_web_search_call {
                    for block in &msg.content {
                        if let ContentBlock::WebSearchCall { id, action } = block {
                            items.push(serde_json::json!({
                                "type": "web_search_call",
                                "id": id,
                                "action": action,
                            }));
                        }
                    }
                }
            }
            "tool" => {
                // ToolResult blocks → function_call_output
                for block in &msg.content {
                    if let ContentBlock::ToolResult { tool_use_id, content, .. } = block {
                        items.push(serde_json::json!({
                            "type": "function_call_output",
                            "call_id": tool_use_id,
                            "output": content,
                        }));
                    }
                }
            }
            _ => {}
        }
    }

    (items, instructions)
}

fn extract_text(blocks: &[ContentBlock]) -> String {
    for b in blocks {
        if let ContentBlock::Text { text } = b {
            return text.clone();
        }
    }
    String::new()
}

fn convert_user_content(blocks: &[ContentBlock]) -> Vec<serde_json::Value> {
    let mut parts: Vec<serde_json::Value> = Vec::new();
    let mut img_idx: usize = 0;
    for b in blocks {
        match b {
            ContentBlock::Text { text } => {
                parts.push(serde_json::json!({"type": "input_text", "text": text}));
            }
            ContentBlock::Image { mime_type, data } => {
                parts.push(serde_json::json!({
                    "type": "input_text",
                    "text": format!(
                        "[Image #{img_idx}: {mime_type}, ~{} bytes — call image_query(image_index={img_idx}, prompt=\"...\") to analyze]",
                        data.len()
                    )
                }));
                img_idx += 1;
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        parts.push(serde_json::json!({"type": "input_text", "text": ""}));
    }
    parts
}

fn convert_tools(
    tools: Option<Vec<ToolDef>>,
    compat: &ResponsesCompat,
) -> Option<Vec<serde_json::Value>> {
    let mut items: Vec<serde_json::Value> = Vec::new();
    if let Some(tds) = tools {
        for td in tds {
            items.push(serde_json::json!({
                "type": "function",
                "name": td.function.name,
                "description": td.function.description,
                "parameters": td.function.parameters,
            }));
        }
    }
    // Built-in server-side search tool. Injected even when no function tools
    // are registered: the model may decide on its own to search (tool_choice
    // default `auto`). Providers that ignore unknown tool types (compat
    // web_search=false) are unaffected.
    if compat.web_search {
        items.push(serde_json::json!({"type": "web_search"}));
    }
    if items.is_empty() {
        None
    } else {
        Some(items)
    }
}

/// Clamp a requested reasoning effort to the provider's upper bound.
///
/// DeepSeek accepts `none / minimal / low / medium / high / xhigh / max`;
/// OpenAI stops at `high`. The ladder is ordered so a value above the bound
/// (e.g. "xhigh" against an OpenAI endpoint) degrades gracefully instead of
/// being rejected or silently misinterpreted.
fn clamp_effort(effort: Option<String>, max: &str) -> String {
    const ORDER: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];
    let requested = effort.unwrap_or_else(|| "medium".into());
    let max_idx = ORDER.iter().position(|&v| v == max).unwrap_or(4);
    let idx = ORDER
        .iter()
        .position(|&v| v == requested)
        .unwrap_or(max_idx)
        .min(max_idx);
    ORDER[idx].to_string()
}

// ── Public API ──

#[allow(clippy::string_slice)]
pub fn chat_stream_responses(
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
    let compat = &provider.responses_compat;
    let (input_items, instructions) = convert_messages_to_input(&messages, compat);
    let responses_tools = convert_tools(tools, compat);

    let mut body_map = serde_json::Map::new();
    body_map.insert("model".into(), serde_json::json!(model));
    body_map.insert("input".into(), serde_json::Value::Array(input_items));
    body_map.insert("stream".into(), serde_json::json!(true));
    body_map.insert("store".into(), serde_json::json!(false));
    body_map.insert("parallel_tool_calls".into(), serde_json::json!(true));
    if let Some(ref instructions) = instructions {
        body_map.insert("instructions".into(), serde_json::json!(instructions));
    }
    if max_tokens > 0 {
        body_map.insert("max_output_tokens".into(), serde_json::json!(max_tokens));
    }

    if let Some(ref t) = responses_tools {
        body_map.insert("tools".into(), serde_json::Value::Array(t.clone()));
    }

    body_map.insert(
        "reasoning".into(),
        serde_json::json!({
            "effort": clamp_effort(effort, &compat.effort_max),
            "summary": "auto",
        }),
    );
    if compat.send_include {
        // `include` requests encrypted reasoning content (OpenAI semantics).
        // DeepSeek ignores it silently (documented); strict compatible
        // endpoints that reject unknown members turn this off via compat.
        body_map.insert(
            "include".into(),
            serde_json::json!(["reasoning.encrypted_content"]),
        );
    }
    if compat.supports_user {
        if let Some(ref uid) = user_id {
            body_map.insert("user".into(), serde_json::json!(uid));
        }
    }

    let body = serde_json::Value::Object(body_map);
    let url = build_responses_url(&provider.base_url, provider.responses_path.as_deref());

    let mut attempt = 0u32;
    loop {
        attempt += 1;
        if is_cancelled(cancel) {
            return Err(anyhow::anyhow!("cancelled by user"));
        }

        match block_on(async {
            GLOBAL_CLIENT
                .post(&url)
                .header("Authorization", format!("Bearer {}", provider.api_key))
                .header("Content-Type", "application/json")
                .body(serde_json::to_string(&body).unwrap_or_default())
                .send()
                .await
        }) {
            Ok(resp) => {
                if !resp.status().is_success() {
                    let status = resp.status().as_u16();
                    let err_body = block_on(async { resp.text().await }).unwrap_or_default();
                    if status == 401 {
                        // Some providers echo the API key tail in auth errors
                        // (e.g. DeepSeek: "Your api key: ****test is invalid").
                        // Never surface credential material in error output.
                        return Err(anyhow::anyhow!("HTTP 401 (authentication failed)"));
                    }
                    if status == 429 || status == 500 || status == 502 || status == 503 {
                        if attempt < MAX_RETRIES {
                            let delay = Duration::from_secs(2u64.pow(attempt));
                            on_event(StreamEvent::Retrying {
                                attempt,
                                max_retries: MAX_RETRIES,
                                delay_secs: delay.as_secs(),
                                error: format!("HTTP {} (retryable)", status),
                            });
                            if sleep_with_cancel(delay, cancel) {
                                return Err(anyhow::anyhow!("cancelled by user"));
                            }
                            continue;
                        }
                    }
                    let msg = if err_body.len() > 200 { &err_body[..200] } else { &err_body };
                    return Err(anyhow::anyhow!("HTTP {}: {}", status, msg));
                }
                return parse_responses_sse(resp, cancel, on_event);
            }
            Err(e) => {
                if attempt < MAX_RETRIES {
                    let delay = Duration::from_secs(2u64.pow(attempt));
                    on_event(StreamEvent::Retrying {
                        attempt,
                        max_retries: MAX_RETRIES,
                        delay_secs: delay.as_secs(),
                        error: format!("transport error: {e}"),
                    });
                    if sleep_with_cancel(delay, cancel) {
                        return Err(anyhow::anyhow!("cancelled by user"));
                    }
                    continue;
                }
                return Err(anyhow::anyhow!("Request failed: {}", e));
            }
        }
    }
}

/// Synchronous non-streaming call via Responses API.
pub fn chat_sync_responses(
    provider: &ProviderConfig,
    model: &str,
    messages: Vec<Message>,
    max_tokens: u32,
) -> Result<String, String> {
    let compat = &provider.responses_compat;
    let (input_items, instructions) = convert_messages_to_input(&messages, compat);
    let responses_tools = convert_tools(None, compat);

    let mut body_map = serde_json::Map::new();
    body_map.insert("model".into(), serde_json::json!(model));
    body_map.insert("input".into(), serde_json::Value::Array(input_items));
    body_map.insert("stream".into(), serde_json::json!(false));
    body_map.insert("store".into(), serde_json::json!(false));
    if let Some(ref instructions) = instructions {
        body_map.insert("instructions".into(), serde_json::json!(instructions));
    }
    if max_tokens > 0 {
        body_map.insert("max_output_tokens".into(), serde_json::json!(max_tokens));
    }
    if let Some(ref t) = responses_tools {
        body_map.insert("tools".into(), serde_json::Value::Array(t.clone()));
    }
    if compat.send_include {
        body_map.insert(
            "include".into(),
            serde_json::json!(["reasoning.encrypted_content"]),
        );
    }
    body_map.insert(
        "reasoning".into(),
        serde_json::json!({
            "effort": clamp_effort(None, &compat.effort_max),
            "summary": "auto",
        }),
    );

    let body = serde_json::Value::Object(body_map);
    let url = build_responses_url(&provider.base_url, provider.responses_path.as_deref());

    let resp = block_on(async {
        GLOBAL_CLIENT
            .post(&url)
            .header("Authorization", format!("Bearer {}", provider.api_key))
            .header("Content-Type", "application/json")
            .body(serde_json::to_string(&body).unwrap_or_default())
            .send()
            .await
    }).map_err(|e| format!("Request failed: {}", e))?;

    let status = resp.status().as_u16();
    let text = block_on(async { resp.text().await }).map_err(|e| format!("Read error: {}", e))?;

    if status < 200 || status >= 300 {
        if status == 401 {
            // Never surface credential material echoed by the provider.
            return Err("HTTP 401 (authentication failed)".into());
        }
        let msg = if text.len() > 200 { &text[..200] } else { &text };
        return Err(format!("HTTP {}: {}", status, msg));
    }

    let parsed: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("JSON parse: {}", e))?;

    let mut result = String::new();
    if let Some(output) = parsed.get("output").and_then(|o| o.as_array()) {
        for item in output {
            if item.get("type").map_or(false, |t| t == "message") {
                if let Some(content) = item.get("content").and_then(|c| c.as_array()) {
                    for part in content {
                        if part.get("type").map_or(false, |t| t == "output_text") {
                            if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                                result.push_str(t);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(result)
}

// ── SSE parsing ──

/// Outcome of processing a single Responses API SSE event.
#[derive(Debug)]
enum EventAction {
    /// Keep consuming the stream.
    Continue,
    /// Terminal event received; emit the accumulated `Done` now.
    Completed { stop_reason: Option<String> },
    /// Terminal failure; abort with the given error message.
    Failed(String),
}

/// Mutable state accumulated across Responses API SSE events.
#[derive(Default)]
struct ResponsesParseState {
    accumulated_text: String,
    reasoning_text: String,
    tool_calls: Vec<serde_json::Value>,
    tool_index: usize,
    /// Completed function calls in model output order: (call_id, name, parsed input).
    /// Attached to `Done.raw_message` as `ToolUse` blocks so the agent loop can
    /// execute them and continue the next round.
    tool_uses: Vec<(String, String, serde_json::Value)>,
    /// Completed server-side web search calls in output order: (id, action).
    /// Attached to `Done.raw_message` as `WebSearchCall` blocks so the agent
    /// loop can echo them back and the server restores its search results.
    web_search_calls: Vec<(String, serde_json::Value)>,
    usage: Option<deepx_types::UsageInfo>,
}

/// Parse `usage` from the `response` object carried by terminal events.
///
/// DeepSeek reports context-cache hits under `input_tokens_details.cached_tokens`
/// and chain-of-thought tokens under `output_tokens_details.reasoning_tokens`.
fn parse_usage(resp_data: &serde_json::Value) -> Option<deepx_types::UsageInfo> {
    let u = resp_data.get("usage")?;
    let input_tokens = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let output_tokens = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let cached = u
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    Some(deepx_types::UsageInfo {
        prompt_tokens: input_tokens,
        completion_tokens: output_tokens,
        total_tokens: u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        prompt_cache_hit_tokens: cached,
        prompt_cache_miss_tokens: input_tokens.saturating_sub(cached),
        reasoning_tokens: u
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        cache_usage_reported: Some(true),
    })
}

/// Emit the final `Done` event from accumulated state (idempotent: consumes state).
fn emit_done(
    state: &mut ResponsesParseState,
    stop_reason: Option<String>,
    on_event: &mut dyn FnMut(StreamEvent),
) {
    let mut content_blocks: Vec<ContentBlock> = Vec::new();
    if !state.reasoning_text.is_empty() {
        content_blocks.push(ContentBlock::Reasoning {
            reasoning: std::mem::take(&mut state.reasoning_text),
        });
    }
    if !state.accumulated_text.is_empty() {
        content_blocks.push(ContentBlock::Text {
            text: std::mem::take(&mut state.accumulated_text),
        });
    }
    for (id, name, input) in std::mem::take(&mut state.tool_uses) {
        content_blocks.push(ContentBlock::ToolUse { id, name, input });
    }
    for (id, action) in std::mem::take(&mut state.web_search_calls) {
        content_blocks.push(ContentBlock::WebSearchCall { id, action });
    }
    let raw_message = Message {
        msg_id: None,
        role: "assistant".into(),
        name: None,
        content: content_blocks,
    };
    on_event(StreamEvent::Done {
        raw_message,
        usage: state.usage.take(),
        stop_reason,
    });
}

/// Process one parsed Responses API SSE event.
///
/// Terminal-event contract (OpenAI + DeepSeek): `response.completed` is the last
/// event on success, `response.incomplete` when truncated (e.g. max_output_tokens),
/// and `response.failed` on error — each carries the full response object.
fn handle_responses_event(
    data: &serde_json::Value,
    state: &mut ResponsesParseState,
    on_event: &mut dyn FnMut(StreamEvent),
) -> EventAction {
    let typ = data.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match typ {
        "response.web_search_call.in_progress"
        | "response.web_search_call.searching"
        | "response.web_search_call.completed" => {
            let status = typ.trim_start_matches("response.web_search_call.").to_string();
            on_event(StreamEvent::WebSearchStatus(status));
            EventAction::Continue
        }
        "response.output_text.delta" => {
            if let Some(delta) = data.get("delta").and_then(|d| d.as_str()) {
                if delta.is_empty() {
                    return EventAction::Continue;
                }
                state.accumulated_text.push_str(delta);
                on_event(StreamEvent::ContentDelta(delta.to_string()));
            }
            EventAction::Continue
        }
        "response.reasoning_text.delta" => {
            if let Some(delta) = data.get("delta").and_then(|d| d.as_str()) {
                if delta.is_empty() {
                    return EventAction::Continue;
                }
                state.reasoning_text.push_str(delta);
                on_event(StreamEvent::ReasoningDelta(delta.to_string()));
            }
            EventAction::Continue
        }
        "response.function_call_arguments.delta" => {
            if let Some(delta) = data.get("delta").and_then(|d| d.as_str()) {
                if delta.is_empty() {
                    return EventAction::Continue;
                }
                let item_id = data.get("item_id").and_then(|i| i.as_str()).unwrap_or("");
                if let Some(tc) = state.tool_calls.iter_mut().find(|tc| {
                    tc.get("item_id").and_then(|i| i.as_str()) == Some(item_id)
                }) {
                    let cur = tc.get("args").and_then(|a| a.as_str()).unwrap_or("");
                    let new_args = format!("{}{}", cur, delta);
                    if let Some(obj) = tc.as_object_mut() {
                        obj.insert("args".into(), serde_json::json!(new_args));
                    }
                } else {
                    state.tool_calls.push(serde_json::json!({
                        "item_id": item_id,
                        "args": delta,
                    }));
                }
            }
            EventAction::Continue
        }
        "response.output_item.done" => {
            if let Some(item) = data.get("item") {
                let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if item_type == "function_call" {
                    let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    let args = item.get("arguments").and_then(|a| a.as_str()).unwrap_or("");
                    let call_id = item.get("call_id").and_then(|c| c.as_str()).unwrap_or("");
                    on_event(StreamEvent::ToolCallProgress {
                        index: state.tool_index,
                        id: call_id.to_string(),
                        name: name.to_string(),
                        args_so_far: args.to_string(),
                    });
                    // Preserve the completed call so `emit_done` can attach
                    // ToolUse blocks — the agent loop executes tools from
                    // Done.raw_message, not from preview events.
                    let input: serde_json::Value =
                        serde_json::from_str(args).unwrap_or(serde_json::Value::Null);
                    if !call_id.is_empty() && !name.is_empty() {
                        state
                            .tool_uses
                            .push((call_id.to_string(), name.to_string(), input));
                    }
                    state.tool_index += 1;
                } else if item_type == "web_search_call" {
                    let call_id = item.get("id").and_then(|c| c.as_str()).unwrap_or("");
                    let action = item
                        .get("action")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({"type": "search"}));
                    if !call_id.is_empty() {
                        state
                            .web_search_calls
                            .push((call_id.to_string(), action));
                    }
                }
            }
            EventAction::Continue
        }
        "response.completed" => {
            if let Some(usage) = data.get("response").and_then(parse_usage) {
                state.usage = Some(usage.clone());
                on_event(StreamEvent::UsageUpdate(usage));
            }
            EventAction::Completed { stop_reason: None }
        }
        "response.incomplete" => {
            if let Some(usage) = data.get("response").and_then(parse_usage) {
                state.usage = Some(usage.clone());
                on_event(StreamEvent::UsageUpdate(usage));
            }
            // Truncated (e.g. max_output_tokens): emit accumulated content with
            // the reason so callers can surface the incomplete state.
            let stop_reason = data
                .get("response")
                .and_then(|r| r.get("incomplete_details"))
                .and_then(|d| d.get("reason"))
                .and_then(|v| v.as_str())
                .map(String::from);
            EventAction::Completed { stop_reason }
        }
        "response.failed" => {
            let message = data
                .get("response")
                .and_then(|r| r.get("error"))
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("response failed")
                .to_string();
            EventAction::Failed(message)
        }
        _ => EventAction::Continue,
    }
}

#[allow(clippy::string_slice)]
fn parse_responses_sse(
    resp: reqwest::Response,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut dyn FnMut(StreamEvent),
) -> anyhow::Result<()> {
    let mut stream = resp.bytes_stream();
    // 字节级累积缓冲：TCP/HTTP chunk 可能把一个 UTF-8 多字节字符切成两半，
    // 绝不能逐 chunk 做 lossy 解码（会产生 U+FFFD 替换符并永久污染文本）。
    // 只在拿到完整的 `\n` 结尾行之后，才对该行做严格 UTF-8 解码。
    let mut buf: Vec<u8> = Vec::new();

    let mut state = ResponsesParseState::default();

    loop {
        if is_cancelled(cancel) {
            return Err(anyhow::anyhow!("cancelled by user"));
        }

        let chunk = match block_on(async {
            futures::future::select(
                Box::pin(stream.next()),
                Box::pin(tokio::time::sleep(SSE_POLL_INTERVAL)),
            )
            .await
        }) {
            futures::future::Either::Left((Some(Ok(bytes)), _)) => bytes,
            futures::future::Either::Left((Some(Err(e)), _)) => {
                return Err(anyhow::anyhow!("Stream error: {}", e));
            }
            futures::future::Either::Left((None, _)) => break,
            futures::future::Either::Right(_) => continue,
        };

        buf.extend_from_slice(&chunk);
        match feed_responses_sse(&mut buf, &mut state, on_event) {
            Ok(SseProgress::Continue) => {}
            Ok(SseProgress::Done) => return Ok(()),
            Err(message) => return Err(anyhow::anyhow!("{}", message)),
        }
    }

    // 流结束：处理缓冲区中最后一条没有换行结尾的行（补一个换行触发消费）。
    if !buf.is_empty() {
        buf.push(b'\n');
        match feed_responses_sse(&mut buf, &mut state, on_event) {
            Ok(SseProgress::Continue | SseProgress::Done) => {}
            Err(message) => return Err(anyhow::anyhow!("{}", message)),
        }
    }

    // Stream closed without a terminal event: emit whatever was accumulated.
    emit_done(&mut state, None, on_event);

    Ok(())
}

/// SSE 处理进展：`Done` 表示应终止读取（收到 `[DONE]` 或 terminal 事件）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SseProgress {
    Continue,
    Done,
}

/// 把新到达的字节追加进缓冲，并消费所有已完整的行。
///
/// 关键约束（UTF-8 真实性）：
/// - 只在字节层面按 `\n` 切分；`\n` (0x0A) 不可能出现在 UTF-8 多字节序列内部，
///   因此完整行天然对齐字符边界；
/// - 整行用 `str::from_utf8` 严格解码；非法行跳过并告警，**绝不 lossy 注入替换符**；
/// - `event:` / `data:` 配对在行解码之后进行。
fn feed_responses_sse(
    buf: &mut Vec<u8>,
    state: &mut ResponsesParseState,
    on_event: &mut dyn FnMut(StreamEvent),
) -> Result<SseProgress, String> {
    loop {
        let Some(line_end) = buf.iter().position(|&b| b == b'\n') else {
            return Ok(SseProgress::Continue);
        };
        let line_bytes: Vec<u8> = buf.drain(..=line_end).collect();
        let line = match std::str::from_utf8(&line_bytes[..line_bytes.len() - 1]) {
            Ok(line) => line.trim(),
            Err(e) => {
                log::warn!("[GATE] responses SSE line is not valid UTF-8, skipping: {e}");
                continue;
            }
        };

        if line.is_empty() || line.starts_with(':') {
            continue;
        }

        // Handle SSE "event:" + "data:" pairs
        let data_str: String = if let Some(_rest) = line.strip_prefix("event: ") {
            let Some(next_end) = buf.iter().position(|&b| b == b'\n') else {
                return Ok(SseProgress::Continue);
            };
            let next_bytes: Vec<u8> = buf.drain(..=next_end).collect();
            let next_line = match std::str::from_utf8(&next_bytes[..next_bytes.len() - 1]) {
                Ok(next) => next.trim(),
                Err(e) => {
                    log::warn!("[GATE] responses SSE data line is not valid UTF-8, skipping: {e}");
                    continue;
                }
            };
            // Strip "data: " prefix from the next line
            match next_line.strip_prefix("data: ") {
                Some(payload) => payload.trim().to_string(),
                None => next_line.to_string(),
            }
        } else if let Some(payload) = line.strip_prefix("data: ") {
            payload.trim().to_string()
        } else {
            continue;
        };

        if data_str == "[DONE]" {
            return Ok(SseProgress::Done);
        }

        let data: serde_json::Value = match serde_json::from_str(&data_str) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("[GATE] responses SSE JSON parse failed: {e} — data: {data_str}");
                continue;
            }
        };

        match handle_responses_event(&data, state, on_event) {
            EventAction::Continue => {}
            EventAction::Completed { stop_reason } => {
                emit_done(state, stop_reason, on_event);
                return Ok(SseProgress::Done);
            }
            EventAction::Failed(message) => return Err(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepx_types::{ContentBlock, Message, ToolDef, ToolFunction};

    // ── SSE UTF-8 边界 ──

    #[test]
    fn sse_utf8_char_split_across_chunks_is_not_corrupted() {
        let mut state = ResponsesParseState::default();
        let mut buf: Vec<u8> = Vec::new();
        let mut events: Vec<StreamEvent> = Vec::new();

        // "中" = E4 B8 AD，故意把字节切到两个网络 chunk 里。
        let chunk1 = b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"\xe4\xb8";
        let chunk2 = b"\xad\"}\n\n";

        buf.extend_from_slice(chunk1);
        let progress = feed_responses_sse(&mut buf, &mut state, &mut |e| events.push(e)).unwrap();
        assert_eq!(progress, SseProgress::Continue);
        assert!(events.is_empty(), "半行不得提前派发");
        assert_eq!(state.accumulated_text, "");

        buf.extend_from_slice(chunk2);
        let progress = feed_responses_sse(&mut buf, &mut state, &mut |e| events.push(e)).unwrap();
        assert_eq!(progress, SseProgress::Continue);
        assert_eq!(state.accumulated_text, "中");
        assert!(matches!(
            events.as_slice(),
            [StreamEvent::ContentDelta(d)] if d == "中"
        ));
    }

    #[test]
    fn sse_emoji_split_across_chunks_is_not_corrupted() {
        let mut state = ResponsesParseState::default();
        let mut buf: Vec<u8> = Vec::new();
        let mut events: Vec<StreamEvent> = Vec::new();

        // "👍" = F0 9F 91 8D，切成 2+2 与 3+1 两种边界都要还原。
        let chunk1 = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"\xf0\x9f";
        let chunk2 = b"\x91\x8d\"}\n\n";
        buf.extend_from_slice(chunk1);
        buf.extend_from_slice(chunk2);
        let progress = feed_responses_sse(&mut buf, &mut state, &mut |e| events.push(e)).unwrap();
        assert_eq!(progress, SseProgress::Continue);
        assert_eq!(state.accumulated_text, "👍");
        assert!(matches!(
            events.as_slice(),
            [StreamEvent::ContentDelta(d)] if d == "👍"
        ));
    }

    #[test]
    fn sse_utf8_split_in_tool_arguments_is_not_corrupted() {
        let mut state = ResponsesParseState::default();
        let mut buf: Vec<u8> = Vec::new();
        let mut events: Vec<StreamEvent> = Vec::new();

        // 工具参数 JSON 里的中文字段值被 TCP 切断。
        let chunk1 = b"event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"i1\",\"delta\":\"{\\\"path\\\":\\\"\xe8\xaf\xbe";
        let chunk2 = b"\xe9\xa2\x98\\\"}\"}\n\n";
        buf.extend_from_slice(chunk1);
        buf.extend_from_slice(chunk2);
        let progress = feed_responses_sse(&mut buf, &mut state, &mut |e| events.push(e)).unwrap();
        assert_eq!(progress, SseProgress::Continue);
        assert_eq!(state.tool_calls.len(), 1);
        assert_eq!(
            state.tool_calls[0].get("args").and_then(|a| a.as_str()),
            Some("{\"path\":\"课题\"}")
        );
        assert!(events.is_empty(), "arguments.delta 只累积，不派发 preview");

        // output_item.done 携带完整参数并派发 ToolCallProgress。
        buf.extend_from_slice(
            b"event: response.output_item.done\n\
              data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\
              \"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"\xe8\xaf\xbe\xe9\xa2\x98\\\"}\",\"call_id\":\"call_1\"}}\n\n",
        );
        let progress = feed_responses_sse(&mut buf, &mut state, &mut |e| events.push(e)).unwrap();
        assert_eq!(progress, SseProgress::Continue);
        assert!(matches!(
            events.as_slice(),
            [StreamEvent::ToolCallProgress { args_so_far, .. }] if args_so_far == "{\"path\":\"课题\"}"
        ));
    }

    #[test]
    fn empty_deltas_are_not_carried_into_context_or_tools() {
        let mut state = ResponsesParseState::default();
        let mut buf: Vec<u8> = Vec::new();
        let mut events: Vec<StreamEvent> = Vec::new();

        // 空 delta 不得进入累积文本、推理文本或工具参数，也不得派发事件。
        buf.extend_from_slice(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"\"}\n\n\
              data: {\"type\":\"response.reasoning_text.delta\",\"delta\":\"\"}\n\n\
              data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"i1\",\"delta\":\"\"}\n\n",
        );
        let progress = feed_responses_sse(&mut buf, &mut state, &mut |e| events.push(e)).unwrap();
        assert_eq!(progress, SseProgress::Continue);
        assert_eq!(state.accumulated_text, "");
        assert_eq!(state.reasoning_text, "");
        assert!(state.tool_calls.is_empty(), "空参数增量不得创建 args 条目");
        assert!(events.is_empty(), "空 delta 不得派发任何事件");
    }

    #[test]
    fn sse_done_terminates_and_final_line_without_newline_is_consumed() {
        let mut state = ResponsesParseState::default();
        let mut buf: Vec<u8> = Vec::new();
        let mut events: Vec<StreamEvent> = Vec::new();

        // 最后一个事件没有结尾换行：EOF 时必须补 \n 消费，不能丢内容。
        buf.extend_from_slice(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n\
              data: {\"type\":\"response.output_text.delta\",\"delta\":\"tail\"}",
        );
        let progress = feed_responses_sse(&mut buf, &mut state, &mut |e| events.push(e)).unwrap();
        assert_eq!(progress, SseProgress::Continue);
        assert_eq!(state.accumulated_text, "ok");
        assert!(!buf.is_empty(), "无换行尾巴应保留在缓冲里由 EOF 路径消费");

        buf.push(b'\n');
        let progress = feed_responses_sse(&mut buf, &mut state, &mut |e| events.push(e)).unwrap();
        assert_eq!(progress, SseProgress::Continue);
        assert_eq!(state.accumulated_text, "oktail");

        buf.extend_from_slice(b"data: [DONE]\n\n");
        let progress = feed_responses_sse(&mut buf, &mut state, &mut |e| events.push(e)).unwrap();
        assert_eq!(progress, SseProgress::Done);
    }

    /// OpenAI-reference compat used by conversion tests. `web_search` stays on
    /// by default so tests exercise the full tool list, matching production.
    fn test_compat() -> ResponsesCompat {
        ResponsesCompat::default()
    }

    // ── URL construction ──

    #[test]
    fn url_appends_responses_to_base() {
        assert_eq!(
            build_responses_url("https://api.openai.com/v1", None),
            "https://api.openai.com/v1/responses"
        );
    }

    #[test]
    fn url_uses_custom_path() {
        assert_eq!(
            build_responses_url("https://api.openai.com/v1", Some("/v1/responses")),
            "https://api.openai.com/v1/v1/responses"
        );
    }

    #[test]
    fn url_no_double_slash() {
        assert_eq!(
            build_responses_url("https://api.openai.com/v1/", None),
            "https://api.openai.com/v1/responses"
        );
    }

    #[test]
    fn url_already_has_responses() {
        assert_eq!(
            build_responses_url("https://api.openai.com/v1/responses", None),
            "https://api.openai.com/v1/responses"
        );
    }

    #[test]
    fn url_absolute_path_override() {
        assert_eq!(
            build_responses_url("https://foo.com", Some("https://bar.com/v1/responses")),
            "https://bar.com/v1/responses"
        );
    }

    // ── Message conversion ──

    #[test]
    fn user_message_becomes_input() {
        let msgs = vec![Message::user("hello")];
        let (input, _instructions) = convert_messages_to_input(&msgs, &test_compat());
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
        let content = input[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[0]["text"], "hello");
    }

    #[test]
    fn first_system_becomes_instructions_rest_stay_developer() {
        // 第一条 system → 顶层 instructions（文档推荐承载位）
        let msgs = vec![Message::system("you are helpful")];
        let (input, instructions) = convert_messages_to_input(&msgs, &test_compat());
        assert_eq!(instructions.as_deref(), Some("you are helpful"));
        assert!(input.is_empty(), "base system must not duplicate into input[]");

        // 动态注入的后续 system → developer item
        let msgs = vec![
            Message::system("base prompt"),
            Message::system("skills catalog"),
            Message::system("context envelope"),
        ];
        let (input, instructions) = convert_messages_to_input(&msgs, &test_compat());
        assert_eq!(instructions.as_deref(), Some("base prompt"));
        assert_eq!(input.len(), 2, "two dynamic system messages stay as developer items");
        for item in &input {
            assert_eq!(item["role"], "developer");
        }
    }

    #[test]
    fn assistant_with_text() {
        let msgs = vec![Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: vec![ContentBlock::Text { text: "I'll help".into() }],
        }];
        let (input, _instructions) = convert_messages_to_input(&msgs, &test_compat());
        assert_eq!(input[0]["role"], "assistant");
        let content = input[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "output_text");
        assert_eq!(content[0]["text"], "I'll help");
    }

    #[test]
    fn assistant_tool_use_becomes_function_call() {
        let msgs = vec![Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: vec![ContentBlock::ToolUse {
                id: "tc_1".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "/x.txt"}),
            }],
        }];
        let (input, _instructions) = convert_messages_to_input(&msgs, &test_compat());
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["call_id"], "tc_1");
        assert_eq!(input[0]["name"], "read_file");
        assert_eq!(input[0]["status"], "completed");
        assert!(input[0]["arguments"].as_str().unwrap().contains("path"));
    }

    #[test]
    fn tool_message_becomes_function_call_output() {
        let msgs = vec![Message {
            msg_id: None,
            role: "tool".into(),
            name: None,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tc_1".into(),
                content: "file contents".into(),
                success: true,
            }],
        }];
        let (input, _instructions) = convert_messages_to_input(&msgs, &test_compat());
        assert_eq!(input[0]["type"], "function_call_output");
        assert_eq!(input[0]["call_id"], "tc_1");
        assert_eq!(input[0]["output"], "file contents");
    }

    #[test]
    fn assistant_with_text_and_tool_call() {
        let msgs = vec![Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: vec![
                ContentBlock::Text { text: "let me check".into() },
                ContentBlock::ToolUse {
                    id: "tc_2".into(),
                    name: "search".into(),
                    input: serde_json::json!({"q": "rust"}),
                },
            ],
        }];
        let (input, _instructions) = convert_messages_to_input(&msgs, &test_compat());
        // Should have: message (with text) + function_call
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["name"], "search");
    }

    #[test]
    fn empty_user_message_gets_default_text() {
        let msgs = vec![Message {
            msg_id: None,
            role: "user".into(),
            name: None,
            content: vec![],
        }];
        let (input, _instructions) = convert_messages_to_input(&msgs, &test_compat());
        let content = input[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["text"], "");
    }

    #[test]
    fn reasoning_content_preserved_in_final_message() {
        // Verify the ContentBlock::Reasoning variant is used correctly
        let block = ContentBlock::Reasoning { reasoning: "thinking...".into() };
        assert_eq!(
            match &block {
                ContentBlock::Reasoning { reasoning } => reasoning.clone(),
                _ => panic!("wrong variant"),
            },
            "thinking..."
        );
    }

    // ── Tool conversion ──

    #[test]
    fn convert_tools_empty() {
        // Built-in web_search is injected even with no function tools.
        let with_web = convert_tools(None, &test_compat()).unwrap();
        assert_eq!(with_web.len(), 1);
        assert_eq!(with_web[0]["type"], "web_search");
        // Disabled compat → no tools at all.
        let mut compat = test_compat();
        compat.web_search = false;
        assert!(convert_tools(None, &compat).is_none());
        assert!(convert_tools(Some(vec![]), &compat).is_none());
    }

    #[test]
    fn convert_tools_normal() {
        let tools = vec![ToolDef {
            call_type: "function".into(),
            function: ToolFunction {
                name: "search".into(),
                description: "searches".into(),
                parameters: serde_json::json!({"type": "object"}),
            },
        }];
        let result = convert_tools(Some(tools), &test_compat()).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["type"], "function");
        assert_eq!(result[0]["name"], "search");
        assert_eq!(result[0]["description"], "searches");
        assert_eq!(result[1]["type"], "web_search");
    }

    #[test]
    fn convert_tools_web_search_placement() {
        // Function tools keep their order; web_search is appended last.
        let tools = vec![ToolDef {
            call_type: "function".into(),
            function: ToolFunction {
                name: "a".into(),
                description: String::new(),
                parameters: serde_json::json!({"type": "object"}),
            },
        }];
        let result = convert_tools(Some(tools), &test_compat()).unwrap();
        assert_eq!(result[0]["name"], "a");
        assert_eq!(result[1]["type"], "web_search");
    }

    // ── effort clamping ──

    #[test]
    fn clamp_effort_defaults_to_medium() {
        assert_eq!(clamp_effort(None, "high"), "medium");
    }

    #[test]
    fn clamp_effort_respects_provider_bound() {
        // OpenAI bound: xhigh/max collapse to high.
        assert_eq!(clamp_effort(Some("xhigh".into()), "high"), "high");
        assert_eq!(clamp_effort(Some("max".into()), "high"), "high");
        // DeepSeek bound: everything passes through.
        assert_eq!(clamp_effort(Some("xhigh".into()), "max"), "xhigh");
        assert_eq!(clamp_effort(Some("low".into()), "max"), "low");
        // Unknown values fall back to the bound (never rejected).
        assert_eq!(clamp_effort(Some("ultra".into()), "high"), "high");
    }

    // ── web_search_call echo ──

    #[test]
    fn web_search_call_echoed_when_compat_allows() {
        let msgs = vec![Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: vec![
                ContentBlock::Text { text: "searching...".into() },
                ContentBlock::WebSearchCall {
                    id: "ws_1".into(),
                    action: serde_json::json!({"type": "search"}),
                },
            ],
        }];
        let (input, _instructions) = convert_messages_to_input(&msgs, &test_compat());
        let ws_items: Vec<_> = input
            .iter()
            .filter(|i| i.get("type").and_then(|t| t.as_str()) == Some("web_search_call"))
            .collect();
        assert_eq!(ws_items.len(), 1);
        assert_eq!(ws_items[0]["id"], "ws_1");
        assert_eq!(ws_items[0]["action"]["type"], "search");
    }

    #[test]
    fn web_search_call_suppressed_when_compat_disallows() {
        let mut compat = test_compat();
        compat.echo_web_search_call = false;
        let msgs = vec![Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: vec![ContentBlock::WebSearchCall {
                id: "ws_1".into(),
                action: serde_json::json!({"type": "search"}),
            }],
        }];
        let (input, _instructions) = convert_messages_to_input(&msgs, &compat);
        assert!(
            input
                .iter()
                .all(|i| i.get("type").and_then(|t| t.as_str()) != Some("web_search_call"))
        );
    }

    // ── extract_text ──

    #[test]
    fn extract_text_from_blocks() {
        let blocks = vec![ContentBlock::Text { text: "hello".into() }];
        assert_eq!(extract_text(&blocks), "hello");
    }

    #[test]
    fn extract_text_empty() {
        assert_eq!(extract_text(&[]), "");
    }

    // ── Full round-trip scenarios ──

    #[test]
    fn multi_turn_conversation() {
        let msgs = vec![
            Message::system("be helpful"),
            Message::user("hi"),
            Message {
                msg_id: None,
                role: "assistant".into(),
                name: None,
                content: vec![ContentBlock::Text { text: "hello!".into() }],
            },
            Message::user("read x.txt"),
        ];
        let (input, instructions) = convert_messages_to_input(&msgs, &test_compat());
        // 第一条 system 已被提取为 instructions
        assert_eq!(instructions.as_deref(), Some("be helpful"));
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[2]["role"], "user");
    }

    // ── SSE event handling (OpenAI + DeepSeek terminal events) ──

    #[test]
    fn completed_event_parses_usage_and_cache() {
        let mut state = ResponsesParseState::default();
        let mut events: Vec<StreamEvent> = Vec::new();
        let data = serde_json::json!({
            "type": "response.completed",
            "response": {
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 20,
                    "total_tokens": 120,
                    "input_tokens_details": { "cached_tokens": 60 },
                    "output_tokens_details": { "reasoning_tokens": 5 }
                }
            }
        });
        let action = handle_responses_event(&data, &mut state, &mut |e| events.push(e));
        assert!(matches!(action, EventAction::Completed { stop_reason: None }));
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::UsageUpdate(u) => {
                assert_eq!(u.prompt_tokens, 100);
                assert_eq!(u.completion_tokens, 20);
                assert_eq!(u.total_tokens, 120);
                assert_eq!(u.prompt_cache_hit_tokens, 60);
                assert_eq!(u.prompt_cache_miss_tokens, 40);
                assert_eq!(u.reasoning_tokens, 5);
                assert_eq!(u.cache_usage_reported, Some(true));
            }
            other => panic!("expected UsageUpdate, got {other:?}"),
        }
        let stored = state.usage.as_ref().expect("usage stored in state");
        assert_eq!(stored.prompt_tokens, 100);
        assert_eq!(stored.prompt_cache_hit_tokens, 60);
        assert_eq!(stored.reasoning_tokens, 5);
    }

    #[test]
    fn incomplete_event_yields_stop_reason_and_usage() {
        let mut state = ResponsesParseState::default();
        let mut events: Vec<StreamEvent> = Vec::new();
        let data = serde_json::json!({
            "type": "response.incomplete",
            "response": {
                "status": "incomplete",
                "incomplete_details": { "reason": "max_output_tokens" },
                "usage": { "input_tokens": 10, "output_tokens": 200, "total_tokens": 210 }
            }
        });
        let action = handle_responses_event(&data, &mut state, &mut |e| events.push(e));
        match action {
            EventAction::Completed { stop_reason } => {
                assert_eq!(stop_reason.as_deref(), Some("max_output_tokens"));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        assert!(matches!(&events[0], StreamEvent::UsageUpdate(_)));
    }

    #[test]
    fn failed_event_yields_error_message() {
        let mut state = ResponsesParseState::default();
        let mut events: Vec<StreamEvent> = Vec::new();
        let data = serde_json::json!({
            "type": "response.failed",
            "response": {
                "error": { "message": "upstream model error" }
            }
        });
        let action = handle_responses_event(&data, &mut state, &mut |e| events.push(e));
        match action {
            EventAction::Failed(message) => assert_eq!(message, "upstream model error"),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(events.is_empty());
    }

    #[test]
    fn function_call_done_appears_in_done_message() {
        // Simulate the full tool-call stream: argument deltas, then the
        // completed function_call item, then a terminal event.
        let mut state = ResponsesParseState::default();
        let mut events: Vec<StreamEvent> = Vec::new();
        let delta = serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "item_id": "fc_1",
            "delta": "{\"city\": \"Beijing\"}"
        });
        assert!(matches!(
            handle_responses_event(&delta, &mut state, &mut |e| events.push(e)),
            EventAction::Continue
        ));
        let done = serde_json::json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_abc",
                "name": "get_weather",
                "arguments": "{\"city\": \"Beijing\"}",
                "status": "completed"
            }
        });
        assert!(matches!(
            handle_responses_event(&done, &mut state, &mut |e| events.push(e)),
            EventAction::Continue
        ));
        // Preview event emitted for the UI.
        assert!(matches!(
            &events[0],
            StreamEvent::ToolCallProgress { id, name, .. } if id == "call_abc" && name == "get_weather"
        ));

        emit_done(&mut state, None, &mut |e| events.push(e));
        match &events[1] {
            StreamEvent::Done { raw_message, .. } => {
                let tool_uses: Vec<_> = raw_message
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse { id, name, input } => {
                            Some((id.as_str(), name.as_str(), input.clone()))
                        }
                        _ => None,
                    })
                    .collect();
                assert_eq!(tool_uses.len(), 1);
                let (id, name, input) = &tool_uses[0];
                assert_eq!(*id, "call_abc");
                assert_eq!(*name, "get_weather");
                assert_eq!(input["city"], "Beijing");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn web_search_call_events_stream_and_roundtrip() {
        // Status events surface progress; the completed item attaches a
        // WebSearchCall block to Done so the next turn echoes it back.
        let mut state = ResponsesParseState::default();
        let mut events: Vec<StreamEvent> = Vec::new();

        let searching = serde_json::json!({
            "type": "response.web_search_call.searching",
            "item_id": "ws_1",
        });
        assert!(matches!(
            handle_responses_event(&searching, &mut state, &mut |e| events.push(e)),
            EventAction::Continue
        ));
        assert!(matches!(
            &events[0],
            StreamEvent::WebSearchStatus(s) if s == "searching"
        ));

        let done = serde_json::json!({
            "type": "response.output_item.done",
            "item": {
                "type": "web_search_call",
                "id": "ws_1",
                "action": {"type": "search"},
                "status": "completed"
            }
        });
        assert!(matches!(
            handle_responses_event(&done, &mut state, &mut |e| events.push(e)),
            EventAction::Continue
        ));

        emit_done(&mut state, None, &mut |e| events.push(e));
        match &events[1] {
            StreamEvent::Done { raw_message, .. } => {
                let ws: Vec<_> = raw_message
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::WebSearchCall { id, action } => {
                            Some((id.as_str(), action.clone()))
                        }
                        _ => None,
                    })
                    .collect();
                assert_eq!(ws.len(), 1);
                assert_eq!(ws[0].0, "ws_1");
                assert_eq!(ws[0].1["type"], "search");
            }
            other => panic!("expected Done, got {other:?}"),
        }

        // Round-trip: the echoed input item restores the search call.
        let raw_message = match &events[1] {
            StreamEvent::Done { raw_message, .. } => raw_message.clone(),
            other => panic!("expected Done, got {other:?}"),
        };
        let (input, _instructions) = convert_messages_to_input(&[raw_message], &test_compat());
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "web_search_call");
        assert_eq!(input[0]["id"], "ws_1");
    }

    #[test]
    fn done_with_tool_use_roundtrips_to_function_call_input() {
        // A Done message carrying ToolUse must convert back into a
        // function_call input item for the next round (multi-turn contract).
        let state_events = {
            let mut state = ResponsesParseState::default();
            let mut events: Vec<StreamEvent> = Vec::new();
            let done = serde_json::json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": "call_1",
                    "name": "read_file",
                    "arguments": "{\"path\": \"/x.txt\"}",
                    "status": "completed"
                }
            });
            assert!(matches!(
                handle_responses_event(&done, &mut state, &mut |e| events.push(e)),
                EventAction::Continue
            ));
            emit_done(&mut state, None, &mut |e| events.push(e));
            events
        };
        let raw_message = match &state_events[1] {
            StreamEvent::Done { raw_message, .. } => raw_message.clone(),
            other => panic!("expected Done, got {other:?}"),
        };
        let (input, _instructions) = convert_messages_to_input(&[raw_message], &test_compat());
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["call_id"], "call_1");
        assert_eq!(input[0]["name"], "read_file");
        assert_eq!(input[0]["status"], "completed");
        assert!(input[0]["arguments"].as_str().unwrap().contains("x.txt"));
    }

    #[test]
    fn delta_events_accumulate_into_done() {
        let mut state = ResponsesParseState::default();
        let mut events: Vec<StreamEvent> = Vec::new();
        let t1 = serde_json::json!({"type": "response.output_text.delta", "delta": "Hello"});
        let t2 = serde_json::json!({"type": "response.reasoning_text.delta", "delta": "hmm"});
        let t3 = serde_json::json!({"type": "response.output_text.delta", "delta": " world"});
        assert!(matches!(
            handle_responses_event(&t1, &mut state, &mut |e| events.push(e)),
            EventAction::Continue
        ));
        assert!(matches!(
            handle_responses_event(&t2, &mut state, &mut |e| events.push(e)),
            EventAction::Continue
        ));
        assert!(matches!(
            handle_responses_event(&t3, &mut state, &mut |e| events.push(e)),
            EventAction::Continue
        ));
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], StreamEvent::ContentDelta(d) if d == "Hello"));
        assert!(matches!(&events[1], StreamEvent::ReasoningDelta(d) if d == "hmm"));

        emit_done(&mut state, None, &mut |e| events.push(e));
        match &events[3] {
            StreamEvent::Done { raw_message, stop_reason, .. } => {
                assert_eq!(stop_reason, &None);
                assert!(raw_message
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Reasoning { .. })));
                assert!(raw_message.content.iter().any(
                    |b| matches!(b, ContentBlock::Text { text } if text == "Hello world")
                ));
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }
}
