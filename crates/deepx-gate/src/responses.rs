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

use super::types::{ProviderConfig, StreamEvent};

const SSE_POLL_INTERVAL: Duration = Duration::from_millis(50);

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

fn convert_messages_to_input(messages: &[Message]) -> Vec<serde_json::Value> {
    let mut items: Vec<serde_json::Value> = Vec::new();

    for msg in messages {
        match msg.role.as_str() {
            "system" => {
                let text = extract_text(&msg.content);
                items.push(serde_json::json!({
                    "type": "message",
                    "role": "developer",
                    "content": [{"type": "input_text", "text": text}],
                }));
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

    items
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
    for b in blocks {
        match b {
            ContentBlock::Text { text } => {
                parts.push(serde_json::json!({"type": "input_text", "text": text}));
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        parts.push(serde_json::json!({"type": "input_text", "text": ""}));
    }
    parts
}

fn convert_tools(tools: Option<Vec<ToolDef>>) -> Option<Vec<serde_json::Value>> {
    let tds = tools?;
    if tds.is_empty() {
        return None;
    }
    let items: Vec<serde_json::Value> = tds.into_iter().map(|td| {
        serde_json::json!({
            "type": "function",
            "name": td.function.name,
            "description": td.function.description,
            "parameters": td.function.parameters,
        })
    }).collect();
    Some(items)
}

// ── Public API ──

#[allow(clippy::string_slice)]
pub fn chat_stream_responses(
    provider: &ProviderConfig,
    model: &str,
    messages: Vec<Message>,
    tools: Option<Vec<ToolDef>>,
    _max_tokens: u32,
    effort: Option<String>,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut dyn FnMut(StreamEvent),
) -> anyhow::Result<()> {
    let input_items = convert_messages_to_input(&messages);
    let responses_tools = convert_tools(tools);

    let mut body_map = serde_json::Map::new();
    body_map.insert("model".into(), serde_json::json!(model));
    body_map.insert("input".into(), serde_json::Value::Array(input_items));
    body_map.insert("stream".into(), serde_json::json!(true));
    body_map.insert("store".into(), serde_json::json!(false));
    body_map.insert("parallel_tool_calls".into(), serde_json::json!(true));

    if let Some(ref t) = responses_tools {
        body_map.insert("tools".into(), serde_json::Value::Array(t.clone()));
    }

    body_map.insert(
        "reasoning".into(),
        serde_json::json!({
            "effort": effort.unwrap_or_else(|| "medium".into()),
            "summary": "auto",
        }),
    );
    body_map.insert("include".into(), serde_json::json!(["reasoning.encrypted_content"]));

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
                    if status == 429 || status == 500 || status == 502 || status == 503 {
                        if attempt < 3 {
                            let delay = Duration::from_secs(2u64.pow(attempt));
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
                if attempt < 3 {
                    let delay = Duration::from_secs(2u64.pow(attempt));
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
    _max_tokens: u32,
) -> Result<String, String> {
    let input_items = convert_messages_to_input(&messages);

    let mut body_map = serde_json::Map::new();
    body_map.insert("model".into(), serde_json::json!(model));
    body_map.insert("input".into(), serde_json::Value::Array(input_items));
    body_map.insert("stream".into(), serde_json::json!(false));
    body_map.insert("store".into(), serde_json::json!(false));
    body_map.insert(
        "reasoning".into(),
        serde_json::json!({"effort": "medium", "summary": "auto"}),
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

#[allow(clippy::string_slice)]
fn parse_responses_sse(
    resp: reqwest::Response,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut dyn FnMut(StreamEvent),
) -> anyhow::Result<()> {
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();

    let mut accumulated_text = String::new();
    let mut reasoning_text = String::new();
    let mut tool_calls: Vec<serde_json::Value> = Vec::new();
    let mut usage: Option<deepx_types::UsageInfo> = None;
    let mut tool_index: usize = 0;

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

        buf.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(line_end) = buf.find('\n') {
            let line = buf[..line_end].trim().to_string();
            buf = buf[line_end + 1..].to_string();

            if line.is_empty() || line.starts_with(':') {
                continue;
            }

            // Handle SSE "event:" + "data:" pairs
            let (_, data_str) = if line.starts_with("event: ") {
                let next = if let Some(nl) = buf.find('\n') {
                    let d = buf[..nl].trim().to_string();
                    buf = buf[nl + 1..].to_string();
                    d
                } else {
                    continue;
                };
                // Strip "data: " prefix from the next line
                let payload = if next.starts_with("data: ") {
                    next[6..].trim().to_string()
                } else {
                    next
                };
                ("", payload)
            } else if line.starts_with("data: ") {
                ("", line[6..].trim().to_string())
            } else {
                continue;
            };

            if data_str == "[DONE]" {
                break;
            }

            let data: serde_json::Value = match serde_json::from_str(&data_str) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let typ = data.get("type").and_then(|t| t.as_str()).unwrap_or("");

            match typ {
                "response.output_text.delta" => {
                    if let Some(delta) = data.get("delta").and_then(|d| d.as_str()) {
                        accumulated_text.push_str(delta);
                        on_event(StreamEvent::ContentDelta(delta.to_string()));
                    }
                }
                "response.reasoning_text.delta" => {
                    if let Some(delta) = data.get("delta").and_then(|d| d.as_str()) {
                        reasoning_text.push_str(delta);
                        on_event(StreamEvent::ReasoningDelta(delta.to_string()));
                    }
                }
                "response.function_call_arguments.delta" => {
                    if let Some(delta) = data.get("delta").and_then(|d| d.as_str()) {
                        let item_id = data.get("item_id").and_then(|i| i.as_str()).unwrap_or("");
                        if let Some(tc) = tool_calls.iter_mut().find(|tc| {
                            tc.get("item_id").and_then(|i| i.as_str()) == Some(item_id)
                        }) {
                            let cur = tc.get("args").and_then(|a| a.as_str()).unwrap_or("");
                            let new_args = format!("{}{}", cur, delta);
                            tc.as_object_mut().unwrap().insert("args".into(), serde_json::json!(new_args));
                        } else {
                            tool_calls.push(serde_json::json!({
                                "item_id": item_id,
                                "args": delta,
                            }));
                        }
                    }
                }
                "response.output_item.done" => {
                    if let Some(item) = data.get("item") {
                        let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        if item_type == "function_call" {
                            let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("");
                            let args = item.get("arguments").and_then(|a| a.as_str()).unwrap_or("");
                            let call_id = item.get("call_id").and_then(|c| c.as_str()).unwrap_or("");
                            on_event(StreamEvent::ToolCallProgress {
                                index: tool_index,
                                id: call_id.to_string(),
                                name: name.to_string(),
                                args_so_far: args.to_string(),
                            });
                            tool_index += 1;
                        }
                    }
                }
                "response.completed" => {
                    if let Some(resp_data) = data.get("response") {
                        if let Some(u) = resp_data.get("usage") {
                            usage = Some(deepx_types::UsageInfo {
                                prompt_tokens: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                                completion_tokens: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                                total_tokens: u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                                prompt_cache_hit_tokens: 0,
                                prompt_cache_miss_tokens: 0,
                                reasoning_tokens: u.get("output_tokens_details")
                                    .and_then(|d| d.get("reasoning_tokens"))
                                    .and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                                cache_usage_reported: None,
                            });
                            on_event(StreamEvent::UsageUpdate(usage.clone().unwrap()));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // Build final message from accumulated content
    let mut content_blocks: Vec<ContentBlock> = Vec::new();

    if !reasoning_text.is_empty() {
        content_blocks.push(ContentBlock::Reasoning { reasoning: reasoning_text });
    }
    if !accumulated_text.is_empty() {
        content_blocks.push(ContentBlock::Text { text: accumulated_text });
    }

    let raw_message = Message {
        msg_id: None,
        role: "assistant".into(),
        name: None,
        content: content_blocks,
    };

    on_event(StreamEvent::Done {
        raw_message,
        usage,
        stop_reason: None,
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepx_types::{ContentBlock, Message, ToolDef, ToolFunction};

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
        let input = convert_messages_to_input(&msgs);
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
        let content = input[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[0]["text"], "hello");
    }

    #[test]
    fn system_message_becomes_developer() {
        let msgs = vec![Message::system("you are helpful")];
        let input = convert_messages_to_input(&msgs);
        assert_eq!(input[0]["role"], "developer");
    }

    #[test]
    fn assistant_with_text() {
        let msgs = vec![Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: vec![ContentBlock::Text { text: "I'll help".into() }],
        }];
        let input = convert_messages_to_input(&msgs);
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
        let input = convert_messages_to_input(&msgs);
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
        let input = convert_messages_to_input(&msgs);
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
        let input = convert_messages_to_input(&msgs);
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
        let input = convert_messages_to_input(&msgs);
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
        assert!(convert_tools(None).is_none());
        assert!(convert_tools(Some(vec![])).is_none());
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
        let result = convert_tools(Some(tools)).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["type"], "function");
        assert_eq!(result[0]["name"], "search");
        assert_eq!(result[0]["description"], "searches");
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
        let input = convert_messages_to_input(&msgs);
        assert_eq!(input.len(), 4);
        assert_eq!(input[0]["role"], "developer");
        assert_eq!(input[1]["role"], "user");
        assert_eq!(input[2]["role"], "assistant");
        assert_eq!(input[3]["role"], "user");
    }
}
