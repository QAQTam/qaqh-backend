//! Pitfall guide: cross-session self-learning from mistakes.

use serde::{Deserialize, Serialize};
use crate::health::{AgentEmotion, ErrorKind};
use super::semantic::now_epoch;

// ── Pitfall Guide (cross-session self-learning) ──

/// A single AI-identified pitfall — what went wrong and how to avoid it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PitfallEntry {
    pub emotion: String,          // AgentEmotion label at time of recording
    pub error_kind: String,       // ErrorKind variant name
    pub tool: String,             // which tool was involved
    pub description: String,      // AI's own description: what happened
    pub lesson: String,           // AI's own advice: how to avoid next time
    pub files: Vec<String>,       // files involved
    pub frequency: u32,
    pub first_seen: u64,
    pub last_seen: u64,
}

impl PitfallEntry {
    /// Check if this pitfall matches an incoming error.
    pub fn matches(&self, kind: ErrorKind, tool: &str) -> bool {
        let kind_name = format!("{:?}", kind);
        self.error_kind == kind_name && (self.tool.is_empty() || self.tool == tool)
    }
}

/// Cross-session pitfall knowledge base.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PitfallGuide {
    pub entries: Vec<PitfallEntry>,
    pub total_updates: u64,
}

impl Default for PitfallGuide {
    fn default() -> Self {
        Self { entries: Vec::new(), total_updates: 0 }
    }
}

impl PitfallGuide {
    /// Add or merge a pitfall entry. If a matching pattern exists, increments frequency.
    pub fn upsert(&mut self, emotion: AgentEmotion, kind: ErrorKind, tool: &str,
                  description: &str, lesson: &str, files: &[String]) {
        let kind_name = format!("{:?}", kind);
        let emotion_label = emotion.label().to_string();
        let now = now_epoch();

        // Try to merge with existing entry (same error_kind + tool + description prefix)
        let desc_prefix: String = description.chars().take(40).collect();
        for entry in &mut self.entries {
            if entry.error_kind == kind_name && entry.tool == tool
                && entry.description.starts_with(&desc_prefix)
            {
                entry.frequency += 1;
                entry.last_seen = now;
                if !files.is_empty() {
                    for f in files {
                        if !entry.files.contains(f) {
                            entry.files.push(f.clone());
                        }
                    }
                }
                entry.lesson = format!("{}\n{}", entry.lesson, lesson);
                if entry.lesson.len() > 500 {
                    entry.lesson = entry.lesson[..500].to_string();
                }
                self.total_updates += 1;
                return;
            }
        }

        // New entry
        self.entries.push(PitfallEntry {
            emotion: emotion_label,
            error_kind: kind_name,
            tool: tool.to_string(),
            description: description.chars().take(300).collect(),
            lesson: lesson.chars().take(200).collect(),
            files: files.to_vec(),
            frequency: 1,
            first_seen: now,
            last_seen: now,
        });
        self.total_updates += 1;

        // Cap at 50 entries: evict lowest-frequency, oldest
        if self.entries.len() > 50 {
            self.entries.sort_by(|a, b| {
                a.frequency.cmp(&b.frequency)
                    .then_with(|| a.last_seen.cmp(&b.last_seen))
            });
            self.entries.remove(0);
        }
    }

    /// Get entries sorted by frequency (highest first).
    pub fn top(&self, n: usize) -> Vec<&PitfallEntry> {
        let mut sorted: Vec<&PitfallEntry> = self.entries.iter().collect();
        sorted.sort_by(|a, b| b.frequency.cmp(&a.frequency));
        sorted.truncate(n);
        sorted
    }

    /// Find matching pitfalls for an error (by ErrorKind).
    pub fn find_matches(&self, kind: ErrorKind, tool: &str) -> Vec<&PitfallEntry> {
        self.entries.iter()
            .filter(|e| e.matches(kind, tool))
            .collect()
    }

    /// Render to a compact markdown block for context injection.
    pub fn render(&self, max_entries: usize) -> String {
        let top = self.top(max_entries);
        if top.is_empty() {
            return String::new();
        }
        let mut out = String::from("## Pitfall Guide (from past sessions)\n");
        for e in &top {
            let files = if e.files.is_empty() {
                String::new()
            } else {
                format!(" [{}]", e.files.join(", "))
            };
            out.push_str(&format!(
                "- {}: {} ×{}{} — {}\n",
                e.error_kind, trunc_desc(&e.description, 80),
                e.frequency, files, trunc_desc(&e.lesson, 100),
            ));
        }
        out
    }
}

fn trunc_desc(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_string() } else {
        let mut end = max;
        while !s.is_char_boundary(end) { end -= 1; }
        format!("{}…", &s[..end])
    }
}
