//! Context compaction: token-aware Turn pruning + layered context assembly.
//!
//! Operates directly on `&[Turn]` (from ContextAssembler), never on flat `Vec<Message>`.
//! No round-trip flatten + split_rounds — Turn is already the natural unit.
//!
//! Output: (system_string, Vec<AnthropicMessage>, TokenBreakdown).
//! The system string includes System Prompt + Backbone + Semantic blocks + compact cards.
//! The AnthropicMessage list contains only full (non-cardified) conversation turns.

use crate::conversation::{ContextAssembler, Step, Turn};
use crate::tokenizer;
use crate::tokenizer::TokenBreakdown;
use crate::types::{AnthropicContent, AnthropicMessage};
use std::collections::HashSet;

// ── Compaction ──

/// What level of compaction was applied.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub enum CompactLevel {
    None = 0,
    L1 = 1,  // output truncation (Anthropic API: thinking blocks must be preserved)
    L2 = 2,  // old turns → structured cards
    L3 = 3,  // only last 3 turns kept full
    L4 = 4,  // only last 1 turn kept full
}

/// Compact turns directly, returning card text + Anthropic-format messages.
///
/// Thresholds (based on V4 degradation observations):
///   > 40% → L1  (always: truncate long outputs — reasoning NOT stripped)
///   > 50% → L2  (card-ify low-value turns, keep 5 full)
///   > 65% → L3  (keep only last 3 full turns + combined card)
///   > 80% → L4  (keep only last 1 full turn + combined card)
///
/// Returns (card_text, episodic_messages, token_count, level).
/// `card_text` is prepended to the system string by ContextBuilder.
/// `scores` — per-turn importance (higher = more valuable, one per Turn).
/// `known_files` — paths in SemanticMemory (card builders skip known files).
pub fn compact_turns(
    turns: &[Turn],
    context_limit: u32,
    scores: &[f32],
    known_files: &HashSet<String>,
) -> (String, Vec<AnthropicMessage>, u32, CompactLevel) {
    if turns.is_empty() {
        return (String::new(), vec![], 0, CompactLevel::None);
    }

    // Convert all turns to Anthropic format and count tokens
    let mut full_msgs = turn_to_anthropic_messages(turns);
    apply_l1_to_anthropic(&mut full_msgs);
    let total = estimate_anthropic_tokens(&full_msgs);
    let ratio = total as f64 / context_limit as f64;

    // L1 only: always applied, no card-ification needed
    if ratio <= 0.50 {
        return (String::new(), full_msgs, total, CompactLevel::L1);
    }

    // L2-L4: determine strictness
    let (keep_full, is_danger) = if ratio > 0.80 {
        (1, true)    // L4: emergency
    } else if ratio > 0.65 {
        (3, true)    // L3: danger
    } else {
        (5, false)   // L2: healthy
    };

    if turns.len() <= keep_full {
        return (String::new(), full_msgs, total, CompactLevel::L1);
    }

    let card_count = turns.len() - keep_full;
    let to_card = lowest_scored_indices(turns.len(), card_count, scores);

    // Build card text for the system string
    let card_text = if is_danger {
        // L3/L4: one combined card for all card-ified turns
        let card_turns: Vec<&Turn> = to_card.iter().map(|&i| &turns[i]).collect();
        build_combined_card_from_turns(&card_turns, known_files, is_danger)
    } else {
        // L2: per-turn cards
        let mut parts: Vec<String> = Vec::new();
        for &i in &to_card {
            parts.push(build_turn_card(&turns[i], known_files, false));
        }
        parts.join("\n")
    };

    // Keep only full (non-cardified) turns in the Anthropic output
    let keep_msgs = turn_to_anthropic_messages_filtered(turns, &to_card);
    let keep_tokens = estimate_anthropic_tokens(&keep_msgs);

    let level = if keep_full == 1 { CompactLevel::L4 }
        else if keep_full == 3 { CompactLevel::L3 }
        else { CompactLevel::L2 };

    (card_text, keep_msgs, keep_tokens, level)
}

/// Manually force compaction to a level. Returns (card_text, messages, tokens).
pub fn force_compact_turns(
    turns: &[Turn],
    level: CompactLevel,
    scores: &[f32],
    known_files: &HashSet<String>,
) -> (String, Vec<AnthropicMessage>, u32) {
    let empty = HashSet::new();
    let kf = if known_files.is_empty() { &empty } else { known_files };
    match level {
        CompactLevel::None => {
            let msgs = turn_to_anthropic_messages(turns);
            let t = estimate_anthropic_tokens(&msgs);
            (String::new(), msgs, t)
        }
        CompactLevel::L1 => {
            let mut msgs = turn_to_anthropic_messages(turns);
            apply_l1_to_anthropic(&mut msgs);
            let t = estimate_anthropic_tokens(&msgs);
            (String::new(), msgs, t)
        }
        CompactLevel::L2 => {
            if turns.len() <= 5 { return force_compact_turns(turns, CompactLevel::L1, scores, kf); }
            let to_card = lowest_scored_indices(turns.len(), turns.len() - 5, scores);
            let cards: Vec<&Turn> = to_card.iter().map(|&i| &turns[i]).collect();
            let card_text = build_combined_card_from_turns(&cards, kf, false);
            let msgs = turn_to_anthropic_messages_filtered(turns, &to_card);
            let t = estimate_anthropic_tokens(&msgs);
            (card_text, msgs, t)
        }
        CompactLevel::L3 => {
            if turns.len() <= 3 { return force_compact_turns(turns, CompactLevel::L1, scores, kf); }
            let to_card = lowest_scored_indices(turns.len(), turns.len() - 3, scores);
            let cards: Vec<&Turn> = to_card.iter().map(|&i| &turns[i]).collect();
            let card_text = build_combined_card_from_turns(&cards, kf, true);
            let msgs = turn_to_anthropic_messages_filtered(turns, &to_card);
            let t = estimate_anthropic_tokens(&msgs);
            (card_text, msgs, t)
        }
        CompactLevel::L4 => {
            if turns.len() <= 1 { return force_compact_turns(turns, CompactLevel::L1, scores, kf); }
            let to_card: HashSet<usize> = (0..turns.len() - 1).collect();
            let cards: Vec<&Turn> = to_card.iter().map(|&i| &turns[i]).collect();
            let card_text = build_combined_card_from_turns(&cards, kf, true);
            let msgs = turn_to_anthropic_messages_filtered(turns, &to_card);
            let t = estimate_anthropic_tokens(&msgs);
            (card_text, msgs, t)
        }
    }
}

// ── L1: Zero-loss cleanup on Anthropic format ──

fn apply_l1_to_anthropic(msgs: &mut [AnthropicMessage]) {
    for msg in msgs.iter_mut() {
        if msg.role != "user" { continue; }
        for block in msg.content.iter_mut() {
            if let AnthropicContent::ToolResult { content, .. } = block {
                // Truncate long tool outputs (>100 lines → keep first+last 50)
                let line_count = content.lines().count();
                if line_count > 100 {
                    let lines: Vec<&str> = content.lines().collect();
                    let head = lines[..50].join("\n");
                    let tail = lines[lines.len().saturating_sub(50)..].join("\n");
                    *content = format!(
                        "{}\n... ({} lines trimmed) ...\n{}",
                        head,
                        line_count - 100,
                        tail,
                    );
                }
                // Truncate long file reads (>15K chars → keep head 5K + tail 2K)
                if content.len() > 15000 {
                    let head_end = char_boundary(content, 5000);
                    let tail_start = char_boundary(content, content.len().saturating_sub(2000));
                    let head = content[..head_end].to_string();
                    let tail = content[tail_start..].to_string();
                    *content = format!(
                        "{}\n\n... ({} chars trimmed) ...\n\n{}",
                        head,
                        content.len() - 7000,
                        tail,
                    );
                }
            }
        }
    }
}

// ── Turn → Anthropic format conversion ──

/// Convert all turns to Anthropic API message list.
fn turn_to_anthropic_messages(turns: &[Turn]) -> Vec<AnthropicMessage> {
    let mut out = Vec::new();
    for turn in turns {
        turn_to_anthropic(turn, &mut out);
    }
    out
}

/// Convert turns, skipping those at cardified indices.
fn turn_to_anthropic_messages_filtered(turns: &[Turn], skip_indices: &HashSet<usize>) -> Vec<AnthropicMessage> {
    let mut out = Vec::new();
    for (i, turn) in turns.iter().enumerate() {
        if !skip_indices.contains(&i) {
            turn_to_anthropic(turn, &mut out);
        }
    }
    out
}

/// Append one Turn's Anthropic messages to the output vec.
fn turn_to_anthropic(turn: &Turn, out: &mut Vec<AnthropicMessage>) {
    // User message
    let user_text = turn.user.content.clone().unwrap_or_default();
    out.push(AnthropicMessage {
        role: "user".into(),
        content: vec![AnthropicContent::Text { text: user_text }],
    });

    // Each step: assistant → user(tool_results if any)
    for step in &turn.steps {
        step_to_anthropic(step, out);
    }
}

/// Append one Step's Anthropic messages to the output vec.
fn step_to_anthropic(step: &Step, out: &mut Vec<AnthropicMessage>) {
    // Assistant
    let mut blocks = Vec::new();
    if let Some(ref r) = step.assistant.reasoning_content {
        if !r.is_empty() {
            blocks.push(AnthropicContent::Thinking {
                thinking: r.clone(),
                signature: step.assistant.thinking_signature.clone()
                    .unwrap_or_else(|| "dsx-session".into()),
            });
        }
    }
    if let Some(ref text) = step.assistant.content {
        if !text.is_empty() {
            blocks.push(AnthropicContent::Text { text: text.clone() });
        }
    }
    if let Some(ref tcs) = step.assistant.tool_calls {
        for tc in tcs {
            let input = serde_json::from_str(&tc.function.arguments)
                .unwrap_or(serde_json::Value::Null);
            blocks.push(AnthropicContent::ToolUse {
                id: tc.id.clone(),
                name: tc.function.name.clone(),
                input,
            });
        }
    }
    out.push(AnthropicMessage { role: "assistant".into(), content: blocks });

    // Tool results (bundled as one user message)
    if !step.tool_results.is_empty() {
        let tr_blocks: Vec<AnthropicContent> = step.tool_results.iter()
            .map(|tr| {
                let text = tr.content.clone().unwrap_or_default();
                let is_error = text.starts_with("[ERROR]") || text.starts_with("[FAIL]");
                AnthropicContent::ToolResult {
                    tool_use_id: tr.tool_call_id.clone().unwrap_or_default(),
                    content: text,
                    is_error: if is_error { Some(true) } else { None },
                }
            })
            .collect();
        out.push(AnthropicMessage { role: "user".into(), content: tr_blocks });
    }
}

/// Rough token estimation for Anthropic-format messages.
fn estimate_anthropic_tokens(msgs: &[AnthropicMessage]) -> u32 {
    let mut t = 0u32;
    for msg in msgs {
        t += 4; // role + message overhead
        for block in &msg.content {
            match block {
                AnthropicContent::Text { text } => t += tokenizer::count_tokens(text),
                AnthropicContent::Thinking { thinking, .. } => t += tokenizer::count_tokens(thinking),
                AnthropicContent::ToolUse { name, input, .. } => {
                    t += tokenizer::count_tokens(name);
                    t += tokenizer::count_tokens(&input.to_string());
                    t += 8;
                }
                AnthropicContent::ToolResult { content, .. } => t += tokenizer::count_tokens(content),
            }
        }
    }
    t
}

// ── Turn scoring ──

/// Return indices of `count` lowest-scored turns (always keeps the last turn).
fn lowest_scored_indices(total: usize, count: usize, scores: &[f32]) -> HashSet<usize> {
    if count >= total { return (0..total).collect(); }
    let mut indices: Vec<usize> = (0..total.saturating_sub(1)).collect(); // exclude last
    indices.sort_by(|&a, &b| {
        let sa = scores.get(a).copied().unwrap_or(0.0);
        let sb = scores.get(b).copied().unwrap_or(0.0);
        sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
    });
    indices.iter().take(count.min(indices.len())).copied().collect()
}

// ── Card builders ──

/// Build a compact card for a single turn (used in L2 — healthy tier).
fn build_turn_card(turn: &Turn, known_files: &HashSet<String>, is_danger: bool) -> String {
    let files = extract_files_from_turn(turn);
    let sigs = extract_signatures_from_turn(turn);
    let errors = extract_errors_from_turn(turn);

    // If all files known and no errors, minimal card
    if !files.is_empty() && errors.is_empty() && files.iter().all(|f| known_files.contains(f.as_str())) {
        let lines: Vec<String> = files.iter()
            .map(|f| format!("F: {} — (known in SemanticMemory)", f))
            .collect();
        return lines.join("\n");
    }

    let mut lines: Vec<String> = Vec::new();
    for f in &files {
        let f_sigs: Vec<&str> = sigs.iter().filter(|s| s.contains(f.as_str())).map(|s| s.as_str()).collect();
        let sym_part = if f_sigs.is_empty() {
            String::new()
        } else {
            format!(" ({})", f_sigs.join(", "))
        };
        let known = if is_danger || known_files.contains(f.as_str()) { "" } else { " (new)" };
        lines.push(format!("F: {}{}{}", f, sym_part, known));
    }

    // Danger: skip single-occurrence errors
    if !is_danger {
        for e in &errors {
            lines.push(format!("E: {}", truncate_str(e, 120)));
        }
    }

    lines.join("\n")
}

/// Build a combined card for multiple turns (used in L3/L4 — danger tier).
fn build_combined_card_from_turns(turns: &[&Turn], known_files: &HashSet<String>, is_danger: bool) -> String {
    let mut all_files = Vec::new();
    let mut all_sigs: Vec<String> = Vec::new();
    let mut all_errors: Vec<String> = Vec::new();

    for turn in turns {
        all_files.extend(extract_files_from_turn(turn));
        all_sigs.extend(extract_signatures_from_turn(turn));
        all_errors.extend(extract_errors_from_turn(turn));
    }

    all_files.sort();
    all_files.dedup();

    let mut lines: Vec<String> = Vec::new();
    for f in &all_files {
        let f_sigs: Vec<&str> = all_sigs.iter().filter(|s| s.contains(f.as_str())).map(|s| s.as_str()).collect();
        let sym_part = if f_sigs.is_empty() {
            String::new()
        } else {
            format!(" ({})", f_sigs.join(", "))
        };
        let known = if is_danger || known_files.contains(f.as_str()) { "" } else { " (new)" };
        lines.push(format!("F: {}{}{}", f, sym_part, known));
    }

    if is_danger {
        // Danger: only recurring errors (>=2 times across turns)
        let mut error_counts: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
        for e in &all_errors {
            *error_counts.entry(e.as_str()).or_insert(0) += 1;
        }
        let recurring: Vec<&&str> = error_counts.iter()
            .filter(|(_, &c)| c >= 2)
            .map(|(e, _)| e)
            .collect();
        for e in recurring.iter().take(4) {
            lines.push(format!("E: {}", truncate_str(e, 120)));
        }
    } else {
        // Healthy: deduped errors, up to 8
        all_errors.dedup();
        for e in all_errors.iter().take(8) {
            lines.push(format!("E: {}", truncate_str(e, 120)));
        }
    }

    lines.join("\n")
}

// ── Card data extractors (operate on Turn, not flat Round) ──

fn extract_files_from_turn(turn: &Turn) -> Vec<String> {
    let mut files = Vec::new();
    for step in &turn.steps {
        if let Some(ref tcs) = step.assistant.tool_calls {
            for tc in tcs {
                if tc.function.name == "write_file" || tc.function.name == "edit_file" {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&tc.function.arguments) {
                        if let Some(path) = v.get("path").and_then(|p| p.as_str()) {
                            files.push(path.to_string());
                        }
                    }
                }
            }
        }
    }
    files.sort();
    files.dedup();
    files
}

fn extract_signatures_from_turn(turn: &Turn) -> Vec<String> {
    let mut sigs = Vec::new();
    for step in &turn.steps {
        if let Some(ref content) = step.assistant.content {
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("fn ") || trimmed.starts_with("pub fn ")
                    || trimmed.starts_with("struct ") || trimmed.starts_with("pub struct ")
                    || trimmed.starts_with("enum ") || trimmed.starts_with("pub enum ")
                    || trimmed.starts_with("trait ") || trimmed.starts_with("pub trait ")
                    || trimmed.starts_with("impl ") || trimmed.starts_with("pub impl ")
                    || trimmed.starts_with("type ") || trimmed.starts_with("pub type ")
                {
                    let sig = if let Some(pos) = trimmed.find('{') {
                        &trimmed[..pos]
                    } else {
                        trimmed
                    };
                    sigs.push(sig.trim().to_string());
                }
            }
        }
    }
    sigs.sort();
    sigs.dedup();
    sigs.truncate(12);
    sigs
}

fn extract_errors_from_turn(turn: &Turn) -> Vec<String> {
    let mut errors = Vec::new();
    for step in &turn.steps {
        for tr in &step.tool_results {
            if let Some(ref content) = tr.content {
                let has_error = content.contains("error[")
                    || content.contains("Error:")
                    || content.contains("error:")
                    || content.contains("failed")
                    || content.contains("FAILED")
                    || content.contains("exit code: 1")
                    || content.contains("exit code: 101");

                if has_error {
                    errors.push(truncate_str(content, 300));
                }
            }
        }
    }
    errors.truncate(5);
    errors
}

// ── ContextBuilder: layered context assembly ──

/// Assembles the final Anthropic API request with clear layer tracking.
///
/// New API (v4):
/// - Input: `&ContextAssembler` (Turn+Step structure, never flat `Vec<Message>`)
/// - Output: `(system_string, Vec<AnthropicMessage>, TokenBreakdown)`
///
/// Layer order (stable prefix, never changes):
///   0. System Prompt (fixed)
///   1. Long-term Backbone (nearly static)
///   2. Semantic Memory (changes when files are read)
///   3. Episodic History (compacted conversation turns)
pub struct ContextBuilder<'a> {
    assembler: &'a ContextAssembler,
    system_prompt: &'a str,
    semantic_block: &'a str,
    long_mem_block: &'a str,
    scores: &'a [f32],
    known_files: &'a HashSet<String>,
    context_limit: u32,
}

impl<'a> ContextBuilder<'a> {
    pub fn new(
        assembler: &'a ContextAssembler,
        system_prompt: &'a str,
        semantic_block: &'a str,
        long_mem_block: &'a str,
        scores: &'a [f32],
        known_files: &'a HashSet<String>,
        context_limit: u32,
    ) -> Self {
        Self { assembler, system_prompt, semantic_block, long_mem_block, scores, known_files, context_limit }
    }

    /// Build the final Anthropic-formatted output with token tracking.
    ///
    /// Returns (system_string, messages, token_breakdown).
    /// - `system_string`: concatenation of System Prompt + Backbone + Semantic + compact cards
    /// - `messages`: conversation turns in Anthropic API format (already compacted)
    /// - `TokenBreakdown`: per-layer token accounting
    pub fn build(&self) -> (String, Vec<AnthropicMessage>, TokenBreakdown) {
        let mut bd = TokenBreakdown::default();

        // Layer 0: System prompt (NEVER changes)
        bd.system = tokenizer::count_tokens(self.system_prompt);

        // Layer 1: Long-term memory / Backbone (nearly static)
        bd.long_mem = if self.long_mem_block.is_empty() { 0 } else { tokenizer::count_tokens(self.long_mem_block) };

        // Layer 2: Semantic memory (changes only when files are read)
        bd.semantic = if self.semantic_block.is_empty() { 0 } else { tokenizer::count_tokens(self.semantic_block) };

        // Layer 3: Episodic — compact turns directly (no flatten round-trip)
        let turns = self.assembler.turns();
        let (card_text, episodic_msgs, episodic_tokens, _level) = compact_turns(
            turns, self.context_limit, self.scores, self.known_files,
        );
        bd.episodic = episodic_tokens;

        // Assemble system string: Layer 0 + Layer 1 + Layer 2 + card text
        let mut system_parts: Vec<&str> = Vec::with_capacity(4);
        if !self.system_prompt.is_empty() { system_parts.push(self.system_prompt); }
        if !self.long_mem_block.is_empty() { system_parts.push(self.long_mem_block); }
        if !self.semantic_block.is_empty() { system_parts.push(self.semantic_block); }
        // Only add card_text as a separate category if non-empty
        let card_str: &str = &card_text;
        if !card_str.is_empty() { system_parts.push(card_str); }

        let system = system_parts.join("\n\n");

        // Total
        bd.total = bd.system + bd.long_mem + bd.semantic + bd.episodic;

        (system, episodic_msgs, bd)
    }
}

// ── Helpers ──

fn char_boundary(s: &str, pos: usize) -> usize {
    if pos >= s.len() { return s.len(); }
    let mut p = pos;
    while p > 0 && !s.is_char_boundary(p) { p -= 1; }
    p
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max { return s.to_string(); }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) { end -= 1; }
    format!("{}…", &s[..end])
}
