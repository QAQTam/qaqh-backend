//! Context compaction: build the compressed message list for each API request.
use crate::{context, tokenizer};
use crate::tui::app::App;
use crate::types::Message;

/// Build the compacted+prepped context for next API request.
pub fn prepare_and_compact(app: &mut App) -> Vec<Message> {
    // Repair first
    super::session_persistence::repair_messages(app);

    let known_files = std::collections::HashSet::new();
    let prompt = crate::config::system_prompt(&app.config.prompt_lang);
    let empty = String::new();
    let builder = context::ContextBuilder::new(
        &app.messages, prompt, &empty,
        &empty,
        &app.turn_scores, &known_files, app.config.context_limit,
    );
    let (mut prepared, breakdown) = builder.build();
    let total = breakdown.total;
    app.token_estimate = total;
    app.token_breakdown = Some(breakdown);

    // Pitfall guide
    let pitfall_block = app.pitfall_guide.render(3);
    if !pitfall_block.is_empty() {
        let pitfall_pos = 1.min(prepared.len());
        prepared.insert(pitfall_pos, Message::system(&pitfall_block));
    }
    app.health.context_tokens = app.tokens_used();
    app.health.context_tier = crate::health::ContextTier::from_tokens(app.health.context_tokens, app.config.context_limit);

    // Status message based on compaction level
    let pct = breakdown.episodic as f64 / app.config.context_limit as f64;
    if pct > 0.80 {
        app.status_msg = format!("CRIT: {} tok ({}%)", tokenizer::format_tokens(breakdown.episodic), (pct * 100.0) as u32);
    } else if pct > 0.65 {
        app.status_msg = format!("⚠ Compact: {} tok ({}%)", tokenizer::format_tokens(breakdown.episodic), (pct * 100.0) as u32);
    } else if pct > 0.50 {
        app.status_msg = format!("Compact: {} tok ({}%)", tokenizer::format_tokens(breakdown.episodic), (pct * 100.0) as u32);
    }

    // Inject skill bodies
    if !app.active_skill_bodies.is_empty() && !prepared.is_empty() {
        let insert_at = if prepared.len() > 1 && prepared[1].role == "system" { 2 } else { 1 };
        for (name, body) in &app.active_skill_bodies {
            let skill_msg = Message::system(&format!("## Skill: {}\n{}\n---\n", name, body));
            if insert_at <= prepared.len() {
                prepared.insert(insert_at, skill_msg);
            }
        }
    }
    prepared
}
