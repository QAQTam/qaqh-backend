//! Audit — session integrity checking and /fix repair.
//!
//! Checks conversation state for alternation violations, orphan tool_uses,
//! and orphan tool_results. Runs repair via `ContextAssembler::from_legacy`
//! when /fix is invoked.
//!
//! Ported from `repair_messages()` and the alternation check in `health/mod.rs`.

use crate::conversation::ContextAssembler;
use crate::types::Message;

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
/// any repairs needed. Does **not** modify the input.
///
/// Ported from the alternation check in `health/mod.rs` and the
/// `repair_messages` flow in `tui/app.rs`.
pub fn audit_session(messages: &[Message]) -> AuditResult {
    // Check for obvious alternation violations first (fast path)
    if let Some(issue) = check_alternation(messages) {
        return AuditResult::Repairable {
            issues: vec![issue.to_string()],
            fix_description: "Alternation violation detected — run repair to fix.".into(),
        };
    }

    // Full structural check via ContextAssembler
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

/// Lightweight alternation check — scans a flat message list for
/// common patterns that would cause a 400 from the API.
///
/// Ported from `check_alternation` in `health/mod.rs`.
fn check_alternation(messages: &[Message]) -> Option<&'static str> {
    let mut i = 0;
    while i < messages.len() {
        if messages[i].role == "assistant" {
            let has_tc = messages[i].tool_calls.as_ref()
                .map(|t| !t.is_empty())
                .unwrap_or(false);
            if has_tc {
                let next = i + 1;
                if next >= messages.len() || messages[next].role != "tool" {
                    return Some("orphan tool_use — no tool_result follows");
                }
            }
        }
        if messages[i].role == "user" && i > 0 {
            if messages[i - 1].role == "user" {
                return Some("consecutive user messages");
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;

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
    fn test_orphan_tool_use_detected() {
        let mut msg = make_msg("assistant", "");
        msg.tool_calls = Some(vec![crate::types::ToolCall {
            id: "tc1".into(),
            call_type: "function".into(),
            function: crate::types::FunctionCall {
                name: "test".into(),
                arguments: "{}".into(),
            },
        }]);
        let msgs = vec![make_msg("user", "do it"), msg];
        let result = audit_session(&msgs);
        assert!(matches!(result, AuditResult::Repairable { .. }));
    }
}
