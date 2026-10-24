//! Replay-safe partial responses. Recovery never invents a user message or
//! executes a tool call whose provider response has not reached a terminal.

use qaqh_types::{ContentBlock, Message, ReasoningKind};

pub(super) fn partial_message(
    content: &str,
    reasoning: &str,
    reasoning_kind: Option<ReasoningKind>,
    tool_calls: &serde_json::Value,
    items: Vec<ContentBlock>,
) -> Result<Message, &'static str> {
    if tool_calls.as_array().is_some_and(|calls| !calls.is_empty())
        || items.iter().any(|block| {
            matches!(block,
                ContentBlock::ResponseOutputItem { item }
                    if item.get("type").and_then(serde_json::Value::as_str) == Some("function_call")
            )
        })
    {
        return Err("Stream ended before its tool calls were confirmed; no tools were executed");
    }
    if content.is_empty() && items.is_empty() {
        return Err("Stream ended without replayable assistant content");
    }
    let mut message = super::util::build_assistant_message(content, reasoning, reasoning_kind, &[]);
    message.content.extend(items);
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_message::{ContextFlow, MessageStore, builtin};
    use serde_json::json;

    #[test]
    fn new_input_resets_exhausted_stream_recovery_budget() {
        let mut engine = super::super::engine_turn::TurnEngine::new();
        engine.continuation_count = 3;
        engine.begin_input("t1", "input1").unwrap();
        assert_eq!(engine.continuation_count, 0);
    }

    #[test]
    fn partial_answer_survives_context_rebuild_without_synthetic_user_input() {
        let mut store = MessageStore::new_ephemeral("partial-context");
        let mut flow = ContextFlow::new();
        flow.register(builtin::model_source());
        store.push_user("Explain the result");
        for text in ["First part. ", "Second part. "] {
            let message = partial_message(text, "", None, &json!(null), vec![]).unwrap();
            assert!(flow.ingest(&mut store, builtin::MODEL, message).stored);
        }
        let messages = store.build_context_for_gate(&[]);
        assert_eq!(messages.iter().filter(|m| m.role == "user").count(), 1);
        let text: String = messages
            .iter()
            .filter(|m| m.role == "assistant")
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "First part. Second part. ");
        assert_eq!(messages.last().unwrap().role, "assistant");
    }

    #[test]
    fn partial_response_never_replays_unconfirmed_tools() {
        assert!(partial_message("Checking", "", None, &json!([{"id":"call_1"}]), vec![]).is_err());
        let item = ContentBlock::ResponseOutputItem {
            item: json!({"type":"function_call"}),
        };
        assert!(partial_message("Checking", "", None, &json!(null), vec![item]).is_err());
    }

    #[test]
    fn reasoning_summary_alone_cannot_resume_provider_state() {
        assert!(
            partial_message(
                "",
                "summary is not encrypted state",
                Some(ReasoningKind::Summary),
                &json!(null),
                vec![]
            )
            .is_err()
        );
    }

    #[test]
    fn complete_native_reasoning_state_is_retained_verbatim() {
        let item = ContentBlock::ResponseOutputItem {
            item: json!({"type":"reasoning", "encrypted_content":"opaque"}),
        };
        let message = partial_message("", "", None, &json!(null), vec![item.clone()]).unwrap();
        assert_eq!(message.content, vec![item]);
    }
}
