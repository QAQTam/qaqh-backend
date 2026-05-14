//! HTTP client: request building, retry loop, SSE streaming to StreamEvent channel.

use crate::config::Config;
use crate::types::*;
use crate::api::{AccumulatedToolCall, StreamEvent};
use futures_util::StreamExt;
use reqwest::Client as HttpClient;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;

use super::converter::anthropic_url;

/// Streaming chat via Anthropic Messages endpoint.
///
/// Takes pre-validated Anthropic-format messages from the ContextAssembler.
/// The conversion and validation are done upstream — this function only handles
/// the HTTP request and SSE parsing.
pub async fn chat_stream(
    config: &Config,
    model: &str,
    system: Option<String>,
    anthropic_msgs: Vec<AnthropicMessage>,
    _thinking: bool,
    effort: Option<&str>,
    tools: Option<Vec<ToolDef>>,
    tool_choice: Option<&str>,
    max_tokens: u32,
    _temperature: Option<f64>,
    _top_p: Option<f64>,
    _user_id: Option<&str>,
    tx: mpsc::Sender<StreamEvent>,
) -> anyhow::Result<()> {
    // Messages are already converted by ContextAssembler — no convert_messages() call needed

    // Convert tools to Anthropic format
    let anthropic_tools: Option<Vec<AnthropicTool>> = tools.map(|tds| {
        tds.into_iter()
            .map(|td| AnthropicTool {
                name: td.function.name,
                description: td.function.description,
                input_schema: td.function.parameters,
            })
            .collect()
    });

    // Build tool_choice
    let tc = tool_choice.map(|s| {
        if s == "auto" {
            serde_json::json!({"type": "auto"})
        } else if s == "none" {
            serde_json::json!({"type": "none"})
        } else {
            serde_json::json!({"type": "tool", "name": s})
        }
    });

    // Set effort via output_config (DeepSeek's Anthropic extension)
    let _effort = effort.map(|s| s.to_string());
    let is_deepseek = config.base_url.contains("deepseek.com");

    // Build request with full Anthropic field set
    let request = AnthropicRequest {
        model: model.to_string(),
        max_tokens,
        system,
        messages: anthropic_msgs,
        tools: anthropic_tools,
        tool_choice: tc,
        stream: true,
        thinking: if is_deepseek {
            Some(AnthropicThinking {
                thinking_type: "enabled".into(),
                budget_tokens: Some(max_tokens * 3 / 4),
            })
        } else {
            Some(AnthropicThinking {
                thinking_type: "enabled".into(),
                budget_tokens: None,
            })
        },
        temperature: None,
        top_p: None,
        top_k: None,
        stop_sequences: None,
        metadata: None,
    };

    let url = anthropic_url(&config.base_url);

    // Build JSON — inject output_config only for DeepSeek
    let mut body_json = serde_json::to_value(&request)?;
    if let Some(ref e) = _effort {
        if is_deepseek {
            body_json["output_config"] = serde_json::json!({"effort": e});
        }
    }

    log::info!(
        "api request: {} model={} max_tokens={} tools={}",
        &url,
        body_json["model"],
        body_json["max_tokens"],
        body_json["tools"].as_array().map(|a| a.len()).unwrap_or(0)
    );

    // ── Retry loop for 500/503 (server errors) and 429 (rate limit) ──
    let max_retries = 10;
    let mut response = None;

    for attempt in 0..=max_retries {
        let (_timeout, wait) = match attempt {
            0..=3 => (3u64, 0u64), // steps 0-3: no wait before first
            4..=8 => (10, 10),     // steps 4-8: 10s wait before each
            _ => (60, 30),         // steps 9-10: 30s wait before each
        };

        if attempt > 0 {
            let wait_msg = format!(
                "[retry {}/{}] waiting {}s before retry...",
                attempt, max_retries, wait
            );
            log::warn!("{}", wait_msg);
            let _ = tx
                .send(StreamEvent::BalanceResult(format!(" ⏳ {}", wait_msg)))
                .await;
            tokio::time::sleep(Duration::from_secs(wait)).await;
        }

        let http = match HttpClient::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("HTTP client error: {}", e);
                let _ = tx.send(StreamEvent::Error(msg.clone())).await;
                return Err(anyhow::anyhow!("{}", msg));
            }
        };

        let resp = http
            .post(&url)
            .header("x-api-key", &config.api_key)
            .header("Content-Type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .json(&body_json)
            .send()
            .await;

        match resp {
            Ok(r) => {
                let status = r.status();
                if status.is_success() {
                    response = Some(r);
                    break;
                }

                let status_code = status.as_u16();
                // Read Retry-After BEFORE consuming response body
                let retry_after = if status_code == 429 {
                    r.headers()
                        .get("Retry-After")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(10)
                } else {
                    0
                };
                let text = r.text().await.unwrap_or_default();
                log::error!(
                    "api error: {} {} (attempt {}/{})",
                    status,
                    text,
                    attempt,
                    max_retries
                );

                // 400/401/402/403 — client errors, no retry
                if status_code == 400
                    || status_code == 401
                    || status_code == 402
                    || status_code == 403
                {
                    let msg = format!("API {}: {}", status, text);
                    let _ = tx.send(StreamEvent::Error(msg.clone())).await;
                    return Err(anyhow::anyhow!("{}", msg));
                }

                // 500/503 — retry
                if status_code == 500 || status_code == 503 {
                    if attempt < max_retries {
                        let _ = tx
                            .send(StreamEvent::BalanceResult(format!(
                                " ⏳ server error {}: retrying ({}/{})...",
                                status,
                                attempt + 1,
                                max_retries
                            )))
                            .await;
                        continue;
                    }
                    let msg = format!(
                        "Server error {} after {} retries. DeepSeek servers may be overloaded. Please try again later.",
                        status, max_retries
                    );
                    let _ = tx.send(StreamEvent::Error(msg.clone())).await;
                    return Err(anyhow::anyhow!("{}", msg));
                }

                // 429 — rate limit: use Retry-After header
                if status_code == 429 {
                    if attempt < max_retries {
                        let wait_s = retry_after.min(30);
                        let msg = format!(
                            "Rate limited (429). Waiting {}s before retry ({}/{})...",
                            wait_s,
                            attempt + 1,
                            max_retries
                        );
                        log::warn!("{}", msg);
                        let _ = tx
                            .send(StreamEvent::BalanceResult(format!(" ⏳ {}", msg)))
                            .await;
                        tokio::time::sleep(Duration::from_secs(wait_s)).await;
                        continue;
                    }
                    let msg =
                        "Rate limited (429) after max retries. Reduce request frequency.".to_string();
                    let _ = tx.send(StreamEvent::Error(msg.clone())).await;
                    return Err(anyhow::anyhow!("{}", msg));
                }

                // 402 — insufficient balance
                if status_code == 402 {
                    let _ = tx
                        .send(StreamEvent::Error(
                            "API 402: Insufficient balance. Please top up at https://platform.deepseek.com"
                                .to_string(),
                        ))
                        .await;
                    return Err(anyhow::anyhow!("API 402: Insufficient balance"));
                }

                // 400, 401, 422 — no retry, fatal
                let _ = tx
                    .send(StreamEvent::Error(format!("API {}: {}", status, text)))
                    .await;
                return Err(anyhow::anyhow!("API error {}: {}", status, text));
            }
            Err(e) => {
                // Connection timeout or network error — retry
                if attempt < max_retries {
                    log::warn!(
                        "connection error (attempt {}/{}): {} — retrying",
                        attempt + 1,
                        max_retries,
                        e
                    );
                    let _ = tx
                        .send(StreamEvent::BalanceResult(format!(
                            " ⏳ connection error: retrying ({}/{})...",
                            attempt + 1,
                            max_retries
                        )))
                        .await;
                    continue;
                }
                let msg = format!(
                    "Cannot connect to DeepSeek API after {} retries: {}\nCheck your network or API server status.",
                    max_retries, e
                );
                let _ = tx.send(StreamEvent::Error(msg.clone())).await;
                return Err(anyhow::anyhow!("{}", msg));
            }
        }
    }

    let response = response.unwrap();

    // Accumulated response state
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: HashMap<usize, AccumulatedToolCall> = HashMap::new();
    let mut _current_block_index: usize = 0;
    let mut current_tool_name: Option<String> = None;
    let mut current_tool_id: Option<String> = None;
    let mut usage_info: Option<UsageInfo> = None;
    let mut thinking_signature: Option<String> = None;
    let mut stop_reason: Option<String> = None;

    // Hybrid: stream reasoning in real-time, buffer content for final markdown render
    let mut byte_stream = response.bytes_stream();
    let mut sse_buffer = String::new();
    while let Some(chunk) = byte_stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                log::error!("SSE transport error: {}", e);
                let has_output = !content.is_empty() || !tool_calls.is_empty();
                if !has_output {
                    let _ = tx
                        .send(StreamEvent::Error(format!(
                            "Stream connection lost before receiving output: {}",
                            e
                        )))
                        .await;
                    return Err(anyhow::anyhow!("SSE stream transport error: {}", e));
                }
                log::warn!("SSE transport error after receiving output, finalizing partial response");
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
            if data_str.is_empty() {
                continue;
            }

            let event: AnthropicStreamEvent = match serde_json::from_str(&data_str) {
                Ok(e) => e,
                Err(_) => continue,
            };

            match event {
                AnthropicStreamEvent::ContentBlockDelta { index, delta } => {
                    _current_block_index = index;
                    match delta {
                        AnthropicDelta::TextDelta { text } => {
                            content.push_str(&text);
                            let _ = tx.send(StreamEvent::ContentDelta(text)).await;
                        }
                        AnthropicDelta::ThinkingDelta { thinking } => {
                            reasoning.push_str(&thinking);
                            // Emit reasoning in real-time (流式)
                            let _ = tx.send(StreamEvent::ReasoningDelta(thinking)).await;
                        }
                        AnthropicDelta::SignatureDelta { signature } => {
                            thinking_signature = Some(signature);
                        }
                        AnthropicDelta::InputJsonDelta { partial_json } => {
                            let entry = tool_calls.entry(index).or_insert_with(|| {
                                AccumulatedToolCall {
                                    id: current_tool_id.clone().unwrap_or_default(),
                                    name: current_tool_name.clone().unwrap_or_default(),
                                    arguments: String::new(),
                                }
                            });
                            entry.arguments.push_str(&partial_json);
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
                }
                AnthropicStreamEvent::ContentBlockStart {
                    index,
                    content_block,
                } => {
                    _current_block_index = index;
                    if let AnthropicContentBlockStart::ToolUse { id, name, input } = content_block {
                        current_tool_name = Some(name.clone());
                        current_tool_id = Some(id.clone());
                        let entry = tool_calls.entry(index).or_insert_with(|| {
                            AccumulatedToolCall {
                                id: id.clone(),
                                name: name.clone(),
                                arguments: String::new(),
                            }
                        });
                        entry.id = id;
                        entry.name = name.clone();
                        if !input.is_null() && input != serde_json::json!({}) {
                            entry.arguments = input.to_string();
                        }
                        let _ = tx
                            .send(StreamEvent::ToolCallProgress {
                                name,
                                args_so_far: entry.arguments.clone(),
                            })
                            .await;
                    }
                }
                AnthropicStreamEvent::ContentBlockStop { .. } => {
                    current_tool_name = None;
                    current_tool_id = None;
                }
                AnthropicStreamEvent::MessageDelta { delta, usage } => {
                    stop_reason = delta.stop_reason;
                    if let Some(ref sr) = stop_reason {
                        log::info!("stop_reason: {}", sr);
                        if sr == "max_tokens" {
                            log::warn!("Model hit max_tokens limit — response may be truncated");
                        }
                    }
                    if let Some(u) = usage {
                        usage_info = Some(u.to_usage_info());
                    }
                }
                AnthropicStreamEvent::MessageStop => {
                    // Content was already streamed per-chunk above
                }
                _ => {}
            }
        }
    }

    log::info!(
        "stream ended: content_len={} reasoning_len={} tool_count={} stop_reason={:?}",
        content.len(),
        reasoning.len(),
        tool_calls.len(),
        stop_reason
    );

    // Assemble the final raw_message
    let assembled_tool_calls: Vec<ToolCall> = if tool_calls.is_empty() {
        Vec::new()
    } else {
        let mut sorted: Vec<_> = tool_calls.into_values().collect();
        // Sort by numeric part of id for stable ordering
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
                function: FunctionCall {
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
        thinking_signature,
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

/// List available models via the OpenAI-compatible `/models` endpoint.
#[allow(dead_code)]
pub async fn list_models(config: &Config) -> anyhow::Result<Vec<crate::types::ModelInfo>> {
    let url = format!("{}/models", config.base_url);
    let http = HttpClient::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let resp = http
        .get(&url)
        .header("Authorization", format!("Bearer {}", config.api_key))
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "Failed to list models: {}",
            resp.status()
        ));
    }
    let list: crate::types::ModelList = resp.json().await?;
    Ok(list.data)
}

/// Query account balance via the OpenAI-compatible `/user/balance` endpoint.
pub async fn get_balance(config: &Config) -> anyhow::Result<crate::types::BalanceInfo> {
    let url = format!("{}/user/balance", config.base_url);
    let http = HttpClient::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let resp = http
        .get(&url)
        .header("Authorization", format!("Bearer {}", config.api_key))
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "Failed to get balance: {}",
            resp.status()
        ));
    }
    Ok(resp.json().await?)
}
