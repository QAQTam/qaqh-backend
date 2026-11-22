//! Token counting: CJK-aware character heuristic.
//!
//! HuggingFace `tokenizers` 精确计数面已整体退役（ADR-0036）：OnceLock 进程级
//! tokenizer、`init_tokenizer` 与 `tokenizer_path` 配置设施同批移除，
//! `count_tokens` 即本启发式。

/// Count tokens using a CJK-aware character heuristic.
///
/// Non-CJK characters are counted as `len / 3.3`; CJK characters as
/// `count / 1.67`.  These ratios are derived from empirical DeepSeek
/// tokenizer measurements.
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
