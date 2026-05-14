//! ContextAssembler — canonical message container with strict alternation guarantees.
//!
//! Replaces raw `Vec<Message>` as the source of truth for all message state.
//! Uses a Turn+Step model that makes user→user, orphan tool_use, and orphan
//! tool_result structurally impossible. The only output path to the API is
//! `to_anthropic_messages()`, which is guaranteed valid by construction.
//!
//! ## Data model
//!
//! ```text
//! Conversation
//!   ├─ System messages (prompt, skills, semantic memory)
//!   ├─ Turn 0
//!   │    ├─ user: "fix this"
//!   │    └─ steps:
//!   │         ├─ Step 0: assistant(tool_use: read_file) → tool_result
//!   │         └─ Step 1: assistant(tool_use: write_file) → tool_result
//!   ├─ Turn 1
//!   │    ├─ user: "now add tests"
//!   │    └─ steps:
//!   │         └─ Step 0: assistant("done")
//! ```
//!
//! ## Invariants enforced at type level
//! - Turns alternate: user → assistant(s) → user → assistant(s)
//! - Each Step: one assistant, zero or more matching tool_results
//! - Tool results always match a tool_use in the same step
//! - No consecutive user messages
//! - System notes folded into content, never create new messages
//!
//! ## Anthropic format output
//!
//! ```text
//! user("fix this")
//!   → assistant(tool_use: read_file)
//!   → user(tool_result: read_file)
//!   → assistant(tool_use: write_file)
//!   → user(tool_result: write_file)
//! user("now add tests")
//!   → assistant("done")
//! ```

use crate::types::{AnthropicContent, AnthropicMessage, Message};

/// Errors the assembler can surface when invariants would be violated.
#[derive(Debug, Clone, PartialEq)]
pub enum AssemblerError {
    /// Attempted to push a user message while the current turn has incomplete steps.
    TurnIncomplete { missing: String },
    /// Attempted to push an assistant while no user message is pending.
    NoUserPending,
    /// Attempted to push a tool result referencing a tool_call_id not present
    /// in the current step's tool_calls.
    OrphanToolResult { tool_call_id: String },
    /// Attempted to push tool result but current step has no tool_calls.
    NoToolUseInStep,
}

// ── Data model ──

/// One assistant response + its tool results.
#[derive(Debug, Clone)]
pub(crate) struct Step {
    pub(crate) assistant: Message,
    pub(crate) tool_results: Vec<Message>,
}

impl Step {
    fn new(assistant: Message) -> Self {
        Self { assistant, tool_results: Vec::new() }
    }

    fn has_tool_call(&self, id: &str) -> bool {
        self.assistant.tool_calls.as_ref()
            .map(|tcs| tcs.iter().any(|tc| tc.id == id))
            .unwrap_or(false)
    }

    fn all_tools_satisfied(&self) -> bool {
        let Some(ref tcs) = self.assistant.tool_calls else { return true };
        if tcs.is_empty() { return true; }
        tcs.iter().all(|tc| {
            self.tool_results.iter().any(|tr| tr.tool_call_id.as_deref() == Some(&tc.id))
        })
    }

    fn missing_tool_count(&self) -> usize {
        let Some(ref tcs) = self.assistant.tool_calls else { return 0 };
        tcs.iter().filter(|tc| {
            !self.tool_results.iter().any(|tr| tr.tool_call_id.as_deref() == Some(&tc.id))
        }).count()
    }

    fn pending_tool_ids(&self) -> Vec<String> {
        let Some(ref tcs) = self.assistant.tool_calls else { return vec![] };
        tcs.iter()
            .filter(|tc| !self.tool_results.iter().any(|tr| tr.tool_call_id.as_deref() == Some(&tc.id)))
            .map(|tc| tc.id.clone())
            .collect()
    }

    fn has_pending_tools(&self) -> bool {
        self.assistant.tool_calls.as_ref()
            .map(|tcs| !tcs.is_empty())
            .unwrap_or(false)
    }
}

/// A single conversation turn: one user message + a chain of assistant steps.
#[derive(Debug, Clone)]
pub(crate) struct Turn {
    pub(crate) user: Message,
    pub(crate) steps: Vec<Step>,
}

impl Turn {
    fn new(user: Message) -> Self {
        Self { user, steps: Vec::new() }
    }

    /// The current step (last in the chain).
    fn current_step(&self) -> Option<&Step> {
        self.steps.last()
    }

    fn current_step_mut(&mut self) -> Option<&mut Step> {
        self.steps.last_mut()
    }

    /// All steps complete? (every step's tool_uses satisfied)
    fn all_steps_satisfied(&self) -> bool {
        self.steps.iter().all(|s| s.all_tools_satisfied())
    }

    /// Total missing tool results across all incomplete steps.
    fn total_missing_tools(&self) -> usize {
        self.steps.iter().map(|s| s.missing_tool_count()).sum()
    }

    /// Find a step that owns a specific tool_call_id.
    fn find_step_for(&self, tool_call_id: &str) -> Option<&Step> {
        self.steps.iter().find(|s| s.has_tool_call(tool_call_id))
    }

    fn find_step_for_mut(&mut self, tool_call_id: &str) -> Option<&mut Step> {
        self.steps.iter_mut().find(|s| s.has_tool_call(tool_call_id))
    }
}

// ── ContextAssembler ──

/// The canonical message container.
///
/// All message mutations go through this assembler. It is the single source of
/// truth for message state and the only path to produce Anthropic-format output.
#[derive(Debug, Clone)]
pub struct ContextAssembler {
    system_messages: Vec<Message>,
    turns: Vec<Turn>,
    pending_annotations: Vec<String>,
    dirty: bool,
}

impl ContextAssembler {
    // ── Construction ──

    pub fn new() -> Self {
        Self {
            system_messages: Vec::new(),
            turns: Vec::new(),
            pending_annotations: Vec::new(),
            dirty: false,
        }
    }

    // ── System messages ──

    pub fn push_system(&mut self, msg: Message) {
        assert_eq!(msg.role, "system", "push_system requires role=system");
        self.system_messages.push(msg);
        self.dirty = true;
    }

    pub fn set_system_messages(&mut self, msgs: Vec<Message>) {
        self.system_messages = msgs;
        self.dirty = true;
    }

    // ── User messages ──

    /// Push a user message. Actively rejects if the current turn has unfulfilled tool calls.
    pub fn push_user(&mut self, text: &str) -> Result<(), AssemblerError> {
        if self.has_unfulfilled_tool_calls() {
            return Err(AssemblerError::TurnIncomplete {
                missing: format!(
                    "{} step(s) with missing tool results — cannot push user yet",
                    self.turns.last().map(|t| t.total_missing_tools()).unwrap_or(0)
                ),
            });
        }
        if let Some(last) = self.turns.last() {
            if last.steps.is_empty() {
                return Err(AssemblerError::TurnIncomplete {
                    missing: "current turn has no assistant response yet".into(),
                });
            }
        }
        self.turns.push(Turn::new(Message::user(text)));
        self.dirty = true;
        Ok(())
    }

    /// Push user unconditionally — RESTORE ONLY. Do not use in normal flow.
    /// Skipped checks: turn completeness, alternation.
    #[doc(hidden)]
    pub fn push_user_restore(&mut self, text: &str) {
        if let Some(last) = self.turns.last_mut() {
            if last.steps.is_empty() {
                last.user.content = Some(format!(
                    "{}\n\n{}",
                    last.user.content.clone().unwrap_or_default(),
                    text
                ));
                self.dirty = true;
                return;
            }
        }
        self.turns.push(Turn::new(Message::user(text)));
        self.dirty = true;
    }

    // ── Assistant messages ──

    /// Push an assistant response. Creates a new Step in the current turn.
    /// Actively rejects if current turn has unfulfilled tool calls.
    pub fn push_assistant(&mut self, msg: Message) -> Result<(), AssemblerError> {
        assert_eq!(msg.role, "assistant");
        let turn = self.turns.last_mut().ok_or(AssemblerError::NoUserPending)?;
        // Previous step must be satisfied before starting a new one
        if self.has_unfulfilled_tool_calls() {
            if let Some(last_step) = turn.current_step() {
                return Err(AssemblerError::TurnIncomplete {
                    missing: format!(
                        "{} tool result(s) missing from previous step",
                        last_step.missing_tool_count()
                    ),
                });
            }
        }
        turn.steps.push(Step::new(msg));
        self.dirty = true;
        Ok(())
    }

    /// Push assistant without validation — RESTORE ONLY. Do not use in normal flow.
    /// Skipped checks: turn existence, alternation, pending tools.
    #[doc(hidden)]
    pub fn push_assistant_restore(&mut self, msg: Message) {
        if self.turns.is_empty() {
            self.turns.push(Turn::new(Message::user("")));
        }
        let turn = self.turns.last_mut().unwrap();
        turn.steps.push(Step::new(msg));
        self.dirty = true;
    }

    // ── Tool results ──

    /// Push a tool result to the CURRENT step. Validates tool_call_id exists.
    pub fn push_tool_result(&mut self, tool_call_id: &str, result: &str) -> Result<(), AssemblerError> {
        let turn = self.turns.last_mut()
            .ok_or(AssemblerError::OrphanToolResult { tool_call_id: tool_call_id.into() })?;
        let step = turn.current_step_mut()
            .ok_or(AssemblerError::NoToolUseInStep)?;

        if !step.has_tool_call(tool_call_id) {
            return Err(AssemblerError::OrphanToolResult { tool_call_id: tool_call_id.into() });
        }

        if !step.tool_results.iter().any(|tr| tr.tool_call_id.as_deref() == Some(tool_call_id)) {
            step.tool_results.push(Message::tool(tool_call_id, result));
            self.dirty = true;
        }
        Ok(())
    }

    /// Push a tool result by searching all turns for the matching assistant.
    /// Used by async exec results that may arrive across turns.
    pub fn push_tool_result_for(&mut self, tool_call_id: &str, result: &str) -> Result<(), AssemblerError> {
        for turn in self.turns.iter_mut().rev() {
            if let Some(step) = turn.find_step_for_mut(tool_call_id) {
                if !step.tool_results.iter().any(|tr| tr.tool_call_id.as_deref() == Some(tool_call_id)) {
                    step.tool_results.push(Message::tool(tool_call_id, result));
                    self.dirty = true;
                }
                return Ok(());
            }
        }
        Err(AssemblerError::OrphanToolResult { tool_call_id: tool_call_id.into() })
    }

    // ── Annotations ──

    pub fn annotate(&mut self, tag: &str, msg: &str) {
        self.pending_annotations.push(format!("[{}] {}", tag, msg));
    }

    pub fn flush_annotations(&mut self) {
        if self.pending_annotations.is_empty() { return; }
        let block = self.pending_annotations.join("\n");
        self.pending_annotations.clear();
        let text = format!("\n\n[System Notes]\n{}", block);

        // Append to last assistant's content, or last user if no assistant
        if let Some(turn) = self.turns.last_mut() {
            if let Some(step) = turn.steps.last_mut() {
                step.assistant.content = match step.assistant.content.take() {
                    Some(mut c) => { c.push_str(&text); Some(c) }
                    None => Some(text),
                };
            } else {
                turn.user.content = match turn.user.content.take() {
                    Some(mut c) => { c.push_str(&text); Some(c) }
                    None => Some(text),
                };
            }
        }
        self.dirty = true;
    }

    // ── Validation ──
    //
    // CANONICAL implementation — all check_alternation call sites in
    // gate.rs, audit.rs, and checks.rs have been consolidated here.
    // This function checks: orphan tool_use/tool_result, turn/step
    // completeness, system role correctness. Do NOT add parallel
    // alternation checks elsewhere.

    pub fn validate(&self) -> Result<(), String> {
        if self.turns.is_empty() && self.system_messages.is_empty() {
            return Ok(()); // empty is valid
        }
        for (i, turn) in self.turns.iter().enumerate() {
            if turn.steps.is_empty() && i < self.turns.len() - 1 {
                return Err(format!("Turn {}: no assistant steps (not last turn)", i));
            }
            for (j, step) in turn.steps.iter().enumerate() {
                if !step.all_tools_satisfied() && j < turn.steps.len() - 1 {
                    return Err(format!("Turn {} step {}: incomplete but not last step", i, j));
                }
                for tr in &step.tool_results {
                    let Some(ref tid) = tr.tool_call_id else { continue };
                    if !step.has_tool_call(tid) {
                        return Err(format!("Turn {} step {}: orphan tool_result {}", i, j, tid));
                    }
                }
            }
        }
        for (i, m) in self.system_messages.iter().enumerate() {
            if m.role != "system" {
                return Err(format!("System[{}]: expected role=system, got {}", i, m.role));
            }
        }
        Ok(())
    }

    pub fn ready_for_api(&self) -> Result<(), String> {
        self.validate()?;
        if self.turns.is_empty() {
            return Err("No conversation turns".into());
        }
        let last = self.turns.last().unwrap();
        if last.steps.is_empty() {
            return Err("Last turn has no assistant response".into());
        }
        if !last.all_steps_satisfied() {
            return Err(format!("{} missing tool result(s)", last.total_missing_tools()));
        }
        Ok(())
    }

    // ── Conversion to Anthropic format (guaranteed valid by debug_assert) ──

    pub fn to_anthropic_messages(&self) -> (Option<String>, Vec<AnthropicMessage>) {
        // Validate ALL steps are complete before emitting.
        // Unsatisfied tool_uses → assistant without matching tool_result → API 400.
        debug_assert!({
            let mut ok = true;
            for turn in &self.turns {
                for step in &turn.steps {
                    if !step.all_tools_satisfied() {
                        ok = false;
                    }
                }
            }
            ok
        }, "to_anthropic_messages called with unsatisfied tool_uses — would cause API 400");
        let system = if self.system_messages.is_empty() {
            None
        } else {
            Some(self.system_messages.iter()
                .filter_map(|m| m.content.clone())
                .collect::<Vec<_>>()
                .join("\n\n"))
        };

        let mut out = Vec::new();

        for turn in &self.turns {
            // User message
            let user_text = turn.user.content.clone().unwrap_or_default();
            out.push(AnthropicMessage {
                role: "user".into(),
                content: vec![AnthropicContent::Text { text: user_text }],
            });

            // Each step: assistant → user(tool_results if any)
            for step in &turn.steps {
                // Assistant
                let mut blocks = Vec::new();
                if let Some(ref r) = step.assistant.reasoning_content {
                    if !r.is_empty() {
                        blocks.push(AnthropicContent::Thinking {
                            thinking: r.clone(),
                            signature: step.assistant.thinking_signature.clone()
                                .unwrap_or_else(|| "dsx-session".into()),
                        });
                    }
                }
                if let Some(ref text) = step.assistant.content {
                    if !text.is_empty() {
                        blocks.push(AnthropicContent::Text { text: text.clone() });
                    }
                }
                if let Some(ref tcs) = step.assistant.tool_calls {
                    for tc in tcs {
                        let input: serde_json::Value = serde_json::from_str(&tc.function.arguments)
                            .unwrap_or(serde_json::Value::Null);
                        blocks.push(AnthropicContent::ToolUse {
                            id: tc.id.clone(),
                            name: tc.function.name.clone(),
                            input,
                        });
                    }
                }
                out.push(AnthropicMessage { role: "assistant".into(), content: blocks });

                // Tool results (bundled as one user message)
                if !step.tool_results.is_empty() {
                    let tr_blocks: Vec<AnthropicContent> = step.tool_results.iter()
                        .map(|tr| {
                            let text = tr.content.clone().unwrap_or_default();
                            let is_error = text.starts_with("[ERROR]") || text.starts_with("[FAIL]");
                            AnthropicContent::ToolResult {
                                tool_use_id: tr.tool_call_id.clone().unwrap_or_default(),
                                content: text,
                                is_error: if is_error { Some(true) } else { None },
                            }
                        })
                        .collect();
                    out.push(AnthropicMessage { role: "user".into(), content: tr_blocks });
                }
            }
        }

        (system, out)
    }

    // ── Flat view ──

    /// Collect all messages into a flat Vec for UI rendering and session persistence.
    pub fn to_vec(&self) -> Vec<Message> {
        let mut v: Vec<Message> = self.system_messages.clone();
        for turn in &self.turns {
            v.push(turn.user.clone());
            for step in &turn.steps {
                v.push(step.assistant.clone());
                v.extend(step.tool_results.clone());
            }
        }
        v
    }

    pub fn message_count(&self) -> usize {
        self.to_vec().len()
    }

    pub fn turn_count(&self) -> usize {
        self.turns.len()
    }

    // ── State queries ──

    /// Read-only access to all turns for ContextBuilder compaction.
    /// Turn+Step structure is preserved — no flatten round-trip needed.
    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    /// True if the current turn has any unfulfilled tool calls in any step.
    /// Used by push_user and push_assistant to actively reject invalid state transitions.
    pub fn has_unfulfilled_tool_calls(&self) -> bool {
        self.turns.last()
            .map(|t| !t.all_steps_satisfied())
            .unwrap_or(false)
    }

    pub fn last_turn(&self) -> Option<&Turn> {
        self.turns.last()
    }

    pub fn last_turn_mut(&mut self) -> Option<&mut Turn> {
        self.turns.last_mut()
    }

    /// Whether the last step has unsatisfied tool calls.
    pub fn has_pending_tools(&self) -> bool {
        self.turns.last()
            .and_then(|t| t.current_step())
            .map(|s| !s.all_tools_satisfied())
            .unwrap_or(false)
    }

    pub fn pending_tool_count(&self) -> usize {
        self.turns.last()
            .and_then(|t| t.current_step())
            .map(|s| s.missing_tool_count())
            .unwrap_or(0)
    }

    pub fn pending_tool_call_ids(&self) -> Vec<String> {
        self.turns.last()
            .and_then(|t| t.current_step())
            .map(|s| s.pending_tool_ids())
            .unwrap_or_default()
    }

    pub fn last_assistant_tool_calls(&self) -> Option<&Vec<crate::types::ToolCall>> {
        self.turns.last()
            .and_then(|t| t.current_step())
            .and_then(|s| s.assistant.tool_calls.as_ref())
    }

    pub fn is_dirty(&self) -> bool { self.dirty }
    pub fn mark_clean(&mut self) { self.dirty = false; }

    // ── Import from legacy Vec<Message> (session restore) ──

    pub fn from_legacy(msgs: Vec<Message>) -> (Self, Vec<String>) {
        let mut assembler = Self::new();
        let mut repairs = Vec::new();
        let mut i = 0;

        while i < msgs.len() && msgs[i].role == "system" {
            assembler.system_messages.push(msgs[i].clone());
            i += 1;
        }

        while i < msgs.len() {
            match msgs[i].role.as_str() {
                "user" => {
                    let text = msgs[i].content.clone().unwrap_or_default();
                    let _ = assembler.push_user(&text);
                    i += 1;
                }
                "assistant" => {
                    let msg = msgs[i].clone();
                    assembler.push_assistant_restore(msg);
                    i += 1;
                }
                "tool" => {
                    let tc_id = msgs[i].tool_call_id.clone().unwrap_or_default();
                    let result = msgs[i].content.clone().unwrap_or_default();
                    match assembler.push_tool_result_for(&tc_id, &result) {
                        Ok(()) => {}
                        Err(AssemblerError::OrphanToolResult { .. }) => {
                            repairs.push(format!("orphan tool_result {} deleted on import", tc_id));
                        }
                        Err(e) => {
                            repairs.push(format!("import error: {:?}", e));
                        }
                    }
                    i += 1;
                }
                _ => { i += 1; }
            }
        }

        // Repair: inject [ERROR] for orphan tool_uses
        for turn in assembler.turns.iter_mut() {
            for step in turn.steps.iter_mut() {
                let missing: Vec<(String, String)> = {
                    let Some(ref tcs) = step.assistant.tool_calls else { continue };
                    tcs.iter()
                        .filter(|tc| !step.tool_results.iter().any(|tr| tr.tool_call_id.as_deref() == Some(&tc.id)))
                        .map(|tc| (tc.id.clone(), tc.function.name.clone()))
                        .collect()
                };
                for (id, name) in missing {
                    let err = format!(
                        "[ERROR] Tool '{}' was cancelled — session was interrupted before result arrived.\n[HINT] Do NOT retry the same call.",
                        name
                    );
                    step.tool_results.push(Message::tool(&id, &err));
                    repairs.push(format!("injected [ERROR] for orphan tool_use {}", id));
                }
            }
        }

        (assembler, repairs)
    }

    // ── Remove last incomplete step (for stream cancel recovery) ──

    /// If the last step has unsatisfied tools, remove it.
    /// Used when the user cancels streaming during a tool_use response.
    pub fn remove_last_step_if_incomplete(&mut self) -> bool {
        if let Some(turn) = self.turns.last_mut() {
            if let Some(step) = turn.steps.last() {
                if !step.all_tools_satisfied() {
                    turn.steps.pop();
                    self.dirty = true;
                    return true;
                }
            }
        }
        false
    }

    /// Remove the last step entirely (cancelled assistant).
    pub fn remove_last_step(&mut self) {
        if let Some(turn) = self.turns.last_mut() {
            turn.steps.pop();
            self.dirty = true;
        }
    }
}
