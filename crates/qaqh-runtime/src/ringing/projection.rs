//! 持久化消息 → UI turn 投影。
//!
//! 由 daemon 在 Agent worker 存在前消费（冷附接直接返回 canonical transcript，
//! 不依赖后续 live 事件）。


// ── Persisted-history UI projection (moved verbatim from the former msgloop crate's util, PR-1-4) ──

/// Build the same UI turn projection directly from persisted messages.
///
/// The daemon uses this before an Agent worker exists, so a cold attach can
/// return a canonical transcript without depending on a later live event.
///
/// **回放过滤（BUG-2026-09-13-13 / #13）**：中断流遗留的悬挂 `ToolUse`
/// （缺 id 或 name，见 [`qaqh_types::ContentBlock::is_hanging_tool_use`]）
/// 不投影。写侧已在持久化前清洗新历史（`MessageStore::push_assistant`），
/// 这里额外过滤是为了**既有归档**：已落盘的旧会话不会因写侧修复而自愈，
/// 必须让每次 reload 的投影面也看不见它们。
pub fn build_turns_from_messages(
    session_id: &str,
    messages: &[qaqh_types::Message],
    start: Option<usize>,
    max_count: Option<usize>,
) -> Vec<qaqh_domain::TurnData> {
    project_turns_from_messages(session_id, messages, start, max_count).1
}

/// Return both the total persisted turn count and the requested UI window.
pub fn project_turns_from_messages(
    session_id: &str,
    messages: &[qaqh_types::Message],
    start: Option<usize>,
    max_count: Option<usize>,
) -> (usize, Vec<qaqh_domain::TurnData>) {
    let (store, _) = qaqh_message::MessageStore::from_messages(session_id, messages, 0);
    let total = store.turns().len();
    (total, build_turns(store.turns(), start, max_count))
}

/// Build only the tail window used by a cold daemon Snapshot.
pub fn project_recent_turns_from_messages(
    session_id: &str,
    messages: &[qaqh_types::Message],
    max_count: usize,
) -> (usize, Vec<qaqh_domain::TurnData>) {
    let (store, _) = qaqh_message::MessageStore::from_messages(session_id, messages, 0);
    let total = store.turns().len();
    let start = total.saturating_sub(max_count);
    (
        total,
        build_turns(store.turns(), Some(start), Some(max_count)),
    )
}

fn build_turns(
    all_turns: &[qaqh_message::Turn],
    start: Option<usize>,
    max_count: Option<usize>,
) -> Vec<qaqh_domain::TurnData> {
    use qaqh_types::ContentBlock;
    let range_start = start.unwrap_or(0).min(all_turns.len());
    let range_end = match max_count {
        Some(n) => (range_start + n).min(all_turns.len()),
        None => all_turns.len(),
    };

    let mut turns = Vec::new();
    for (ti, turn) in all_turns
        .iter()
        .enumerate()
        .skip(range_start)
        .take(range_end - range_start)
    {
        let mut rounds = Vec::new();
        for (ri, step) in turn.steps.iter().enumerate() {
            let thinking = step.assistant.content.iter().find_map(|b| {
                if let ContentBlock::Reasoning { reasoning } = b {
                    Some(reasoning.clone())
                } else {
                    None
                }
            });
            let answer = step.assistant.content.iter().find_map(|b| {
                if let ContentBlock::Text { text } = b {
                    Some(text.clone())
                } else {
                    None
                }
            });
            let tcs: Vec<qaqh_domain::ToolCallDef> = step
                .assistant
                .content
                .iter()
                .filter_map(|b| {
                    if let ContentBlock::ToolUse { id, name, input } = b {
                        (!b.is_hanging_tool_use()).then(|| qaqh_domain::ToolCallDef {
                            id: id.clone(),
                            name: name.clone(),
                            args_display: name.clone(),
                            args_json: input.to_string(),
                        })
                    } else {
                        None
                    }
                })
                .collect();
            let blocks: Vec<qaqh_domain::RoundBlock> = step
                .assistant
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Reasoning { reasoning } if !reasoning.is_empty() => {
                        Some(qaqh_domain::RoundBlock::Reasoning {
                            content: reasoning.clone(),
                        })
                    }
                    ContentBlock::Text { text } if !text.is_empty() => {
                        Some(qaqh_domain::RoundBlock::Text {
                            content: text.clone(),
                        })
                    }
                    ContentBlock::ToolUse { id, name, input } => {
                        // 悬挂 ToolUse 不进回放面（见模块文档）：前端会把它渲染成
                        // 永远 running 的幽灵卡，且不可执行。
                        if b.is_hanging_tool_use() {
                            return None;
                        }
                        Some(qaqh_domain::RoundBlock::Tool {
                            card: qaqh_domain::ToolCallDef {
                                id: id.clone(),
                                name: name.clone(),
                                args_display: name.clone(),
                                args_json: input.to_string(),
                            },
                        })
                    }
                    _ => None,
                })
                .collect();
            let trs: Vec<qaqh_domain::ToolResultDef> = step
                .tool_results
                .iter()
                .flat_map(|msg| {
                    msg.content.iter().filter_map(|b| {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            result,
                        } = b
                        {
                            Some(qaqh_domain::ToolResultDef {
                                tool_call_id: tool_use_id.clone(),
                                output: result.model_text().to_string(),
                                success: result.is_success(),
                                status: Some(result.status),
                                file: None,
                                metrics: result.metrics.clone(),
                                display: result.display().cloned(),
                                error: result.error.clone(),
                            })
                        } else {
                            None
                        }
                    })
                })
                .collect();
            rounds.push(qaqh_domain::RoundData {
                round_num: ri as u32,
                is_final: ri + 1 == turn.steps.len(),
                thinking,
                answer,
                tool_calls: tcs,
                tool_results: trs,
                blocks,
            });
        }
        let user_text = turn
            .user
            .content
            .iter()
            .find_map(|b| {
                if let ContentBlock::Text { text } = b {
                    Some(text.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default();
        turns.push(qaqh_domain::TurnData {
            turn_id: format!("t{}", ti + 1),
            user_text,
            rounds,
        });
    }
    turns
}

#[cfg(test)]
mod persisted_projection_tests {
    use super::project_recent_turns_from_messages;
    use qaqh_types::{ContentBlock, Message};

    fn assistant(text: &str) -> Message {
        Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: vec![ContentBlock::text(text)],
        }
    }

    #[test]
    fn cold_snapshot_projects_the_recent_persisted_turns() {
        let messages = vec![
            Message::system("system"),
            Message::user("first"),
            assistant("answer one"),
            Message::user("second"),
            assistant("answer two"),
        ];
        let (total, turns) = project_recent_turns_from_messages("seed", &messages, 1);
        assert_eq!(total, 2);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].turn_id, "t2");
        assert_eq!(turns[0].user_text, "second");
        assert_eq!(turns[0].rounds[0].answer.as_deref(), Some("answer two"));
    }

    /// #13：中断流抢救路径落盘的悬挂 ToolUse（`id:""` / `name:""` / `input:null`）
    /// 必须被回放投影过滤——它不可执行，投影出去只会变成前端幽灵工具卡。
    #[test]
    fn hanging_tool_use_never_enters_the_projection() {
        let hanging = Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: vec![
                ContentBlock::text("partial answer"),
                ContentBlock::ToolUse {
                    id: String::new(),
                    name: String::new(),
                    input: serde_json::Value::Null,
                },
            ],
        };
        let messages = vec![Message::user("interrupted"), hanging];
        let (_, turns) = project_recent_turns_from_messages("seed", &messages, 1);
        let round = &turns[0].rounds[0];
        assert!(
            round.tool_calls.is_empty(),
            "hanging tool_use must not surface as a tool_call: {:?}",
            round.tool_calls
        );
        assert!(
            !round
                .blocks
                .iter()
                .any(|b| matches!(b, qaqh_domain::RoundBlock::Tool { .. })),
            "hanging tool_use must not surface as a tool block: {:?}",
            round.blocks
        );
        // 同一回合里的正文不受影响（只丢悬挂工具，不丢回答）。
        assert_eq!(round.answer.as_deref(), Some("partial answer"));
    }

    /// 反例对照：完整的 tool_use ↔ tool_result 对仍必须投影出来。
    #[test]
    fn well_formed_tool_use_still_projects() {
        let assistant = Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: vec![ContentBlock::ToolUse {
                id: "call-1".into(),
                name: "read".into(),
                input: serde_json::json!({ "path": "/tmp/a" }),
            }],
        };
        let messages = vec![
            Message::user("go"),
            assistant,
            Message::tool("call-1", "ok", true),
        ];
        let (_, turns) = project_recent_turns_from_messages("seed", &messages, 1);
        let round = &turns[0].rounds[0];
        assert_eq!(round.tool_calls.len(), 1);
        assert_eq!(round.tool_calls[0].name, "read");
        assert_eq!(round.tool_results.len(), 1);
    }
}
