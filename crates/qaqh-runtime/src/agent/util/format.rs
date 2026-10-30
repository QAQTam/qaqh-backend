//! Display formatting and tool-call parsing helpers (split from the former
//! util monolith).

use sha2::Digest;

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
        // legacy 名 "todo" 的显示分支已随 todo 工具拆分（todo_write/todo_update/
        // todo_list）退役删除——原分支读 title/subject 字段，与现役入参结构
        // （items/ids）对不上，从未命中；现役三名走兜底 display_name 即可。
        "web_fetch" => input
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

pub(crate) fn ulid_from_text(text: &str) -> String {
    let mut hash = sha2::Sha256::digest(text.as_bytes());
    hash[0] = 0x01;
    hash[1] = 0x4a;
    let mut encoded = String::with_capacity(26);
    const CROCKFORD_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    for i in 0..26 {
        let byte = hash[i % hash.len()];
        encoded.push(CROCKFORD_ALPHABET[(byte % 32) as usize] as char);
    }
    encoded
}

pub(crate) fn canonical_call_id(wire_call_id: &str) -> qaqh_session::session_fact_v2::ToolCallId {
    qaqh_session::session_fact_v2::ToolCallId::new(format!("call_{}", ulid_from_text(wire_call_id)))
}

pub(crate) fn canonical_turn_id(wire_turn_id: &str) -> qaqh_session::session_fact_v2::TurnId {
    qaqh_session::session_fact_v2::TurnId::new(format!("turn_{}", ulid_from_text(wire_turn_id)))
}

pub(crate) fn canonical_interaction_id(
    wire_interaction_id: &str,
) -> qaqh_session::session_fact_v2::InteractionId {
    qaqh_session::session_fact_v2::InteractionId::new(format!(
        "int_{}",
        ulid_from_text(wire_interaction_id)
    ))
}

pub(crate) fn permission_id_matches(stored_wire_id: &str, incoming: &str) -> bool {
    stored_wire_id == incoming || canonical_call_id(stored_wire_id).as_str() == incoming
}

pub(crate) fn interaction_id_matches(stored_wire_id: &str, incoming: &str) -> bool {
    stored_wire_id == incoming || canonical_interaction_id(stored_wire_id).as_str() == incoming
}

// (projection family moved to qaqh-runtime::ringing::projection — PR-1-4)

/// Emitter-trait version of emit_round_complete for the new Loop architecture.
pub(crate) fn emit_round_complete_via_emitter(
    emitter: &dyn crate::agent::types::Emitter,
    turn_id: &str,
    round_num: u32,
    assistant_msg: &qaqh_types::Message,
    _content: &str,
    _reasoning: &str,
    _parsed: &[qaqh_types::ToolCall],
) {
    use qaqh_types::ContentBlock;
    let mut blocks = Vec::new();
    let mut tool_calls = Vec::new();
    for cb in &assistant_msg.content {
        match cb {
            ContentBlock::Reasoning { reasoning, kind } if !reasoning.is_empty() => {
                blocks.push(qaqh_domain::RoundBlock::Reasoning {
                    content: reasoning.clone(),
                    kind: *kind,
                });
            }
            ContentBlock::Text { text } if !text.is_empty() => {
                blocks.push(qaqh_domain::RoundBlock::Text {
                    content: text.clone(),
                });
            }
            ContentBlock::ToolUse { id, name, input } => {
                let display = format_tool_args_display(name, input);
                tool_calls.push(qaqh_domain::ToolCallDef {
                    id: id.clone(),
                    name: name.clone(),
                    args_display: display.clone(),
                    args_json: input.to_string(),
                });
                blocks.push(qaqh_domain::RoundBlock::Tool {
                    card: qaqh_domain::ToolCallDef {
                        id: id.clone(),
                        name: name.clone(),
                        args_display: display,
                        args_json: input.to_string(),
                    },
                });
            }
            ContentBlock::WebSearchCall { action, .. } => {
                blocks.push(qaqh_domain::RoundBlock::WebSearch {
                    action: action.to_string(),
                });
            }
            _ => {}
        }
    }
    // Ringing 双发：RoundCompleted（该 round 的权威全量终态，携带全量
    // thinking/answer）。daemon 收到后折叠该 round 的 RoundDelta；否则每个
    // token 增量都会永久累积在 conversation journal 中（磁盘与内存无界
    // 增长——曾实测 3 个 turn 积累 36k 条 delta / 18.5MB）。
    emitter.emit_domain(qaqh_domain::DomainEvent::Conversation(
        qaqh_domain::ConversationEvent::RoundCompleted {
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
            output_ref: None,
            is_final: tool_calls.is_empty(),
        },
    ));
}
