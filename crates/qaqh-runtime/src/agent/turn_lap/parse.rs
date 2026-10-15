//! Parse lap 阶段：响应→assistant 入库 + RoundComplete + seal (knife-7 A2 step4)
//!
//! 从 `engine_turn.rs` 原样搬运，行为不变，仅可见性 `pub(crate)` 化。

use std::collections::HashSet;

use qaqh_types::{ContentBlock, Message, ToolCall};

use crate::agent::types::RingContext;
use crate::agent::util;

/// 一次 parse 阶段的聚合结果（knife-7 A2 step4：从 run_lap 收敛出的贯穿状态）。
///
/// `parsed`/`assistant_msg`/`turn_completed` 供后续 admit/tools 消费；
/// `active_stream_block`/`timeline_tools_open` 通过 `&mut` 原地更新，调用方直接持有。
pub(crate) struct ParseOutput {
    pub(crate) parsed: Vec<ToolCall>,
    pub(crate) assistant_msg: Message,
    pub(crate) turn_completed: bool,
}

/// 响应→assistant 入库 + RoundComplete + seal。
///
/// 覆盖原 `run_lap` 中 gate 成功后到 `match effect` 前的全部步骤（verbatim）：
/// - `util::parse_tool_calls_from_response`
/// - 为未提前 `ToolCallProgress` 的结构化工具调用补 `BlockOpened`
/// - `util::build_assistant_message` + `response_output_items` 追加
/// - `ctx.flow.ingest(MODEL, assistant_msg)` + `receipt.turn_completed` + `flush_meta`
/// - `util::emit_round_complete_via_emitter`
/// - `gate::seal_active_stream_block`
/// - `parsed.is_empty() ⇒ RoundSealed(is_final:true)`
///
/// `active_stream_block` / `timeline_tools_open` 通过 `&mut` 原地更新；
/// 返回的 `ParseOutput` 供剩余 `run_lap` 消费（tools/admit 回填）。
#[allow(clippy::too_many_arguments)] // 参数面塑形另立项（PLAN D-5）
pub(crate) fn parse_and_ingest(
    ctx: &mut RingContext<'_>,
    turn_id: &str,
    round_num: u32,
    content: &str,
    reasoning: &str,
    reasoning_kind: Option<qaqh_types::ReasoningKind>,
    tool_calls_raw: &serde_json::Value,
    response_output_items: Vec<ContentBlock>,
    active_stream_block: &mut Option<(qaqh_domain::TimelineBlockKind, String)>,
    timeline_tools_open: &mut HashSet<String>,
) -> ParseOutput {
    // ── Parse ──
    let parsed = util::parse_tool_calls_from_response(tool_calls_raw);
    // Structured/non-streamed tool calls can arrive without a prior
    // ToolCallProgress event. Open their native blocks here so every
    // later lifecycle patch has one stable target.
    for tool_call in &parsed {
        if timeline_tools_open.insert(tool_call.id.clone()) {
            let args_json = tool_call.function.arguments.clone();
            ctx.emitter
                .emit_timeline(qaqh_domain::TimelineIntent::BlockOpened {
                    turn_id: turn_id.to_string(),
                    round_num,
                    block_id: format!("tool:{}", tool_call.id),
                    kind: qaqh_domain::TimelineBlockKind::Tool,
                    reasoning_kind: None,
                    tool: Some(qaqh_domain::TimelineTool {
                        exit_code: None,
                        completed_at_ms: None,
                        tool_call_id: tool_call.id.clone(),
                        name: tool_call.function.name.clone(),
                        state: qaqh_domain::TimelineToolState::Prepared,
                        summary: None,
                        args_json: Some(args_json),
                        output: None,
                        diff: None,
                        progress: String::new(),
                        progress_truncated: false,
                        progress_stream: None,
                        progress_bytes_total: 0,
                        display: None,
                        failure: None,
                        permission: None,
                    }),
                });
        }
    }
    report_unexecutable_tool_calls(ctx, turn_id, round_num, tool_calls_raw, &parsed);

    let mut assistant_msg =
        util::build_assistant_message(content, reasoning, reasoning_kind, &parsed);
    assistant_msg.content.extend(response_output_items);
    let receipt = ctx.flow.ingest(
        &mut ctx.agent.msg,
        qaqh_message::builtin::MODEL,
        assistant_msg.clone(),
    );
    // model 源（Sink::Step）的 store 层决策透传：true 结束回合，
    // false 表示可能有工具待执行。
    let turn_completed = receipt.turn_completed;
    ctx.agent
        .msg
        .flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);

    util::emit_round_complete_via_emitter(
        ctx.emitter,
        turn_id,
        round_num,
        &assistant_msg,
        content,
        reasoning,
        &parsed,
    );

    // Native transcript sealing is independent from the legacy
    // RoundComplete projection. A Markdown consumer only sees these
    // blocks as final after the explicit seal.
    super::gate::seal_active_stream_block(ctx, turn_id, round_num, active_stream_block);
    if parsed.is_empty() {
        ctx.emitter
            .emit_timeline(qaqh_domain::TimelineIntent::RoundSealed {
                turn_id: turn_id.to_string(),
                round_num,
                is_final: true,
            });
    }

    // BUG-015 run_lap 级断言：parse 后 timeline_tools_open 必须与 parsed 去重一致，且 seal 已完成
    debug_assert!(
        timeline_tools_open.len() >= parsed.len(),
        "BUG-015: timeline_tools_open {} < parsed {}",
        timeline_tools_open.len(),
        parsed.len()
    );
    for tc in &parsed {
        debug_assert!(
            timeline_tools_open.contains(&tc.id),
            "BUG-015: parsed tool {} not in timeline_tools_open",
            tc.id
        );
    }
    if let Some((kind, block_id)) = active_stream_block.as_ref() {
        debug_assert!(
            matches!(
                kind,
                qaqh_domain::TimelineBlockKind::Text | qaqh_domain::TimelineBlockKind::Reasoning
            ),
            "active_stream_block must be text/reasoning, got {:?}",
            kind
        );
        debug_assert!(
            block_id.starts_with(&format!("round-{round_num}:")),
            "active_stream_block not round-scoped: {}",
            block_id
        );
    }

    ParseOutput {
        parsed,
        assistant_msg,
        turn_completed,
    }
}

/// 上游报了工具调用、但有条目活不过归一化时，在 timeline 上留一个**可见**的失败块。
///
/// `qaqh_gate::tool_parser::parse_tool_calls` 对缺 `id` 或缺函数名的条目是整条跳过
/// 的（畸形条目不得污染整批，也不会进历史）。问题在于跳过的后果原本只有
/// 「什么都没发生」：工具不执行、assistant 消息里没有这次调用、回合照常
/// 以 completed 收口——前端没有任何反馈，用户看到的是模型说了半句话然后静默。
/// 端点把身份放在另一种帧上（Anthropic Messages 的 `content_block_start`）时
/// 整批调用都会走到这里，一次都不该被无声吞掉。
/// 一条被归一化丢掉的工具调用：够渲染失败块、也够单测判定的最小事实。
#[derive(Debug, PartialEq, Eq)]
struct UnexecutableToolCall {
    /// timeline 块键：有 call id 用 id；身份缺席时退到回合内槽位号。
    block_key: String,
    name: String,
    arguments: String,
    missing: &'static str,
}

/// 纯判定：`tool_calls_raw` 里哪些条目没活到 `parsed`。
fn unexecutable_tool_calls(
    tool_calls_raw: &serde_json::Value,
    parsed: &[ToolCall],
    round_num: u32,
) -> Vec<UnexecutableToolCall> {
    let Some(entries) = tool_calls_raw
        .as_array()
        .filter(|entries| !entries.is_empty())
    else {
        return Vec::new();
    };
    if entries.len() == parsed.len() {
        return Vec::new();
    }
    // flat（gate 收口形态）与 nested（OpenAI wire 形态）都接住，与
    // `qaqh_gate::tool_parser::parse_tool_calls` 同源。
    let field = |entry: &serde_json::Value, key: &str| -> String {
        entry
            .get("function")
            .and_then(|f| f.get(key))
            .and_then(|v| v.as_str())
            .or_else(|| entry.get(key).and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string()
    };
    let kept: HashSet<&str> = parsed.iter().map(|call| call.id.as_str()).collect();
    entries
        .iter()
        .enumerate()
        .filter_map(|(slot, entry)| {
            let id = field(entry, "id");
            let name = field(entry, "name");
            let executable = !id.is_empty() && !name.is_empty();
            if executable || (kept.contains(id.as_str()) && !id.is_empty()) {
                return None;
            }
            let missing = match (id.is_empty(), name.is_empty()) {
                (true, true) => "调用 id 与函数名",
                (true, false) => "调用 id",
                _ => "函数名",
            };
            let block_key = if id.is_empty() {
                format!("unparsable-{round_num}-{slot}")
            } else {
                id
            };
            Some(UnexecutableToolCall {
                block_key,
                name,
                arguments: field(entry, "arguments"),
                missing,
            })
        })
        .collect()
}

fn report_unexecutable_tool_calls(
    ctx: &mut RingContext<'_>,
    turn_id: &str,
    round_num: u32,
    tool_calls_raw: &serde_json::Value,
    parsed: &[ToolCall],
) {
    for call in unexecutable_tool_calls(tool_calls_raw, parsed, round_num) {
        let UnexecutableToolCall {
            block_key,
            name,
            arguments,
            missing,
        } = call;
        let message = format!(
            "上游工具调用缺少{missing}，已丢弃未执行：{}",
            arguments.chars().take(120).collect::<String>()
        );
        log::error!(
            "[TURN] 上游工具调用无法执行 turn_id={turn_id} round_num={round_num} name={:?}: {message}",
            name
        );
        let arguments_for_block = arguments.clone();
        let display_name = if name.is_empty() {
            "<无名>".to_string()
        } else {
            name.clone()
        };
        ctx.emitter
            .emit_timeline(qaqh_domain::TimelineIntent::BlockOpened {
                turn_id: turn_id.to_string(),
                round_num,
                block_id: format!("tool:{block_key}"),
                kind: qaqh_domain::TimelineBlockKind::Tool,
                reasoning_kind: None,
                tool: Some(qaqh_domain::TimelineTool {
                    exit_code: None,
                    completed_at_ms: None,
                    tool_call_id: block_key.clone(),
                    name: display_name,
                    state: qaqh_domain::TimelineToolState::Failed,
                    summary: Some(message.clone()),
                    args_json: Some(arguments_for_block),
                    output: None,
                    diff: None,
                    progress: String::new(),
                    progress_truncated: false,
                    progress_stream: None,
                    progress_bytes_total: 0,
                    display: None,
                    failure: Some(qaqh_domain::TimelineFailure {
                        code: "tool_call_unparsable".to_string(),
                        message,
                    }),
                    permission: None,
                }),
            });
        // 同一个 key 只开一次：timeline 拒绝重块，重复帧会被静默丢掉，
        // 所以补一块就得同时认领它。
        ctx.emitter
            .emit_timeline(qaqh_domain::TimelineIntent::BlockSealed {
                turn_id: turn_id.to_string(),
                round_num,
                block_id: format!("tool:{block_key}"),
            });
    }
}

#[cfg(test)]
mod tests {
    use super::unexecutable_tool_calls;
    use qaqh_types::{FunctionCall, ToolCall};
    use serde_json::json;

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: name.to_string(),
                arguments: "{}".to_string(),
            },
        }
    }

    #[test]
    fn unnamed_tool_call_is_flagged_not_swallowed() {
        // Messages wire 丢函数名的真实形态：id 在、name 空。
        let raw = json!([{"id": "call_1", "name": "", "arguments": "{}"}]);
        let flagged = unexecutable_tool_calls(&raw, &[], 3);
        assert_eq!(flagged.len(), 1, "无名条目必须被点名: {flagged:?}");
        assert_eq!(flagged[0].block_key, "call_1");
        assert_eq!(flagged[0].missing, "函数名");
    }

    #[test]
    fn absent_identity_falls_back_to_round_scoped_block_key() {
        let raw = json!([{"id": "", "name": "exec", "arguments": "{\"command\":\"ls\"}"}]);
        let flagged = unexecutable_tool_calls(&raw, &[], 7);
        assert_eq!(flagged.len(), 1);
        assert_eq!(flagged[0].block_key, "unparsable-7-0", "回合内稳定可寻址");
        assert_eq!(flagged[0].missing, "调用 id");
        assert_eq!(flagged[0].arguments, "{\"command\":\"ls\"}");
    }

    #[test]
    fn well_formed_calls_are_not_flagged() {
        let raw = json!([
            {"id": "a", "name": "read", "arguments": "{}"},
            {"id": "b", "name": "exec", "arguments": "{}"},
        ]);
        let parsed = vec![call("a", "read"), call("b", "exec")];
        assert!(unexecutable_tool_calls(&raw, &parsed, 0).is_empty());
    }

    #[test]
    fn nested_openai_shape_is_understood() {
        let raw = json!([{"id": "c", "function": {"name": "", "arguments": "{}"}}]);
        let flagged = unexecutable_tool_calls(&raw, &[], 1);
        assert_eq!(flagged.len(), 1, "nested 形态同样不得静默: {flagged:?}");
        assert_eq!(flagged[0].block_key, "c");
    }
}
