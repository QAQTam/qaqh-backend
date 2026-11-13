//! Native Anthropic Messages API streaming client.
//!
//! Takes internal `Vec<Message>` (OpenAI-format), converts to `AnthropicRequest`,
//! sends to Anthropic `/v1/messages`, parses SSE, emits `StreamEvent`.
//!
//! No intermediate translation layers — conversion from internal format to
//! Anthropic's content-block format happens in exactly one place.

use crate::{GatewayConfig, StreamEvent};
use dsx_types::{
    AnthropicCacheControl, AnthropicSystemBlock,
    AnthropicContent, AnthropicMessage, AnthropicTool, AnthropicThinking, AnthropicRequest,
    AnthropicStreamEvent, AnthropicDelta,
    Message, ToolDef, UsageInfo,
};
use futures_util::StreamExt;
use reqwest::Client as HttpClient;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;

// ── Public entry point ──

/// Stream a chat completion via the native Anthropic Messages API.
///
/// # Arguments
/// - `system`: the base prompt (will be the first system block with `cache_control`).
/// - `messages`: OpenAI-format messages. System-role messages are extracted and
///   appended to the system block array. The rest are converted to Anthropic's
///   content-block format.
/// - `thinking_budget`: if `Some(n)`, enables Anthropic extended thinking with
///   a budget of `n` tokens. Derived from the agent's `effort` setting.
pub async fn chat_stream_anthropic(
    config: &GatewayConfig,
    model: &str,
    system: Option<String>,
    messages: Vec<Message>,
    tools: Option<Vec<ToolDef>>,
    max_tokens: u32,
    thinking_budget: Option<u32>,
    tx: mpsc::Sender<StreamEvent>,
) -> anyhow::Result<()> {
    // ── 1. Build system blocks (with cache_control on the LAST block) ──
    let mut system_blocks: Vec<AnthropicSystemBlock> = Vec::new();

    if let Some(ref base) = system {
        if !base.is_empty() {
            system_blocks.push(AnthropicSystemBlock {
                block_type: "text".into(),
                text: base.clone(),
                cache_control: None,
            });
        }
    }

    // Extract system-role messages → additional system blocks
    let mut conv_msgs: Vec<Message> = Vec::new();
    for msg in messages {
        if msg.role == "system" {
            if let Some(ref content) = msg.content {
                if !content.is_empty() {
                    system_blocks.push(AnthropicSystemBlock {
                        block_type: "text".into(),
                        text: content.clone(),
                        cache_control: None,
                    });
                }
            }
        } else {
            conv_msgs.push(msg);
        }
    }

    // Mark the LAST system block for caching (covers all system blocks +
    // everything sent before the penultimate message breakpoint).
    if let Some(last) = system_blocks.last_mut() {
        last.cache_control = Some(AnthropicCacheControl { cache_type: "ephemeral".into() });
    }

    // ── 2. Convert conversation messages to Anthropic content-block format ──
    let mut anthropic_msgs = openai_to_anthropic_msgs(conv_msgs);

    // ── 3. Add cache_control to the penultimate message for max prefix caching ──
    //     This tells Anthropic to cache everything before this message.
    //     Only the last message (current user input) is uncached.
    if anthropic_msgs.len() >= 2 {
        let idx = anthropic_msgs.len() - 2;
        if let Some(last_block) = anthropic_msgs[idx].content.last_mut() {
            match last_block {
                AnthropicContent::Text { ref mut cache_control, .. }
                | AnthropicContent::ToolUse { ref mut cache_control, .. } => {
                    *cache_control = Some(AnthropicCacheControl { cache_type: "ephemeral".into() });
                }
                _ => {}
            }
        }
    }

    // ── 4. Convert tool definitions (with cache_control on the last tool) ──
    let anthropic_tools: Option<Vec<AnthropicTool>> = tools.map(|tds| {
        let count = tds.len();
        tds.into_iter()
            .enumerate()
            .map(|(i, td)| AnthropicTool {
                name: td.function.name,
                description: td.function.description,
                input_schema: td.function.parameters,
                cache_control: if i == count - 1 {
                    Some(AnthropicCacheControl { cache_type: "ephemeral".into() })
                } else {
                    None
                },
            })
            .collect()
    });

    // ── 5. Build native Anthropic request ──
    let request = AnthropicRequest {
        system: if system_blocks.is_empty() { None } else { Some(system_blocks) },
        messages: anthropic_msgs,
        model: model.to_string(),
        max_tokens,
        stream: true,
        tools: anthropic_tools,
        tool_choice: None,
        thinking: thinking_budget.map(|budget| AnthropicThinking {
            thinking_type: "enabled".into(),
            budget_tokens: Some(budget),
        }),
        temperature: None,
        top_p: None,
        top_k: None,
        stop_sequences: None,
        metadata: None,
    };

    // ── 6. HTTP POST ──
    let url = build_anthropic_url(&config.base_url);
    let client = HttpClient::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .pool_max_idle_per_host(0)
        .build()?;

    let resp = client
        .post(&url)
        .header("x-api-key", &config.api_key)
        .header("anthropic-version", "2023-06-01")
        .header("Content-Type", "application/json")
        .json(&request)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let msg = format!("Anthropic API {}: {}", status, text);
        let _ = tx.send(StreamEvent::Error(msg.clone())).await;
        return Err(anyhow::anyhow!("{}", msg));
    }

    // ── 7. Parse SSE stream ──
    let mut byte_stream = resp.bytes_stream();
    let mut sse_buf = String::new();
    let mut text_buf = String::new();
    let mut think_buf = String::new();
    let mut think_sig: Option<String> = None;
    let mut tool_acc: HashMap<usize, (String, String, String)> = HashMap::new();
    let mut usage_info: Option<UsageInfo> = None;
    let mut stop_reason: Option<String> = None;

    while let Some(chunk) = byte_stream.next().await {
        let chunk = chunk?;
        sse_buf.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(pos) = sse_buf.find("\n\n") {
            let raw = sse_buf[..pos].to_string();
            sse_buf = sse_buf[pos + 2..].to_string();

            let mut data_str = String::new();
            for line in raw.lines() {
                let t = line.trim();
                if let Some(dt) = t.strip_prefix("data: ") {
                    data_str = dt.to_string();
                }
            }

            if data_str.is_empty() {
                continue;
            }

            let event: AnthropicStreamEvent = match serde_json::from_str(&data_str) {
                Ok(e) => e,
                Err(e) => {
                    log::warn!("Anthropic SSE: failed to deserialize event: {} — data: {}", e, data_str);
                    continue;
                }
            };

            use dsx_types::AnthropicStreamEvent::*;
            match event {
                ContentBlockStart { index, content_block } => {
                    use dsx_types::AnthropicContentBlockStart::*;
                    match content_block {
                        ToolUse { id, name, .. } => {
                            tool_acc.entry(index).or_insert_with(||
                                (id, name, String::new())
                            );
                        }
                        _ => {}
                    }
                }
                ContentBlockDelta { index, delta } => {
                    match delta {
                        AnthropicDelta::TextDelta { text } => {
                            text_buf.push_str(&text);
                            let _ = tx.send(StreamEvent::ContentDelta(text.clone())).await;
                        }
                        AnthropicDelta::ThinkingDelta { thinking } => {
                            think_buf.push_str(&thinking);
                            let _ = tx.send(StreamEvent::ReasoningDelta(thinking)).await;
                        }
                        AnthropicDelta::SignatureDelta { signature } => {
                            think_sig = Some(signature);
                        }
                        AnthropicDelta::InputJsonDelta { partial_json } => {
                            if let Some(entry) = tool_acc.get_mut(&index) {
                                entry.2.push_str(&partial_json);
                                let _ = tx.send(StreamEvent::ToolCallProgress {
                                    name: entry.1.clone(),
                                    args_so_far: entry.2.clone(),
                                }).await;
                            }
                        }
                    }
                }
                MessageDelta { delta, usage } => {
                    stop_reason = delta.stop_reason;
                    if let Some(u) = usage {
                        usage_info = Some(UsageInfo {
                            prompt_tokens: u.input_tokens,
                            completion_tokens: u.output_tokens,
                            total_tokens: u.input_tokens + u.output_tokens,
                            prompt_cache_hit_tokens: u.cache_read_input_tokens,
                            prompt_cache_miss_tokens: u.cache_creation_input_tokens,
                            completion_tokens_details: None,
                        });
                    }
                }
                _ => {} // message_start, content_block_stop, message_stop, ping
            }
        }
    }

    // ── 8. Build final Message ──
    let raw_message = Message {
        role: "assistant".into(),
        content: if text_buf.is_empty() { None } else { Some(text_buf) },
        name: None,
        tool_calls: if tool_acc.is_empty() {
            None
        } else {
            let mut sorted: Vec<_> = tool_acc.into_values().collect();
            sorted.sort_by_key(|(id, _, _)| id.clone());
            Some(sorted.into_iter().map(|(id, name, args)| dsx_types::ToolCall {
                id,
                call_type: "function".into(),
                function: dsx_types::FunctionCall {
                    name,
                    arguments: args,
                },
            }).collect())
        },
        tool_call_id: None,
        reasoning_content: if think_buf.is_empty() { None } else { Some(think_buf) },
        thinking_signature: think_sig,
    };

    let _ = tx.send(StreamEvent::Done { raw_message, usage: usage_info, stop_reason }).await;
    Ok(())
}

// ── Internal helpers ──

/// Convert OpenAI-format `Vec<Message>` to Anthropic `Vec<AnthropicMessage>`.
///
/// Rules:
/// - System messages → handled by caller, skipped here.
/// - User messages → `AnthropicContent::Text`
/// - Assistant messages → text + optional thinking + optional tool_use blocks
/// - Tool results → grouped by adjacency into a single user message with
///   `AnthropicContent::ToolResult` blocks (Anthropic requires tool results to
///    be in a user-role message).
fn openai_to_anthropic_msgs(msgs: Vec<Message>) -> Vec<AnthropicMessage> {
    let mut out: Vec<AnthropicMessage> = Vec::new();
    // Accumulator for consecutive tool results → one Anthropic user message
    let mut tool_results: Vec<AnthropicContent> = Vec::new();

    for msg in msgs {
        match msg.role.as_str() {
            "user" => {
                flush_tool_results(&mut out, &mut tool_results);
                let text = msg.content.unwrap_or_default();
                // Prevent user→user consecutive messages (Anthropic requires alternation).
                // This can occur when tool results are resolved but no assistant response
                // follows before the next user message (e.g. tool failures, API errors).
                if let Some(last) = out.last_mut() {
                    if last.role == "user" {
                        last.content.push(AnthropicContent::Text { text, cache_control: None });
                        continue;
                    }
                }
                out.push(AnthropicMessage {
                    role: "user".into(),
                    content: vec![AnthropicContent::Text { text, cache_control: None }],
                });
            }
            "assistant" => {
                flush_tool_results(&mut out, &mut tool_results);
                let mut blocks: Vec<AnthropicContent> = Vec::new();

                // Thinking block (from reasoning_content)
                if let Some(ref reasoning) = msg.reasoning_content {
                    let sig = msg.thinking_signature.clone().unwrap_or_default();
                    blocks.push(AnthropicContent::Thinking {
                        thinking: reasoning.clone(),
                        signature: sig,
                    });
                }

                // Text block
                if let Some(ref text) = msg.content {
                    if !text.is_empty() {
                        blocks.push(AnthropicContent::Text {
                            text: text.clone(),
                            cache_control: None,
                        });
                    }
                }

                // Tool_use blocks
                if let Some(ref calls) = msg.tool_calls {
                    for tc in calls {
                        let input: serde_json::Value =
                            serde_json::from_str(&tc.function.arguments)
                                .unwrap_or(serde_json::Value::Null);
                        blocks.push(AnthropicContent::ToolUse {
                            id: tc.id.clone(),
                            name: tc.function.name.clone(),
                            input,
                            cache_control: None,
                        });
                    }
                }

                out.push(AnthropicMessage {
                    role: "assistant".into(),
                    content: blocks,
                });
            }
            "tool" => {
                let result_text = msg.content.unwrap_or_default();
                let is_error = result_text.starts_with("[ERROR]") || result_text.starts_with("[FAIL]");
                tool_results.push(AnthropicContent::ToolResult {
                    tool_use_id: msg.tool_call_id.unwrap_or_default(),
                    content: result_text,
                    is_error: if is_error { Some(true) } else { None },
                });
            }
            _ => {}
        }
    }

    flush_tool_results(&mut out, &mut tool_results);
    out
}

/// Flush accumulated tool results as a user-role Anthropic message.
fn flush_tool_results(out: &mut Vec<AnthropicMessage>, pending: &mut Vec<AnthropicContent>) {
    if pending.is_empty() {
        return;
    }
    out.push(AnthropicMessage {
        role: "user".into(),
        content: std::mem::take(pending),
    });
}

/// Build the Anthropic `/v1/messages` URL from the configured base URL.
fn build_anthropic_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1/messages") {
        base.to_string()
    } else if base.ends_with("/v1") {
        format!("{}/messages", base)
    } else {
        format!("{}/v1/messages", base)
    }
}
