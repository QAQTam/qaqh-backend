//! Long-term memory: architecture invariants and project knowledge.

use serde::{Deserialize, Serialize};
use crate::memory::semantic::SemanticMemory;

// ── Long-Term Memory ──

/// Persistent cross-session memory stored as `long-mem.md`.
/// Written by system extraction (not by AI directly) — contains architecture,
/// decisions, file change summaries, and user goals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LongTermMemory {
    /// File path → responsibility description
    pub architecture: Vec<(String, String)>,
    /// Key architectural decisions with reasons
    pub decisions: Vec<(String, String)>,
    /// Recent file changes: path → (lines_added, lines_removed)
    pub file_changes: Vec<(String, u32, u32)>,
    /// Critical invariants that must never be violated
    pub critical_invariants: Vec<String>,
}

impl Default for LongTermMemory {
    fn default() -> Self {
        Self {
            architecture: Vec::new(),
            decisions: Vec::new(),
            file_changes: Vec::new(),
            critical_invariants: Vec::new(),
        }
    }
}

impl LongTermMemory {
    /// Render to markdown string for storage and AI context injection.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("## Project Backbone\n\n");

        // Module map — file path → one-line purpose (most important for AI orientation)
        if !self.architecture.is_empty() {
            out.push_str("### Module Map\n");
            for (path, role) in &self.architecture {
                out.push_str(&format!("- `{}` — {}\n", path, role));
            }
            out.push('\n');
        }

        // Critical invariants — must never be violated
        if !self.critical_invariants.is_empty() {
            out.push_str("### Critical Invariants\n");
            for inv in self.critical_invariants.iter().rev().take(10) {
                out.push_str(&format!("- {}\n", inv));
            }
            out.push('\n');
        }

        if !self.decisions.is_empty() {
            out.push_str("### Decisions\n");
            for (summary, reason) in self.decisions.iter().rev().take(8) {
                out.push_str(&format!("- {} → {}\n", summary, reason));
            }
            out.push('\n');
        }
        if !self.file_changes.is_empty() {
            out.push_str("### File Changes\n");
            for (path, added, removed) in self.file_changes.iter().rev().take(10) {
                out.push_str(&format!("- `{}` +{}/-{}\n", path, added, removed));
            }
            out.push('\n');
        }
        out
    }

    /// Render a minimal backbone for re-injection in long-context scenarios.
    /// Target: ~300-500 tokens. Only module map + critical invariants.
    pub fn render_backbone_compact(&self) -> String {
        let mut out = String::new();
        for (path, role) in &self.architecture {
            out.push_str(&format!("- `{}` — {}\n", path, role));
        }
        if !self.critical_invariants.is_empty() {
            out.push('\n');
            for inv in &self.critical_invariants {
                out.push_str(&format!("- INVARIANT: {}\n", inv));
            }
        }
        if out.is_empty() {
            return out;
        }
        format!("## Project Backbone\n\n{}", out)
    }

    /// Sync module map from SemanticMemory. For each file entry with a purpose,
    /// ensure the architecture field has an entry.
    pub fn sync_from_semantic(&mut self, sem: &SemanticMemory) {
        for (path, entry) in &sem.entries {
            if let Some(ref purpose) = entry.purpose {
                self.add_architecture(path, purpose);
            }
        }
    }

    /// Add a file change entry, deduplicating by path.
    pub fn add_file_change(&mut self, path: &str, added: u32, removed: u32) {
        if let Some(entry) = self.file_changes.iter_mut().find(|(p, _, _)| p == path) {
            entry.1 += added;
            entry.2 += removed;
        } else {
            self.file_changes.push((path.to_string(), added, removed));
        }
        if self.file_changes.len() > 50 {
            self.file_changes.remove(0);
        }
    }

    /// Add an architecture entry, deduplicating by path.
    pub fn add_architecture(&mut self, path: &str, role: &str) {
        if let Some(entry) = self.architecture.iter_mut().find(|(p, _)| p == path) {
            entry.1 = role.to_string();
        } else {
            self.architecture.push((path.to_string(), role.to_string()));
        }
        if self.architecture.len() > 30 {
            self.architecture.remove(0);
        }
    }

    /// Add a critical invariant. Deduplicates by content.
    pub fn add_invariant(&mut self, inv: &str) {
        let inv = inv.to_string();
        if !self.critical_invariants.contains(&inv) {
            self.critical_invariants.push(inv);
        }
        if self.critical_invariants.len() > 20 {
            self.critical_invariants.remove(0);
        }
    }
}
