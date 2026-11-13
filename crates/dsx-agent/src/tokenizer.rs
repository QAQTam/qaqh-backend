//! Token counting and estimation.
//!
//! Provides DeepSeek-tokenizer-backed counting with heuristic fallback.
//! The [`TokenCount`] trait from `dsx-types` is implemented here so that
//! the DeepSeek tokenizer binary embedded in this crate is used for
//! accurate counting, rather than the heuristic-only base implementation.
//!
//! Utility functions (`format_tokens`, `context_usage_ratio`, `TokenBreakdown`)
//! are re-exported from `dsx-types` for caller convenience.

use dsx_types::Message;
pub use dsx_types::token::{TokenBreakdown, format_tokens, context_usage_ratio};
use std::sync::OnceLock;
use tokenizers::Tokenizer;

// ── Lazy tokenizer ──

static DEEPSEEK_TOKENIZER: OnceLock<Option<Tokenizer>> = OnceLock::new();

fn get_tokenizer() -> Option<&'static Tokenizer> {
    DEEPSEEK_TOKENIZER.get_or_init(|| {
        let bytes = include_bytes!("tokenizer.json");
        match Tokenizer::from_bytes(bytes) {
            Ok(tok) => Some(tok),
            Err(e) => { eprintln!("[dsx] tokenizer load failed: {} — using heuristic", e); None }
        }
    }).as_ref()
}

pub fn count_tokens(text: &str) -> u32 {
    if let Some(tok) = get_tokenizer() {
        tok.encode(text.to_string(), false).map(|e| e.get_ids().len() as u32).unwrap_or_else(|_| heuristic_count(text))
    } else { heuristic_count(text) }
}

fn heuristic_count(text: &str) -> u32 {
    let cjk: usize = text.chars().filter(|c| matches!(c,
        '\u{4e00}'..='\u{9fff}' | '\u{3400}'..='\u{4dbf}' |
        '\u{3000}'..='\u{303f}' | '\u{ff00}'..='\u{ffef}' |
        '\u{3040}'..='\u{30ff}'
    )).count();
    (text.len().saturating_sub(cjk) as f64 / 3.3 + cjk as f64 / 1.67) as u32
}

// ── Message counting ──

pub fn count_message_tokens(msg: &Message) -> u32 {
    let mut t = 4u32;
    if let Some(ref c) = msg.content { t += count_tokens(c); }
    if let Some(ref r) = msg.reasoning_content { t += count_tokens(r); }
    if let Some(ref tc) = msg.tool_calls {
        for tc in tc {
            t += count_tokens(&tc.function.name);
            t += count_tokens(&tc.function.arguments);
            t += 8;
        }
    }
    if msg.tool_call_id.is_some() { t += 2; }
    t
}

pub fn estimate_messages_tokens(messages: &[Message]) -> u32 {
    messages.iter().map(count_message_tokens).sum()
}

// ── TokenCount trait impl for Message ──
//
// Uses the DeepSeek tokenizer when available, falling back to the
// heuristic in dsx-types. The orphan rule applies here, so we wrap
// Message in a newtype. However, dsx-types already provides blanket
// impls via `count_tokens` methods on Message. We just use those.
// See dsx-types::token::TokenCount for the trait definition.
