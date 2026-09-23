//! Versioned request/spec types shared by the daemon and helper.
//!
//! The helper receives this payload over stdin. Target argv is kept out of the
//! command line so large commands and shell fragments do not hit argv limits.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::SandboxBackend;

/// Network policy for one exec call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    Deny,
    Allow,
}

/// Canonical sandbox policy for one exec call.
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
            backend: SandboxBackend::LinuxLandlockSeccomp,
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

/// One helper invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxRequest {
    pub spec: SandboxSpec,
    pub argv: Vec<String>,
}
