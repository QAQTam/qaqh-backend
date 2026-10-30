//! Construction of neutral assistant messages from model output.

pub fn build_assistant_message(
    content: &str,
    reasoning: &str,
    reasoning_kind: Option<qaqh_types::ReasoningKind>,
    parsed: &[qaqh_types::ToolCall],
) -> qaqh_types::Message {
    use qaqh_types::{ContentBlock, Message};
    let mut blocks = Vec::new();
    if !reasoning.is_empty() {
        blocks.push(ContentBlock::Reasoning {
            reasoning: reasoning.to_string(),
            kind: reasoning_kind,
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
