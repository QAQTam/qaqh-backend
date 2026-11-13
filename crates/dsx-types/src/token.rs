//! Token counting: trait + CJK heuristic + helpers.
//!
//! `dsx-types` provides the trait and a character-count heuristic.
//! Precise tokenizer-backed [`TokenCount`] impls live in the crate that
//! owns the tokenizer binary (e.g. `dsx-agent`).

// ── Trait ──

/// Any unit that consumes tokens can implement [`TokenCount`].
///
/// # Example (in dsx-agent)
///
/// ```ignore
/// impl TokenCount for Message {
///     fn count_tokens(&self) -> u32 {
///         let mut t = 4u32;
///         if let Some(ref c) = self.content { t += c.count_tokens(); }
///         if let Some(ref r) = self.reasoning_content { t += r.count_tokens(); }
///         // ...
///         t
///     }
/// }
/// ```
pub trait TokenCount {
    /// Returns the estimated number of tokens this value consumes.
    fn count_tokens(&self) -> u32;
}

impl TokenCount for str {
    fn count_tokens(&self) -> u32 {
        count_tokens(self)
    }
}

impl TokenCount for String {
    fn count_tokens(&self) -> u32 {
        count_tokens(self)
    }
}

/// Estimate total tokens for a slice of [`TokenCount`] items.
pub fn estimate_messages_tokens<T: TokenCount>(messages: &[T]) -> u32 {
    messages.iter().map(|m| m.count_tokens()).sum()
}

// ── Heuristic-only counting (no tokenizer dependency) ──

/// Count tokens using a CJK-aware character heuristic.
///
/// Non-CJK characters are counted as `len / 3.3`; CJK characters as
/// `count / 1.67`.  These ratios are derived from empirical DeepSeek
/// tokenizer measurements and serve as a dependency-free fallback when
/// no `tokenizer.json` is available.
pub fn count_tokens(text: &str) -> u32 {
    let cjk: usize = text
        .chars()
        .filter(|c| {
            matches!(
                c,
                '\u{4e00}'..='\u{9fff}'
                    | '\u{3400}'..='\u{4dbf}'
                    | '\u{3000}'..='\u{303f}'
                    | '\u{ff00}'..='\u{ffef}'
                    | '\u{3040}'..='\u{30ff}'
            )
        })
        .count();
    (text.len().saturating_sub(cjk) as f64 / 3.3 + cjk as f64 / 1.67) as u32
}

// ── Breakdown ──

/// Token usage broken down by category.
#[derive(Debug, Default, Clone, Copy)]
pub struct TokenBreakdown {
    pub system: u32,
    pub episodic: u32,
    pub total: u32,
}

// ── Formatting helpers ──

/// Human-friendly token count display.
///
/// ```
/// use dsx_types::token::format_tokens;
/// assert_eq!(format_tokens(1500), "1.5K");
/// assert_eq!(format_tokens(2_000_000), "2.0M");
/// assert_eq!(format_tokens(500), "500");
/// ```
pub fn format_tokens(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Fraction of the context window consumed, clamped to `[0.0, 1.0]`.
pub fn context_usage_ratio(used: u32, max_tokens: u32) -> f64 {
    (used as f64 / max_tokens as f64).clamp(0.0, 1.0)
}
