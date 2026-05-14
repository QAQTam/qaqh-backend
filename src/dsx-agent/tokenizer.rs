//! Token counting and estimation.

use crate::types::Message;
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

pub fn format_tokens(n: u32) -> String {
    if n >= 1_000_000 { format!("{:.1}M", n as f64 / 1_000_000.0) }
    else if n >= 1_000 { format!("{:.1}K", n as f64 / 1_000.0) }
    else { n.to_string() }
}

pub fn context_usage_ratio(used: u32, max_tokens: u32) -> f64 {
    (used as f64 / max_tokens as f64).clamp(0.0, 1.0)
}

// ── Token breakdown ──

#[derive(Debug, Default, Clone, Copy)]
pub struct TokenBreakdown {
    pub system: u32,
    pub semantic: u32,
    pub long_mem: u32,
    pub episodic: u32,
    pub total: u32,
}
