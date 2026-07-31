//! Utility functions for the message loop: calendar, token logging, tool display formatting.
//! TODO: split into `datetime.rs` (chrono_local_date, chrono_local_datetime, epoch_to_date)
//! and `format.rs` (resolve_effective_name, has_xml, format_tool_args_display).

use crate::state::agent::AgentState;
use deepx_proto;
use deepx_types;

/// Convert epoch seconds to human-readable UTC date.
pub(crate) fn epoch_to_date(epoch_secs: u64) -> String {
    use deepx_types::platform::civil_from_days;
    let total_days = (epoch_secs / 86400) as i64;
    let (y, m, d) = civil_from_days(total_days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Resolve a legacy `name`/`action` pair before policy evaluation.
pub(crate) fn resolve_effective_name(
    name: &str,
    action: &str,
    _args: &serde_json::Value,
) -> String {
    if action.is_empty() {
        name.to_string()
    } else {
        format!("{name}_{action}")
    }
}

/// Return today's date as "YYYY-MM-DD" (UTC+8).
pub(crate) fn chrono_local_date() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs() + 8 * 3600;
    let days = secs / 86400;
    let (y, m, d) = deepx_types::platform::civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Return current time as "UTC+8 YYYY-MM-DD HH:MM".
pub(crate) fn chrono_local_datetime() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs() + 8 * 3600;
    let days = secs / 86400;
    let day_secs = secs % 86400;
    let hours = day_secs / 3600;
    let minutes = (day_secs % 3600) / 60;
    let (y, m, d) = deepx_types::platform::civil_from_days(days as i64);
    format!("UTC+8 {y:04}-{m:02}-{d:02} {hours:02}:{minutes:02}")
}

/// Append per-turn token usage to `token_stats.jsonl` for dashboard aggregation.
pub(crate) fn record_token_usage(usage: &deepx_types::UsageInfo, model: &str) {
    use std::io::Write;
    let dir = deepx_types::platform::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("token_stats.jsonl");
    let today = chrono_local_date();
    let line = serde_json::json!({
        "date": today,
        "prompt_tokens": usage.prompt_tokens,
        "completion_tokens": usage.completion_tokens,
        "cache_hit": usage.prompt_cache_hit_tokens,
        "cache_miss": usage.prompt_cache_miss_tokens,
        "model": model,
    });
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{}", serde_json::to_string(&line).unwrap_or_default());
    }
}

pub(crate) fn cache_hit_pct(usage: &deepx_types::UsageInfo) -> f64 {
    let total = usage
        .prompt_cache_hit_tokens
        .saturating_add(usage.prompt_cache_miss_tokens);
    if total == 0 {
        0.0
    } else {
        f64::from(usage.prompt_cache_hit_tokens) * 100.0 / f64::from(total)
    }
}

pub(crate) fn has_xml(s: &str) -> bool {
    // Require <tool_calls> wrapper to avoid false positives from
    // examples, explanations, or markdown containing bare <invoke> tags.
    s.contains("<tool_calls>")
}

/// Extract a short human-readable display string from a tool call's arguments.
pub(crate) fn format_tool_args_display(name: &str, input: &serde_json::Value) -> String {
    let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("");
    let display_name = if action.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", name, action)
    };

    match name {
        "exec" => input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|c| c.chars().take(80).collect())
            .unwrap_or(display_name),
        "file" => {
            let path = match action {
                "search" => input.get("pattern").and_then(|v| v.as_str()),
                "move" | "copy" => input.get("dest").and_then(|v| v.as_str()),
                "diff" => input.get("path_b").and_then(|v| v.as_str()),
                _ => input.get("path").and_then(|v| v.as_str()),
            };
            path.map(|p| p.chars().take(60).collect::<String>())
                .unwrap_or(display_name)
        }
        "task" => input
            .get("subject")
            .and_then(|v| v.as_str())
            .map(|s| s.chars().take(60).collect::<String>())
            .unwrap_or(display_name),
        "web" => input
            .get("url")
            .or_else(|| input.get("query"))
            .or_else(|| input.get("name"))
            .and_then(|v| v.as_str())
            .map(|s| s.chars().take(80).collect())
            .unwrap_or(display_name),
        "process" => input
            .get("id")
            .and_then(|v| v.as_u64())
            .map(|id| id.to_string())
            .unwrap_or(display_name),
        "ask_user" => input
            .get("question")
            .and_then(|v| v.as_str())
            .map(|q| q.chars().take(60).collect())
            .unwrap_or(display_name),
        _ => display_name,
    }
}

/// Build TurnData for IPC. If `start` is provided, only turns from that
/// index (0-based) onward are built — avoids cloning full tool results
/// for hundreds of old turns when only the tail is needed (resume / load-more).
pub(crate) fn build_turns_from_context(
    agent: &AgentState,
    start: Option<usize>,
    max_count: Option<usize>,
) -> Vec<deepx_proto::TurnData> {
    build_turns(agent.msg.turns(), start, max_count)
}

/// Build the same UI turn projection directly from persisted messages.
///
/// The daemon uses this before an Agent worker exists, so a cold attach can
/// return a canonical transcript without depending on a later live event.
pub fn build_turns_from_messages(
    seed: &str,
    messages: &[deepx_types::Message],
    start: Option<usize>,
    max_count: Option<usize>,
) -> Vec<deepx_proto::TurnData> {
    project_turns_from_messages(seed, messages, start, max_count).1
}

/// Return both the total persisted turn count and the requested UI window.
pub fn project_turns_from_messages(
    seed: &str,
    messages: &[deepx_types::Message],
    start: Option<usize>,
    max_count: Option<usize>,
) -> (usize, Vec<deepx_proto::TurnData>) {
    let (store, _) = deepx_message::MessageStore::from_messages(seed, messages, 0);
    let total = store.turns().len();
    (total, build_turns(store.turns(), start, max_count))
}

/// Build only the tail window used by a cold daemon Snapshot.
pub fn project_recent_turns_from_messages(
    seed: &str,
    messages: &[deepx_types::Message],
    max_count: usize,
) -> (usize, Vec<deepx_proto::TurnData>) {
    let (store, _) = deepx_message::MessageStore::from_messages(seed, messages, 0);
    let total = store.turns().len();
    let start = total.saturating_sub(max_count);
    (
        total,
        build_turns(store.turns(), Some(start), Some(max_count)),
    )
}

fn build_turns(
    all_turns: &[deepx_message::Turn],
    start: Option<usize>,
    max_count: Option<usize>,
) -> Vec<deepx_proto::TurnData> {
    use deepx_types::ContentBlock;
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
            let tcs: Vec<deepx_proto::ToolCallDef> = step
                .assistant
                .content
                .iter()
                .filter_map(|b| {
                    if let ContentBlock::ToolUse { id, name, input } = b {
                        Some(deepx_proto::ToolCallDef {
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
            let blocks: Vec<deepx_proto::RoundBlock> = step
                .assistant
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Reasoning { reasoning } if !reasoning.is_empty() => {
                        Some(deepx_proto::RoundBlock::Reasoning {
                            content: reasoning.clone(),
                        })
                    }
                    ContentBlock::Text { text } if !text.is_empty() => {
                        Some(deepx_proto::RoundBlock::Text {
                            content: text.clone(),
                        })
                    }
                    ContentBlock::ToolUse { id, name, input } => {
                        Some(deepx_proto::RoundBlock::Tool {
                            card: deepx_proto::ToolCallDef {
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
            let trs: Vec<deepx_proto::ToolResultDef> = step
                .tool_results
                .iter()
                .flat_map(|msg| {
                    msg.content.iter().filter_map(|b| {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            success,
                        } = b
                        {
                            Some(deepx_proto::ToolResultDef {
                                tool_call_id: tool_use_id.clone(),
                                output: content.clone(),
                                success: *success,
                                file: None,
                            })
                        } else {
                            None
                        }
                    })
                })
                .collect();
            rounds.push(deepx_proto::RoundData {
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
        turns.push(deepx_proto::TurnData {
            turn_id: format!("t{}", ti + 1),
            user_text,
            rounds,
        });
    }
    turns
}

pub(crate) fn parse_tool_calls_from_response(
    content: &str,
    _reasoning: &str,
    tool_calls_raw: &serde_json::Value,
    agent: &AgentState,
) -> Vec<deepx_types::ToolCall> {
    let mut parsed = deepx_gate::tool_parser::parse_tool_calls(tool_calls_raw);
    if parsed.is_empty() {
        let stripped = deepx_gate::tool_parser::strip_fenced_code(content);
        if deepx_gate::tool_parser::has_dsml(&stripped) {
            let (_, dsml) =
                deepx_gate::tool_parser::parse_dsml_tool_calls(&stripped, &agent.tool_defs);
            if !dsml.is_empty() {
                parsed = dsml;
            }
        }
        if parsed.is_empty() && has_xml(content) {
            let names: Vec<String> = agent
                .tool_defs
                .iter()
                .map(|t| t.function.name.clone())
                .collect();
            let stripped2 = deepx_gate::tool_parser::strip_fenced_code(content);
            let (_, xml) = deepx_gate::tool_parser::parse_xml_tool_calls(&stripped2, &names);
            if !xml.is_empty() {
                parsed = xml;
            }
        }
    }
    parsed
}

pub(crate) fn build_assistant_message(
    content: &str,
    reasoning: &str,
    parsed: &[deepx_types::ToolCall],
) -> deepx_types::Message {
    use deepx_types::{ContentBlock, Message};
    let mut blocks = Vec::new();
    if !reasoning.is_empty() {
        blocks.push(ContentBlock::Reasoning {
            reasoning: reasoning.to_string(),
        });
    }
    if !content.is_empty() {
        blocks.push(ContentBlock::Text {
            text: content.to_string(),
        });
    }
    for tc in parsed {
        let input: serde_json::Value =
            serde_json::from_str(&tc.function.arguments).unwrap_or_default();
        blocks.push(ContentBlock::ToolUse {
            id: tc.id.clone(),
            name: tc.function.name.clone(),
            input,
        });
    }
    Message {
        msg_id: None,
        role: "assistant".into(),
        name: None,
        content: blocks,
    }
}

pub(crate) fn emit_round_complete(
    event_tx: &std::sync::mpsc::SyncSender<deepx_proto::Agent2Ui>,
    turn_id: &str,
    round_num: u32,
    assistant_msg: &deepx_types::Message,
    _content: &str,
    _reasoning: &str,
    _parsed: &[deepx_types::ToolCall],
) {
    use deepx_types::ContentBlock;
    let mut blocks = Vec::new();
    let mut tool_calls = Vec::new();
    for cb in &assistant_msg.content {
        match cb {
            ContentBlock::Reasoning { reasoning } if !reasoning.is_empty() => {
                blocks.push(deepx_proto::RoundBlock::Reasoning {
                    content: reasoning.clone(),
                });
            }
            ContentBlock::Text { text } if !text.is_empty() => {
                blocks.push(deepx_proto::RoundBlock::Text {
                    content: text.clone(),
                });
            }
            ContentBlock::ToolUse { id, name, input } => {
                let display = format_tool_args_display(name, input);
                tool_calls.push(deepx_proto::ToolCallDef {
                    id: id.clone(),
                    name: name.clone(),
                    args_display: display.clone(),
                    args_json: input.to_string(),
                });
                blocks.push(deepx_proto::RoundBlock::Tool {
                    card: deepx_proto::ToolCallDef {
                        id: id.clone(),
                        name: name.clone(),
                        args_display: display,
                        args_json: input.to_string(),
                    },
                });
            }
            ContentBlock::WebSearchCall { action, .. } => {
                blocks.push(deepx_proto::RoundBlock::WebSearch {
                    action: action.to_string(),
                });
            }
            _ => {}
        }
    }
    let _ = event_tx.send(deepx_proto::Agent2Ui::RoundComplete {
        turn_id: turn_id.into(),
        round_num,
        thinking: if _reasoning.is_empty() {
            None
        } else {
            Some(_reasoning.into())
        },
        answer: if _content.is_empty() {
            None
        } else {
            Some(_content.into())
        },
        tool_calls: tool_calls.clone(),
        blocks,
        is_final: tool_calls.is_empty(),
    });
}

/// Emitter-trait version of emit_round_complete for the new Loop architecture.
pub(crate) fn emit_round_complete_via_emitter(
    emitter: &dyn crate::ring::types::Emitter,
    turn_id: &str,
    round_num: u32,
    assistant_msg: &deepx_types::Message,
    _content: &str,
    _reasoning: &str,
    _parsed: &[deepx_types::ToolCall],
) {
    use deepx_types::ContentBlock;
    let mut blocks = Vec::new();
    let mut tool_calls = Vec::new();
    for cb in &assistant_msg.content {
        match cb {
            ContentBlock::Reasoning { reasoning } if !reasoning.is_empty() => {
                blocks.push(deepx_proto::RoundBlock::Reasoning {
                    content: reasoning.clone(),
                });
            }
            ContentBlock::Text { text } if !text.is_empty() => {
                blocks.push(deepx_proto::RoundBlock::Text {
                    content: text.clone(),
                });
            }
            ContentBlock::ToolUse { id, name, input } => {
                let display = format_tool_args_display(name, input);
                tool_calls.push(deepx_proto::ToolCallDef {
                    id: id.clone(),
                    name: name.clone(),
                    args_display: display.clone(),
                    args_json: input.to_string(),
                });
                blocks.push(deepx_proto::RoundBlock::Tool {
                    card: deepx_proto::ToolCallDef {
                        id: id.clone(),
                        name: name.clone(),
                        args_display: display,
                        args_json: input.to_string(),
                    },
                });
            }
            ContentBlock::WebSearchCall { action, .. } => {
                blocks.push(deepx_proto::RoundBlock::WebSearch {
                    action: action.to_string(),
                });
            }
            _ => {}
        }
    }
    emitter.emit(deepx_proto::Agent2Ui::RoundComplete {
        turn_id: turn_id.into(),
        round_num,
        thinking: if _reasoning.is_empty() {
            None
        } else {
            Some(_reasoning.into())
        },
        answer: if _content.is_empty() {
            None
        } else {
            Some(_content.into())
        },
        tool_calls: tool_calls.clone(),
        blocks,
        is_final: tool_calls.is_empty(),
    });
}

#[cfg(test)]
mod persisted_projection_tests {
    use super::project_recent_turns_from_messages;
    use deepx_types::{ContentBlock, Message};

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
}
