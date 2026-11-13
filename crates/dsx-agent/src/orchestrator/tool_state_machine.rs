//! Tool state machine: explore-before-act enforcement.
//!
//! Centralizes the explore -> declare -> read -> write/exec chain
//! so that tool gates are consistent and auditable.

use crate::agent::AgentState;
use std::collections::HashSet;

// ── Public types ──

/// Outcome of a tool gate check.
pub enum ToolGateResult {
    /// Tool may proceed.
    Allow,
    /// Tool is blocked with a reason and hint.
    Block { reason: String, hint: String },
    /// Tool should be redirected to a different tool.
    Redirect { to_tool: String, reason: String },
}

/// A file the assistant declared intent to touch.
pub struct DeclaredIntent {
    pub path: String,
    pub action: String,  // "read", "write", "edit", "exec"
    pub reason: String,  // why — from assistant reasoning or content
}

/// Centralised state machine enforcing the explore-before-act discipline.
pub struct ToolStateMachine {
    /// Whether the project has been explored at least once.
    pub explored: bool,
    /// Files the assistant declared it intends to touch.
    pub declared_files: Vec<DeclaredIntent>,
    /// Files that have been read (used to gate edits on unread files).
    pub read_files: HashSet<String>,
    /// Files written in the current turn (used to gate same-turn exec).
    pub written_this_turn: Vec<String>,
}

impl ToolStateMachine {
    pub fn new() -> Self {
        Self {
            explored: false,
            declared_files: Vec::new(),
            read_files: HashSet::new(),
            written_this_turn: Vec::new(),
        }
    }

    // ── Gate checks ──

    /// Check whether a `file(read, ...)` call is allowed.
    pub fn check_read(&self, state: &AgentState, path: &str) -> ToolGateResult {
        if !state.has_explored {
            return ToolGateResult::Block {
                reason: "Must explore the project first".into(),
                hint: "Call exec(explore).".into(),
            };
        }
        if !self.declared_files.iter().any(|d| d.path == path) {
            return ToolGateResult::Block {
                reason: "Must declare intent to read this file first.".into(),
                hint: "Explain which file you need to read and why.".into(),
            };
        }
        ToolGateResult::Allow
    }

    /// Check whether a `file(write|edit, ...)` call is allowed.
    pub fn check_write(&self, state: &AgentState, path: &str, reason: &str) -> ToolGateResult {
        if !state.has_explored {
            return ToolGateResult::Block {
                reason: "Must explore the project first".into(),
                hint: "Call exec(explore).".into(),
            };
        }
        if !self.read_files.contains(path) {
            return ToolGateResult::Block {
                reason: "Must read this file before editing.".into(),
                hint: format!("Call file(read, path=\"{}\") first.", path),
            };
        }
        if reason.trim().is_empty() {
            return ToolGateResult::Block {
                reason: "Must state why you're changing this file.".into(),
                hint: "Explain the purpose of this edit in your reasoning.".into(),
            };
        }
        if state.turns_since_last_read >= 4 {
            return ToolGateResult::Block {
                reason: "File may be stale. Re-read first.".into(),
                hint: format!("Call file(read, path=\"{}\") to refresh.", path),
            };
        }
        ToolGateResult::Allow
    }

    /// Check whether an `exec(execute, ...)` call is allowed.
    pub fn check_exec(&self, state: &AgentState, command: &str) -> ToolGateResult {
        if !state.has_explored {
            return ToolGateResult::Block {
                reason: "Must explore the project first".into(),
                hint: "Call exec(explore).".into(),
            };
        }
        if Self::is_danger_command(command) {
            return ToolGateResult::Redirect {
                to_tool: "file/read".into(),
                reason: format!(
                    "Use read_file/write_file instead. If you MUST use exec, declare why. Command: {}",
                    command.chars().take(60).collect::<String>()
                ),
            };
        }
        for written in &state.files_written_this_turn {
            if command.contains(written) {
                return ToolGateResult::Block {
                    reason: format!("This file was written this turn: {}", written),
                    hint: "Wait until next turn to operate on this file.".into(),
                };
            }
        }
        if state.exec_pending >= 2 {
            return ToolGateResult::Block {
                reason: "Max 2 concurrent execs.".into(),
                hint: "Wait for pending execs to complete.".into(),
            };
        }
        ToolGateResult::Allow
    }

    // ── Recording side-effects ──

    /// Record that a file was read so future edit gates allow editing it.
    pub fn record_read(&mut self, path: &str) {
        self.read_files.insert(path.to_string());
    }

    /// Record that the assistant declared intent to touch a file.
    pub fn record_declare(&mut self, path: &str, action: &str, reason: &str) {
        self.declared_files.retain(|d| d.path != path);
        self.declared_files.push(DeclaredIntent {
            path: path.to_string(),
            action: action.to_string(),
            reason: reason.to_string(),
        });
    }

    // ── Helpers ──

    /// Check if a command should use file tools instead of exec.
    pub fn is_danger_command(cmd: &str) -> bool {
        let first_word = cmd.split_whitespace().next().unwrap_or("");
        matches!(
            first_word,
            "cat" | "sed" | "grep" | "cp" | "mv" | "rm" | "head" | "tail" | "wc" | "awk"
        )
    }
}

impl Default for ToolStateMachine {
    fn default() -> Self {
        Self::new()
    }
}
