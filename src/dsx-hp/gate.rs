//! Alternation gate + sandbox pre-flight validation module.
//!
//! Before any context is sent to the API, it passes through this gate.
//! The gate validates message alternation, checks for orphans, simulates the
//! Anthropic format conversion, and either:
//!
//! - **Pass**: context is clean, proceed to API
//! - **Block**: violation found, real context is untouched
//!
//! This is the key architectural improvement over v3.x: the real message
//! state is NEVER polluted by a failed validation. We validate a candidate,
//! not the live state.

use crate::conversation::ContextAssembler;
use crate::types::{AnthropicContent, Message};

/// Context for the gate check.
pub struct GateContext<'a> {
    pub assembler: &'a ContextAssembler,
    pub has_orphan_tool_uses: bool,
}

/// Result of a gate check.
#[derive(Debug, Clone)]
pub enum GateResult {
    /// Context is clean — proceed to API.
    Pass,
    /// Block the request. Contains a user-facing reason and whether /fix can repair.
    Block {
        reason: String,
        /// Whether /fix can repair this issue.
        repairable: bool,
    },
}

impl GateResult {
    pub fn is_pass(&self) -> bool {
        matches!(self, GateResult::Pass)
    }

    pub fn is_block(&self) -> bool {
        matches!(self, GateResult::Block { .. })
    }
}

/// Run full pre-flight validation on a candidate context.
///
/// This does NOT mutate the assembler or any health state.
/// It only reads the candidate and reports findings.
pub fn check_gate(ctx: &GateContext) -> GateResult {
    // 1. Assembler self-validation (structural checks)
    if let Err(reason) = ctx.assembler.validate() {
        return GateResult::Block {
            reason: format!("Context integrity error: {}", reason),
            repairable: true,
        };
    }

    // 2. Check ready_for_api (must have complete turns, no pending tools)
    if let Err(reason) = ctx.assembler.ready_for_api() {
        return GateResult::Block {
            reason: format!("Not ready for API: {}", reason),
            repairable: false,
        };
    }

    // 3. Simulate Anthropic conversion (dry run) and validate alternation
    let (_, anthropic_msgs) = ctx.assembler.to_anthropic_messages();
    if let Err(result) = validate_anthropic_messages(&anthropic_msgs) {
        return result;
    }

    // 4. Check orphan tool_use flag from health platform
    if ctx.has_orphan_tool_uses {
        return GateResult::Block {
            reason: "Session has orphan tool_use entries — repair with /fix before sending to API"
                .into(),
            repairable: true,
        };
    }

    GateResult::Pass
}

/// Validate Anthropic-format messages for alternation correctness.
///
/// Checks for:
/// - Consecutive same-role messages (user->user or assistant->assistant)
/// - Empty message list
/// - Soft warnings for tool_use without thinking
pub fn validate_anthropic_messages(
    msgs: &[crate::types::AnthropicMessage],
) -> Result<(), GateResult> {
    if msgs.is_empty() {
        return Err(GateResult::Block {
            reason: "No messages to send to API".into(),
            repairable: false,
        });
    }

    let mut prev_role: Option<&str> = None;

    for (i, msg) in msgs.iter().enumerate() {
        let role = msg.role.as_str();

        // Check: no consecutive same-role messages
        if let Some(prev) = prev_role {
            if prev == "user" && role == "user" {
                return Err(GateResult::Block {
                    reason: format!(
                        "Consecutive user messages at position {} — this would cause API 400. Type /fix to repair.",
                        i
                    ),
                    repairable: true,
                });
            }
            if prev == "assistant" && role == "assistant" {
                return Err(GateResult::Block {
                    reason: format!(
                        "Consecutive assistant messages at position {} — this would cause API 400. Type /fix to repair.",
                        i
                    ),
                    repairable: true,
                });
            }
        }

        // Check: user messages with tool_results should not also have text
        // (Anthropic allows this but DeepSeek may not)
        if role == "user" {
            let has_tool_results = msg
                .content
                .iter()
                .any(|c| matches!(c, AnthropicContent::ToolResult { .. }));
            let has_text = msg
                .content
                .iter()
                .any(|c| matches!(c, AnthropicContent::Text { .. }));
            if has_tool_results && has_text {
                log::debug!("User message at {} has both text and tool_results", i);
            }
        }

        // Check: assistant messages with tool_use should have thinking
        // (soft warning — DeepSeek's Anthropic endpoint requires thinking blocks
        // when tool_use is present, but v4-flash sometimes omits them)
        if role == "assistant" {
            let has_tool_use = msg
                .content
                .iter()
                .any(|c| matches!(c, AnthropicContent::ToolUse { .. }));
            let has_thinking = msg
                .content
                .iter()
                .any(|c| matches!(c, AnthropicContent::Thinking { .. }));

            if has_tool_use && !has_thinking {
                log::warn!("Assistant at {} has tool_use without thinking block", i);
            }
        }

        prev_role = Some(role);
    }

    Ok(())
}

/// Quick check: is the assembler in a state where we can safely call the API?
///
/// This is a fast-path check called from the event loop before spawning an API task.
/// It only checks the last turn's completeness and pending tools.
pub fn quick_check(assembler: &ContextAssembler) -> Result<(), String> {
    if let Some(last) = assembler.last_turn() {
        if last.steps.is_empty() {
            return Err("No assistant response yet".into());
        }
        if assembler.has_pending_tools() {
            return Err(format!(
                "{} tool results missing",
                assembler.pending_tool_count()
            ));
        }
    }
    Ok(())
}

/// Validate message alternation on a raw `Message` slice.
///
/// Checks for:
/// - Orphan tool_use: assistant has tool_calls but no tool_result follows
/// - Consecutive user messages
///
/// This is the original check from the health platform that operates on
/// flat message vectors (e.g., session restore or pre-assembler data).
pub fn check_alternation(messages: &[Message]) -> Option<&'static str> {
    let mut i = 0;
    while i < messages.len() {
        if messages[i].role == "assistant" {
            let has_tc = messages[i]
                .tool_calls
                .as_ref()
                .map(|t| !t.is_empty())
                .unwrap_or(false);
            if has_tc {
                let next = i + 1;
                if next >= messages.len() || messages[next].role != "tool" {
                    return Some("orphan tool_use — no tool_result follows");
                }
            }
        }
        if messages[i].role == "user" && i > 0 {
            if messages[i - 1].role == "user" {
                return Some("consecutive user messages");
            }
        }
        i += 1;
    }
    None
}
