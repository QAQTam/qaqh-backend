//! OpenAI-compatible chat completion streaming client.

use crate::{GatewayConfig, StreamEvent};
use dsx_types::{Message, ToolCall, ToolDef, UsageInfo};
use futures_util::StreamExt;
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;

// ── OpenAI request types ──

#[derive(Serialize)]
struct OpenAIToolDef {
    #[serde(rename = "type")]
    tool_type: String,
    function: OpenAIToolFunctionDef,
}

#[derive(Serialize)]
struct OpenAIToolFunctionDef {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Serialize)]
struct OpenAIChatRequest {
    model: String,
    messages: Vec<Message>,
    max_tokens: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAIToolDef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
}

// ── OpenAI SSE response types ──

#[derive(Deserialize)]
struct OpenAIChatChunk {
    choices: Vec<OpenAIChoice>,
    #[serde(default)]
    usage: Option<OpenAIUsageInfo>,
}

#[derive(Deserialize)]
struct OpenAIChoice {
    delta: OpenAIDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
#[allow(dead_code)]
struct OpenAIDelta {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAIToolCallDelta>>,
}

#[derive(Deserialize)]
struct OpenAIToolCallDelta {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<OpenAIFunctionDelta>,
}

#[derive(Deserialize, Default)]
struct OpenAIFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct OpenAIUsageInfo {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
    #[serde(default)]
    prompt_cache_hit_tokens: Option<u32>,
    #[serde(default)]
    prompt_cache_miss_tokens: Option<u32>,
}

/// Internal accumulator for building tool calls from streaming deltas.
struct AccumulatedToolCall {
    id: String,
    name: String,
    arguments: String,
}

// ── Public function ──

/// Streaming chat via OpenAI-compatible `/chat/completions` endpoint.
///
/// Messages are already in OpenAI format — no conversion needed.
pub async fn chat_stream_openai(
    config: &GatewayConfig,
    model: &str,
    system: Option<String>,
    messages: Vec<Message>,
    tools: Option<Vec<ToolDef>>,
    tool_choice: Option<&str>,
    max_tokens: u32,
    reasoning_effort: Option<&str>,
    tx: mpsc::Sender<StreamEvent>,
) -> anyhow::Result<()> {
    let mut openai_msgs: Vec<Message> = Vec::new();

    if let Some(sys) = system {
        openai_msgs.push(Message::system(&sys));
    }

    openai_msgs.extend(messages);

    // Convert ToolDef to OpenAI function format
    let openai_tools: Option<Vec<OpenAIToolDef>> = tools.map(|tds| {
        tds.into_iter()
            .map(|td| OpenAIToolDef {
                tool_type: "function".into(),
                function: OpenAIToolFunctionDef {
                    name: td.function.name,
                    description: td.function.description,
                    parameters: td.function.parameters,
                },
            })
            .collect()
    });

    let tc = tool_choice.map(|s| match s {
        "auto" => serde_json::json!("auto"),
        "none" => serde_json::json!("none"),
        "required" => serde_json::json!("required"),
        name => serde_json::json!({"type": "function", "function": {"name": name}}),
    });

    // Determine URL for OpenAI-compatible endpoint.
    let url = {
        let raw = config.base_url.strip_suffix('/').unwrap_or(&config.base_url);
        if raw.ends_with("/chat/completions") {
            raw.to_string()
        } else if raw.ends_with("/v1") {
            format!("{}/chat/completions", raw)
        } else {
            match raw.find("://") {
                Some(scheme_end) => {
                    let after_scheme = &raw[scheme_end + 3..];
                    match after_scheme.find('/') {
                        Some(slash_pos) => format!("{}/chat/completions", &raw[..scheme_end + 3 + slash_pos]),
                        None => format!("{raw}/v1/chat/completions"),
                    }
                }
                None => format!("{raw}/chat/completions"),
            }
        }
    };

    let request = OpenAIChatRequest {
        model: model.to_string(),
        messages: openai_msgs,
        max_tokens,
        stream: true,
        tools: openai_tools,
        tool_choice: tc,
        reasoning_effort: reasoning_effort.map(|s| s.to_string()),
    };

    let body_json = serde_json::to_value(&request)?;

    log::info!(
        "openai api request: {} model={} max_tokens={}",
        &url,
        body_json["model"],
        body_json["max_tokens"]
    );

    let http = HttpClient::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .pool_max_idle_per_host(0)
        .build()?;

    let resp = http
        .post(&url)
        .header("Authorization", format!("Bearer {}", config.api_key))
        .header("Content-Type", "application/json")
        .json(&body_json)
        .send()
        .await;

    let resp = match resp {
        Ok(r) => {
            if !r.status().is_success() {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                // Log full request body on 400+ for diagnostics
                log::error!(
                    "openai api error {} — request: {}",
                    status,
                    serde_json::to_string_pretty(&body_json).unwrap_or_default()
                );
                let msg = format!("OpenAI API {}: {}", status, text);
                let _ = tx.send(StreamEvent::Error(msg.clone())).await;
                return Err(anyhow::anyhow!("{}", msg));
            }
            r
        }
        Err(e) => {
            let msg = format!("OpenAI connection error: {}", e);
            let _ = tx.send(StreamEvent::Error(msg.clone())).await;
            return Err(anyhow::anyhow!("{}", msg));
        }
    };

    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: HashMap<usize, AccumulatedToolCall> = HashMap::new();
    let mut usage_info: Option<UsageInfo> = None;
    let mut stop_reason: Option<String> = None;

    let mut byte_stream = resp.bytes_stream();
    let mut sse_buffer = String::new();
    while let Some(chunk) = byte_stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                log::error!("OpenAI SSE transport error: {}", e);
                let has_output = !content.is_empty() || !tool_calls.is_empty();
                if !has_output {
                    let _ = tx
                        .send(StreamEvent::Error(format!(
                            "Stream connection lost before receiving output: {}",
                            e
                        )))
                        .await;
                    return Err(anyhow::anyhow!("OpenAI SSE stream transport error: {}", e));
                }
                log::warn!("OpenAI SSE transport error after receiving output, finalizing partial response");
                break;
            }
        };
        sse_buffer.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(pos) = sse_buffer.find("\n\n") {
            let event_text = sse_buffer[..pos].to_string();
            sse_buffer = sse_buffer[pos + 2..].to_string();

            let mut data_str = String::new();
            for line in event_text.lines() {
                let line = line.trim();
                if let Some(dt) = line.strip_prefix("data: ") {
                    data_str = dt.to_string();
                }
            }
            if data_str.is_empty() || data_str == "[DONE]" {
                continue;
            }

            let chunk: OpenAIChatChunk = match serde_json::from_str(&data_str) {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("OpenAI SSE: failed to deserialize chunk: {} — data: {}", e, data_str);
                    continue;
                }
            };

            for choice in chunk.choices {
                // Emit content delta
                if let Some(ref text) = choice.delta.content {
                    if !text.is_empty() {
                        content.push_str(text);
                        let _ = tx.send(StreamEvent::ContentDelta(text.clone())).await;
                    }
                }

                // Emit reasoning delta
                if let Some(ref reasoning_text) = choice.delta.reasoning_content {
                    if !reasoning_text.is_empty() {
                        reasoning.push_str(reasoning_text);
                        let _ = tx
                            .send(StreamEvent::ReasoningDelta(reasoning_text.clone()))
                            .await;
                    }
                }

                // Accumulate tool calls
                if let Some(ref tcs) = choice.delta.tool_calls {
                    for tc in tcs {
                        let entry = tool_calls.entry(tc.index).or_insert_with(|| {
                            AccumulatedToolCall {
                                id: String::new(),
                                name: String::new(),
                                arguments: String::new(),
                            }
                        });
                        if let Some(ref id) = tc.id {
                            entry.id = id.clone();
                        }
                        if let Some(ref func) = tc.function {
                            if let Some(ref name) = func.name {
                                entry.name = name.clone();
                            }
                            if let Some(ref args) = func.arguments {
                                entry.arguments.push_str(args);
                            }
                        }
                        if !entry.name.is_empty() {
                            let _ = tx
                                .send(StreamEvent::ToolCallProgress {
                                    name: entry.name.clone(),
                                    args_so_far: entry.arguments.clone(),
                                })
                                .await;
                        }
                    }
                }

                if let Some(ref reason) = choice.finish_reason {
                    stop_reason = Some(reason.clone());
                }
            }

            if let Some(ref usage) = chunk.usage {
                usage_info = Some(UsageInfo {
                    prompt_tokens: usage.prompt_tokens,
                    completion_tokens: usage.completion_tokens,
                    total_tokens: usage.total_tokens,
                    prompt_cache_hit_tokens: usage.prompt_cache_hit_tokens.unwrap_or(0),
                    prompt_cache_miss_tokens: usage.prompt_cache_miss_tokens.unwrap_or(0),
                    completion_tokens_details: None,
                });
            }
        }
    }

    log::info!(
        "openai stream ended: content_len={} reasoning_len={} tool_count={} stop_reason={:?}",
        content.len(),
        reasoning.len(),
        tool_calls.len(),
        stop_reason
    );

    let assembled_tool_calls: Vec<ToolCall> = if tool_calls.is_empty() {
        Vec::new()
    } else {
        let mut sorted: Vec<_> = tool_calls.into_values().collect();
        sorted.sort_by_key(|t| {
            t.id
                .chars()
                .filter(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse::<u32>()
                .unwrap_or(0)
        });
        sorted
            .into_iter()
            .map(|atc| ToolCall {
                id: atc.id,
                call_type: "function".into(),
                function: dsx_types::FunctionCall {
                    name: atc.name,
                    arguments: atc.arguments,
                },
            })
            .collect()
    };

    let raw_message = Message {
        role: "assistant".into(),
        content: if content.is_empty() {
            None
        } else {
            Some(content)
        },
        name: None,
        tool_calls: if assembled_tool_calls.is_empty() {
            None
        } else {
            Some(assembled_tool_calls)
        },
        tool_call_id: None,
        reasoning_content: if reasoning.is_empty() {
            None
        } else {
            Some(reasoning)
        },
        thinking_signature: None,
    };

    let _ = tx
        .send(StreamEvent::Done {
            raw_message,
            usage: usage_info,
            stop_reason,
        })
        .await;

    Ok(())
}
