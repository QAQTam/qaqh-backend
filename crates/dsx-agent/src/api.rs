use dsx_types::{Message, ToolDef, UsageInfo};

/// Stream events from the API — matches the legacy enum shape.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    ContentDelta(String),
    ReasoningDelta(String),
    ToolCallProgress { name: String, args_so_far: String },
    Done { raw_message: dsx_types::Message, usage: Option<UsageInfo>, stop_reason: Option<String> },
    Error(String),
    BalanceResult(String),
    ExecProgress(String),
    ModelListResult(Vec<String>),
    SudoDone(String),
    ExecDone(String, String),
    ExecStarted(String, u32),
}

/// Find HP port from env var DSX_HP_PORT or platform port file.
fn hp_port() -> u16 {
    if let Ok(port_str) = std::env::var("DSX_HP_PORT") {
        if let Ok(port) = port_str.trim().parse::<u16>() {
            return port;
        }
    }
    let path = dsx_types::platform::hp_port_path();
    std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

/// Stream a chat completion by sending `api_chat` frame to HP over TCP.
pub async fn chat_stream(
    cfg: &crate::config::Config,
    model: &str,
    system: Option<String>,
    messages: Vec<Message>,
    _extended: bool,
    effort: Option<&str>,
    tools: Option<Vec<ToolDef>>,
    _tool_choice: Option<&str>,
    max_tokens: u32,
    _stop: Option<&[String]>,
    _uid: Option<&str>,
    tx: tokio::sync::mpsc::Sender<StreamEvent>,
) -> anyhow::Result<()> {
    let port = hp_port();
    if port == 0 {
        let _ = tx.send(StreamEvent::Error("HP not running".into())).await;
        return Err(anyhow::anyhow!("HP not running"));
    }

    // Connect to HP
    let stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}")).await
        .map_err(|e| {
            let msg = format!("HP connection failed: {e}");
            let _ = tx.blocking_send(StreamEvent::Error(msg.clone()));
            anyhow::anyhow!("{}", msg)
        })?;

    let (reader_half, mut writer_half) = stream.into_split();

    // Build api_chat frame
    let messages_json = serde_json::to_value(&messages).unwrap_or_default();
    let tools_json = tools.map(|tds| {
        serde_json::to_value(&tds.into_iter().map(|td| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": td.function.name,
                    "description": td.function.description,
                    "parameters": td.function.parameters,
                }
            })
        }).collect::<Vec<_>>()).unwrap_or_default()
    });

    let chat = dsx_proto::AgentToHp::ApiChat {
        model: if model.is_empty() { cfg.model.clone() } else { model.to_string() },
        system: system.filter(|s| !s.is_empty()),
        messages: messages_json,
        effort: effort.map(|s| s.to_string()).or_else(|| cfg.effort.clone()),
        max_tokens: Some(if max_tokens > 0 { max_tokens } else { cfg.max_tokens }),
        tools: tools_json,
    };

    // Send frame
    let frame = serde_json::to_string(&chat).map_err(|e| {
        let msg = format!("serialize api_chat: {e}");
        let _ = tx.blocking_send(StreamEvent::Error(msg.clone()));
        anyhow::anyhow!("{}", msg)
    })?;
    use tokio::io::AsyncWriteExt;
    writer_half.write_all(frame.as_bytes()).await.map_err(|e| {
        let msg = format!("HP write failed: {e}");
        let _ = tx.blocking_send(StreamEvent::Error(msg.clone()));
        anyhow::anyhow!("{}", msg)
    })?;
    writer_half.write_all(b"\n").await.ok();
    writer_half.flush().await.ok();
    drop(writer_half);

    // Read streaming response
    use tokio::io::AsyncBufReadExt;
    let mut reader = tokio::io::BufReader::new(reader_half);
    let mut line = String::new();
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut think_sig: Option<String> = None;
    let mut usage_info: Option<UsageInfo> = None;
    let mut stop_reason: Option<String> = None;

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break, // EOF
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() { continue; }

                let hp_resp: dsx_proto::HpToAgent = match serde_json::from_str(trimmed) {
                    Ok(r) => r,
                    Err(_) => continue,
                };

                match hp_resp {
                    dsx_proto::HpToAgent::ContentDelta { delta, reasoning: r } => {
                        if !delta.is_empty() {
                            content += &delta;
                            let _ = tx.send(StreamEvent::ContentDelta(delta)).await;
                        }
                        if let Some(ref r) = r {
                            reasoning += r;
                            let _ = tx.send(StreamEvent::ReasoningDelta(r.clone())).await;
                        }
                    }
                    dsx_proto::HpToAgent::ToolProgress { id, content: args, .. } => {
                        let _ = tx.send(StreamEvent::ToolCallProgress {
                            name: id,
                            args_so_far: args,
                        }).await;
                    }
                    dsx_proto::HpToAgent::ApiResponse {
                        content: final_content,
                        tool_calls,
                        stop_reason: sr,
                        reasoning_content,
                        thinking_signature: tsig,
                        usage,
                    } => {
                        if !final_content.is_empty() {
                            content = final_content;
                        }
                        stop_reason = sr;
                        think_sig = tsig;
                        if let Some(u) = usage {
                            usage_info = Some(UsageInfo {
                                prompt_tokens: u.prompt_tokens,
                                completion_tokens: u.completion_tokens,
                                total_tokens: u.total_tokens,
                                prompt_cache_hit_tokens: u.prompt_cache_hit_tokens,
                                prompt_cache_miss_tokens: u.prompt_cache_miss_tokens,
                                completion_tokens_details: None,
                            });
                        }

                        let parsed_tcs: Vec<dsx_types::ToolCall> = tool_calls.as_ref()
                            .and_then(|tc| tc.as_array())
                            .map(|arr| {
                                arr.iter().filter_map(|v| {
                                    let id = v.get("id")?.as_str()?;
                                    let (name, arguments) = if let Some(func) = v.get("function") {
                                        (func.get("name")?.as_str()?, func.get("arguments")?.as_str()?)
                                    } else {
                                        (v.get("name")?.as_str()?, v.get("arguments")?.as_str()?)
                                    };
                                    Some(dsx_types::ToolCall {
                                        id: id.to_string(),
                                        call_type: "function".into(),
                                        function: dsx_types::FunctionCall {
                                            name: name.to_string(),
                                            arguments: arguments.to_string(),
                                        },
                                    })
                                }).collect()
                            })
                            .unwrap_or_default();

                        let raw_message = Message {
                            role: "assistant".into(),
                            content: if content.is_empty() { None } else { Some(content.clone()) },
                            name: None,
                            tool_calls: if parsed_tcs.is_empty() { None } else { Some(parsed_tcs) },
                            tool_call_id: None,
                            reasoning_content: if reasoning.is_empty() { reasoning_content } else { Some(reasoning.clone()) },
                            thinking_signature: think_sig,
                        };
                        let _ = tx.send(StreamEvent::Done {
                            raw_message,
                            usage: usage_info,
                            stop_reason,
                        }).await;
                        return Ok(());
                    }
                    dsx_proto::HpToAgent::Error { message } => {
                        let _ = tx.send(StreamEvent::Error(message)).await;
                        return Err(anyhow::anyhow!("HP error"));
                    }
                    _ => {} // ok, verdicts, health, etc. — ignore
                }
            }
            Err(e) => {
                let msg = format!("HP read error: {e}");
                let _ = tx.blocking_send(StreamEvent::Error(msg.clone()));
                return Err(anyhow::anyhow!("{}", msg));
            }
        }
    }

    // EOF without api_response — partial output
    let raw_message = Message {
        role: "assistant".into(),
        content: if content.is_empty() { None } else { Some(content) },
        name: None,
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: if reasoning.is_empty() { None } else { Some(reasoning) },
        thinking_signature: think_sig,
    };
    let _ = tx.send(StreamEvent::Done {
        raw_message,
        usage: usage_info,
        stop_reason,
    }).await;
    Ok(())
}

/// Stub: get account balance. Phase 4: replace with real API call.
pub async fn get_balance(_cfg: &crate::config::Config) -> anyhow::Result<dsx_types::BalanceInfo> {
    Ok(dsx_types::BalanceInfo {
        is_available: true,
        balance_infos: vec![],
    })
}
