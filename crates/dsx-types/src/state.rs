use serde::{Deserialize, Serialize};

// ── Task phase (AI-declared via status tool, drives auto-mode routing) ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskPhase {
    Plan,
    Coding,
    Debug,
}

impl Default for TaskPhase {
    fn default() -> Self { TaskPhase::Coding }
}

impl TaskPhase {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "plan" => TaskPhase::Plan,
            "coding" | "code" => TaskPhase::Coding,
            "debug" => TaskPhase::Debug,
            _ => TaskPhase::Coding,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DebugLevel {
    Low,
    Medium,
    High,
}

impl Default for DebugLevel {
    fn default() -> Self { DebugLevel::Medium }
}

impl DebugLevel {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "low" => DebugLevel::Low,
            "high" => DebugLevel::High,
            _ => DebugLevel::Medium,
        }
    }
}

// ── Router IPC ──

/// Command to update the router's phase/debug-level via IPC.
///
/// Used by the status tool and TUI to communicate phase changes
/// across process boundaries without direct function calls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RouterCommand {
    SetPhase {
        phase: TaskPhase,
        level: DebugLevel,
    },
}
