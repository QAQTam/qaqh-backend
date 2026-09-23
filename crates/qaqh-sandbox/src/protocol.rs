//! Versioned request type shared by the daemon and helper.
//!
//! The helper receives this payload over stdin. Target argv is kept out of the
//! command line so large commands and shell fragments do not hit argv limits.

use serde::{Deserialize, Serialize};

use crate::SandboxSpec;

/// One helper invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxRequest {
    pub spec: SandboxSpec,
    pub argv: Vec<String>,
}
