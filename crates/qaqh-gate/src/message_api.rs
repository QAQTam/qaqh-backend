//! Anthropic Messages API streaming client — synchronous facade over reqwest.
//! Covers `POST /v1/messages` and `POST /api/anthropic/v1/messages` (ZCode).
//!
//! Key design notes (mirrors the `openAIToAnthropic` mapping):
//! - `system` is a **top-level `system` field**, not a leading `messages`
//!   entry. `proxybun` verified `glm-5.3-flash` via
//!   `https://open.bigmodel.cn/api/anthropic/v1/messages` with this shape
//!   (`200`). `qaqh-gate:chat_completions_api.rs:convert_messages` treating `system` as
//!   first message is OpenAI-specific and must not be reused here.
//! - `messages` role alternation with `user`/`assistant` only; consecutive
//!   same-role messages are merged to satisfy Anthropic's strict alternation.
//! - Tools: `{name, description, input_schema}` (Anthropic) vs
//!   `{type:function,function:{name,description,parameters}}` (OpenAI).

use futures::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use qaqh_types::{ContentBlock, Message, ToolDef, UsageInfo};

use super::sse::SseDecoder;
use super::transport::{
    Attempt, SseTrace, StatefulFilter, block_on, filter_stateful_messages, http_error_description,
    is_cancelled, is_retryable, normalize_skill_envelope, parse_retry_after, run_with_retry,
    stateful_noop_done_event, stateful_noop_sync_error,
};
use super::transport::{RetryPolicy, SSE_POLL_INTERVAL};
use super::types::{EmptyStreamEof, ProviderConfig, StreamEvent, safe_provider_error_body};

fn build_anthropic_url(base_url: &str, anthropic_path: Option<&str>) -> String {
    if let Some(path) = anthropic_path {
        if path.starts_with("http") {
            return path.to_string();
        }
        let base = base_url.trim_end_matches('/');
        return format!("{}{}", base, path);
    }
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1/messages") || base.ends_with("/api/anthropic/v1/messages") {
        base.to_string()
    } else {
        format!("{}/v1/messages", base)
    }
}

fn map_anthropic_stop_reason(s: &str) -> String {
    match s {
        "end_turn" => "stop".to_string(),
        "tool_use" => "tool_calls".to_string(),
        "max_tokens" => "length".to_string(),
        _ => s.to_string(),
    }
}

fn extract_text(content: &[ContentBlock]) -> Option<String> {
    let mut out = String::new();
    for b in content {
        if let ContentBlock::Text { text } = b {
            out.push_str(text);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Convert harness `Message`s to Anthropic `(system, messages)` pair.
///
/// `system` is top-level `system` (string, joined with "\n") per
/// The `openAIToAnthropic` mapping and the ZCode gateway
/// `open.bigmodel.cn/api/anthropic/v1/messages` spec.
fn convert_messages_to_anthropic(
    messages: Vec<Message>,
    image_index_base: usize,
) -> (Option<String>, Vec<serde_json::Value>) {
    let mut system_parts: Vec<String> = Vec::new();
    let mut raw: Vec<serde_json::Value> = Vec::new();
    let mut img_idx = image_index_base;

    for msg in messages {
        match msg.role.as_str() {
            "system" | "developer" => {
                if let Some(t) = extract_text(&msg.content) {
                    system_parts.push(t);
                }
            }
            "user" => {
                let mut content_parts: Vec<serde_json::Value> = Vec::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => {
                            if !text.is_empty() {
                                content_parts.push(serde_json::json!({"type":"text","text": text}));
                            }
                        }
                        ContentBlock::Image { mime_type, data } => {
                            // Harness image placeholder — let model call `read_image`.
                            let placeholder = format!(
                                "[Image #{}: {}, ~{} bytes — call read_image(image_index={}) to view it yourself]",
                                img_idx,
                                mime_type,
                                data.len(),
                                img_idx
                            );
                            content_parts
                                .push(serde_json::json!({"type":"text","text": placeholder}));
                            img_idx += 1;
                        }
                        ContentBlock::ImageRef {
                            mime_type,
                            bytes_len,
                            ..
                        } => {
                            // A-2 L0：外置图片仅持索引，占位符用 bytes_len 显示。
                            let placeholder = format!(
                                "[Image #{}: {}, ~{bytes_len} bytes — call read_image(image_index={}) to view it yourself]",
                                img_idx, mime_type, img_idx
                            );
                            content_parts
                                .push(serde_json::json!({"type":"text","text": placeholder}));
                            img_idx += 1;
                        }
                        ContentBlock::ToolResult { .. } => {}
                        _ => {}
                    }
                }
                if content_parts.is_empty() {
                    // BUG-2026-09-13-27：所有块都被过滤/忽略的 user 消息（空 text、
                    // 无可承载变体）不能兜底成 `{"type":"text","text":""}` ——
                    // Anthropic 及严格端点会以 400 拒绝且不可重试，整个回合 Fatal。
                    // 改为丢弃该消息：它本来就零信息量，后续轮次的正式 user 消息仍在。
                    log::warn!(
                        "anthropic: dropping user message with no convertible content block"
                    );
                    continue;
                }
                raw.push(serde_json::json!({"role":"user","content": content_parts}));
            }
            "assistant" => {
                let mut parts: Vec<serde_json::Value> = Vec::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } if !text.is_empty() => {
                            parts.push(serde_json::json!({"type":"text","text": text}));
                        }
                        ContentBlock::Reasoning { reasoning } if !reasoning.is_empty() => {
                            // Preserve reasoning as Anthropic `thinking` block for
                            // round-trip fidelity when `thinking` is enabled.
                            // Strict endpoints safely ignore unknown block types;
                            // permissive ones (ZCode) accept it. Fallback to text
                            // would also work, but `thinking` keeps stream
                            // parity with `thinking_delta` events.
                            parts
                                .push(serde_json::json!({"type":"thinking","thinking": reasoning}));
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            parts.push(serde_json::json!({"type":"tool_use","id": id, "name": name, "input": input}));
                        }
                        ContentBlock::ResponseOutputItem { .. } => {}
                        ContentBlock::WebSearchCall { .. } => {}
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    raw.push(serde_json::json!({"role":"assistant","content": parts}));
                }
            }
            "tool" => {
                // Each harness `tool` message → Anthropic `user` with `tool_result` block(s).
                //
                // 去重：工具图（read_image）在存储层是「内联 result.images + 兄弟
                // ImageRef」双份——qaqh-message/src/store.rs:874 外置落盘后没有剥离
                // 原字节。二者是同一份字节，投影时必须只发一份，否则同一张图在请求
                // 里出现两次（线上实测：16 张工具图 → 32 个 image 块）。
                let ref_count = msg
                    .content
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::ImageRef { .. }))
                    .count();
                let mut skip_inline = ref_count;
                let mut tr_parts: Vec<serde_json::Value> = Vec::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            result,
                        } => {
                            let content_str = result.render_xml_envelope();
                            let is_error = !result.is_success();
                            tr_parts.push(serde_json::json!({
                                "type": "tool_result",
                                "tool_use_id": tool_use_id,
                                "content": content_str,
                                "is_error": is_error
                            }));
                            for img in &result.images {
                                if skip_inline > 0 {
                                    // 已有等价的外置引用，跳过内联副本
                                    skip_inline -= 1;
                                    continue;
                                }
                                tr_parts.push(serde_json::json!({
                                    "type": "image",
                                    "source": {"type":"base64","media_type": img.mime_type, "data": img.data}
                                }));
                            }
                        }
                        ContentBlock::Image { mime_type, data } => {
                            if skip_inline > 0 {
                                skip_inline -= 1;
                                continue;
                            }
                            tr_parts.push(serde_json::json!({
                                "type": "image",
                                "source": {"type":"base64","media_type": mime_type, "data": data}
                            }));
                        }
                        ContentBlock::ImageRef {
                            sha256, mime_type, ..
                        } => {
                            // A-2 L0：按需读盘后走同一 base64 source 路径。
                            match qaqh_types::image_store::load_image_b64(sha256, mime_type) {
                                Ok(data) => tr_parts.push(serde_json::json!({
                                    "type": "image",
                                    "source": {"type":"base64","media_type": mime_type, "data": data}
                                })),
                                Err(e) => log::warn!(
                                    "[gate] image {sha256} load failed, dropped from anthropic request: {e}"
                                ),
                            }
                        }
                        _ => {}
                    }
                }
                if !tr_parts.is_empty() {
                    raw.push(serde_json::json!({"role":"user","content": tr_parts}));
                }
            }
            _ => {}
        }
    }

    // Merge consecutive same-role messages (Anthropic requires strict alternation).
    // Only merge tool_result carriers together or plain text together — keep a
    // preceding plain `user` ("hi") separate from subsequent `tool_result`
    // blocks so `consecutive_user_messages_are_merged` stays meaningful and
    // real tool-loop batches still coalesce.
    let mut merged: Vec<serde_json::Value> = Vec::new();
    for msg in raw {
        if let Some(last) = merged.last_mut() {
            let last_role = last.get("role").and_then(|v| v.as_str()).unwrap_or("");
            let cur_role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
            if last_role == cur_role {
                let last_is_tool = last
                    .get("content")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .any(|p| p.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                    })
                    .unwrap_or(false);
                let cur_is_tool = msg
                    .get("content")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .any(|p| p.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                    })
                    .unwrap_or(false);
                // Merge when kinds match (both tool or both non-tool), and also
                // when a plain-text `user` directly follows a tool_result carrier:
                // the carrier absorbs it so no Anthropic endpoint ever sees two
                // consecutive `user` messages. The stable partition below keeps
                // every `tool_result` ahead of the absorbed text, which is the
                // shape strict compat layers accept (see the [tr,tr,img,img] note).
                // The protected case is untouched: a plain `user` BEFORE
                // tool_results still starts its own turn (last=false, cur=true).
                if last_is_tool == cur_is_tool || (last_is_tool && !cur_is_tool) {
                    let last_content = last.get_mut("content").and_then(|v| v.as_array_mut());
                    let cur_content = msg.get("content").and_then(|v| v.as_array());
                    if let (Some(dst), Some(src)) = (last_content, cur_content) {
                        for p in src {
                            dst.push(p.clone());
                        }
                        continue;
                    }
                }
            }
        }
        merged.push(msg);
    }

    // Ensure the sequence starts with a user message: Anthropic forbids
    // leading assistant. If the first merged entry is assistant (can happen
    // when history was tool-heavy), drop it or prepend a no-op user.
    if merged
        .first()
        .and_then(|v| v.get("role"))
        .and_then(|v| v.as_str())
        == Some("assistant")
    {
        log::warn!("anthropic: dropping leading assistant block to satisfy user-first constraint");
        merged.remove(0);
    }

    // tool_result 必须**连续且位于 user 消息内容开头**：严格端点（实测 opencode zen
    // 的 anthropic 兼容层）会以 400 拒绝 [tool_result, image, tool_result, image]
    // 这种交错形态。同一 assistant 的多个 tool_result 被上面的同角色合并进
    // 同一条 user 消息后，会呈现「tr/img 交替」——这里做一次稳定分区纠正为先
    // 全部 tool_result、再其它块（图片）。实测：[tr,tr,img,img] → 200。
    for msg in merged.iter_mut() {
        let Some(blocks) = msg.get_mut("content").and_then(|v| v.as_array_mut()) else {
            continue;
        };
        let is_tr =
            |b: &serde_json::Value| b.get("type").and_then(|t| t.as_str()) == Some("tool_result");
        if !blocks.iter().any(is_tr) {
            continue;
        }
        let (trs, rest): (Vec<_>, Vec<_>) = blocks.drain(..).partition(is_tr);
        blocks.extend(trs);
        blocks.extend(rest);
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n"))
    };
    (system, merged)
}

fn convert_tools(tools: Option<Vec<ToolDef>>) -> Option<Vec<serde_json::Value>> {
    tools.map(|tds| {
        tds.into_iter()
            .map(|td| {
                serde_json::json!({
                    "name": td.function.name,
                    "description": td.function.description,
                    "input_schema": td.function.parameters,
                })
            })
            .collect()
    })
}

// ── SSE streaming state ──

struct ToolState {
    id: String,
    name: String,
    buffer: String,
}

enum FrameAction {
    Continue,
    Done,
}

#[allow(clippy::too_many_arguments)] // 参数面塑形另立项（PLAN D-5）
fn handle_anthropic_frame(
    data_str: &str,
    text_buf: &mut String,
    reasoning_buf: &mut String,
    tool_states: &mut HashMap<usize, ToolState>,
    usage_info: &mut Option<UsageInfo>,
    prompt_tokens_acc: &mut u32,
    stop_reason: &mut Option<String>,
    on_event: &mut dyn FnMut(StreamEvent),
) -> anyhow::Result<FrameAction> {
    if data_str.is_empty() {
        return Ok(FrameAction::Continue);
    }
    // Anthropic error envelope without `type` (e.g. `{type:"error",error:{...}}`)
    let ev: serde_json::Value = match serde_json::from_str(data_str) {
        Ok(v) => v,
        Err(e) => {
            log::warn!(
                "Anthropic SSE: deserialize fail: {} — data: {}",
                e,
                data_str
            );
            return Ok(FrameAction::Continue);
        }
    };
    let typ = ev.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match typ {
        "message_start" => {
            if let Some(msg) = ev.get("message")
                && let Some(usage) = msg.get("usage")
            {
                // Anthropic 语义：`input_tokens` **不含**缓存读写部分，全量输入 =
                // input + cache_read + cache_creation。只取 input_tokens 会把上下文
                // 用量报成"未命中缓存的零头"（实测本会话 481 vs 真实 413k）：前端用量条
                // ≈0%、token 校准样本被严重低估、自动压缩几乎永不触发。
                let uncached = usage
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                // Anthropic 2024 prompt caching: ZCode bun 已透传
                // cache_read_input_tokens / cache_creation_input_tokens
                // （Anthropic usage 解析惯例），
                // 开启 cache_control 后命中可达 70%+，GLM 直连/未开启时为 0。
                let cached = usage
                    .get("cache_read_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                let created = usage
                    .get("cache_creation_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                let pt = uncached.saturating_add(cached).saturating_add(created);
                *prompt_tokens_acc = pt;
                // created 不计入 hit，单算写入（miss 侧）。
                let hit = cached;
                let miss = uncached.saturating_add(created);
                let reported = cached != 0
                    || created != 0
                    || usage.get("cache_read_input_tokens").is_some()
                    || usage.get("cache_creation_input_tokens").is_some();
                if pt != 0 || reported {
                    let u = UsageInfo {
                        prompt_tokens: pt,
                        completion_tokens: 0,
                        total_tokens: pt,
                        prompt_cache_hit_tokens: hit,
                        prompt_cache_miss_tokens: miss,
                        reasoning_tokens: 0,
                        cache_usage_reported: Some(reported),
                    };
                    *usage_info = Some(u.clone());
                    on_event(StreamEvent::UsageUpdate(u));
                }
            }
            Ok(FrameAction::Continue)
        }
        "content_block_start" => {
            let idx = ev.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if let Some(block) = ev.get("content_block") {
                let bt = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if bt == "tool_use" {
                    let id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    tool_states.insert(
                        idx,
                        ToolState {
                            id: id.clone(),
                            name: name.clone(),
                            buffer: String::new(),
                        },
                    );
                    on_event(StreamEvent::ToolCallProgress {
                        index: idx,
                        id,
                        name,
                        args_so_far: String::new(),
                    });
                }
            }
            Ok(FrameAction::Continue)
        }
        "content_block_delta" => {
            let idx = ev.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if let Some(delta) = ev.get("delta") {
                let d_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match d_type {
                    "text_delta" => {
                        if let Some(t) = delta.get("text").and_then(|v| v.as_str())
                            && !t.is_empty()
                        {
                            text_buf.push_str(t);
                            on_event(StreamEvent::ContentDelta(t.to_string()));
                        }
                    }
                    "thinking_delta" => {
                        // Anthropic streams `thinking_delta.thinking`
                        if let Some(t) = delta.get("thinking").and_then(|v| v.as_str()) {
                            if !t.is_empty() {
                                reasoning_buf.push_str(t);
                                on_event(StreamEvent::ReasoningDelta(t.to_string()));
                            }
                        } else if let Some(t) = delta.get("text").and_then(|v| v.as_str()) {
                            // Some proxies (zcode-proxy anthropic→openai layer) forward as text
                            if !t.is_empty() {
                                reasoning_buf.push_str(t);
                                on_event(StreamEvent::ReasoningDelta(t.to_string()));
                            }
                        }
                    }
                    "input_json_delta" => {
                        if let Some(pj) = delta.get("partial_json").and_then(|v| v.as_str())
                            && !pj.is_empty()
                        {
                            let entry = tool_states.entry(idx).or_insert(ToolState {
                                id: String::new(),
                                name: String::new(),
                                buffer: String::new(),
                            });
                            entry.buffer.push_str(pj);
                            on_event(StreamEvent::ToolCallProgress {
                                index: idx,
                                id: entry.id.clone(),
                                name: entry.name.clone(),
                                args_so_far: entry.buffer.clone(),
                            });
                        }
                    }
                    "signature_delta" => {
                        // Anthropic thinking signature — ignore but keep flow.
                    }
                    _ => {}
                }
            }
            Ok(FrameAction::Continue)
        }
        "content_block_stop" => Ok(FrameAction::Continue),
        "message_delta" => {
            if let Some(delta) = ev.get("delta")
                && let Some(sr) = delta.get("stop_reason").and_then(|v| v.as_str())
                && !sr.is_empty()
                && sr != "null"
            {
                *stop_reason = Some(map_anthropic_stop_reason(sr));
            }
            if let Some(usage) = ev.get("usage") {
                let ot = usage
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                let pt = *prompt_tokens_acc;
                // delta 阶段也可能携带最终 cache 计费（部分上游延迟上报）
                let delta_cached = usage
                    .get("cache_read_input_tokens")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);
                let hit = delta_cached.unwrap_or_else(|| {
                    usage_info
                        .as_ref()
                        .map(|u| u.prompt_cache_hit_tokens)
                        .unwrap_or(0)
                });
                let reported = delta_cached.is_some()
                    || usage_info
                        .as_ref()
                        .and_then(|u| u.cache_usage_reported)
                        .unwrap_or(false);
                let u = UsageInfo {
                    prompt_tokens: pt,
                    completion_tokens: ot,
                    total_tokens: pt + ot,
                    prompt_cache_hit_tokens: hit,
                    prompt_cache_miss_tokens: pt.saturating_sub(hit),
                    reasoning_tokens: 0,
                    cache_usage_reported: Some(reported),
                };
                *usage_info = Some(u.clone());
                on_event(StreamEvent::UsageUpdate(u));
            }
            Ok(FrameAction::Continue)
        }
        "message_stop" => Ok(FrameAction::Done),
        "ping" => Ok(FrameAction::Continue),
        "error" => {
            let msg = ev
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .or_else(|| ev.get("message").and_then(|m| m.as_str()))
                .unwrap_or("anthropic error");
            on_event(StreamEvent::Error(msg.to_string()));
            Err(anyhow::anyhow!("anthropic error: {}", msg))
        }
        "" => {
            // Some proxies emit `data: [DONE]` style termination even for anthropic
            if data_str == "[DONE]" {
                return Ok(FrameAction::Done);
            }
            Ok(FrameAction::Continue)
        }
        _ => Ok(FrameAction::Continue),
    }
}

/// T8: anthropic 流式 SSE 主循环（外层由 `run_with_retry` 传入统一策略）。
fn stream_sse_anthropic_with_policy(
    resp: reqwest::Response,
    provider: &ProviderConfig,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut dyn FnMut(StreamEvent),
    policy: &RetryPolicy,
) -> anyhow::Result<()> {
    let mut decoder = SseDecoder::new();
    let mut stream = resp.bytes_stream();
    let mut text_buf = String::new();
    let mut reasoning_buf = String::new();
    let mut tool_states: HashMap<usize, ToolState> = HashMap::new();
    let mut usage_info: Option<UsageInfo> = None;
    let mut prompt_tokens_acc: u32 = 0;
    let mut stop_reason: Option<String> = None;
    let mut trace = SseTrace::from_env();
    let callback = on_event as *mut dyn FnMut(StreamEvent);
    // SAFETY: trace wrapper borrows callback for duration of this fn
    let mut traced = |event: StreamEvent| {
        trace.record(&event);
        unsafe { (*callback)(event) };
    };
    let mut done_reached = false;
    // 流中途抢救标记（语义同 chat 路径）：读错误/空闲超时且已有产出时置位，
    // 走下方 partial-output 组装（stop_reason=None → runtime 续写接管）。
    let mut stream_interrupted = false;
    // 空闲看门狗：最后一次收到字节的时刻（对齐 codex stream_idle_timeout）。
    let mut last_rx = std::time::Instant::now();
    loop {
        if is_cancelled(cancel) {
            return Err(anyhow::anyhow!("cancelled by user"));
        }
        while let Some(frame) = decoder.next_frame() {
            let Ok(data_str) = frame else {
                continue;
            };
            match handle_anthropic_frame(
                &data_str,
                &mut text_buf,
                &mut reasoning_buf,
                &mut tool_states,
                &mut usage_info,
                &mut prompt_tokens_acc,
                &mut stop_reason,
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
                // 分类同 chat 路径：零产出并入 EmptyStreamEof 重试；
                // 有产出且非 stateful → 收口续写；stateful → 判死。
                if reasoning_buf.is_empty() && text_buf.is_empty() && tool_states.is_empty() {
                    log::warn!("Anthropic SSE read error (no content, will retry): {e}");
                    return Err(anyhow::Error::new(EmptyStreamEof));
                }
                if !provider.stateful {
                    log::warn!("Anthropic SSE interrupted mid-stream, keeping partial output: {e}");
                    stream_interrupted = true;
                    break;
                }
                let msg = format!("SSE read error: {e}");
                traced(StreamEvent::Error(msg.clone()));
                return Err(anyhow::anyhow!("{}", msg));
            }
            Ok(None) => break,
            Err(_elapsed) => {
                if last_rx.elapsed() >= policy.idle_timeout {
                    if reasoning_buf.is_empty() && text_buf.is_empty() && tool_states.is_empty() {
                        log::warn!(
                            "Anthropic SSE idle {}s (no content, will retry)",
                            policy.idle_timeout.as_secs()
                        );
                        return Err(anyhow::Error::new(EmptyStreamEof));
                    }
                    if !provider.stateful {
                        log::warn!(
                            "Anthropic SSE idle {}s mid-stream, keeping partial output",
                            policy.idle_timeout.as_secs()
                        );
                        stream_interrupted = true;
                        break;
                    }
                    let msg = format!("SSE idle timeout after {}s", policy.idle_timeout.as_secs());
                    traced(StreamEvent::Error(msg.clone()));
                    return Err(anyhow::anyhow!("{}", msg));
                }
                continue;
            }
        };
        decoder.push(&chunk);
    }
    if !done_reached && decoder.has_pending() {
        decoder.push(b"\n\n");
        while let Some(frame) = decoder.next_frame() {
            let Ok(data_str) = frame else {
                continue;
            };
            // interrupted 时残帧是已送达的尾部数据，冲刷中的帧错误只记日志不判死。
            if let Err(e) = handle_anthropic_frame(
                &data_str,
                &mut text_buf,
                &mut reasoning_buf,
                &mut tool_states,
                &mut usage_info,
                &mut prompt_tokens_acc,
                &mut stop_reason,
                &mut traced,
            ) {
                if stream_interrupted {
                    log::warn!("Anthropic SSE: trailing frame error after interrupt ignored: {e}");
                } else {
                    return Err(e);
                }
            }
        }
    }
    if !done_reached && stop_reason.is_none() {
        if reasoning_buf.is_empty() && text_buf.is_empty() && tool_states.is_empty() {
            return Err(anyhow::Error::new(EmptyStreamEof));
        }
        log::warn!(
            "Anthropic SSE: upstream closed stream without message_stop — partial output kept"
        );
    }
    let mut blocks: Vec<ContentBlock> = Vec::new();
    if !reasoning_buf.is_empty() {
        blocks.push(ContentBlock::Reasoning {
            reasoning: reasoning_buf,
        });
    }
    if !text_buf.is_empty() {
        blocks.push(ContentBlock::text(&text_buf));
    }
    let mut sorted: Vec<(usize, String, String, String)> = tool_states
        .into_iter()
        .map(|(idx, s)| (idx, s.id, s.name, s.buffer))
        .collect();
    sorted.sort_by_key(|(idx, _, _, _)| *idx);
    for (_idx, id, name, args_json) in sorted {
        let input: serde_json::Value =
            serde_json::from_str(&args_json).unwrap_or(serde_json::Value::Null);
        // Anthropic tool `input` may be null when model bails mid-json; pass empty object
        let input = if input.is_null() {
            serde_json::json!({})
        } else {
            input
        };
        let id = if id.is_empty() {
            format!("toolu_{}", uuid_simple())
        } else {
            id
        };
        blocks.push(ContentBlock::ToolUse { id, name, input });
    }
    let raw_message = Message {
        msg_id: None,
        role: "assistant".into(),
        name: None,
        content: blocks,
    };
    traced(StreamEvent::Done {
        raw_message,
        usage: usage_info,
        stop_reason,
    });
    Ok(())
}

fn uuid_simple() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut h = DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut h);
    format!("{:016x}", h.finish())
}

// ── Public entry ──

#[allow(clippy::too_many_arguments)] // 参数面塑形另立项（PLAN D-5）
pub fn chat_stream_anthropic(
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
    let (messages, image_index_base) = if provider.stateful {
        match filter_stateful_messages(messages) {
            StatefulFilter::Incremental {
                messages,
                dropped_images,
            } => (messages, dropped_images),
            // BUG-2026-09-13-12：增量全灭 → 零 HTTP 请求 no-op 成功。
            StatefulFilter::Empty => {
                log::info!(
                    "stateful increment empty (tail assistant) — skipping anthropic request as no-op"
                );
                on_event(stateful_noop_done_event());
                return Ok(());
            }
        }
    } else {
        (messages, 0)
    };
    let (system, api_messages) = convert_messages_to_anthropic(messages, image_index_base);
    let anth_tools = convert_tools(tools);

    // max_tokens: Anthropic requires >=1 and > budget when thinking enabled.
    let mut max_toks = if max_tokens == 0 { 8192 } else { max_tokens };
    // Pre-normalize effort (QAQ always forces thinking at least `low`).
    let effort_norm = crate::types::normalize_reasoning_effort(effort.as_deref());

    let mut body_map = serde_json::Map::new();
    body_map.insert("model".into(), serde_json::json!(model));
    body_map.insert("messages".into(), serde_json::Value::Array(api_messages));
    body_map.insert("stream".into(), serde_json::json!(true));
    body_map.insert("max_tokens".into(), serde_json::json!(max_toks));
    if let Some(sys) = system
        && !sys.is_empty()
    {
        body_map.insert("system".into(), serde_json::json!(sys));
    }
    if let Some(t) = anth_tools
        && !t.is_empty()
    {
        body_map.insert("tools".into(), serde_json::Value::Array(t));
    }
    // Thinking budget: only when provider explicitly supports it.
    if provider.supports_thinking
        && let Some(e) = effort_norm.as_deref()
    {
        // 大上下文模型（EndpointSpec::thinking_budget_large，如 zcode
        // GLM-5.3 1M 窗口）放宽到 16k-96k；默认档保持 1k-16k。
        let budget: u32 = if provider.thinking_budget_large {
            match e {
                "low" => 16384,
                "medium" => 32768,
                "high" => 65536,
                "xhigh" => 81920,
                "max" => 96000,
                _ => 32768,
            }
        } else {
            match e {
                "low" => 1024,
                "medium" => 2048,
                "high" => 4096,
                "xhigh" => 8192,
                "max" => 16384,
                _ => 4096,
            }
        };
        max_toks = max_toks.max(budget + 1024);
        body_map.insert("max_tokens".into(), serde_json::json!(max_toks));
        body_map.insert(
            "thinking".into(),
            serde_json::json!({"type":"enabled","budget_tokens": budget}),
        );
    }
    // If effort is None but thinking is supported, do not force thinking;
    // let provider default (CLAUDE streams thinking only when asked).

    let body = serde_json::Value::Object(body_map);
    let url = build_anthropic_url(&provider.base_url, provider.anthropic_path.as_deref());

    // T8: 统一重试执行器（同 chat 路径）；闭包只做"一次尝试"并分类。
    let policy = RetryPolicy::from_spec(provider.retry.as_ref());
    run_with_retry(&policy, cancel, on_event, |_attempt, on_event| {
        let resp = match block_on(async {
            let mut req = provider
                .apply_opencode_headers(crate::shared_http_client().post(&url))
                .header("Content-Type", "application/json")
                .header("x-api-key", &provider.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("Authorization", format!("Bearer {}", provider.api_key));
            // 透传 harness session 到 bun 网关按会话看 1M/131K 占比
            // bun: getSessionId() 读 X-Session-Id|X-Task-Id|metadata.session_id，默认 default
            // 仅网关侧落地 usage_logs.session_id + dashboard bySession，不回传上游 z.ai
            if let Some(ref sid) = user_id
                && !sid.is_empty()
            {
                req = req.header("X-Session-Id", sid.clone());
                // 兼容旧 zcode 前缀头
                req = req.header("X-Task-Id", sid.clone());
            }
            req.json(&body).send().await
        }) {
            Ok(resp) => resp,
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
            return match stream_sse_anthropic_with_policy(resp, provider, cancel, on_event, &policy)
            {
                Ok(()) => Attempt::Ok(()),
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
        let retry_after = parse_retry_after(resp.headers(), &policy);
        let text = block_on(resp.text()).unwrap_or_default();
        let code_desc = http_error_description(status);
        if !is_retryable(status) {
            let msg = format!("Anthropic API HTTP {} ({})", status, code_desc);
            let detail = if status == 401 {
                "authentication failed".into()
            } else {
                safe_provider_error_body(&text, &provider.api_key)
            };
            on_event(StreamEvent::Error(format!("{}: {}", msg, detail)));
            return Attempt::Fatal(anyhow::anyhow!("{}", msg));
        }
        let msg = format!("Anthropic API HTTP {} ({})", status, code_desc);
        let detail = safe_provider_error_body(&text, &provider.api_key);
        Attempt::Retry {
            retry_after,
            reason: format!("HTTP {} ({})", status, code_desc),
            final_error: format!("{}: {}", msg, detail),
        }
    })
}

pub fn chat_sync_anthropic(
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
            // BUG-2026-09-13-12：sync 无流式收口，返回可重试错误而非空数组 400。
            StatefulFilter::Empty => {
                log::warn!("stateful increment empty (tail assistant) — anthropic sync skipped");
                return Err(stateful_noop_sync_error());
            }
        }
    } else {
        (messages, 0)
    };
    let (system, api_messages) = convert_messages_to_anthropic(messages, image_index_base);
    let mut max_toks = if max_tokens == 0 { 4096 } else { max_tokens };
    if provider.supports_thinking {
        // Keep a minimal budget headroom for sync thinking paths.
        max_toks = max_toks.max(4096);
    }
    let mut body = serde_json::json!({
        "model": model,
        "messages": api_messages,
        "max_tokens": max_toks,
        "stream": false,
    });
    if let Some(sys) = system
        && !sys.is_empty()
    {
        body["system"] = serde_json::json!(sys);
    }
    let url = build_anthropic_url(&provider.base_url, provider.anthropic_path.as_deref());
    // T8: sync 路径（compact/title）补轻量重试——仅传输错误 + 可重试 HTTP。
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
                .header("Content-Type", "application/json")
                .header("x-api-key", &provider.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("Authorization", format!("Bearer {}", provider.api_key))
                .json(&body)
                .send()
                .await
        }) {
            Ok(resp) => resp,
            Err(e) => {
                log::warn!("Anthropic sync attempt {attempt} transport error, will retry: {e}");
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
                log::warn!("Anthropic sync attempt {attempt} HTTP {status} retryable, will retry");
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
            return Attempt::Fatal(anyhow::anyhow!(
                "anthropic HTTP {}: {}",
                status,
                safe_provider_error_body(&text, &provider.api_key)
            ));
        }
        let json: serde_json::Value = match block_on(resp.json()) {
            Ok(j) => j,
            Err(e) => {
                return Attempt::Fatal(anyhow::anyhow!("compact parse failed: {e}"));
            }
        };
        // Anthropic non-stream: `content: [{type:"text",text:"..."}]`
        if let Some(arr) = json.get("content").and_then(|v| v.as_array()) {
            let mut out = String::new();
            for block in arr {
                if block.get("type").and_then(|v| v.as_str()) == Some("text")
                    && let Some(t) = block.get("text").and_then(|v| v.as_str())
                {
                    out.push_str(t);
                }
            }
            if !out.is_empty() {
                return Attempt::Ok(out);
            }
        }
        // Fallback: direct string field
        match json.get("content").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => Attempt::Ok(s.to_string()),
            _ => Attempt::Fatal(anyhow::anyhow!(
                "compact: no content in anthropic response: {json}"
            )),
        }
    })
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_types::ContentBlock;

    /// 双 read_image（一个 assistant 消息两个 tool_call，各带一张真图）在
    /// Anthropic 路径下的形态：图片跟 tool_result 同属一条 user 消息，且同一
    /// assistant 的全部 tool_result 会被「同角色合并」成**一条** user 消息
    /// ⇒ 不存在 chat-completions 那条「合成 user 插在 tool 消息之间」的缺陷。
    #[test]
    fn parallel_tool_result_images_merge_into_one_user_turn() {
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
        let msgs = vec![
            Message::user("look at both"),
            Message {
                msg_id: None,
                role: "assistant".into(),
                name: None,
                content: vec![
                    ContentBlock::ToolUse {
                        id: "toolu_1".into(),
                        name: "read_image".into(),
                        input: serde_json::json!({"path": "a.png"}),
                    },
                    ContentBlock::ToolUse {
                        id: "toolu_2".into(),
                        name: "read_image".into(),
                        input: serde_json::json!({"path": "b.png"}),
                    },
                ],
            },
            tool_msg("toolu_1", "Zm9v"),
            tool_msg("toolu_2", "YmFy"),
        ];
        let (_system, api) = convert_messages_to_anthropic(msgs, 0);
        let roles: Vec<&str> = api
            .iter()
            .map(|m| m["role"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(roles, vec!["user", "assistant", "user"], "{api:#?}");
        let blocks = api[2]["content"].as_array().expect("content array");
        let kinds: Vec<&str> = blocks
            .iter()
            .map(|b| b["type"].as_str().unwrap_or(""))
            .collect();
        // 实测（opencode zen anthropic 兼容层）：[tool_result, image, tool_result, image]
        // 这种交错形态 → HTTP 400；必须先全部 tool_result、再图片。
        assert_eq!(
            kinds,
            vec!["tool_result", "tool_result", "image", "image"],
            "tool_result 必须连续且在 user 消息开头：{api:#?}"
        );
        // 图片必须是真 base64 图片块，不是占位文本；且与各自 tool_result 的配对不丢。
        assert_eq!(blocks[2]["source"]["data"], "Zm9v");
        assert_eq!(blocks[3]["source"]["data"], "YmFy");
        assert_eq!(blocks[0]["tool_use_id"], "toolu_1");
        assert_eq!(blocks[1]["tool_use_id"], "toolu_2");
    }

    /// 回归 BUG-2026-09-16-03：工具图（read_image）在存储层是「内联 `result.images`
    /// 字节 + 兄弟 `ImageRef`」双份（`qaqh-message/src/store.rs:874` 外置落盘后未剥离
    /// 原字节）。投影时必须只发一份——线上实测曾把 16 张工具图发成 32 个 image 块。
    #[test]
    fn tool_image_inline_and_ref_are_projected_once() {
        let b64 = "Zm9vYmFy";
        let sha =
            qaqh_types::image_store::store_image_b64(b64, "image/png").expect("store image b64");
        let result = qaqh_types::ToolResult::ok("image attached").with_image("image/png", b64);
        let msgs = vec![
            Message::user("look"),
            Message {
                msg_id: None,
                role: "assistant".into(),
                name: None,
                content: vec![ContentBlock::ToolUse {
                    id: "toolu_1".into(),
                    name: "read_image".into(),
                    input: serde_json::json!({"path": "a.png"}),
                }],
            },
            Message {
                msg_id: None,
                role: "tool".into(),
                name: None,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "toolu_1".into(),
                        result,
                    },
                    ContentBlock::ImageRef {
                        sha256: sha,
                        mime_type: "image/png".into(),
                        bytes_len: b64.len(),
                    },
                ],
            },
        ];
        let (_system, api) = convert_messages_to_anthropic(msgs, 0);
        let blocks = api[2]["content"].as_array().expect("content array");
        let images: Vec<&serde_json::Value> =
            blocks.iter().filter(|b| b["type"] == "image").collect();
        assert_eq!(images.len(), 1, "同一张图只应投影一次：{api:#?}");
        assert_eq!(images[0]["source"]["data"], b64);
        assert_eq!(
            blocks[0]["type"], "tool_result",
            "tool_result 必须仍在前：{api:#?}"
        );
    }

    /// 回归 BUG-2026-09-16-02：Anthropic 的 `input_tokens` **不含**缓存读写部分，
    /// 全量输入 = input + cache_read + cache_creation。只取 `input_tokens` 会把
    /// 上下文用量报成零头（实测 481 vs 真实 413k）→ 前端用量条≈0%、校准样本被
    /// 低估、自动压缩几乎永不触发。
    #[test]
    fn anthropic_usage_counts_cached_input_tokens() {
        let mut usage: Option<UsageInfo> = None;
        let (mut text, mut reasoning) = (String::new(), String::new());
        let mut tools = std::collections::HashMap::new();
        let mut pt_acc = 0u32;
        let mut stop = None;
        let mut events: Vec<StreamEvent> = Vec::new();
        let frame = serde_json::json!({
            "type": "message_start",
            "message": {"usage": {
                "input_tokens": 481,
                "output_tokens": 0,
                "cache_creation_input_tokens": 50,
                "cache_read_input_tokens": 413_000
            }}
        })
        .to_string();
        handle_anthropic_frame(
            &frame,
            &mut text,
            &mut reasoning,
            &mut tools,
            &mut usage,
            &mut pt_acc,
            &mut stop,
            &mut |e| events.push(e),
        )
        .expect("frame ok");
        let u = usage.expect("usage parsed");
        assert_eq!(u.prompt_tokens, 481 + 413_000 + 50, "全量输入须含缓存读/写");
        assert_eq!(u.prompt_cache_hit_tokens, 413_000);
        assert_eq!(
            u.prompt_cache_miss_tokens,
            481 + 50,
            "miss = 未命中读取 + 缓存写入"
        );
        assert_eq!(u.total_tokens, u.prompt_tokens);
        assert_eq!(pt_acc, u.prompt_tokens, "message_delta 用的是同一累计值");
        assert!(matches!(events.first(), Some(StreamEvent::UsageUpdate(_))));
    }

    #[test]
    fn system_is_top_level_not_message() {
        let msgs = vec![Message::system("you are helpful"), Message::user("hi")];
        let (system, api) = convert_messages_to_anthropic(msgs, 0);
        assert_eq!(system.as_deref(), Some("you are helpful"));
        assert_eq!(api.len(), 1);
        assert_eq!(api[0]["role"], "user");
        // system must NOT appear in messages array
        assert!(!api.iter().any(|m| m["role"] == "system"));
    }

    #[test]
    fn multiple_system_joined_with_newline() {
        let msgs = vec![
            Message::system("base"),
            Message::system("catalog"),
            Message::user("hi"),
        ];
        let (system, _) = convert_messages_to_anthropic(msgs, 0);
        assert_eq!(system.as_deref(), Some("base\ncatalog"));
    }

    #[test]
    fn developer_also_goes_to_system() {
        let msgs = vec![Message::developer("injected skills"), Message::user("hi")];
        let (system, _) = convert_messages_to_anthropic(msgs, 0);
        assert_eq!(system.as_deref(), Some("injected skills"));
    }

    #[test]
    fn tool_result_mapped_to_user_tool_result_block() {
        let tool_msg = Message {
            msg_id: None,
            role: "tool".into(),
            name: None,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_1".into(),
                result: qaqh_types::ToolResult::ok("42"),
            }],
        };
        // need provider; but conversion doesn't need provider directly
        let (system, api) = convert_messages_to_anthropic(
            vec![
                Message::user("call tool"),
                Message {
                    msg_id: None,
                    role: "assistant".into(),
                    name: None,
                    content: vec![ContentBlock::ToolUse {
                        id: "toolu_1".into(),
                        name: "exec".into(),
                        input: serde_json::json!({"cmd":"echo 42"}),
                    }],
                },
                tool_msg,
            ],
            0,
        );
        assert_eq!(system, None);
        // messages: user, assistant, user(tool_result)
        assert_eq!(api.len(), 3);
        assert_eq!(api[2]["role"], "user");
        let content = api[2]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["tool_use_id"], "toolu_1");
    }

    #[test]
    fn consecutive_user_messages_are_merged() {
        let msgs = vec![
            Message::user("hi"),
            Message {
                msg_id: None,
                role: "tool".into(),
                name: None,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "a".into(),
                    result: qaqh_types::ToolResult::ok("1"),
                }],
            },
            Message {
                msg_id: None,
                role: "tool".into(),
                name: None,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "b".into(),
                    result: qaqh_types::ToolResult::ok("2"),
                }],
            },
        ];
        let (_, api) = convert_messages_to_anthropic(msgs, 0);
        // user, then merged tool results as single user with 2 blocks
        assert_eq!(api.len(), 2);
        assert_eq!(api[1]["content"].as_array().unwrap().len(), 2);
    }

    /// PR2 design: the workspace-change audit injection lands on a separate
    /// `user` message AFTER the tool results. This pins the real Anthropic
    /// wire shape: the merge rule coalesces same-kind messages, and a
    /// tool_result carrier absorbs a plain-text `user` that directly follows
    /// it, so the injection never becomes a second consecutive `user` turn.
    /// The carve-out PR2b must NOT break: a plain `user` that precedes the
    /// tool_result carrier stays its own turn.
    #[test]
    fn plain_user_before_tool_results_is_not_merged_into_the_carrier() {
        let msgs = vec![
            Message::user("first"),
            Message {
                msg_id: None,
                role: "assistant".into(),
                name: None,
                content: vec![ContentBlock::ToolUse {
                    id: "a".into(),
                    name: "exec".into(),
                    input: serde_json::json!({ "command": "x" }),
                }],
            },
            Message::user("steered while running"),
            Message {
                msg_id: None,
                role: "tool".into(),
                name: None,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "a".into(),
                    result: qaqh_types::ToolResult::ok("ran"),
                }],
            },
        ];
        let (_, api) = convert_messages_to_anthropic(msgs, 0);
        let roles: Vec<&str> = api
            .iter()
            .map(|m| m["role"].as_str().unwrap_or(""))
            .collect();
        let kinds: Vec<Vec<String>> = api
            .iter()
            .map(|m| {
                m["content"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .map(|b| b["type"].as_str().unwrap_or("?").to_string())
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(roles, vec!["user", "assistant", "user", "user"], "{api:#?}");
        assert_eq!(kinds[2], vec!["text"], "前置纯 user 不得被吸收：{api:#?}");
        assert_eq!(kinds[3], vec!["tool_result"], "{api:#?}");
    }

    #[test]
    fn workspace_diff_injection_is_absorbed_by_tool_result_carrier() {
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
            Message::user("run two commands"),
            Message {
                msg_id: None,
                role: "assistant".into(),
                name: None,
                content: vec![
                    ContentBlock::ToolUse {
                        id: "toolu_1".into(),
                        name: "exec".into(),
                        input: serde_json::json!({ "command": "x" }),
                    },
                    ContentBlock::ToolUse {
                        id: "toolu_2".into(),
                        name: "exec".into(),
                        input: serde_json::json!({ "command": "y" }),
                    },
                ],
            },
            tool_msg("toolu_1"),
            tool_msg("toolu_2"),
            Message {
                msg_id: None,
                role: "user".into(),
                name: Some("workspace".into()),
                content: vec![ContentBlock::text(
                    "[workspace-changes] app.py is now empty",
                )],
            },
        ];
        let (_, api) = convert_messages_to_anthropic(msgs, 0);
        let roles: Vec<&str> = api
            .iter()
            .map(|m| m["role"].as_str().unwrap_or(""))
            .collect();
        let kinds: Vec<Vec<String>> = api
            .iter()
            .map(|m| {
                m["content"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .map(|b| b["type"].as_str().unwrap_or("?").to_string())
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect();
        eprintln!("ROLES = {roles:?}");
        for (i, k) in kinds.iter().enumerate() {
            eprintln!("  [{i}] blocks = {k:?}");
        }
        // PR2b: the tool_result carrier absorbs the trailing injection, so the
        // request stays strictly alternating and the diff text still arrives
        // after every tool_result in the same turn.
        assert_eq!(roles, vec!["user", "assistant", "user"], "{api:#?}");
        assert_eq!(
            kinds[2],
            vec!["tool_result", "tool_result", "text"],
            "tool_result 必须连续且在前，吸收的文本在后：{api:#?}"
        );
        // 配对关系一字未动：两个 tool_use 仍各自对应一条 tool_result。
        let blocks = api[2]["content"].as_array().expect("content array");
        assert_eq!(blocks[0]["tool_use_id"], "toolu_1");
        assert_eq!(blocks[1]["tool_use_id"], "toolu_2");
        assert_eq!(
            blocks[2]["text"].as_str().unwrap_or(""),
            "[workspace-changes] app.py is now empty"
        );
    }

    #[test]
    fn anthropic_stream_text_delta_emits_content_delta() {
        let mut text_buf = String::new();
        let mut reasoning = String::new();
        let mut tools = HashMap::new();
        let mut usage = None;
        let mut pt = 0;
        let mut sr = None;
        let mut events = Vec::new();
        let delta = serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}});
        let s = delta.to_string();
        let action = handle_anthropic_frame(
            &s,
            &mut text_buf,
            &mut reasoning,
            &mut tools,
            &mut usage,
            &mut pt,
            &mut sr,
            &mut |e| events.push(e),
        )
        .unwrap();
        assert!(matches!(action, FrameAction::Continue));
        assert_eq!(text_buf, "Hello");
        assert!(matches!(&events[0], StreamEvent::ContentDelta(d) if d=="Hello"));
    }

    #[test]
    fn anthropic_url_builder() {
        assert_eq!(
            build_anthropic_url("https://api.anthropic.com", None),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            build_anthropic_url(
                "https://open.bigmodel.cn",
                Some("/api/anthropic/v1/messages")
            ),
            "https://open.bigmodel.cn/api/anthropic/v1/messages"
        );
        assert_eq!(
            build_anthropic_url(
                "https://open.bigmodel.cn/",
                Some("/api/anthropic/v1/messages")
            ),
            "https://open.bigmodel.cn/api/anthropic/v1/messages"
        );
    }
}
