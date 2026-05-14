//! Short-term memory: per-session learning, recent rounds.

use serde::{Deserialize, Serialize};
use super::semantic::now_epoch;

// ── Short-Term Memory ──

/// Per-session round index stored as `short-mem.md`.
/// Written by system after each agent loop turn. AI reads only.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShortTermMemory {
    pub seed: String,
    pub model: String,
    pub started_at: u64,
    pub rounds: Vec<RoundEntry>,
    pub active_issues: Vec<String>,
    pub pending_tasks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundEntry {
    pub num: usize,
    pub tools: Vec<String>,
    pub files: Vec<String>,
    pub summary: String,
}

impl ShortTermMemory {
    pub fn new(seed: &str, model: &str) -> Self {
        Self {
            seed: seed.to_string(),
            model: model.to_string(),
            started_at: now_epoch(),
            rounds: Vec::new(),
            active_issues: Vec::new(),
            pending_tasks: Vec::new(),
        }
    }

    /// Render to markdown string for storage and AI context injection.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("## Short-Term Memory\n\n");
        out.push_str(&format!("Session `{}` · model `{}`\n\n", self.seed, self.model));

        if !self.rounds.is_empty() {
            out.push_str("### Recent Rounds\n");
            for r in self.rounds.iter().rev().take(10) {
                let tools = r.tools.join(", ");
                let files = if r.files.is_empty() { "-".into() } else { r.files.join(", ") };
                out.push_str(&format!("- R{}: {} → {} — {}\n", r.num, tools, files, r.summary));
            }
            out.push('\n');
        }
        if !self.active_issues.is_empty() {
            out.push_str("### Active Issues\n");
            for issue in &self.active_issues {
                out.push_str(&format!("- {}\n", issue));
            }
            out.push('\n');
        }
        if !self.pending_tasks.is_empty() {
            out.push_str("### Pending Tasks\n");
            for task in &self.pending_tasks {
                out.push_str(&format!("- {}\n", task));
            }
            out.push('\n');
        }
        out
    }

    /// Append a round entry (replaces existing round with same number).
    pub fn append_round(&mut self, num: usize, tools: &[String], files: &[String], summary: &str) {
        if let Some(existing) = self.rounds.iter_mut().find(|r| r.num == num) {
            existing.tools = tools.to_vec();
            existing.files = files.to_vec();
            existing.summary = summary.to_string();
            return;
        }
        self.rounds.push(RoundEntry {
            num,
            tools: tools.to_vec(),
            files: files.to_vec(),
            summary: summary.to_string(),
        });
        if self.rounds.len() > 50 {
            self.rounds.remove(0);
        }
    }

    /// Append an AI-authored note (via mem_save tool).
    pub fn append_issue(&mut self, issue: &str) {
        self.active_issues.push(issue.to_string());
        if self.active_issues.len() > 10 {
            self.active_issues.remove(0);
        }
    }
}
