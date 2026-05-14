//! Quality checks: content and reasoning validation.

use super::monitor::MonitorState;
use super::DsAgentsHealthPlatform;

// ── Content quality (free function on MonitorState) ──

/// Check content for gibberish or repetitive patterns.
///
/// Returns a warning string if the content buffer shows suspicious repetition
/// (same byte-level pattern repeated 10+ times).
///
/// Ported from `DsAgentsHealthPlatform::check_content`.
pub fn check_content(state: &mut MonitorState, text: &str) -> Option<String> {
    state.content_buffer.push_str(text);
    if state.content_buffer.len() > 1200 {
        let start = state.content_buffer.len().saturating_sub(600);
        let cut = boundary(&state.content_buffer, start);
        state.content_buffer = state.content_buffer[cut..].to_string();
    }
    if state.content_buffer.len() < 40 {
        return None;
    }

    for len in 1..=4 {
        let bytes = state.content_buffer.as_bytes();
        let mut i = 0;
        while i + len * 10 <= bytes.len() {
            let pattern = &bytes[i..i + len];
            let mut count = 1;
            let mut j = i + len;
            while j + len <= bytes.len() && &bytes[j..j + len] == pattern {
                count += 1;
                j += len;
            }
            if count >= 10 {
                let pat_str = String::from_utf8_lossy(pattern);
                if pat_str.chars().any(|c| c.is_alphanumeric() && !c.is_ascii_punctuation()) {
                    return Some(format!("Content repetition: '{}' x{}", pat_str, count));
                }
            }
            i += len;
        }
    }
    None
}

// ── Reasoning quality (free function on MonitorState) ──

/// Check reasoning for repetitive loops.
///
/// Returns a warning string if stall words repeat excessively or
/// the same thought segment appears 4+ times in the last 6 samples.
///
/// Ported from `DsAgentsHealthPlatform::check_reasoning`.
pub fn check_reasoning(state: &mut MonitorState, text: &str) -> Option<String> {
    if !text.trim().is_empty() {
        state.reasoning_sample.push(text.to_string());
        if state.reasoning_sample.len() > 12 {
            state.reasoning_sample.remove(0);
        }
    }
    if state.reasoning_sample.len() < 4 {
        return None;
    }

    let combined = state.reasoning_sample.join(" ");
    let stall_words = [
        "however", "but", "好吧", "让我想想", "可是", "但是", "不过", "然而",
    ];
    for &word in &stall_words {
        let count = combined.matches(word).count();
        if count >= 6 {
            return Some(format!("Reasoning loop: '{}' repeated {} times", word, count));
        }
    }

    if state.reasoning_sample.len() >= 6 {
        let last = &state.reasoning_sample[state.reasoning_sample.len() - 1];
        let same = state
            .reasoning_sample
            .iter()
            .rev()
            .take(6)
            .filter(|s| *s == last)
            .count();
        if same >= 4 {
            return Some("Reasoning loop: repeating same thought segment".into());
        }
    }

    None
}

// ── Platform-level quality checks ──

impl DsAgentsHealthPlatform {
    pub fn check_reasoning(&mut self, text: &str) -> Option<String> {
        if !text.trim().is_empty() {
            self.reasoning_sample.push(text.to_string());
            if self.reasoning_sample.len() > 12 {
                self.reasoning_sample.remove(0);
            }
        }
        if self.reasoning_sample.len() < 4 {
            return None;
        }

        let combined = self.reasoning_sample.join(" ");
        let stall_words =
            ["可是", "但是", "不过", "然而", "however", "but", "好吧", "让我想想"];
        for word in &stall_words {
            let count = combined.matches(word).count();
            if count >= 6 {
                return Some(format!(
                    "Reasoning loop: '{}' repeated {} times",
                    word, count
                ));
            }
        }
        if self.reasoning_sample.len() >= 6 {
            let last = &self.reasoning_sample[self.reasoning_sample.len() - 1];
            let same = self
                .reasoning_sample
                .iter()
                .rev()
                .take(6)
                .filter(|s| *s == last)
                .count();
            if same >= 4 {
                return Some("Reasoning loop: repeating same thought segment".into());
            }
        }
        None
    }

    pub fn check_content(&mut self, text: &str) -> Option<String> {
        self.content_buffer.push_str(text);
        if self.content_buffer.len() > 1200 {
            let start = self.content_buffer.len().saturating_sub(600);
            let cut = boundary(&self.content_buffer, start);
            self.content_buffer = self.content_buffer[cut..].to_string();
        }
        if self.content_buffer.len() < 40 {
            return None;
        }

        for len in 1..=4 {
            let mut i = 0;
            let bytes = self.content_buffer.as_bytes();
            while i + len * 10 <= bytes.len() {
                let pattern = &bytes[i..i + len];
                let mut count = 1;
                let mut j = i + len;
                while j + len <= bytes.len() && &bytes[j..j + len] == pattern {
                    count += 1;
                    j += len;
                }
                if count >= 10 {
                    let pat_str = String::from_utf8_lossy(pattern);
                    if pat_str
                        .chars()
                        .any(|c| c.is_alphanumeric() && !c.is_ascii_punctuation())
                    {
                        return Some(format!("Content repetition: '{}' \u{d7}{}", pat_str, count));
                    }
                }
                i += len;
            }
        }
        None
    }
}

// ── Helpers ──

/// Find the nearest UTF-8 character boundary at or before `pos`.
fn boundary(s: &str, pos: usize) -> usize {
    if pos >= s.len() {
        return s.len();
    }
    let mut p = pos;
    while p > 0 && !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::monitor::MonitorState;

    #[test]
    fn test_check_reasoning_detects_repetitive_thought() {
        let mut state = MonitorState::new();
        // Push the same text 6 times
        let thought = "I need to think about this more";
        for _ in 0..6 {
            let result = check_reasoning(&mut state, thought);
            if let Some(warn) = result {
                assert!(warn.contains("repeating same thought segment"));
                return;
            }
        }
        panic!("expected reasoning loop warning");
    }

    #[test]
    fn test_check_content_detects_repetition() {
        let mut state = MonitorState::new();
        let repeated = "abc".repeat(15);
        let result = check_content(&mut state, &repeated);
        assert!(result.is_some());
        assert!(result.unwrap().contains("Content repetition"));
    }
}
