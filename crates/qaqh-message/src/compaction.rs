//! Pure helpers for selecting and formatting compaction input.

/// Why a message sequence cannot produce a compaction head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadSelectionError {
    EmptyHistory,
    NothingEligible,
}

/// Split a model context into the compactable head and the retained tail.
/// Returned references exclude system messages and preserve their original order.
pub fn select_head(
    messages: &[qaqh_types::Message],
    keep_tokens: usize,
) -> Result<(Vec<&qaqh_types::Message>, usize), HeadSelectionError> {
    let messages: Vec<_> = messages
        .iter()
        .filter(|message| message.role != "system")
        .collect();
    if messages.is_empty() {
        return Err(HeadSelectionError::EmptyHistory);
    }
    let mut kept_idx = messages.len();
    let mut kept_tokens = 0usize;
    for (index, message) in messages.iter().enumerate().rev() {
        let tokens = estimate_message_tokens(message);
        if kept_tokens.saturating_add(tokens) > keep_tokens {
            kept_idx = index + 1;
            break;
        }
        kept_tokens += tokens;
        kept_idx = index;
    }
    // If a single oversized tail message exhausted the budget, retain its
    // entire user turn instead of compacting every turn.
    if kept_idx == messages.len() {
        kept_idx = messages
            .iter()
            .rposition(|message| message.role == "user")
            .ok_or(HeadSelectionError::NothingEligible)?;
    }
    if kept_idx == 0 || compactable_head_user_count(&messages[..kept_idx]) == 0 {
        return Err(HeadSelectionError::NothingEligible);
    }
    Ok((messages, kept_idx))
}

/// Apply the same fixed image charge as gate pre-flight accounting.
pub fn estimate_message_tokens(message: &qaqh_types::Message) -> usize {
    use qaqh_types::image_accounting::{image_token_charge, redact_image_payloads};

    let mut accounted = [message.clone()];
    let images = redact_image_payloads(&mut accounted);
    let serialized = serde_json::to_string(&accounted).unwrap_or_default();
    (u64::from(qaqh_types::count_tokens(&serialized)) + image_token_charge(images)) as usize
}

fn is_compact_summary_message(message: &qaqh_types::Message) -> bool {
    message.role == "user"
        && message.content.iter().any(|block| {
            matches!(
                block,
                qaqh_types::ContentBlock::Text { text } if text.starts_with("[Compacted ")
            )
        })
}

pub fn compact_history_head<'a>(
    head: &[&'a qaqh_types::Message],
    has_previous_summary: bool,
) -> Vec<&'a qaqh_types::Message> {
    if has_previous_summary {
        head.iter()
            .copied()
            .filter(|message| !is_compact_summary_message(message))
            .collect()
    } else {
        head.to_vec()
    }
}

pub fn compactable_head_user_count(head: &[&qaqh_types::Message]) -> usize {
    head.iter()
        .filter(|message| message.role == "user" && !is_compact_summary_message(message))
        .count()
}

pub fn serialize_messages(
    head: &[&qaqh_types::Message],
    kept: &[&qaqh_types::Message],
) -> Vec<String> {
    let mut out = Vec::new();
    for m in head {
        let role = &m.role;
        let lines: Vec<String> = m
            .content
            .iter()
            .filter_map(|b| match b {
                qaqh_types::ContentBlock::Text { text } => Some(format!("[{role}]: {text}")),
                qaqh_types::ContentBlock::Reasoning { .. } => None,
                qaqh_types::ContentBlock::ToolUse { name, input, .. } => {
                    let args = serde_json::to_string(input).unwrap_or_default();
                    let end = args.floor_char_boundary(args.len().min(120));
                    Some(format!(
                        "[{role} tool call]: {}({})",
                        name,
                        args.get(..end).unwrap_or(&args)
                    ))
                }
                qaqh_types::ContentBlock::WebSearchCall { action, .. } => {
                    let action_str = serde_json::to_string(action).unwrap_or_default();
                    Some(format!("[{role} web search]: {action_str}"))
                }
                qaqh_types::ContentBlock::ToolResult { result, .. } => {
                    let compact: String = result
                        .model_text()
                        .lines()
                        .take(5)
                        .map(|l| l.chars().take(200).collect::<String>())
                        .collect::<Vec<_>>()
                        .join(" | ");
                    let end = compact.floor_char_boundary(compact.len().min(600));
                    Some(format!(
                        "[Tool result]: {}",
                        compact.get(..end).unwrap_or(&compact)
                    ))
                }
                qaqh_types::ContentBlock::Image { .. }
                | qaqh_types::ContentBlock::ImageRef { .. } => {
                    Some(format!("[{role}]: [Image attached]"))
                }
                qaqh_types::ContentBlock::ResponseOutputItem { .. } => None,
            })
            .collect();
        if !lines.is_empty() {
            out.push(lines.join("\n"));
        }
    }
    for m in kept {
        if m.role == "tool"
            && let Some(qaqh_types::ContentBlock::ToolResult { result, .. }) = m.content.first()
        {
            let compact: String = result
                .model_text()
                .lines()
                .take(3)
                .map(|l| l.chars().take(200).collect::<String>())
                .collect::<Vec<_>>()
                .join(" | ");
            let end = compact.floor_char_boundary(compact.len().min(400));
            out.push(format!(
                "[Tool result (recent)]: {}",
                compact.get(..end).unwrap_or(&compact)
            ));
        }
    }
    out
}
