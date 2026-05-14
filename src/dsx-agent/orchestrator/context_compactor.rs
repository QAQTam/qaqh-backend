//! Context compaction: build the compressed message list for each API request.
use crate::context;
use crate::agent::AgentState;
use crate::types::{AnthropicMessage, TokenBreakdown};

/// Build the compacted+prepped Anthropic-formatted context for next API request.
///
/// Returns (system_prompt_string, anthropic_messages, token_breakdown).
/// The messages are already in Anthropic API format — no further conversion needed.
pub fn prepare_and_compact(state: &mut AgentState) -> (String, Vec<AnthropicMessage>, TokenBreakdown) {
    // Repair first
    super::session_persistence::repair_messages(state);

    let known_files = std::collections::HashSet::new();
    let prompt = crate::config::system_prompt(&state.config.prompt_lang);
    let empty = String::new();

    // ContextBuilder takes &ContextAssembler, not &[Message]
    let builder = context::ContextBuilder::new(
        &state.ctx,
        prompt,
        &empty,       // semantic_block
        &empty,       // long_mem_block
        &state.turn_scores,
        &known_files,
        state.config.context_limit,
    );
    let (mut system, mut anthropic_msgs, breakdown) = builder.build();

    state.token_estimate = breakdown.total;
    state.token_breakdown = Some(breakdown);
    state.health.context_tokens = state.tokens_used();
    state.health.context_tier = crate::health::ContextTier::from_tokens(
        state.health.context_tokens, state.config.context_limit,
    );

    // Insert pitfall guide as a system-prompt suffix block
    let pitfall_block = state.pitfall_guide.render(3);
    if !pitfall_block.is_empty() {
        system.push_str("\n\n");
        system.push_str(&pitfall_block);
    }

    // Append skill bodies to system prompt
    if !state.active_skill_bodies.is_empty() {
        system.push_str("\n\n## Active Skills\n");
        for (name, body) in &state.active_skill_bodies {
            system.push_str(&format!("### {}\n{}\n---\n", name, body));
        }
    }

    (system, anthropic_msgs, breakdown)
}
