//! OpenAI Chat Completions API streaming client — synchronous facade over reqwest.
//! Includes retry with exponential backoff for transient errors (429, 500, 503, transport).

use futures::StreamExt;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use qaqh_types::{CacheTokenField, ThinkingParamMode};
use qaqh_types::{ContentBlock, Message, ToolDef, UsageInfo};

use super::sse::SseDecoder;
use super::transport::{
    Attempt, STREAM_IDLE_TIMEOUT, SseTrace, StatefulFilter, block_on, filter_stateful_messages,
    http_error_description, is_cancelled, is_retryable, normalize_skill_envelope,
    parse_retry_after, run_with_retry, stateful_noop_done_event, stateful_noop_sync_error,
};
use super::transport::{RetryPolicy, SSE_POLL_INTERVAL};
use super::types::{
    EmptyStreamEof, ProviderConfig, StreamEvent, clamp_effort_to_allowlist,
    normalize_reasoning_effort, safe_provider_error_body,
};

/// Providers use several OpenAI-compatible names for the same hidden
/// reasoning stream. Keep that data out of `content`, which is user-visible.
fn reasoning_delta(delta: &serde_json::Value) -> Option<&str> {
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

/// Send a chat completion request and stream SSE events via `on_event`.
///
/// `cancel` is an optional `Arc<AtomicBool>` that, when set to `true`, causes
/// the streaming loop to abort within one `SSE_POLL_INTERVAL`. This keeps
/// cancellation responsive while the HTTP response body is being streamed.
#[allow(clippy::string_slice)]
#[allow(clippy::too_many_arguments)] // 参数面塑形另立项（PLAN D-5）
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
    let (messages, image_index_base) = if provider.stateful {
        match filter_stateful_messages(messages) {
            StatefulFilter::Incremental {
                messages,
                dropped_images,
            } => (messages, dropped_images),
            // BUG-2026-09-13-12：增量全灭（尾 assistant）→ 不发空数组，
            // 零 HTTP 请求 no-op 成功。远端会话已持有该 assistant 响应，
            // 语义上本次调用无新内容可发；Done 让 runtime 正常收口本回合。
            StatefulFilter::Empty => {
                log::info!("stateful increment empty (tail assistant) — skipping request as no-op");
                on_event(stateful_noop_done_event());
                return Ok(());
            }
        }
    } else {
        (messages, 0)
    };

    let api_msgs = convert_messages(provider, messages, None, image_index_base);

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
    if provider.include_stream_usage {
        body_map.insert(
            "stream_options".into(),
            serde_json::json!({"include_usage": true}),
        );
    }
    // thinking 参数契约（provider.rs `ThinkingParamMode`）由流式与 sync 共用，
    // 见 `apply_thinking_params`——两路径曾各自手写并漂移（sync 曾发错键名），已收敛到单一实现。
    apply_thinking_params(&mut body_map, provider);
    body_map.insert("max_tokens".into(), serde_json::json!(max_tokens));

    if provider.supports_reasoning_effort
        && let Some(ref e) = effort
    {
        // QAQ-Harness always reasons: promote none/minimal/disable to the
        // lowest thinking level instead of sending them through.
        let e = normalize_reasoning_effort(Some(e)).unwrap_or_else(|| e.clone());
        // Router allowlist (e.g. OpenRouter ox-alpha: max/high/low): snap
        // off-domain values to the nearest allowed level — routers ignore
        // or reject unknown efforts when require_parameters is unset.
        let e = match &provider.effort_allowlist {
            Some(list) => clamp_effort_to_allowlist(&e, list),
            None => e,
        };
        body_map.insert("reasoning_effort".into(), serde_json::json!(e));
    }
    if let Some(sample) = provider.do_sample {
        body_map.insert("do_sample".into(), serde_json::json!(sample));
    }
    if let Some(ref t) = openai_tools {
        body_map.insert("tools".into(), serde_json::Value::Array(t.clone()));
        if provider.require_provider_parameters {
            body_map.insert(
                "provider".into(),
                serde_json::json!({"require_parameters": true}),
            );
        }
    }
    if let Some(ref uid) = user_id
        && provider.user_id_mode.is_some()
    {
        body_map.insert("user_id".into(), serde_json::json!(uid));
    }

    let body = serde_json::Value::Object(body_map);
    let url = build_chat_url(&provider.base_url, provider.chat_path.as_deref());

    // T8: 统一重试执行器——计数/取消检查/退避计算（retry-after 优先）/Retrying
    // 事件/可取消睡眠全部集中在 transport::run_with_retry；本闭包只做
    // "一次尝试"并按结果分类。策略缺省即现行全局常量（T9 起可按端点注入）。
    let policy = RetryPolicy::from_spec(provider.retry.as_ref());
    run_with_retry(&policy, cancel, on_event, |_attempt, on_event| {
        let resp = match block_on(async {
            provider
                .apply_opencode_headers(crate::shared_http_client().post(&url))
                .header("Authorization", format!("Bearer {}", provider.api_key))
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
        }) {
            Ok(resp) => resp,
            // Transport / timeout / connection errors
            Err(e) => {
                return Attempt::Retry {
                    retry_after: None,
                    reason: format!("{e}"),
                    final_error: format!("HTTP transport error: {e}"),
                };
            }
        };

        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            return match stream_sse(resp, provider, user_id.as_deref(), cancel, on_event) {
                Ok(()) => Attempt::Ok(()),
                // Upstream closed the stream before [DONE] with zero
                // content (early EOF, mid-stream read error, or idle
                // timeout) — treat like a transport error and retry the
                // whole request (mirrors opencode's stream-level retry).
                Err(e) if e.downcast_ref::<EmptyStreamEof>().is_some() => {
                    let cause = e.to_string();
                    Attempt::Retry {
                        retry_after: None,
                        reason: if cause.is_empty() {
                            "stream closed early (no content)".into()
                        } else {
                            cause
                        },
                        final_error: "upstream closed stream before any content".into(),
                    }
                }
                Err(e) => Attempt::Fatal(e),
            };
        }
        // HTTP error — read body for details（headers 需在 text() 前抓取）
        let retry_after = parse_retry_after(resp.headers(), &policy);
        let text = block_on(resp.text()).unwrap_or_default();
        let code_desc = http_error_description(status);
        if !is_retryable(status) {
            let msg = format!("OpenAI API HTTP {} ({})", status, code_desc);
            let detail = if status == 401 {
                "authentication failed".into()
            } else {
                safe_provider_error_body(&text, &provider.api_key)
            };
            on_event(StreamEvent::Error(format!("{}: {}", msg, detail)));
            return Attempt::Fatal(anyhow::anyhow!("{}", msg));
        }
        let msg = format!("OpenAI API HTTP {} ({})", status, code_desc);
        let detail = safe_provider_error_body(&text, &provider.api_key);
        Attempt::Retry {
            retry_after,
            reason: format!("HTTP {} ({})", status, code_desc),
            final_error: format!("{}: {}", msg, detail),
        }
    })
}

/// 解析单个 SSE chunk 的 `delta` 对象，按模型输出意图派生流式事件。
///
/// **字段处理顺序（顺序即语义）**：
/// 1. `reasoning_content`/`reasoning`/`thinking`/`analysis_content` 在前；
/// 2. `content`（含 inline think 标签切分 + DSML 工具检测）其次；
/// 3. 原生 `tool_calls` 最后。
///
/// 同一 chunk 可能**同时携带 `reasoning_content`（思考尾部）与 `content`
/// （正文开头）两个字段**——模型输出顺序是 reasoning 在前、content 在后
/// （journal `server_ts` 同毫秒拆分的两条 `round_delta` 即证据）。若先发
/// content 会把正文插到思考链中间，造成前端"思考链与正文错排"。
#[allow(clippy::too_many_arguments)] // 参数面塑形另立项（PLAN D-5）
fn emit_delta_fields(
    delta: &serde_json::Value,
    text_buf: &mut String,
    reasoning_buf: &mut String,
    tool_acc: &mut HashMap<usize, (String, String, String)>,
    dsml_buf: &mut String,
    dsml_seen: &mut HashSet<String>,
    inline_thinking: &mut bool,
    traced: &mut dyn FnMut(StreamEvent),
) {
    // 1. Reasoning content 先于 content（同一 chunk 双字段时的正确顺序）。
    if let Some(rc) = reasoning_delta(delta) {
        let r = rc.to_string();
        reasoning_buf.push_str(&r);
        traced(StreamEvent::ReasoningDelta(r));
    }

    // 2. Text content（含 inline thinking 切分与 DSML 工具检测）。
    if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
        for (is_reasoning, t) in split_inline_thinking(text, inline_thinking) {
            if is_reasoning {
                reasoning_buf.push_str(&t);
                traced(StreamEvent::ReasoningDelta(t));
            } else {
                text_buf.push_str(&t);
                traced(StreamEvent::ContentDelta(t.clone()));

                // DSML tool call detection in content stream
                dsml_buf.push_str(&t);
                let mut search_from = 0usize;
                while let Some(start) = dsml_buf[search_from..].find("<｜DSML｜invoke name=\"") {
                    let abs_start = search_from + start;
                    let after_tag = abs_start + "<｜DSML｜invoke name=\"".len();
                    if let Some(rest) = dsml_buf.get(after_tag..)
                        && let Some(quote_end) = rest.find('"')
                    {
                        let name = rest[..quote_end].to_string();
                        if dsml_seen.insert(name.clone()) {
                            let idx = dsml_seen.len() - 1;
                            traced(StreamEvent::ToolCallProgress {
                                index: idx,
                                id: format!("dsml_tc_{}", idx),
                                name,
                                args_chunk: String::new(),
                            });
                        }
                        search_from = after_tag + quote_end + 1;
                        continue;
                    }
                    break;
                }
            }
        }
    }

    // 3. Tool calls (native OpenAI format)
    if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
        for tc in tcs {
            let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
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
                traced(StreamEvent::ToolCallProgress {
                    index: idx,
                    id: entry.0.clone(),
                    name: entry.1.clone(),
                    args_chunk: args.to_string(),
                });
            }
        }
    }
}

/// 单个 SSE 帧的处理结果。
enum FrameAction {
    /// 继续消费流。
    Continue,
    /// 收到 `[DONE]` 终止标记：停止读取。
    Done,
}

/// 处理一帧 chat completions SSE 数据（`data:` payload）。
///
/// 由 [`stream_sse`] 调用；帧行解码与聚合由共享的 [`SseDecoder`] 完成，
/// 本函数只负责 JSON 解析与事件派生，便于独立测试。
#[allow(clippy::too_many_arguments)]
fn handle_chat_frame(
    data_str: &str,
    provider: &ProviderConfig,
    text_buf: &mut String,
    reasoning_buf: &mut String,
    tool_acc: &mut HashMap<usize, (String, String, String)>,
    dsml_buf: &mut String,
    dsml_seen: &mut HashSet<String>,
    usage_info: &mut Option<UsageInfo>,
    stop_reason: &mut Option<String>,
    inline_thinking: &mut bool,
    traced: &mut dyn FnMut(StreamEvent),
) -> anyhow::Result<FrameAction> {
    if data_str.is_empty() {
        return Ok(FrameAction::Continue);
    }
    // `[DONE]` 是 chat completions 官方流终止标记：立即结束读取。
    // 此前此处 `continue` 会在 [DONE] 后继续读——若连接随后被服务器
    // RST（资源紧张/代理超时），events.next() 返回 Err → 已完整输出
    // 的作答被误判为 TurnFailed（"完成作答后返回错误"的根因）。
    if data_str == "[DONE]" {
        return Ok(FrameAction::Done);
    }

    let ev: serde_json::Value = match serde_json::from_str(data_str) {
        Ok(e) => e,
        Err(e) => {
            log::warn!("OpenAI SSE: deserialize fail: {} — data: {}", e, data_str);
            return Ok(FrameAction::Continue);
        }
    };

    // Parse choices
    if let Some(choices) = ev.get("choices").and_then(|c| c.as_array())
        && let Some(choice) = choices.first()
    {
        let finish = choice.get("finish_reason").and_then(|v| v.as_str());
        if let Some(fr) = finish
            && !fr.is_empty()
            && fr != "null"
        {
            *stop_reason = Some(fr.to_string());
        }

        if let Some(delta) = choice.get("delta") {
            emit_delta_fields(
                delta,
                text_buf,
                reasoning_buf,
                tool_acc,
                dsml_buf,
                dsml_seen,
                inline_thinking,
                traced,
            );
        }
    }

    // Usage info (may appear in any chunk).
    // When stream_options.include_usage=true the field is present on
    // every chunk but is null for all intermediate chunks; only the final
    // chunk before [DONE] carries actual token counts.  Skip null to avoid
    // emitting zero-value UsageUpdate events that cause the info panel to
    // flicker between 0 and real values.
    if let Some(u) = ev.get("usage").filter(|v| !v.is_null()) {
        let pt = u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let ct = u
            .get("completion_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let (hit, miss, cache_usage_reported) = match provider.cache_field {
            CacheTokenField::PromptCacheHitTokens => {
                let hit_value = u.get("prompt_cache_hit_tokens");
                let miss_value = u.get("prompt_cache_miss_tokens");
                (
                    hit_value.and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                    miss_value.and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                    hit_value.is_some() || miss_value.is_some(),
                )
            }
            CacheTokenField::PromptDetailsCached => {
                let cached_value = u
                    .get("prompt_tokens_details")
                    .and_then(|d| d.get("cached_tokens"));
                let cached = cached_value.and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                (cached, pt.saturating_sub(cached), cached_value.is_some())
            }
            CacheTokenField::UsageCachedTokens => {
                let cached_value = u.get("cached_tokens");
                let cached = cached_value.and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                (cached, pt.saturating_sub(cached), cached_value.is_some())
            }
            CacheTokenField::None => (0, 0, false),
        };
        let rt = u
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let usage = UsageInfo {
            prompt_tokens: pt,
            completion_tokens: ct,
            total_tokens: pt + ct,
            prompt_cache_hit_tokens: hit,
            prompt_cache_miss_tokens: miss,
            reasoning_tokens: rt,
            cache_usage_reported: Some(cache_usage_reported),
        };
        *usage_info = Some(usage.clone());
        traced(StreamEvent::UsageUpdate(usage));
    }

    Ok(FrameAction::Continue)
}

fn stream_sse(
    resp: reqwest::Response,
    provider: &ProviderConfig,
    _user_id: Option<&str>,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut dyn FnMut(StreamEvent),
) -> anyhow::Result<()> {
    let mut decoder = SseDecoder::new();
    let mut stream = resp.bytes_stream();

    let mut text_buf = String::new();
    let mut reasoning_buf = String::new();
    let mut tool_acc: HashMap<usize, (String, String, String)> = HashMap::new();
    let mut dsml_buf = String::new();
    let mut dsml_seen: HashSet<String> = HashSet::new();
    let mut usage_info: Option<UsageInfo> = None;
    let mut stop_reason: Option<String> = None;
    let mut inline_thinking = false;

    let mut trace = SseTrace::from_env();
    let callback = on_event;
    let mut traced = move |event: StreamEvent| {
        trace.record(&event);
        callback(event);
    };

    let mut done_reached = false;
    // 流中途抢救标记：读错误/空闲超时且已有产出时置位——跳出读取循环后
    // 走下方 partial-output 组装，残帧冲刷错误不再判死（见下方 flush 守卫）。
    let mut stream_interrupted = false;
    // 空闲看门狗：最后一次收到字节的时刻（对齐 codex stream_idle_timeout）。
    let mut last_rx = std::time::Instant::now();
    loop {
        // Check cancel before each read attempt
        if is_cancelled(cancel) {
            return Err(anyhow::anyhow!("cancelled by user"));
        }

        // 先消费缓冲中已完整的帧。
        while let Some(frame) = decoder.next_frame() {
            let Ok(data_str) = frame else {
                continue;
            };
            match handle_chat_frame(
                &data_str,
                provider,
                &mut text_buf,
                &mut reasoning_buf,
                &mut tool_acc,
                &mut dsml_buf,
                &mut dsml_seen,
                &mut usage_info,
                &mut stop_reason,
                &mut inline_thinking,
                &mut traced,
            ) {
                Ok(FrameAction::Continue) => {}
                Ok(FrameAction::Done) => done_reached = true,
                Err(e) => return Err(e),
            }
        }
        if done_reached {
            break;
        }

        let chunk = match block_on(async {
            tokio::time::timeout(SSE_POLL_INTERVAL, stream.next()).await
        }) {
            Ok(Some(Ok(chunk))) => {
                last_rx = std::time::Instant::now();
                chunk
            }
            Ok(Some(Err(e))) => {
                // 中途读错误 = 远端排流/网络中断（对齐 codex ApiError::Stream：
                // 一律可重试）。按产出状态分类，而非一律判死：
                // - 零产出 → 并入 EmptyStreamEof 哨兵，外层整请求重试；
                //   详情只入日志（重试成功则用户无感知）。
                // - 有产出（stateful=false）→ 已流出的增量不可重来，
                //   按"上游排流"收口：stop_reason 留 None，runtime 的
                //   带词重启机制自动续写（对齐 opencode 静默损流处理）。
                // - 有产出（stateful=true）→ 服务端会话状态未知，盲续写
                //   会错位；维持判死（现状）。
                if text_buf.is_empty() && reasoning_buf.is_empty() && tool_acc.is_empty() {
                    log::warn!("OpenAI SSE read error (no content, will retry): {e}");
                    return Err(anyhow::Error::new(EmptyStreamEof));
                }
                if provider.stateful {
                    let msg = format!("SSE read error: {e}");
                    traced(StreamEvent::Error(msg.clone()));
                    return Err(anyhow::anyhow!("{}", msg));
                }
                log::warn!("OpenAI SSE interrupted mid-stream, keeping partial output: {e}");
                stream_interrupted = true;
                break;
            }
            Ok(None) => break, // EOF
            Err(_elapsed) => {
                // 50ms 轮询超时：先检查空闲看门狗，再继续轮询。
                if last_rx.elapsed() >= STREAM_IDLE_TIMEOUT {
                    if text_buf.is_empty() && reasoning_buf.is_empty() && tool_acc.is_empty() {
                        log::warn!(
                            "OpenAI SSE idle {}s (no content, will retry)",
                            STREAM_IDLE_TIMEOUT.as_secs()
                        );
                        return Err(anyhow::Error::new(EmptyStreamEof));
                    }
                    if provider.stateful {
                        let msg =
                            format!("SSE idle timeout after {}s", STREAM_IDLE_TIMEOUT.as_secs());
                        traced(StreamEvent::Error(msg.clone()));
                        return Err(anyhow::anyhow!("{}", msg));
                    }
                    log::warn!(
                        "OpenAI SSE idle {}s mid-stream, keeping partial output",
                        STREAM_IDLE_TIMEOUT.as_secs()
                    );
                    stream_interrupted = true;
                    break;
                }
                continue;
            }
        };
        decoder.push(&chunk);
    }

    // EOF 残帧：处理未以空行收尾的聚合（补行尾+空行触发消费，与帧解析的
    // "空行定界事件"语义一致；等价于 eventsource-stream 的 EOF flush）。
    // stream_interrupted 时残帧是已送达的尾部数据，照常冲刷；冲刷中的帧错误
    // 只记日志不判死（整个流已处于抢救路径）。
    if !done_reached && decoder.has_pending() {
        decoder.push(b"\n\n");
        while let Some(frame) = decoder.next_frame() {
            let Ok(data_str) = frame else {
                continue;
            };
            match handle_chat_frame(
                &data_str,
                provider,
                &mut text_buf,
                &mut reasoning_buf,
                &mut tool_acc,
                &mut dsml_buf,
                &mut dsml_seen,
                &mut usage_info,
                &mut stop_reason,
                &mut inline_thinking,
                &mut traced,
            ) {
                Ok(FrameAction::Continue) => {}
                Ok(FrameAction::Done) => {}
                Err(e) if stream_interrupted => {
                    log::warn!("OpenAI SSE: trailing frame error after interrupt ignored: {e}");
                }
                Err(e) => return Err(e),
            }
        }
    }

    // 上游繁忙时可能不发错误码而是直接终止 HTTP 流：EOF 且既无 `[DONE]`
    // 也无 `finish_reason` 即为掐流。零产出 → 哨兵错误并入重试；有部分
    // 产出 → 照常组装（增量已流出，不可重来），由上层按 stop_reason=None
    // 识别"不完整回合"并续写（对齐 opencode 的带词重启）。
    if !done_reached && stop_reason.is_none() {
        if reasoning_buf.is_empty() && text_buf.is_empty() && tool_acc.is_empty() {
            return Err(anyhow::Error::new(EmptyStreamEof));
        }
        log::warn!(
            "OpenAI SSE: upstream closed stream without [DONE]/finish_reason — partial output kept (stop_reason absent)"
        );
    }

    let raw_message = assemble_streamed_message(reasoning_buf, text_buf, tool_acc);

    traced(StreamEvent::Done {
        raw_message,
        usage: usage_info,
        stop_reason,
    });

    Ok(())
}

/// 由累积缓冲组装最终的 assistant 消息（`Done` 事件载荷）。
///
/// 独立成函数是为了让「参数缺失的 tool_call 如何收口」能被单测直接覆盖
/// （T-7-1 / BUG-2026-09-13-13）：上游掐流时可能只送到 `id`+`name`、
/// `arguments` 增量未到（`args_json == ""`），或显式送来 `null`——两种情况下
/// `serde_json::from_str` 都得 `Null`。若把 `Null` 原样落进 `input`，持久化前
/// 清洗（`is_hanging_tool_use` 只按 id/name 判悬挂）不会拦下它，出站序列化便
/// 写出 `"arguments": "null"`，部分端点直接判 400。
///
/// 与 anthropic 侧（`message_api.rs`）保持同一语义：空/非法参数收敛为 `{}`。
fn assemble_streamed_message(
    reasoning_buf: String,
    text_buf: String,
    mut tool_acc: HashMap<usize, (String, String, String)>,
) -> Message {
    let mut blocks: Vec<ContentBlock> = Vec::new();

    if !reasoning_buf.is_empty() {
        blocks.push(ContentBlock::Reasoning {
            reasoning: reasoning_buf,
        });
    }

    // 2026-10-05：DSML/XML 文本态工具调用解析已整体移除（DeepSeek v4.1 起
    // 原生输出结构化 tool_calls，不再吐脏字符）。正文就是正文，工具调用只
    // 认上游的结构化 tool_acc。
    if !text_buf.is_empty() {
        blocks.push(ContentBlock::text(&text_buf));
    }

    let mut sorted: Vec<(usize, String, String, String)> = tool_acc
        .into_iter()
        .map(|(idx, (id, name, args))| (idx, id, name, args))
        .collect();
    sorted.sort_by_key(|(idx, _, _, _)| *idx);
    for (_idx, id, name, args_json) in sorted {
        let input: serde_json::Value =
            serde_json::from_str(&args_json).unwrap_or(serde_json::Value::Null);
        // anthropic 侧同款兜底：模型中途掐流时 `input` 可能为 null，
        // 收敛为空对象，避免出站 `"arguments": "null"` 被端点判 400。
        let input = if input.is_null() {
            serde_json::json!({})
        } else {
            input
        };
        blocks.push(ContentBlock::ToolUse { id, name, input });
    }

    Message {
        msg_id: None,
        role: "assistant".into(),
        name: None,
        content: blocks,
    }
}

// ── Message conversion ──

/// 把内部 `Message` 列表转换为 Chat Completions 协议的 `messages` JSON 数组。
///
/// 本函数只做纯转换，不做 stateful 增量过滤——过滤（首次请求发全部，后续只发
/// 最后一条 assistant 之后的消息）由调用方先用 `transport::filter_stateful_messages`
/// 完成后传入。
///
/// 入参 `image_index_base` 是被过滤前缀中的图片块数量——`read_image` 的
/// [Image #N] 编号是会话级累加的（与 registry 索引一致），过滤后转换时需要以此为基准续编。
fn convert_messages(
    provider: &ProviderConfig,
    messages: Vec<Message>,
    system: Option<String>,
    image_index_base: usize,
) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::new();
    if let Some(sys) = system
        && !sys.is_empty()
    {
        out.push(serde_json::json!({"role": "system", "content": sys}));
    }

    let mut img_idx: usize = image_index_base;
    // 工具图片降级成的合成 user 消息不能就地落盘：一个 assistant 消息里的多个
    // tool_call 会产生**连续多条** tool 消息，而 OpenAI 要求 assistant(tool_calls)
    // 之后紧跟全部对应的 tool 消息，中间插入 user 会直接 HTTP 400
    //（"An assistant message with 'tool_calls' must be followed by tool messages
    // responding to each tool_call_id"）。所以先把合成消息攒起来，等这串 tool
    // 消息走完（遇到非 tool 消息，或消息遍历结束）再统一落盘。
    let mut pending_media: Vec<serde_json::Value> = Vec::new();
    for msg in messages {
        if msg.role != "tool" && !pending_media.is_empty() {
            out.append(&mut pending_media);
        }
        let name = &msg.name;
        match msg.role.as_str() {
            "system" | "developer" => {
                // `developer`（Responses 专属运行时注入角色）在 Chat
                // Completions 协议下不存在——降级为 system，语义最接近。
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
                let mut text_parts: Vec<String> = Vec::new();
                let mut image_refs: Vec<String> = Vec::new();
                // 图片编号跨消息累加（从 image_index_base 起）：与
                // read_image registry 的会话级顺序索引保持一致。
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text: t } => text_parts.push(t.clone()),
                        ContentBlock::Image { mime_type, data } => {
                            image_refs.push(format!(
                                "[Image #{img_idx}: {mime_type}, ~{} bytes — call read_image(image_index={img_idx}) to view it yourself]",
                                data.len()
                            ));
                            img_idx += 1;
                        }
                        ContentBlock::ImageRef {
                            sha256: _,
                            mime_type,
                            bytes_len,
                        } => {
                            // A-2 L0：外置图片仅持索引，占位符用 bytes_len 显示。
                            image_refs.push(format!(
                                "[Image #{img_idx}: {mime_type}, ~{bytes_len} bytes — call read_image(image_index={img_idx}) to view it yourself]"
                            ));
                            img_idx += 1;
                        }
                        _ => {}
                    }
                }
                let mut combined_text = text_parts.join("");
                if !image_refs.is_empty() {
                    if !combined_text.is_empty() {
                        combined_text.push('\n');
                    }
                    combined_text.push_str(&image_refs.join("\n"));
                }
                let mut obj = serde_json::json!({"role": "user", "content": combined_text});
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
                if provider.supports_reasoning_content && !reasoning.is_empty() {
                    obj["reasoning_content"] = serde_json::json!(reasoning);
                }
                if !tool_calls.is_empty() {
                    if provider.tool_call_content_null && obj.get("content").is_none() {
                        obj["content"] = serde_json::Value::Null;
                    }
                    obj["tool_calls"] = serde_json::json!(tool_calls);
                }
                if obj.as_object().is_some_and(|m| m.len() > 1) {
                    out.push(obj);
                }
            }
            "tool" => {
                for block in &msg.content {
                    match block {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            result,
                            ..
                        } => {
                            out.push(serde_json::json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": result.render_xml_envelope(),
                            }));
                        }
                        // 工具产出的图片（read_image）：OpenAI 兼容端点的
                        // tool 消息只接受字符串 content，图片降级为合成 user
                        // 消息（opencode 同款策略）；落盘时机见 pending_media。
                        ContentBlock::Image { mime_type, data } => {
                            pending_media.push(serde_json::json!({
                                "role": "user",
                                "content": [
                                    {"type": "text", "text": "Attached media from tool result:"},
                                    {"type": "image_url", "image_url": {"url": format!("data:{mime_type};base64,{data}")}},
                                ],
                            }));
                        }
                        ContentBlock::ImageRef {
                            sha256, mime_type, ..
                        } => {
                            // A-2 L0：按需读盘后走同一 data URI 降格路径。
                            match qaqh_types::image_store::load_image_b64(sha256, mime_type) {
                                Ok(data) => pending_media.push(serde_json::json!({
                                    "role": "user",
                                    "content": [
                                        {"type": "text", "text": "Attached media from tool result:"},
                                        {"type": "image_url", "image_url": {"url": format!("data:{mime_type};base64,{data}")}},
                                    ],
                                })),
                                Err(e) => log::warn!(
                                    "[gate] image {sha256} load failed, dropped from chat request: {e}"
                                ),
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    // tool 段结束后再落盘合成 media 消息（含消息列尾的未清空情形）。
    out.append(&mut pending_media);

    out
}

// ── thinking 参数契约（流式 / sync 共用）──

/// 按 provider 的 thinking 参数契约写入请求体顶层键。
///
/// 契约见 `qaqh-types/src/provider.rs` 的 `ThinkingParamMode`：Qwen 用顶层
/// `enable_thinking: true`（而非 `thinking`），MiniMax 还需附带 `reasoning_split`。
/// 流式与 sync（compact/title）路径必须经由本函数写入——两者曾各自手写并漂移
/// （sync 曾把 Qwen 的键发成 `thinking` 且漏发 `reasoning_split`），已收敛到单一实现。
pub(crate) fn apply_thinking_params(
    body: &mut serde_json::Map<String, serde_json::Value>,
    provider: &ProviderConfig,
) {
    if !provider.supports_thinking {
        return;
    }
    match provider.thinking_mode {
        ThinkingParamMode::OpenAi => {
            body.insert("thinking".into(), serde_json::json!({"type": "enabled"}));
        }
        ThinkingParamMode::QwenEnableThinking => {
            body.insert("enable_thinking".into(), serde_json::json!(true));
        }
        ThinkingParamMode::MiniMaxAdaptive => {
            body.insert("thinking".into(), serde_json::json!({"type": "adaptive"}));
            body.insert("reasoning_split".into(), serde_json::json!(true));
        }
    }
}

// ── Synchronous (non-streaming) chat ──

pub fn chat_sync_openai(
    provider: &ProviderConfig,
    model: &str,
    messages: Vec<Message>,
    max_tokens: u32,
) -> Result<String, String> {
    let messages = normalize_skill_envelope(provider, messages)?;
    let (messages, image_index_base) = if provider.stateful {
        match filter_stateful_messages(messages) {
            StatefulFilter::Incremental {
                messages,
                dropped_images,
            } => (messages, dropped_images),
            // BUG-2026-09-13-12：sync（compact/title）无流式收口可用，
            // 返回带稳定诊断码的可重试语义错误，而不是发空数组换 400 Fatal。
            StatefulFilter::Empty => {
                log::warn!("stateful increment empty (tail assistant) — sync call skipped");
                return Err(stateful_noop_sync_error());
            }
        }
    } else {
        (messages, 0)
    };
    let api_msgs = convert_messages(provider, messages, None, image_index_base);
    let url = build_chat_url(&provider.base_url, provider.chat_path.as_deref());

    let mut body = serde_json::json!({
        "model": model,
        "messages": api_msgs,
        "max_tokens": max_tokens,
        "stream": false,
    });
    // thinking 参数契约与流式路径共用（见 `apply_thinking_params`）——
    // 此处曾手写并漂移：Qwen 键名发错（`thinking` 而非 `enable_thinking`）、
    // MiniMax 漏发 `reasoning_split`。
    if let Some(obj) = body.as_object_mut() {
        apply_thinking_params(obj, provider);
    }

    // T8: sync 路径（compact/title）补轻量重试——原来零重试，上游瞬时
    // 429/5xx 或传输抖动即失败；仅重试传输错误 + 可重试 HTTP 状态。
    // sync 无 StreamEvent 回调，重试事件不对外广播，只记日志。
    let policy = RetryPolicy::from_spec(provider.retry.as_ref());
    let mut on_event = |_e: StreamEvent| {};
    run_with_retry(&policy, None, &mut on_event, |attempt, _on_event| {
        // ⚠ `.send()` 必须在 `block_on` **内部**求值（#315）。
        //
        // reqwest 的 `RequestBuilder::send` 是**普通 fn**（request.rs:517），它立刻调
        // `Client::execute_request`（client.rs:2603，也是普通 fn），而后者在构造阶段就
        // 建 `tokio::time::sleep`（client.rs:2658/2664）。写成 `block_on(....send())`
        // 时 `.send()` 是**实参**、先于 `block_on` 求值 —— 在没有 runtime 的线程上
        // （如 `engine_title.rs` 的裸 `session-title` 线程）直接 panic：
        // "there is no reactor running"。放进 `async` 块后，构造推迟到首次 poll，
        // 此时已在 `FALLBACK_RT` 内。
        let resp = match block_on(async {
            provider
                .apply_opencode_headers(crate::shared_http_client().post(&url))
                .header("Authorization", format!("Bearer {}", provider.api_key))
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
        }) {
            Ok(resp) => resp,
            Err(e) => {
                log::warn!("OpenAI sync attempt {attempt} transport error, will retry: {e}");
                return Attempt::Retry {
                    retry_after: None,
                    reason: format!("sync transport error: {e}"),
                    final_error: format!("compact request failed: {e}"),
                };
            }
        };
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            // headers 需在 text() 前抓取
            let retry_after = parse_retry_after(resp.headers(), &policy);
            let text = block_on(resp.text()).unwrap_or_default();
            if status != 401 && is_retryable(status) {
                log::warn!("OpenAI sync attempt {attempt} HTTP {status} retryable, will retry");
                return Attempt::Retry {
                    retry_after,
                    reason: format!("sync HTTP {} ({})", status, http_error_description(status)),
                    final_error: format!(
                        "compact request failed: HTTP {} ({}) {}",
                        status,
                        http_error_description(status),
                        safe_provider_error_body(&text, &provider.api_key)
                    ),
                };
            }
            let detail = if status == 401 {
                "authentication failed".into()
            } else {
                safe_provider_error_body(&text, &provider.api_key)
            };
            return Attempt::Fatal(anyhow::anyhow!(
                "compact request failed: HTTP {} ({}) {}",
                status,
                http_error_description(status),
                detail
            ));
        }
        let json: serde_json::Value = match block_on(resp.json()) {
            Ok(j) => j,
            Err(e) => {
                // 解析失败属于不可恢复响应（非传输层瞬时态），不重试。
                return Attempt::Fatal(anyhow::anyhow!("compact parse failed: {e}"));
            }
        };
        match json["choices"][0]["message"]["content"].as_str() {
            Some(s) => Attempt::Ok(s.to_string()),
            None => Attempt::Fatal(anyhow::anyhow!("compact: no content in response")),
        }
    })
    .map_err(|e| e.to_string())
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

// ── Tests ──

#[cfg(test)]
mod skill_envelope_tests {
    use super::*;

    #[test]
    fn image_placeholders_number_globally_across_messages() {
        let provider = provider();
        let mk = |text: &str| {
            let mut m = Message::user(text);
            m.content.push(ContentBlock::image("image/png", "Zm9v"));
            m
        };
        let messages = vec![mk("one"), mk("two")];
        let out = convert_messages(&provider, messages, None, 0);
        let joined: String = out
            .iter()
            .map(|o| o["content"].as_str().unwrap_or_default())
            .collect();
        assert!(joined.contains("[Image #0:"), "first upload → #0");
        assert!(joined.contains("[Image #1:"), "second upload → #1");
    }

    /// 复现 BUG-2026-09-16-01：一个 assistant 消息里连发两次 `read_image`
    /// （两个 tool_call），两条 tool 消息各带一张图。图片降级成的合成 user
    /// 消息**不得插在两条 tool 消息之间**——OpenAI 要求 assistant(tool_calls)
    /// 之后紧跟全部对应的 tool 消息，否则 HTTP 400：
    /// "An assistant message with 'tool_calls' must be followed by tool
    /// messages responding to each tool_call_id"。
    /// PR2：工作区变更审计注入落在**整批 tool 消息之后**的一条 `user` 消息上。
    /// Chat Completions 路径因此天然合法：assistant(tool_calls) 之后紧跟全部 tool
    /// 消息的硬约束未被破坏，且 `name` 会原样上 wire——来源标识不丢。
    /// 判决性探针：**不给任何工具**，逼模型只凭上下文回答 app.py 的状态。
    ///
    /// 若注入里的 diff 真的进了模型的推理，WITH 臂应能说出「空文件 / 原 111B /
    /// def main() 被删」这类只有报告里才有的细节；WITHOUT 臂只能承认不知道。
    /// 同时打印 usage，量化一条报告的 token 成本。
    #[test]
    #[ignore = "需要真实 provider 端点"]
    fn live_probe_injection_content_reaches_model_reasoning() {
        let Some(base) = std::env::var("QAQH_LIVE_PROBE_URL")
            .ok()
            .filter(|u| !u.is_empty())
        else {
            eprintln!("SKIP: 未设置 QAQH_LIVE_PROBE_URL");
            return;
        };
        let model =
            std::env::var("QAQH_LIVE_PROBE_MODEL").unwrap_or_else(|_| "qwen3.8-flash".into());
        let key = std::env::var("QAQH_LIVE_PROBE_KEY").unwrap_or_else(|_| "probe".into());
        let url = base.trim_end_matches('/').to_string() + "/chat/completions";

        let history = || {
            vec![
                Message::user("跑一下 cleanup.py 清理 app.py。"),
                Message {
                    msg_id: None,
                    role: Message::ROLE_ASSISTANT.into(),
                    name: None,
                    content: vec![ContentBlock::ToolUse {
                        id: "call_1".into(),
                        name: "exec".into(),
                        input: serde_json::json!({ "command": "python cleanup.py" }),
                    }],
                },
                Message {
                    msg_id: None,
                    role: "tool".into(),
                    name: None,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "call_1".into(),
                        result: qaqh_types::ToolResult::ok("cleanup.py exit 0, no output"),
                    }],
                },
            ]
        };
        let question = "现在 app.py 是什么状态？只根据你已经知道的信息回答，不要提出要去读文件。";
        let report = concat!(
            "[workspace-changes turn=t1 round=0 mark=s0001790946308788_0001 calls=call_1]\n",
            "（按工作区状态扫描测得，覆盖本批全部工具的净效果）\n",
            "[workspace] 自标记以来 1 个文件变更\n",
            "[workspace] ⚠ 危险信号：1 个文件可疑，脚本可能没按预期工作\n",
            "  ⚠ app.py: 已变为空文件（原 111B）\n",
            "--- app.py (修改, 111B -> 0B)\n",
            "@@ -1,11 +0,0 @@\n-def main():\n-    conf = load()\n",
        );

        let mut baseline = history();
        baseline.push(Message::user(question));
        let mut injected = history();
        injected.push(Message {
            msg_id: None,
            role: Message::ROLE_USER.into(),
            name: Some("workspace".into()),
            content: vec![ContentBlock::text(report)],
        });
        injected.push(Message::user(question));

        let provider = provider();
        let mut input_without = 0u64;
        let mut input_with = 0u64;
        for (label, msgs, sink) in [
            ("WITHOUT-injection", baseline.clone(), &mut input_without),
            ("WITH-injection", injected.clone(), &mut input_with),
        ] {
            let api_messages = convert_messages(&provider, msgs, None, 0);
            // 故意不带 tools 字段：模型无法靠调工具逃避回答。
            let body = serde_json::json!({
                "model": model,
                "messages": api_messages,
                "max_tokens": 200,
                "stream": false,
            });
            let text = match block_on(async {
                crate::shared_http_client()
                    .post(&url)
                    .header("Authorization", format!("Bearer {key}"))
                    .header("Content-Type", "application/json")
                    .json(&body)
                    .send()
                    .await
            }) {
                Ok(resp) => block_on(resp.text()).unwrap_or_default(),
                Err(error) => format!("transport_error: {error}"),
            };
            let parsed: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
            let content = parsed["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("(none)")
                .to_string();
            *sink = parsed["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
            println!("### {label}  (prompt_tokens={sink})");
            println!("{content}");
            println!();
            assert!(!content.is_empty(), "{label}: 空响应 {text}");
        }
        println!("注入的额外 prompt token ≈ {}", input_with - input_without);
    }

    /// Live 探针：把「注入前 / 注入后」两份请求打给真实端点，对比模型行为。
    ///
    /// messages 全部由生产转换器 convert_messages 产出，所以 wire 形态与真实
    /// daemon 一致，不是手搓 payload 的近似实验。
    ///
    /// 默认 ignore；显式给端点才跑：
    /// QAQH_LIVE_PROBE_URL=http://127.0.0.1:8317/v1 cargo test -p qaqh-gate --lib live_probe -- --ignored --nocapture
    #[test]
    #[ignore = "需要真实 provider 端点"]
    fn live_probe_workspace_injection_changes_model_behaviour() {
        let Some(base) = std::env::var("QAQH_LIVE_PROBE_URL")
            .ok()
            .filter(|u| !u.is_empty())
        else {
            eprintln!("SKIP: 未设置 QAQH_LIVE_PROBE_URL");
            return;
        };
        let model =
            std::env::var("QAQH_LIVE_PROBE_MODEL").unwrap_or_else(|_| "qwen3.8-flash".into());
        let key = std::env::var("QAQH_LIVE_PROBE_KEY").unwrap_or_else(|_| "probe".into());
        let url = base.trim_end_matches('/').to_string() + "/chat/completions";

        // 场景：exec 跑脚本，脚本自报成功，但它把 app.py 清空了——工具回执上
        // 完全看不出来，这正是 exec 盲区。
        let exec_call = ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "exec".into(),
            input: serde_json::json!({ "command": "python cleanup.py" }),
        };
        let base_messages = vec![
            Message::user("跑一下 cleanup.py 清理 app.py，然后告诉我结果。"),
            Message {
                msg_id: None,
                role: Message::ROLE_ASSISTANT.into(),
                name: None,
                content: vec![exec_call],
            },
            Message {
                msg_id: None,
                role: "tool".into(),
                name: None,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    result: qaqh_types::ToolResult::ok("cleanup.py exit 0, no output"),
                }],
            },
        ];
        let report = concat!(
            "[workspace-changes turn=t1 round=0 mark=s0001790946308788_0001 calls=call_1]\n",
            "（按工作区状态扫描测得，覆盖本批全部工具的净效果）\n",
            "[workspace] 自标记以来 1 个文件变更\n",
            "[workspace] ⚠ 危险信号：1 个文件可疑，脚本可能没按预期工作\n",
            "  ⚠ app.py: 已变为空文件（原 111B）\n",
            "--- app.py (修改, 111B -> 0B)\n",
            "@@ -1,11 +0,0 @@\n-def main():\n-    conf = load()\n",
            "\n可执行回滚（journal 工具）:\n",
            "  撤销 app.py: journal action=replay file=app.py at=41 out=app.py\n",
        );
        let mut with_injection = base_messages.clone();
        with_injection.push(Message {
            msg_id: None,
            role: Message::ROLE_USER.into(),
            name: Some("workspace".into()),
            content: vec![ContentBlock::text(report)],
        });

        let tools = serde_json::json!([
            { "type": "function", "function": { "name": "exec", "description": "Run a shell command",
              "parameters": { "type": "object", "properties": { "command": { "type": "string" } },
              "required": ["command"] } } },
            { "type": "function", "function": { "name": "read", "description": "Read a file from the workspace",
              "parameters": { "type": "object", "properties": { "path": { "type": "string" } },
              "required": ["path"] } } }
        ]);

        let provider = provider();
        for (label, msgs) in [
            ("WITHOUT-injection", base_messages),
            ("WITH-injection", with_injection),
        ] {
            let api_messages = convert_messages(&provider, msgs, None, 0);
            let roles: Vec<&str> = api_messages
                .iter()
                .map(|m| m["role"].as_str().unwrap_or(""))
                .collect();
            let body = serde_json::json!({
                "model": model,
                "messages": api_messages,
                "tools": tools,
                "max_tokens": 256,
                "stream": false,
            });
            let send = block_on(async {
                crate::shared_http_client()
                    .post(&url)
                    .header("Authorization", format!("Bearer {key}"))
                    .header("Content-Type", "application/json")
                    .json(&body)
                    .send()
                    .await
            });
            let text = match send {
                Ok(resp) => block_on(resp.text()).unwrap_or_default(),
                Err(error) => format!("transport_error: {error}"),
            };
            let parsed: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
            let msg = &parsed["choices"][0]["message"];
            println!("### {label}");
            println!("  wire roles = {roles:?}");
            println!(
                "  content    = {}",
                msg["content"].as_str().unwrap_or("(none)")
            );
            println!("  tool_calls = {}", msg["tool_calls"]);
            println!();
            assert!(
                parsed["choices"].is_array(),
                "{label}: 端点未返回 choices: {text}"
            );
        }
    }

    #[test]
    fn workspace_diff_injection_after_tool_run_keeps_pairing_and_name() {
        let assistant = Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: vec![
                ContentBlock::ToolUse {
                    id: "call-1".into(),
                    name: "exec".into(),
                    input: serde_json::json!({ "command": "x" }),
                },
                ContentBlock::ToolUse {
                    id: "call-2".into(),
                    name: "exec".into(),
                    input: serde_json::json!({ "command": "y" }),
                },
            ],
        };
        let tool_msg = |id: &str| Message {
            msg_id: None,
            role: "tool".into(),
            name: None,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                result: qaqh_types::ToolResult::ok("ran"),
            }],
        };
        let diff = "[workspace-changes scan=s0001790940017262_0003] app.py is now empty";
        let msgs = vec![
            Message::user("run two commands"),
            assistant,
            tool_msg("call-1"),
            tool_msg("call-2"),
            Message {
                msg_id: None,
                role: "user".into(),
                name: Some("workspace".into()),
                content: vec![ContentBlock::text(diff)],
            },
        ];
        let out = convert_messages(&provider(), msgs, None, 0);
        let roles: Vec<&str> = out
            .iter()
            .map(|m| m["role"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            roles,
            vec!["user", "assistant", "tool", "tool", "user"],
            "注入必须排在整批 tool 消息之后：{out:#?}"
        );
        assert_eq!(out[2]["tool_call_id"], "call-1");
        assert_eq!(out[3]["tool_call_id"], "call-2");
        assert_eq!(out[4]["name"], "workspace", "{out:#?}");
        assert_eq!(out[4]["content"].as_str().unwrap_or(""), diff, "{out:#?}");
    }

    /// 锁定约束（而非期望行为）：若注入落在两条 tool 消息**之间**，转换器
    /// **不会**重排它——user 原样楔在中间，恰是 `convert_messages` 注释里
    /// "must be followed by tool messages responding to each tool_call_id" 的 HTTP 400
    /// 形态。结论：注入必须由 lap 边界（loop_outcome.rs 的 ContinueTurn 分支）
    /// 在整批 tool 结果落盘之后统一 flush，绝不能在工具线程内即时插队。
    #[test]
    fn chat_does_not_reorder_a_user_message_wedged_between_tool_messages() {
        let assistant = Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: vec![
                ContentBlock::ToolUse {
                    id: "call-1".into(),
                    name: "exec".into(),
                    input: serde_json::json!({ "command": "x" }),
                },
                ContentBlock::ToolUse {
                    id: "call-2".into(),
                    name: "exec".into(),
                    input: serde_json::json!({ "command": "y" }),
                },
            ],
        };
        let tool_msg = |id: &str| Message {
            msg_id: None,
            role: "tool".into(),
            name: None,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                result: qaqh_types::ToolResult::ok("ran"),
            }],
        };
        let msgs = vec![
            assistant,
            tool_msg("call-1"),
            Message {
                msg_id: None,
                role: "user".into(),
                name: Some("workspace".into()),
                content: vec![ContentBlock::text("mid-run injection")],
            },
            tool_msg("call-2"),
        ];
        let out = convert_messages(&provider(), msgs, None, 0);
        let roles: Vec<&str> = out
            .iter()
            .map(|m| m["role"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            roles,
            vec!["assistant", "tool", "user", "tool"],
            "楔入的 user 会原样保留在 tool 消息之间（须由调用方避免）：{out:#?}"
        );
    }

    #[test]
    fn parallel_tool_result_images_do_not_split_the_tool_run() {
        let provider = provider();
        let assistant = Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: vec![
                ContentBlock::ToolUse {
                    id: "call-1".into(),
                    name: "read_image".into(),
                    input: serde_json::json!({"path": "a.png"}),
                },
                ContentBlock::ToolUse {
                    id: "call-2".into(),
                    name: "read_image".into(),
                    input: serde_json::json!({"path": "b.png"}),
                },
            ],
        };
        let tool_msg = |id: &str, b64: &str| Message {
            msg_id: None,
            role: "tool".into(),
            name: None,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: id.into(),
                    result: qaqh_types::ToolResult::ok("image attached"),
                },
                ContentBlock::image("image/png", b64),
            ],
        };
        let messages = vec![
            assistant,
            tool_msg("call-1", "Zm9v"),
            tool_msg("call-2", "YmFy"),
        ];
        let out = convert_messages(&provider, messages.clone(), None, 0);
        let roles: Vec<&str> = out
            .iter()
            .map(|m| m["role"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            roles,
            vec!["assistant", "tool", "tool", "user", "user"],
            "两条 tool 消息必须紧邻 assistant(tool_calls)，图片合成消息延后：{out:#?}"
        );

        // 同一串 tool 消息后面还有普通消息时，合成图片消息必须在进入下一条
        // 消息之前就落盘（否则图片会跑到 assistant 回合之后）。
        let mut with_next_turn = messages;
        with_next_turn.push(Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: vec![ContentBlock::text("both images seen")],
        });
        let out = convert_messages(&provider, with_next_turn, None, 0);
        let roles: Vec<&str> = out
            .iter()
            .map(|m| m["role"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            roles,
            vec!["assistant", "tool", "tool", "user", "user", "assistant"],
            "图片合成消息不得跟到 assistant 之后：{out:#?}"
        );
    }

    #[test]
    fn stateful_filter_reports_dropped_images_as_index_base() {
        let provider = provider();
        let mut old = Message::user("old turn");
        old.content.push(ContentBlock::image("image/png", "Zm9v"));
        let assistant = Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: vec![ContentBlock::text("done")],
        };
        let mut fresh = Message::user("new turn");
        fresh.content.push(ContentBlock::image("image/png", "Zm9v"));
        let messages = vec![old, assistant, fresh];

        let (filtered, dropped) = match filter_stateful_messages(messages) {
            StatefulFilter::Incremental {
                messages,
                dropped_images,
            } => (messages, dropped_images),
            StatefulFilter::Empty => panic!("tail user message must yield an increment"),
        };
        assert_eq!(dropped, 1, "dropped prefix holds exactly one image");
        // 以丢弃数为基准续编，增量消息中的图仍是会话级 #1。
        let out = convert_messages(&provider, filtered, None, dropped);
        assert_eq!(out.len(), 1, "incremental request sends only the tail");
        let text = out[0]["content"].as_str().unwrap_or_default();
        assert!(text.contains("[Image #1:"), "got: {text}");
    }

    /// 解包增量结果（非全灭场景的断言便利函数）。
    fn expect_increment(filtered: StatefulFilter) -> Vec<Message> {
        match filtered {
            StatefulFilter::Incremental { messages, .. } => messages,
            StatefulFilter::Empty => panic!("expected an incremental slice, got Empty"),
        }
    }

    /// BUG-2026-09-13-12：显式锁定过滤后的三种分类，原先恒假的兜底
    /// 死分支（`last.role != "assistant"`）再无生存空间。
    #[test]
    fn stateful_tail_assistant_filters_to_empty_not_blank_request() {
        let asst = |text: &str| Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: vec![ContentBlock::text(text)],
        };

        // ① 尾消息即 assistant → Empty（不得回退成全量，更不得发空数组）。
        let tail_assistant = vec![Message::system("base"), Message::user("hi"), asst("done")];
        assert!(matches!(
            filter_stateful_messages(tail_assistant),
            StatefulFilter::Empty
        ));

        // ② 无 assistant 尾 → 全量首请求（增量语义，非 Empty）。
        let first_request = vec![Message::system("base"), Message::user("hi")];
        let full = expect_increment(filter_stateful_messages(first_request.clone()));
        assert_eq!(full.len(), first_request.len());

        // ③ assistant 之后还有消息 → 只发尾部增量。
        let tail_user = vec![
            Message::system("base"),
            Message::user("old"),
            asst("done"),
            Message::user("next"),
        ];
        let increment = expect_increment(filter_stateful_messages(tail_user));
        assert_eq!(
            increment
                .iter()
                .map(|message| message.role.as_str())
                .collect::<Vec<_>>(),
            vec!["user"]
        );

        // ④ 空历史 → Empty（同样不许发空数组）。
        assert!(matches!(
            filter_stateful_messages(Vec::new()),
            StatefulFilter::Empty
        ));
    }

    /// no-op 收口事件必须是完整 Done（非掐流）：stop_reason 缺失会让
    /// runtime 判为"上游掐流"并触发一次必为空的续写。
    #[test]
    fn stateful_noop_done_event_is_terminal_and_empty() {
        match stateful_noop_done_event() {
            StreamEvent::Done {
                raw_message,
                usage,
                stop_reason,
            } => {
                assert_eq!(raw_message.role, "assistant");
                assert!(raw_message.content.is_empty());
                assert!(usage.is_none());
                assert_eq!(stop_reason.as_deref(), Some("stop"));
            }
            other => panic!("expected Done, got {other:?}"),
        }
        assert!(
            stateful_noop_sync_error().starts_with("STATEFUL_INCREMENT_EMPTY"),
            "sync 错误需带稳定诊断码，便于调用方与用户定位"
        );
    }

    #[test]
    fn stateful_first_request_does_not_duplicate_system_slots() {
        let messages = vec![
            Message::system("base"),
            Message::system("catalog"),
            Message::user("hi"),
            Message::system("envelope"),
        ];
        let filtered = expect_increment(filter_stateful_messages(messages.clone()));
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
        let filtered = expect_increment(filter_stateful_messages(messages));
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
            None,
        )
    }

    /// BUG（2026-10-05 注释审计 §3.2）：sync（compact/title）路径曾手写 thinking
    /// 参数——Qwen 键名发成 `thinking`（契约要求顶层 `enable_thinking`）、MiniMax
    /// 漏发 `reasoning_split`。契约现收敛到 `apply_thinking_params` 单一实现，
    /// 本测试锁定三种模式的键位，防止流式 / sync 路径再次漂移。
    #[test]
    fn apply_thinking_params_matches_provider_contract() {
        let mut p = provider();
        p.supports_thinking = true;

        p.thinking_mode = ThinkingParamMode::QwenEnableThinking;
        let mut body = serde_json::Map::new();
        apply_thinking_params(&mut body, &p);
        assert_eq!(body.get("enable_thinking"), Some(&serde_json::json!(true)));
        assert!(body.get("thinking").is_none(), "Qwen 不得发 `thinking` 键");

        p.thinking_mode = ThinkingParamMode::MiniMaxAdaptive;
        let mut body = serde_json::Map::new();
        apply_thinking_params(&mut body, &p);
        assert_eq!(
            body.get("thinking"),
            Some(&serde_json::json!({"type": "adaptive"}))
        );
        assert_eq!(body.get("reasoning_split"), Some(&serde_json::json!(true)));

        p.thinking_mode = ThinkingParamMode::OpenAi;
        let mut body = serde_json::Map::new();
        apply_thinking_params(&mut body, &p);
        assert_eq!(
            body.get("thinking"),
            Some(&serde_json::json!({"type": "enabled"}))
        );
        assert!(body.get("enable_thinking").is_none());

        p.supports_thinking = false;
        let mut body = serde_json::Map::new();
        apply_thinking_params(&mut body, &p);
        assert!(body.is_empty(), "不支持 thinking 的 provider 不得写入任何键");
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
    fn dual_field_chunk_emits_reasoning_before_content() {
        // 同一 SSE chunk 的 delta 同时携带 reasoning_content（思考尾部）与
        // content（正文开头）时，必须按模型输出意图先 reasoning 后 content。
        // 反向（先 content）会把正文插到思考链中间——journal server_ts 同毫秒
        // 拆分的两条 round_delta 即该场景的证据（BUG：思考链与正文错排）。
        let mut delta = serde_json::Map::new();
        delta.insert(
            "reasoning_content".into(),
            serde_json::Value::String("engineer assistant.".into()),
        );
        delta.insert(
            "content".into(),
            serde_json::Value::String("你好！我是 Dee".into()),
        );
        let delta = serde_json::Value::Object(delta);

        let mut text_buf = String::new();
        let mut reasoning_buf = String::new();
        let mut tool_acc = HashMap::new();
        let mut dsml_buf = String::new();
        let mut dsml_seen = HashSet::new();
        let mut inline_thinking = false;
        let mut events = Vec::new();
        let mut traced = |ev: StreamEvent| events.push(ev);

        emit_delta_fields(
            &delta,
            &mut text_buf,
            &mut reasoning_buf,
            &mut tool_acc,
            &mut dsml_buf,
            &mut dsml_seen,
            &mut inline_thinking,
            &mut traced,
        );

        assert_eq!(
            events
                .iter()
                .map(|ev| match ev {
                    StreamEvent::ReasoningDelta(_) => "reasoning",
                    StreamEvent::ContentDelta(_) => "content",
                    _ => "other",
                })
                .collect::<Vec<_>>(),
            vec!["reasoning", "content"],
            "dual-field chunk must emit reasoning before content"
        );
        assert_eq!(reasoning_buf, "engineer assistant.");
        assert_eq!(text_buf, "你好！我是 Dee");
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
        assert!(error.contains("skill_context_sync_unsupported"));
    }

    /// T-7-1 / BUG-2026-09-13-13：上游掐流时可能只送到 tool_call 的
    /// `id`+`name`、`arguments` 增量未到（`args_json == ""`），Done 组装必须把
    /// `input` 收敛成 `{}` 而不是 `null`——否则持久化前清洗（`is_hanging_tool_use`
    /// 只按 id/name 判悬挂）会放行，出站序列化写出 `"arguments": "null"`
    /// 被部分端点判 400。与 anthropic 侧（`message_api.rs`）同语义。
    #[test]
    fn chat_tool_use_null_input_becomes_empty_object() {
        let provider = provider();
        let mut text_buf = String::new();
        let mut reasoning_buf = String::new();
        let mut tool_acc = HashMap::new();
        let mut dsml_buf = String::new();
        let mut dsml_seen = HashSet::new();
        let mut usage_info = None;
        let mut stop_reason = None;
        let mut inline_thinking = false;
        let mut events = Vec::new();
        let mut traced = |ev: StreamEvent| events.push(ev);

        // id+name 已到、arguments 增量未到：tool_acc 里 args 仍是空串。
        let frame = serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_cut",
                        "type": "function",
                        "function": { "name": "read", "arguments": "" }
                    }]
                }
            }]
        })
        .to_string();
        if let Err(error) = handle_chat_frame(
            &frame,
            &provider,
            &mut text_buf,
            &mut reasoning_buf,
            &mut tool_acc,
            &mut dsml_buf,
            &mut dsml_seen,
            &mut usage_info,
            &mut stop_reason,
            &mut inline_thinking,
            &mut traced,
        ) {
            panic!("chat frame failed: {error}");
        }

        let msg = assemble_streamed_message(reasoning_buf, text_buf, tool_acc);
        let (id, name, input) = msg
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::ToolUse { id, name, input } => Some((id, name, input)),
                _ => None,
            })
            .expect("tool_use block assembled from the accumulated call");
        assert_eq!(id, "call_cut");
        assert_eq!(name, "read");
        assert!(!input.is_null(), "input must not stay null: {input:?}");
        assert_eq!(input, &serde_json::json!({}));

        // 显式 `null` 参数（部分端点会这么发）同样收敛为空对象。
        let explicit_null = assemble_streamed_message(
            String::new(),
            String::new(),
            HashMap::from([(
                0usize,
                ("call_x".to_owned(), "read".to_owned(), "null".to_owned()),
            )]),
        );
        assert_eq!(
            explicit_null
                .content
                .into_iter()
                .find_map(|block| match block {
                    ContentBlock::ToolUse { input, .. } => Some(input),
                    _ => None,
                }),
            Some(serde_json::json!({}))
        );
    }
}
