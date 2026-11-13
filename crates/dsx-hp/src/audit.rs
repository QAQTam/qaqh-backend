//! NOTE: Disconnected from dsx-agent — dsx-agent uses its own stubs in health.rs.
//! This module is used by the dsx-hp binary internally. Wire up via IPC later.
//!
//! Audit — session integrity checking and /fix repair.
//!
//! Checks conversation state for alternation violations, orphan tool_uses,
//! and orphan tool_results. Runs repair via `ContextAssembler::from_legacy`
//! when /fix is invoked.
//!
//! Ported from `repair_messages()` and the alternation check in `health/mod.rs`.

use crate::stubs::ContextAssembler;
use dsx_types::Message;

/// Result of a session audit.
#[derive(Debug, Clone, PartialEq)]
pub enum AuditResult {
    /// No issues found.
    Clean,
    /// Issues found that can be fixed automatically.
    Repairable {
        issues: Vec<String>,
        fix_description: String,
    },
    /// Data is corrupted beyond automatic repair.
    Corrupted { reason: String },
}

/// Audit a flat message list for structural issues.
///
/// Runs the messages through `ContextAssembler::from_legacy` and reports
/// any repairs needed. Uses `ContextAssembler::validate()` for structural
/// validation (the canonical check_alternation implementation).
pub fn audit_session(messages: &[Message]) -> AuditResult {
    // Structural check via ContextAssembler (canonical validation authority)
    let (assembler, repairs) = ContextAssembler::from_legacy(messages.to_vec());

    if !repairs.is_empty() {
        return AuditResult::Repairable {
            issues: repairs.clone(),
            fix_description: format!("{} issue(s) found: {}", repairs.len(), repairs.join("; ")),
        };
    }

    match assembler.validate() {
        Ok(()) => AuditResult::Clean,
        Err(e) => AuditResult::Corrupted { reason: e },
    }
}

/// Repair a `ContextAssembler` in-place, returning the audit result
/// and list of repairs performed.
///
/// Ported from `repair_messages()` in `tui/app.rs`.
pub fn repair_session(ctx: &mut ContextAssembler) -> (AuditResult, Vec<String>) {
    let msgs = ctx.to_vec();
    let (new_ctx, repairs) = ContextAssembler::from_legacy(msgs);
    *ctx = new_ctx;

    let result = if !repairs.is_empty() {
        AuditResult::Repairable {
            issues: repairs.clone(),
            fix_description: format!("{} issue(s) repaired", repairs.len()),
        }
    } else {
        AuditResult::Clean
    };

    (result, repairs)
}

	#[cfg(test)]
mod tests {
    use super::*;
    use dsx_types::Message;

    fn make_msg(role: &str, content: &str) -> Message {
        Message {
            role: role.into(),
            content: Some(content.into()),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            thinking_signature: None,
        }
    }

    #[test]
    fn test_clean_session() {
        let msgs = vec![
            make_msg("user", "hello"),
            make_msg("assistant", "hi there"),
        ];
        assert_eq!(audit_session(&msgs), AuditResult::Clean);
    }

    #[test]
    #[ignore] // TODO Phase 4: ContextAssembler::from_legacy is a stub in dsx-hp.
    // The canonical implementation lives in dsx-agent::conversation.
    fn test_orphan_tool_use_detected() {
        let mut msg = make_msg("assistant", "");
        msg.tool_calls = Some(vec![dsx_types::ToolCall {
            id: "tc1".into(),
            call_type: "function".into(),
            function: dsx_types::FunctionCall {
                name: "test".into(),
                arguments: "{}".into(),
            },
        }]);
        let msgs = vec![make_msg("user", "do it"), msg];
        let result = audit_session(&msgs);
        assert!(matches!(result, AuditResult::Repairable { .. }));
    }
}
