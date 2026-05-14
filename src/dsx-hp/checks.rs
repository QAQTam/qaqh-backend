//! Message alternation validation.
//!
//! Checks for structural alternation patterns that could trigger API 400 errors.

use crate::types::Message;

use super::DsAgentsHealthPlatform;

impl DsAgentsHealthPlatform {
    /// Validate message alternation before API request.
    /// Returns Some(issue) if a 400-risk pattern is detected.
    pub fn check_alternation(&self, messages: &[Message]) -> Option<&'static str> {
        let mut i = 0;
        while i < messages.len() {
            if messages[i].role == "assistant" {
                let has_tc = messages[i]
                    .tool_calls
                    .as_ref()
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
}
