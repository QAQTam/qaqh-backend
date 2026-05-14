//! Context compaction: token-aware message pruning (L1/L2/L3).

use crate::tokenizer;
use crate::tokenizer::TokenBreakdown;
use crate::types::Message;
use std::collections::HashSet;

// ── Compaction ──

/// What level of compaction was applied
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub enum CompactLevel {
    None = 0,
    L1 = 1,  // output truncation (Anthropic API: thinking blocks must be preserved, cannot strip)
    L2 = 2,  // old rounds → structured cards
    L3 = 3,  // only last 3 rounds kept full
    L4 = 4,  // only last 1 round kept full
}

/// Main compaction entry point.
///
/// Thresholds (based on V4 degradation observations — model quality drops past ~50%):
///   > 40% → L1  (always applied: truncate long outputs — reasoning is NOT stripped because Anthropic API requires thinking block preservation)
///   > 50% → L2  (card-ify rounds N-5 and older)
///   > 65% → L3  (keep only last 3 full rounds + cards for rest)
///   > 80% → L4  (keep only last 1 full round + one summary card)
///
/// Target: get context under ~45% of max_tokens.
/// Returns (new_token_count, compact_level_applied).
/// `scores` — per-round importance scores (higher = more valuable).
/// `known_files` — paths already tracked in SemanticMemory (skips verbose cards).
pub fn compact_context(messages: &mut Vec<Message>, max_tokens: u32, scores: &[f32], known_files: &HashSet<String>) -> (u32, CompactLevel) {
    if messages.is_empty() { return (0, CompactLevel::None); }

    // Always apply L1 first — truncate long outputs
    let (after_l1, _) = apply_l1(messages);

    let ratio = after_l1 as f64 / max_tokens as f64;

    // L2: card-ify low-value rounds (50%+ → Healthy tier)
    if ratio > 0.50 {
        let after = apply_l2(messages, 5, scores, known_files, false);
        let new_ratio = after as f64 / max_tokens as f64;
        if new_ratio > 0.65 {
            // L3: only last 3 rounds (65%+ → Danger tier: strict filtering)
            let after3 = apply_l3(messages, 3, scores, known_files, true);
            let r3 = after3 as f64 / max_tokens as f64;
            if r3 > 0.80 {
                // L4: emergency — only last round (80%+ → Danger tier)
                let after4 = apply_l3(messages, 1, scores, known_files, true);
                return (after4, CompactLevel::L4);
            }
            return (after3, CompactLevel::L3);
        }
        return (after, CompactLevel::L2);
    }

    (after_l1, CompactLevel::L1)
}

/// Manually force compaction to a level. Returns new token count.
pub fn force_compact(messages: &mut Vec<Message>, level: CompactLevel, scores: &[f32]) -> u32 {
    let empty = HashSet::new();
    match level {
        CompactLevel::None => tokenizer::estimate_messages_tokens(messages),
        CompactLevel::L1 => apply_l1(messages).0,
        CompactLevel::L2 => apply_l2(messages, 5, scores, &empty, false),
        CompactLevel::L3 => apply_l3(messages, 3, scores, &empty, true),
        CompactLevel::L4 => apply_l3(messages, 1, scores, &empty, true),
    }
}

// ── L1: Zero-loss cleanup ──

fn apply_l1(messages: &mut Vec<Message>) -> (u32, bool) {
    let before = tokenizer::estimate_messages_tokens(messages);
    let mut changed = false;

    for msg in messages.iter_mut() {
        // NOTE: We do NOT strip reasoning_content here, even from old turns.
        // The Anthropic API requires thinking blocks (with signatures) to be
        // preserved and passed back in subsequent requests when thinking mode
        // is enabled. Removing them causes 400 Bad Request errors.
        // Whole rounds are removed by L2-L4 compaction instead.

        // Truncate long tool outputs
        if msg.role == "tool" {
            if let Some(ref content) = msg.content {
                let line_count = content.lines().count();
                if line_count > 100 {
                    let truncated: Vec<&str> = content.lines().take(50).collect();
                    let tail: Vec<&str> = content.lines().rev().take(50).collect::<Vec<_>>().into_iter().rev().collect();
                    msg.content = Some(format!(
                        "{}\n... ({} lines trimmed) ...\n{}",
                        truncated.join("\n"),
                        line_count - 100,
                        tail.join("\n")
                    ));
                    changed = true;
                }
            }
        }

        // Truncate long file reads (assistant's tool results for read_file)
        if msg.role == "tool" {
            if let Some(ref content) = msg.content {
                if content.len() > 15000 {
                    let head_end = char_boundary(content, 5000);
                    let tail_start = char_boundary(content, content.len().saturating_sub(2000));
                    let head = &content[..head_end];
                    let tail = &content[tail_start..];
                    msg.content = Some(format!(
                        "{}\n\n... ({} chars trimmed) ...\n\n{}",
                        head,
                        content.len() - 7000,
                        tail
                    ));
                    changed = true;
                }
            }
        }
    }

    (tokenizer::estimate_messages_tokens(messages), changed && before > tokenizer::estimate_messages_tokens(messages))
}

// ── L2: Low-value rounds → structured cards ──

fn apply_l2(messages: &mut Vec<Message>, keep_full_rounds: usize, scores: &[f32], known_files: &HashSet<String>, is_danger: bool) -> u32 {
    let rounds = split_rounds(messages);
    if rounds.len() <= keep_full_rounds { return tokenizer::estimate_messages_tokens(messages); }

    let card_count = rounds.len().saturating_sub(keep_full_rounds);
    // Find the lowest-scored rounds (excluding the most recent)
    let to_card = lowest_scored_indices(rounds.len(), card_count, scores);

    let sys_msgs: Vec<Message> = messages.iter()
        .take_while(|m| m.role == "system").cloned().collect();
    let mut new_messages = sys_msgs;

    for (ri, round) in rounds.iter().enumerate() {
        if to_card.contains(&ri) {
            let card = build_round_card(ri + 1, round, known_files, is_danger);
            new_messages.push(Message::system(&card));
        } else {
            new_messages.extend(round.msgs.iter().cloned());
        }
    }

    *messages = new_messages;
    tokenizer::estimate_messages_tokens(messages)
}

// ── L3: Only last N full rounds (by value) ──

fn apply_l3(messages: &mut Vec<Message>, keep_full: usize, scores: &[f32], known_files: &HashSet<String>, is_danger: bool) -> u32 {
    let rounds = split_rounds(messages);
    if rounds.len() <= keep_full { return tokenizer::estimate_messages_tokens(messages); }

    let card_count = rounds.len().saturating_sub(keep_full);
    let to_card = lowest_scored_indices(rounds.len(), card_count, scores);

    let sys_msgs: Vec<Message> = messages.iter()
        .take_while(|m| m.role == "system").cloned().collect();
    let mut new_messages = sys_msgs;

    // Build one combined card for all card-ified rounds
    let card_rounds: Vec<Round> = to_card.iter().map(|&i| rounds[i].clone()).collect();
    if !card_rounds.is_empty() {
        new_messages.push(Message::system(&build_combined_card(&card_rounds, known_files, is_danger)));
    }

    // Full rounds kept intact
    for (ri, round) in rounds.iter().enumerate() {
        if !to_card.contains(&ri) {
            new_messages.extend(round.msgs.iter().cloned());
        }
    }

    *messages = new_messages;
    tokenizer::estimate_messages_tokens(messages)
}

/// Return indices of `count` lowest-scored rounds (always keeps the last round).
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

// ── Round extraction ──

#[derive(Clone)]
struct Round {
    msgs: Vec<Message>,
}

fn split_rounds(messages: &[Message]) -> Vec<Round> {
    let mut rounds: Vec<Round> = Vec::new();
    let mut current: Vec<Message> = Vec::new();

    // Skip leading system messages
    let start = messages.iter().position(|m| m.role != "system").unwrap_or(0);

    for msg in &messages[start..] {
        if msg.role == "user" && !current.is_empty() {
            rounds.push(Round { msgs: std::mem::take(&mut current) });
        }
        current.push(msg.clone());
    }
    if !current.is_empty() {
        rounds.push(Round { msgs: current });
    }
    rounds
}

// ── Card builders ──

fn build_round_card(_num: usize, round: &Round, known_files: &HashSet<String>, is_danger: bool) -> String {
    let files = extract_files(round);
    let sigs = extract_signatures(round);
    let errors = extract_errors(round);

    // If all files are known in SemanticMemory and no errors, minimal card
    if !files.is_empty() && errors.is_empty() && files.iter().all(|f| known_files.contains(f.as_str())) {
        let lines: Vec<String> = files.iter()
            .map(|f| format!("F: {} — (known in SemanticMemory)", f))
            .collect();
        return lines.join("\n");
    }

    let mut lines: Vec<String> = Vec::new();

    // File entries with symbols
    for f in &files {
        let f_sigs: Vec<&str> = sigs.iter().filter(|s| s.contains(f)).map(|s| s.as_str()).collect();
        let sym_part = if f_sigs.is_empty() {
            String::new()
        } else {
            format!(" ({})", f_sigs.join(", "))
        };
        // Danger tier: omit "(new)" annotation — every token counts
        let known = if is_danger || known_files.contains(f.as_str()) { "" } else { " (new)" };
        lines.push(format!("F: {}{}{}", f, sym_part, known));
    }

    // Danger tier: skip single-occurrence errors, keep only F: entries
    if !is_danger {
        for e in &errors {
            lines.push(format!("E: {}", truncate_str(e, 120)));
        }
    }

    lines.join("\n")
}

fn build_combined_card(rounds: &[Round], known_files: &HashSet<String>, is_danger: bool) -> String {
    let mut all_files = Vec::new();
    let mut all_sigs: Vec<String> = Vec::new();
    let mut all_errors: Vec<String> = Vec::new();

    for round in rounds {
        all_files.extend(extract_files(round));
        all_sigs.extend(extract_signatures(round));
        all_errors.extend(extract_errors(round));
    }

    all_files.sort(); all_files.dedup();

    let mut lines: Vec<String> = Vec::new();

    // File entries with symbols (merged across rounds)
    for f in &all_files {
        let f_sigs: Vec<&str> = all_sigs.iter().filter(|s| s.contains(f)).map(|s| s.as_str()).collect();
        let sym_part = if f_sigs.is_empty() {
            String::new()
        } else {
            format!(" ({})", f_sigs.join(", "))
        };
        // Danger tier: omit "(new)" — every token counts
        let known = if is_danger || known_files.contains(f.as_str()) { "" } else { " (new)" };
        lines.push(format!("F: {}{}{}", f, sym_part, known));
    }

    if is_danger {
        // Danger: only recurring errors (appear >=2 times across rounds)
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

// ── Card data extractors ──

fn extract_files(round: &Round) -> Vec<String> {
    let mut files = Vec::new();
    for msg in &round.msgs {
        if let Some(ref tc) = msg.tool_calls {
            for t in tc {
                if t.function.name == "write_file" || t.function.name == "edit_file" {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t.function.arguments) {
                        if let Some(path) = v.get("path").and_then(|p| p.as_str()) {
                            files.push(path.to_string());
                        }
                    }
                }
            }
        }
        if msg.role == "tool" {
            // Skip — paths come from tool_calls, not tool results
        }
    }
    files.sort(); files.dedup();
    files
}

fn extract_signatures(round: &Round) -> Vec<String> {
    let mut sigs = Vec::new();
    for msg in &round.msgs {
        if msg.role != "assistant" { continue; }
        if let Some(ref content) = msg.content {
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("fn ") || trimmed.starts_with("pub fn ")
                    || trimmed.starts_with("struct ") || trimmed.starts_with("pub struct ")
                    || trimmed.starts_with("enum ") || trimmed.starts_with("pub enum ")
                    || trimmed.starts_with("trait ") || trimmed.starts_with("pub trait ")
                    || trimmed.starts_with("impl ") || trimmed.starts_with("pub impl ")
                    || trimmed.starts_with("type ") || trimmed.starts_with("pub type ")
                {
                    // Take just the signature line (up to first '{')
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
    sigs.sort(); sigs.dedup();
    if sigs.len() > 12 { sigs.truncate(12); }
    sigs
}

fn extract_errors(round: &Round) -> Vec<String> {
    let mut errors = Vec::new();
    for msg in &round.msgs {
        if msg.role != "tool" { continue; }
        if let Some(ref content) = msg.content {
            // Detect error indicators in tool output
            let has_error = content.contains("error[")
                || content.contains("Error:")
                || content.contains("error:")
                || content.contains("failed")
                || content.contains("FAILED")
                || content.contains("exit code: 1")
                || content.contains("exit code: 101");

            if has_error {
                // Extract first 300 chars of the error
                let err_snippet = truncate_str(content, 300);
                errors.push(err_snippet);
            }
        }
    }
    if errors.len() > 5 { errors.truncate(5); }
    errors
}

// ── ContextBuilder: layered context assembly ──

/// Assembles the final message list with clear layer tracking.
/// Order: System → Semantic → Backbone → Episodic (stable prefix, never changes across turns).
pub struct ContextBuilder<'a> {
    messages: &'a [Message],
    system_prompt: &'a str,
    semantic_block: &'a str,
    long_mem_block: &'a str,
    scores: &'a [f32],
    known_files: &'a HashSet<String>,
    context_limit: u32,
}

impl<'a> ContextBuilder<'a> {
    pub fn new(
        messages: &'a [Message],
        system_prompt: &'a str,
        semantic_block: &'a str,
        long_mem_block: &'a str,
        scores: &'a [f32],
        known_files: &'a HashSet<String>,
        context_limit: u32,
    ) -> Self {
        Self { messages, system_prompt, semantic_block, long_mem_block, scores, known_files, context_limit }
    }

    /// Build the final message list with token tracking.
    /// Returns (messages, token_breakdown).
    pub fn build(&self) -> (Vec<Message>, TokenBreakdown) {
        let mut bd = TokenBreakdown::default();

        // Layer 0: System prompt (NEVER changes)
        bd.system = tokenizer::count_tokens(self.system_prompt);

        // Layer 1: Semantic memory block (changes only when files are read)
        bd.semantic = if self.semantic_block.is_empty() { 0 } else { tokenizer::count_tokens(self.semantic_block) };

        // Layer 2: Long-term memory / Backbone (extracted from project, nearly static)
        bd.long_mem = if self.long_mem_block.is_empty() { 0 } else { tokenizer::count_tokens(self.long_mem_block) };

        // Layer 3: Episodic — clone + compact the message history
        let mut prepared = self.messages.to_vec();
        let (episodic_tokens, _level) = compact_context(
            &mut prepared, self.context_limit, self.scores, self.known_files,
        );
        bd.episodic = episodic_tokens.saturating_sub(bd.system);

        // Backbone first (nearly never changes)
        if !self.long_mem_block.is_empty() {
            let insert_pos = 1;
            if insert_pos <= prepared.len() {
                prepared.insert(insert_pos, Message::system(self.long_mem_block));
            }
        }
        // Semantic after backbone (only changes when new files are read)
        if !self.semantic_block.is_empty() && !prepared.is_empty() {
            let insert_pos = if self.long_mem_block.is_empty() { 1 } else { 2 };
            if insert_pos <= prepared.len() {
                prepared.insert(insert_pos, Message::system(self.semantic_block));
            }
        }

        // Total = system + semantic + backbone + conversation
        bd.total = bd.system + bd.semantic + bd.long_mem + bd.episodic;
        (prepared, bd)
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
