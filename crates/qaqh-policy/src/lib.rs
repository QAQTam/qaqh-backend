//! Pure policy vocabulary shared by the runtime, tools, and sandbox backend.
//!
//! This crate deliberately contains no filesystem access, process spawning, or
//! platform detection. It owns the stable decision types and the canonical
//! [`SandboxSpec`] that mechanism crates consume.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub mod approval;
pub mod input_guard;

pub use approval::{ApprovalDecision, ApprovalRegistry, ApprovalTake};
pub use input_guard::content_guard;

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

/// Agent operating permission tier (1–3). These are presets; the policy
/// engine remains the source of allow/deny/ask/amend decisions.
///
/// 2026-10-03 起为三档制(read-only / workspace-write / skip-permissions),
/// 取代旧 L1–L4(MaxLockdown/ReadFree/WorkspaceFree/Unrestricted)。旧配置数值
/// 在 config load 处经 [`PermissionLevel::from_legacy_u8`] 迁移;wire 上仍是
/// 裸 u8(1/2/3),语义单调:数值越大越放行。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum PermissionLevel {
    /// Read-only: 工作区内读自动放行;一切变更(write/exec/net)逐次审批。
    /// fail-closed 兜底档(非法配置值一律落这里)。
    ReadOnly = 1,
    /// Workspace-write: 工作区内读写自动放行;跨工作区写走一次性目录信任;
    /// exec/net 仍逐次审批。
    WorkspaceWrite = 2,
    /// Skip permissions: 显式旁路——普通工具全部自动(含 exec/net)。
    /// 敏感路径守卫(会话文件/平台 config/skill 根)在所有档位都强制审批。
    SkipPermissions = 3,
}

impl PermissionLevel {
    /// Lenient scalar parser: legal tiers map to themselves; any other value
    /// conservatively degrades to the most restrictive tier.
    pub fn from_u8(value: u8) -> Self {
        Self::try_from_u8(value).unwrap_or(Self::ReadOnly)
    }

    /// Strict scalar parser: rejects anything outside `1..=3`.
    pub fn try_from_u8(value: u8) -> Result<Self, String> {
        match value {
            1 => Ok(Self::ReadOnly),
            2 => Ok(Self::WorkspaceWrite),
            3 => Ok(Self::SkipPermissions),
            other => Err(format!(
                "invalid permission level {other} (must be 1-3: 1=read-only, 2=workspace-write, 3=skip-permissions)"
            )),
        }
    }

    /// 旧 L1–L4 数值 → 新三档。`None` = 无法识别(调用方按非法值 fail-closed)。
    ///
    /// | 旧值 | 旧语义 | 新档 |
    /// |---|---|---|
    /// | 1 MaxLockdown | 一切审批 | ReadOnly(旧 L1 无对应档,唯一松动:工作区内读不再逐次弹) |
    /// | 2 ReadFree | 读自动/变更审批 | ReadOnly(语义相同) |
    /// | 3 WorkspaceFree | 工作区自由 | WorkspaceWrite(语义相同) |
    /// | 4 Unrestricted | 危险旁路 | SkipPermissions(语义相同) |
    pub fn from_legacy_u8(value: u8) -> Option<Self> {
        match value {
            1 | 2 => Some(Self::ReadOnly),
            3 => Some(Self::WorkspaceWrite),
            4 => Some(Self::SkipPermissions),
            _ => None,
        }
    }

    pub fn is_valid_u8(value: u8) -> bool {
        (1..=3).contains(&value)
    }

    pub fn to_u8(self) -> u8 {
        self as u8
    }

    /// 稳定小写标签(wire/UI 用;与档名一一对应)。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::SkipPermissions => "skip-permissions",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "Read-Only",
            Self::WorkspaceWrite => "Workspace-Write",
            Self::SkipPermissions => "Skip Permissions",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::ReadOnly => "Workspace reads auto-approve. Every mutation (write/exec/net) requires per-call approval.",
            Self::WorkspaceWrite => "Auto-approve within the workspace; cross-workspace writes are trusted once per folder. Exec/net require approval.",
            Self::SkipPermissions => "Explicit bypass: ordinary tools auto-approve, including exec/net. Sensitive-path guards still ask.",
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
        assert_eq!(PermissionLevel::from_u8(0), PermissionLevel::ReadOnly);
        assert_eq!(PermissionLevel::from_u8(99), PermissionLevel::ReadOnly);
        assert!(PermissionLevel::try_from_u8(0).is_err());
        assert!(PermissionLevel::try_from_u8(4).is_err());
    }

    #[test]
    fn legacy_values_migrate_to_three_tiers() {
        assert_eq!(PermissionLevel::from_legacy_u8(1), Some(PermissionLevel::ReadOnly));
        assert_eq!(PermissionLevel::from_legacy_u8(2), Some(PermissionLevel::ReadOnly));
        assert_eq!(PermissionLevel::from_legacy_u8(3), Some(PermissionLevel::WorkspaceWrite));
        assert_eq!(PermissionLevel::from_legacy_u8(4), Some(PermissionLevel::SkipPermissions));
        assert_eq!(PermissionLevel::from_legacy_u8(0), None);
        assert_eq!(PermissionLevel::from_legacy_u8(5), None);
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
