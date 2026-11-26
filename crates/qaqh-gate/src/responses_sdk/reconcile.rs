//! Responses API 终态对账、截断流解释与 freeform custom 调用收口。

use std::collections::{HashMap, HashSet};

use mutil_ai::{
    ChatResponse, NativeToolCall, NativeToolCompletion, NativeToolKind, NativeToolPayload, Part,
};
use qaqh_types::{ContentBlock, Message, UsageInfo};

use crate::usage;

use super::super::types::{ResponsesCompat, stored_reasoning_kind};
use super::{ModelToolPlan, adapt_native, canonical_function_name};

/// 生产路径（无 plan）的 freeform `apply_patch` 收口：DeepSeek Responses 的
/// custom 工具只认 `{"type":"custom","name":"apply_patch"}`，模型返回
/// `custom_tool_call`、`input` 为裸补丁文本。转成与 function_json 形态相同的
/// `{"input": raw}` ToolUse——执行器、审批、事件流零改动；并回存原样 provider
/// item，下一轮按 `custom_tool_call` / `custom_tool_call_output` 配对回放
/// （形状对齐 plan 路径的 `adapt_native` 双块输出）。
///
/// 返回 `None` = 不是本收口的目标调用（非 custom、非 apply_patch 或未完成），
/// 交回调用方既有逻辑处置。
pub(super) fn freeform_custom_blocks(call: &mutil_ai::NativeToolCall) -> Option<Vec<ContentBlock>> {
    if call.kind != mutil_ai::NativeToolKind::Custom || call.name != "apply_patch" {
        return None;
    }
    call.validate_completed().ok()?;
    let raw = match &call.payload {
        mutil_ai::NativeToolPayload::RawText(text) => text.clone(),
        mutil_ai::NativeToolPayload::JsonArguments(text) => text.clone(),
    };
    let mut item = call
        .provider_state
        .as_ref()
        .map(|state| state.data.clone())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    item["type"] = "custom_tool_call".into();
    item["name"] = call.name.clone().into();
    item["call_id"] = call.call_id.clone().into();
    item["status"] = "completed".into();
    if let Some(object) = item.as_object_mut() {
        object.remove("qaqh_native_call");
    }
    if let Some(id) = &call.item_id {
        item["id"] = id.clone().into();
    }
    item["input"] = raw.clone().into();
    Some(vec![
        ContentBlock::ToolUse {
            id: call.call_id.clone(),
            name: call.name.clone(),
            input: serde_json::json!({ "input": raw }),
        },
        ContentBlock::ResponseOutputItem { item },
    ])
}

/// 两个 id 是否可能互为别名（任一为空，或一方是 item id `fc_…`、另一方是真实 `call_id`）。
/// 两个不同的真实 `call_id`（均非空且都不以 `fc_` 开头）不得按参数内容误合并。
fn ids_could_be_alias(a: &str, b: &str) -> bool {
    a.is_empty() || b.is_empty() || (a.starts_with("fc_") != b.starts_with("fc_"))
}

fn reconcile_one_streamed_tool_call(
    blocks: &mut Vec<ContentBlock>,
    id: String,
    name: String,
    args: String,
) {
    if name.is_empty() {
        return;
    }
    let parsed: Option<serde_json::Value> = mutil_ai::lenient::parse(&args);
    let input = match parsed {
        // `null` / 非对象同样视为「参数不可还原」——不伪造。
        Some(v) if v.is_object() => v,
        _ => {
            let covered = if !id.is_empty() && !id.starts_with("fc_") {
                blocks.iter().any(|b| {
                    matches!(b, ContentBlock::ToolUse { id: known_id, .. } if known_id == &id)
                })
            } else {
                blocks.iter().any(|b| {
                    matches!(b, ContentBlock::ToolUse { name: known_name, .. } if known_name == &name)
                })
            };
            if covered {
                // 终态已带对应调用：流内条目是同一调用的损坏副本，丢弃。
                log::warn!(
                    "Responses 收口流条目参数损坏(name={name:?},args_head={:?})且终态已带对应调用，按重复丢弃",
                    args.chars().take(80).collect::<String>()
                );
                return;
            }
            log::warn!(
                "Responses 收口流条目参数损坏(name={name:?},args_head={:?})，按 Null 参数收口交 runtime 如实报错",
                args.chars().take(80).collect::<String>()
            );
            serde_json::Value::Null
        }
    };
    let duplicate = blocks.iter().any(|block| match block {
        ContentBlock::ToolUse {
            id: known_id,
            name: known_name,
            input: known_input,
        } => {
            if known_name != &name {
                return false;
            }
            known_id == &id || (ids_could_be_alias(known_id, &id) && known_input == &input)
        }
        _ => false,
    });
    if duplicate {
        return;
    }
    log::warn!("Responses 收口缺 function_call(id={id:?},name={name:?})，按流补齐");
    blocks.push(ContentBlock::ToolUse { id, name, input });
}

/// 收口对账：流里明明出现过 `function_call`，terminal 帧却没把它带回来。
///
/// 兼容网关上这是真实形态：`response.output_item.done` 给了完整调用，
/// `response.completed.response.output` 里却只剩 message/reasoning 条目。
/// SDK 只在终态 parts **完全为空**时才回退到自己的累计器
/// （`openai_responses.rs::complete`），所以只要同轮还吐了任何一个文本块，
/// 这条调用就既不进历史也不执行——模型留下一句「我先看看」就静默结束回合，
/// 前端什么也不会显示。这里按流里见过的事实补齐终态块。
///
/// 去重锚点：优先 `call_id`；身份缺席或 `fc_` 别名时才按 `name`+参数去重。
/// 损坏参数不得伪造为空对象；以终态为准去重，缺终态时保留 Null 供执行面拒绝。
pub(super) fn reconcile_streamed_tool_calls(
    blocks: &mut Vec<ContentBlock>,
    streamed: &HashMap<usize, (String, String, String)>,
) {
    let mut sorted: Vec<(usize, String, String, String)> = streamed
        .iter()
        .map(|(index, (id, name, args))| (*index, id.clone(), name.clone(), args.clone()))
        .collect();
    sorted.sort_by_key(|(index, _, _, _)| *index);
    for (_index, id, name, args) in sorted {
        reconcile_one_streamed_tool_call(blocks, id, name, args);
    }
}

fn adapt_or_freeform_native(
    call: &NativeToolCall,
    plan: Option<&ModelToolPlan>,
) -> anyhow::Result<Vec<ContentBlock>> {
    if plan.is_none()
        && let Some(freeform) = freeform_custom_blocks(call)
    {
        return Ok(freeform);
    }
    adapt_native(call, plan)
}

fn readmit_new_tool_uses(
    blocks: &mut Vec<ContentBlock>,
    from_index: usize,
    plan: Option<&ModelToolPlan>,
) -> anyhow::Result<()> {
    if plan.is_none() || from_index >= blocks.len() {
        return Ok(());
    }
    let tail: Vec<ContentBlock> = blocks.drain(from_index..).collect();
    for block in tail {
        if let ContentBlock::ToolUse { id, name, input } = block {
            let native = NativeToolCall {
                kind: NativeToolKind::Function,
                namespace: None,
                name,
                call_id: id,
                item_id: None,
                payload: NativeToolPayload::JsonArguments(input.to_string()),
                completion: NativeToolCompletion::Completed,
                provider_state: None,
            };
            blocks.extend(adapt_native(&native, plan)?);
        } else {
            blocks.push(block);
        }
    }
    Ok(())
}

/// Assemble terminal `ContentBlock`s from a completed Responses `ChatResponse`
/// while preserving the exact part order emitted by the model across
/// `Part::ToolCall` (`function_call`) and `Part::NativeToolCall`
/// (`custom_tool_call`), then reconcile any calls omitted from `output`.
pub(super) fn assemble_terminal_blocks(
    response: &ChatResponse,
    streamed_tools: &HashMap<usize, (String, String, String)>,
    native_calls: HashMap<usize, NativeToolCall>,
    compat: &ResponsesCompat,
    plan: Option<&ModelToolPlan>,
) -> anyhow::Result<Vec<ContentBlock>> {
    let mut mapping_compat = compat.clone();
    if plan.is_some() {
        mapping_compat.search_function_alias = None;
    }

    let mut blocks: Vec<ContentBlock> = Vec::new();
    let mut seen_native_ids: HashSet<String> = HashSet::new();

    for part in &response.message.parts {
        match part {
            Part::Text { text, .. } => {
                if !text.is_empty() {
                    blocks.push(ContentBlock::text(text));
                }
            }
            Part::Reasoning(r) => {
                let kind = stored_reasoning_kind(r.kind.clone());
                let t = r
                    .text
                    .clone()
                    .or_else(|| r.summary.clone())
                    .filter(|s| !s.is_empty());
                if let (true, Some(reasoning)) = (kind.is_displayable(), t) {
                    blocks.push(ContentBlock::Reasoning {
                        reasoning,
                        kind: Some(kind),
                    });
                }
            }
            Part::ToolCall(call) => {
                let id = call.id.clone().unwrap_or_default();
                let name = canonical_function_name(&call.name, &mapping_compat);
                if plan.is_some() {
                    let native = NativeToolCall {
                        kind: NativeToolKind::Function,
                        namespace: None,
                        name,
                        call_id: id,
                        item_id: None,
                        payload: NativeToolPayload::JsonArguments(call.arguments.to_string()),
                        completion: NativeToolCompletion::Completed,
                        provider_state: None,
                    };
                    blocks.extend(adapt_native(&native, plan)?);
                } else {
                    blocks.push(ContentBlock::ToolUse {
                        id,
                        name,
                        input: call.arguments.clone(),
                    });
                }
            }
            Part::NativeToolCall(call) => {
                if !seen_native_ids.insert(call.call_id.clone()) {
                    continue;
                }
                blocks.extend(adapt_or_freeform_native(call, plan)?);
            }
            Part::ProviderItem(item) => {
                let data = item.provider_state.data.clone();
                if data.get("type").and_then(|v| v.as_str()) == Some("web_search_call") {
                    let id = data
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let action = data
                        .get("action")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({"type": "search"}));
                    blocks.push(ContentBlock::WebSearchCall { id, action });
                }
                blocks.push(ContentBlock::ResponseOutputItem { item: data });
            }
            Part::ToolResult(result) => {
                blocks.push(ContentBlock::ToolResult {
                    tool_use_id: result
                        .call_id
                        .clone()
                        .unwrap_or_else(|| result.name.clone()),
                    result: qaqh_types::ToolResult::ok(result.content.clone()),
                });
            }
            _ => {}
        }
    }

    if let Some(output) = response
        .raw
        .get("output")
        .or_else(|| response.raw.get("response").and_then(|r| r.get("output")))
        .and_then(|o| o.as_array())
    {
        for item in output {
            let has_encrypted = item.get("encrypted_content").is_some();
            let it_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if !has_encrypted
                && matches!(it_type, "function_call" | "custom_tool_call" | "message")
            {
                continue;
            }
            let already = blocks.iter().any(|b| {
                matches!(b, ContentBlock::ResponseOutputItem { item: existing } if existing == item)
            });
            if !already {
                blocks.push(ContentBlock::ResponseOutputItem { item: item.clone() });
            }
        }
    }

    let native_ids: HashSet<&str> = response
        .message
        .parts
        .iter()
        .filter_map(|part| match part {
            Part::NativeToolCall(call) => Some(call.call_id.as_str()),
            _ => None,
        })
        .chain(native_calls.values().map(|call| call.call_id.as_str()))
        .collect();
    let ordinary_streamed: HashMap<usize, (String, String, String)> = streamed_tools
        .iter()
        .filter(|(_, (id, _, _))| !native_ids.contains(id.as_str()))
        .map(|(index, value)| (*index, value.clone()))
        .collect();
    let mut unseen_native: Vec<(usize, NativeToolCall)> = native_calls
        .into_iter()
        .filter(|(_, call)| !seen_native_ids.contains(&call.call_id))
        .collect();
    unseen_native.sort_by_key(|(index, _)| *index);

    if unseen_native.is_empty() {
        let before = blocks.len();
        reconcile_streamed_tool_calls(&mut blocks, &ordinary_streamed);
        readmit_new_tool_uses(&mut blocks, before, plan)?;
        return Ok(blocks);
    }

    enum StreamedFallback {
        Ordinary(String, String, String),
        Native(NativeToolCall),
    }
    let mut fallback: Vec<(usize, StreamedFallback)> = ordinary_streamed
        .into_iter()
        .map(|(index, (id, name, args))| (index, StreamedFallback::Ordinary(id, name, args)))
        .chain(
            unseen_native
                .into_iter()
                .map(|(index, call)| (index, StreamedFallback::Native(call))),
        )
        .collect();
    fallback.sort_by_key(|(index, _)| *index);
    for (_, entry) in fallback {
        match entry {
            StreamedFallback::Ordinary(id, name, args) => {
                let before = blocks.len();
                reconcile_one_streamed_tool_call(&mut blocks, id, name, args);
                readmit_new_tool_uses(&mut blocks, before, plan)?;
            }
            StreamedFallback::Native(call) => {
                if seen_native_ids.insert(call.call_id.clone()) {
                    blocks.extend(adapt_or_freeform_native(&call, plan)?);
                }
            }
        }
    }

    Ok(blocks)
}

/// `response.incomplete` 等价收口：SDK 把它当流错误抛出（事件原文在
/// `ProviderStream(json)`），旧契约里它是**带截断原因的正常终态**。这里把
/// 事件里的 response 对象重新取用：output items → Done 块，reason → stop。
pub(super) struct IncompleteTerminal {
    pub stop_reason: Option<String>,
    pub usage: Option<UsageInfo>,
    pub raw_message: Message,
    pub is_failed: bool,
    pub failure_message: String,
}

pub(super) fn interpret_provider_stream(
    raw: &str,
    compat: &ResponsesCompat,
) -> Option<IncompleteTerminal> {
    let event: serde_json::Value = serde_json::from_str(raw).ok()?;
    let typ = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let response = event.get("response")?;
    if typ == "response.failed" || typ == "error" {
        let message = response
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("response failed")
            .to_string();
        return Some(IncompleteTerminal {
            stop_reason: None,
            usage: None,
            raw_message: empty_assistant(),
            is_failed: true,
            failure_message: message,
        });
    }
    if typ != "response.incomplete" {
        return None;
    }
    let stop_reason = response
        .get("incomplete_details")
        .and_then(|d| d.get("reason"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let mut blocks: Vec<ContentBlock> = Vec::new();
    if let Some(output) = response.get("output").and_then(|o| o.as_array()) {
        for item in output {
            let it = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match it {
                // message/reasoning item：只作 ResponseOutputItem 回存；正文/思考
                // 文本来自 delta 缓冲（旧 emit_done 同款分工）。
                "message" | "reasoning" => {}
                "function_call" => {
                    let id = item
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let raw_name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let name = canonical_function_name(raw_name, compat);
                    let input = item
                        .get("arguments")
                        .and_then(|v| v.as_str())
                        // 0.7.1 宽松 JSON 修复：模型经 compat 网关吐出的参数
                        // 可能是 relaxed JSON（glob 值丢引号等），修复失败再落 Null。
                        .and_then(mutil_ai::lenient::parse)
                        .unwrap_or(serde_json::Value::Null);
                    if !id.is_empty() && !name.is_empty() {
                        blocks.push(ContentBlock::ToolUse { id, name, input });
                    }
                    if item.get("namespace").is_some() || item.get("encrypted_content").is_some() {
                        blocks.push(ContentBlock::ResponseOutputItem { item: item.clone() });
                    }
                    continue;
                }
                "custom_tool_call" => {
                    let id = item
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let raw_input = item.get("input").and_then(|v| v.as_str()).unwrap_or("");
                    let status = item.get("status").and_then(|v| v.as_str());
                    if !id.is_empty() && !name.is_empty() {
                        if status != Some("in_progress")
                            && status != Some("incomplete")
                            && !raw_input.is_empty()
                            && name == "apply_patch"
                        {
                            blocks.push(ContentBlock::ToolUse {
                                id,
                                name,
                                input: serde_json::json!({ "input": raw_input }),
                            });
                            blocks.push(ContentBlock::ResponseOutputItem { item: item.clone() });
                        } else {
                            blocks.push(ContentBlock::ToolUse {
                                id,
                                name,
                                input: serde_json::Value::Null,
                            });
                        }
                    }
                    continue;
                }
                "web_search_call" => {
                    let id = item
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let action = item
                        .get("action")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({"type": "search"}));
                    blocks.push(ContentBlock::WebSearchCall { id, action });
                }
                _ => {}
            }
            blocks.push(ContentBlock::ResponseOutputItem { item: item.clone() });
        }
    }
    let usage = response.get("usage").map(|u| {
        let input_tokens = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let output_tokens = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let cached_value = u
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"));
        let cached = cached_value.and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let reported = cached_value.and_then(|v| v.as_u64()).is_some();
        usage::normalize(
            usage::UsageSeed {
                prompt_tokens: input_tokens,
                completion_tokens: output_tokens,
                total_tokens: u
                    .get("total_tokens")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32),
                cache_hit_tokens: cached,
                cache_miss_tokens: if reported {
                    input_tokens.saturating_sub(cached)
                } else {
                    0
                },
                cache_reported: reported,
                reasoning_tokens: u
                    .get("output_tokens_details")
                    .and_then(|d| d.get("reasoning_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
            },
            Some(u),
        )
    });
    Some(IncompleteTerminal {
        stop_reason,
        usage,
        raw_message: Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: blocks,
        },
        is_failed: false,
        failure_message: String::new(),
    })
}

fn empty_assistant() -> Message {
    Message {
        msg_id: None,
        role: "assistant".into(),
        name: None,
        content: Vec::new(),
    }
}
