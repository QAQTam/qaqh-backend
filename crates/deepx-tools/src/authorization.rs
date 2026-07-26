//! Permission admission and single-use authorization proofs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Identity of a single tool invocation destined for a handler.
pub struct ToolInvocation {
    pub session_id: String,
    pub call_id: String,
    pub tool_name: String,
    pub action: String,
    pub args: serde_json::Value,
}

/// Authorization proof required to dispatch a handler.
///
/// Fields and construction stay private to this crate. External callers can
/// only obtain a proof through [`admit`] or [`PermissionChallenge::approve`].
pub struct AuthorizedToolCall {
    invocation: ToolInvocation,
    resources: Vec<PathBuf>,
    _sealed: (),
}

impl AuthorizedToolCall {
    pub(crate) fn new(invocation: ToolInvocation, resources: Vec<PathBuf>) -> Self {
        Self {
            invocation,
            resources,
            _sealed: (),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.invocation.session_id
    }

    pub fn call_id(&self) -> &str {
        &self.invocation.call_id
    }

    pub fn tool_name(&self) -> &str {
        &self.invocation.tool_name
    }

    pub fn action(&self) -> &str {
        &self.invocation.action
    }

    pub fn args(&self) -> &serde_json::Value {
        &self.invocation.args
    }

    pub fn resources(&self) -> &[PathBuf] {
        &self.resources
    }

    pub(crate) fn into_parts(self) -> (ToolInvocation, Vec<PathBuf>) {
        (self.invocation, self.resources)
    }
}

/// Result of the admission gate.
pub enum Admission {
    Authorized(AuthorizedToolCall),
    ApprovalRequired(PermissionChallenge),
    Denied(String),
}

/// Immutable snapshot of a call that requires user approval.
///
/// Approval consumes the challenge, making the grant single-use by type.
pub struct PermissionChallenge {
    session_id: String,
    call_id: String,
    tool_name: String,
    action: String,
    normalized_args: serde_json::Value,
    resources: Vec<PathBuf>,
    reason: String,
    category: crate::permission::ToolCategory,
    risk: crate::permission::PermissionRisk,
    consequence: String,
    created_at: Instant,
    _sealed: (),
}

impl PermissionChallenge {
    fn new(
        invocation: ToolInvocation,
        reason: String,
        resources: Vec<PathBuf>,
        category: crate::permission::ToolCategory,
        risk: crate::permission::PermissionRisk,
        consequence: String,
    ) -> Self {
        Self {
            session_id: invocation.session_id,
            call_id: invocation.call_id,
            tool_name: invocation.tool_name,
            action: invocation.action,
            normalized_args: invocation.args,
            resources,
            reason,
            category,
            risk,
            consequence,
            created_at: Instant::now(),
            _sealed: (),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    pub fn action(&self) -> &str {
        &self.action
    }

    pub fn normalized_args(&self) -> &serde_json::Value {
        &self.normalized_args
    }

    pub fn resources(&self) -> &[PathBuf] {
        &self.resources
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }

    pub fn category(&self) -> &crate::permission::ToolCategory {
        &self.category
    }

    pub fn risk(&self) -> crate::permission::PermissionRisk {
        self.risk
    }

    pub fn consequence(&self) -> &str {
        &self.consequence
    }

    pub fn is_expired(&self, ttl: Duration) -> bool {
        self.created_at.elapsed() > ttl
    }

    pub fn approve(self, approved: bool) -> Result<AuthorizedToolCall, ApprovalError> {
        self.approve_with_ttl(approved, Duration::from_secs(120))
    }

    pub(crate) fn approve_with_ttl(
        self,
        approved: bool,
        ttl: Duration,
    ) -> Result<AuthorizedToolCall, ApprovalError> {
        if !approved {
            return Err(ApprovalError::Rejected);
        }
        if self.is_expired(ttl) {
            return Err(ApprovalError::Expired);
        }
        let invocation = ToolInvocation {
            session_id: self.session_id,
            call_id: self.call_id,
            tool_name: self.tool_name,
            action: self.action,
            args: self.normalized_args,
        };
        Ok(AuthorizedToolCall::new(invocation, self.resources))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalError {
    Rejected,
    Expired,
    MissingOrReplayed,
}

/// Evaluate permission policy and bind the resulting proof to normalized resources.
pub fn admit(
    invocation: ToolInvocation,
    permission_level: u8,
    workspace_root: &Path,
    trusted_dirs: &HashSet<PathBuf>,
) -> Admission {
    let level = crate::permission::PermissionLevel::from_u8(permission_level);
    match crate::permission::needs_permission(
        level,
        &invocation.tool_name,
        &invocation.args,
        workspace_root,
        trusted_dirs,
    ) {
        crate::permission::PermissionDecision::AutoApprove => {
            let mut resources =
                crate::permission::extract_target_paths(&invocation.tool_name, &invocation.args);
            resources.sort();
            resources.dedup();
            Admission::Authorized(AuthorizedToolCall::new(invocation, resources))
        }
        crate::permission::PermissionDecision::AskUser {
            reason,
            paths,
            category,
            risk,
            consequence,
        } => Admission::ApprovalRequired(PermissionChallenge::new(
            invocation,
            reason,
            paths,
            category,
            risk,
            consequence,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::PermissionRisk;

    #[test]
    fn approval_challenge_preserves_backend_risk_and_consequence() {
        let workspace = std::env::temp_dir().join("deepx-authorization-risk");
        let invocation = ToolInvocation {
            session_id: "seed-a".into(),
            call_id: "call-a".into(),
            tool_name: "write".into(),
            action: String::new(),
            args: serde_json::json!({ "path": workspace.join("src/lib.rs") }),
        };

        let Admission::ApprovalRequired(challenge) =
            admit(invocation, 1, &workspace, &HashSet::new())
        else {
            panic!("level 1 write must require approval");
        };

        assert_eq!(challenge.risk(), PermissionRisk::Medium);
        assert_eq!(
            challenge.consequence(),
            "Changes files inside the current workspace."
        );
    }
}
