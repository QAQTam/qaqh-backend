//! Pure policy vocabulary shared by the runtime, tools, and sandbox backend.
//!
//! This crate deliberately contains no filesystem access, process spawning, or
//! platform detection. It owns the stable decision types and the canonical
//! [`SandboxSpec`] that mechanism crates consume.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Risk profile for each tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCategory {
    /// No side effects: read, search, skills, image, ask, process(check/wait),
    /// and read-only git queries.
    Read,
    /// Mutates files or session state: edit, task, and write-oriented git
    /// operations.
    Write,
    /// Executes arbitrary code or controls a running process: exec, process(kill/write).
    Exec,
    /// Outbound network: web_fetch.
    Net,
}

impl ToolCategory {
    /// Stable lowercase tag used by timeline/UI payloads.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Exec => "exec",
            Self::Net => "net",
        }
    }
}

/// Intrinsic impact of the requested action, independent of the configured
/// permission policy level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionRisk {
    Low,
    Medium,
    High,
}

impl PermissionRisk {
    pub fn consequence(self) -> &'static str {
        match self {
            Self::Low => "Reads data without changing it.",
            Self::Medium => "Changes files inside the current workspace.",
            Self::High => "May affect external resources or execute arbitrary actions.",
        }
    }
}

/// Agent operating permission level (1–4). These are presets; the policy
/// engine remains the source of allow/deny/ask/amend decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum PermissionLevel {
    /// Level 1: Every tool call requires user confirmation.
    MaxLockdown = 1,
    /// Level 2: Workspace reads auto-approve; writes, exec, net require confirmation.
    ReadFree = 2,
    /// Level 3: Workspace all auto-approve; cross-workspace writes require one-time folder trust.
    WorkspaceFree = 3,
    /// Level 4: Dangerous bypass. Ordinary tools auto-approve; exec may
    /// escape the workspace until the sandbox is introduced.
    Unrestricted = 4,
}

impl PermissionLevel {
    /// Lenient scalar parser: legal levels map to themselves; any other value
    /// conservatively degrades to the most restrictive level.
    pub fn from_u8(value: u8) -> Self {
        Self::try_from_u8(value).unwrap_or(Self::MaxLockdown)
    }

    /// Strict scalar parser: rejects anything outside `1..=4`.
    pub fn try_from_u8(value: u8) -> Result<Self, String> {
        match value {
            1 => Ok(Self::MaxLockdown),
            2 => Ok(Self::ReadFree),
            3 => Ok(Self::WorkspaceFree),
            4 => Ok(Self::Unrestricted),
            other => Err(format!(
                "invalid permission level {other} (must be 1-4: 1=MaxLockdown, 2=ReadFree, 3=WorkspaceFree, 4=Unrestricted)"
            )),
        }
    }

    pub fn is_valid_u8(value: u8) -> bool {
        (1..=4).contains(&value)
    }

    pub fn to_u8(self) -> u8 {
        self as u8
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::MaxLockdown => "Level 1 — Maximum Lockdown",
            Self::ReadFree => "Level 2 — Read Free",
            Self::WorkspaceFree => "Level 3 — Workspace Free",
            Self::Unrestricted => "Level 4 — Unrestricted",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::MaxLockdown => "All operations require confirmation. No automatic trust.",
            Self::ReadFree => {
                "Reads auto-approve. Writes, execution, and network require confirmation."
            }
            Self::WorkspaceFree => {
                "Auto-approve within workspace. Cross-workspace writes are trusted once per folder."
            }
            Self::Unrestricted => {
                "Dangerous bypass: ordinary tools auto-approve; exec may escape the workspace until sandboxing lands."
            }
        }
    }
}

/// Result of policy evaluation: auto-approve or request confirmation.
#[derive(Debug)]
pub enum PermissionDecision {
    /// No confirmation needed — execute immediately.
    AutoApprove,
    /// Confirmation required. Contains the reason and target paths for the dialog.
    AskUser {
        reason: String,
        paths: Vec<PathBuf>,
        category: ToolCategory,
        risk: PermissionRisk,
        consequence: String,
    },
}

/// Enforcing backend selected for one sandbox spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxBackend {
    /// Select the strongest backend available on the host.
    Auto,
    /// Linux bubblewrap namespace/mount sandbox.
    LinuxBubblewrap,
    /// Linux Landlock + seccomp helper.
    LinuxLandlockSeccomp,
    /// Process-tree/resource hardening only; no filesystem/network sandbox.
    ProcessHardening,
    /// No enforcing backend is currently wired.
    None,
}

/// Network policy for one exec call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    Deny,
    Allow,
}

/// Canonical sandbox policy for one tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxSpec {
    pub enabled: bool,
    pub backend: SandboxBackend,
    pub writable_roots: Vec<PathBuf>,
    pub network: NetworkPolicy,
    pub max_open_files: Option<u64>,
}

impl SandboxSpec {
    /// Default Linux-first policy: workspace-write and network deny.
    pub fn workspace_write(workspace_root: PathBuf) -> Self {
        Self {
            enabled: true,
            backend: SandboxBackend::Auto,
            writable_roots: vec![workspace_root],
            network: NetworkPolicy::Deny,
            max_open_files: Some(1024),
        }
    }

    /// Explicitly disable sandbox wrapping for this call.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            backend: SandboxBackend::None,
            writable_roots: Vec::new(),
            network: NetworkPolicy::Allow,
            max_open_files: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_permission_level_fails_closed() {
        assert_eq!(PermissionLevel::from_u8(0), PermissionLevel::MaxLockdown);
        assert_eq!(PermissionLevel::from_u8(99), PermissionLevel::MaxLockdown);
        assert!(PermissionLevel::try_from_u8(0).is_err());
    }

    #[test]
    fn workspace_write_spec_is_network_deny_and_workspace_scoped() {
        let root = PathBuf::from("/tmp/ws");
        let spec = SandboxSpec::workspace_write(root.clone());
        assert_eq!(spec.backend, SandboxBackend::Auto);
        assert_eq!(spec.network, NetworkPolicy::Deny);
        assert_eq!(spec.writable_roots, vec![root]);
        assert_eq!(spec.max_open_files, Some(1024));
    }
}
