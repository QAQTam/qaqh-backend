//! ContextAssembler — canonical message container with strict alternation guarantees.
//!
//! Replaces raw `Vec<Message>` as the source of truth for all message state.
//! Uses a Turn+Step model that makes user->user, orphan tool_use, and orphan
//! tool_result structurally impossible.
//!
//! This is the SAME implementation as dsx-agent::conversation::ContextAssembler,
//! duplicated here so dsx-hp (a separate crate) can perform gate/audit validation
//! without depending on dsx-agent.

use dsx_types::Message;

// ── Errors ──

#[derive(Debug, Clone, PartialEq)]
pub enum AssemblerError {
    TurnIncomplete { missing: String },
    NoUserPending,
    OrphanToolResult { tool_call_id: String },
    NoToolUseInStep,
}

// ── Data model ──

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
}

#[derive(Debug, Clone)]
pub(crate) struct Turn {
    pub(crate) user: Message,
    pub(crate) steps: Vec<Step>,
}

impl Turn {
    fn new(user: Message) -> Self {
        Self { user, steps: Vec::new() }
    }

    fn current_step(&self) -> Option<&Step> {
        self.steps.last()
    }

    fn current_step_mut(&mut self) -> Option<&mut Step> {
        self.steps.last_mut()
    }

    fn all_steps_satisfied(&self) -> bool {
        self.steps.iter().all(|s| s.all_tools_satisfied())
    }

    fn total_missing_tools(&self) -> usize {
        self.steps.iter().map(|s| s.missing_tool_count()).sum()
    }

    fn find_step_for(&self, tool_call_id: &str) -> Option<&Step> {
        self.steps.iter().find(|s| s.has_tool_call(tool_call_id))
    }

    fn find_step_for_mut(&mut self, tool_call_id: &str) -> Option<&mut Step> {
        self.steps.iter_mut().find(|s| s.has_tool_call(tool_call_id))
    }
}

// ── ContextAssembler ──

#[derive(Debug, Clone)]
pub struct ContextAssembler {
    system_messages: Vec<Message>,
    turns: Vec<Turn>,
    pending_annotations: Vec<String>,
    dirty: bool,
}

impl ContextAssembler {
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
        assert_eq!(msg.role, "system");
        self.system_messages.push(msg);
        self.dirty = true;
    }

    pub fn set_system_messages(&mut self, msgs: Vec<Message>) {
        self.system_messages = msgs;
        self.dirty = true;
    }

    // ── User messages ──

    pub fn push_user(&mut self, text: &str) -> Result<(), AssemblerError> {
        if self.has_unfulfilled_tool_calls() {
            return Err(AssemblerError::TurnIncomplete {
                missing: format!(
                    "{} step(s) with missing tool results",
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

    pub fn push_assistant(&mut self, msg: Message) -> Result<(), AssemblerError> {
        assert_eq!(msg.role, "assistant");
        let has_unfulfilled = self.has_unfulfilled_tool_calls();
        let turn = self.turns.last_mut().ok_or(AssemblerError::NoUserPending)?;
        if has_unfulfilled {
            if let Some(last_step) = turn.current_step() {
                return Err(AssemblerError::TurnIncomplete {
                    missing: format!("{} tool result(s) missing", last_step.missing_tool_count()),
                });
            }
        }
        turn.steps.push(Step::new(msg));
        self.dirty = true;
        Ok(())
    }

    pub fn push_assistant_restore(&mut self, msg: Message) {
        if self.turns.is_empty() {
            self.turns.push(Turn::new(Message::user("")));
        }
        let turn = self.turns.last_mut().unwrap();
        turn.steps.push(Step::new(msg));
        self.dirty = true;
    }

    // ── Tool results ──

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

    pub fn validate(&self) -> Result<(), String> {
        if self.turns.is_empty() && self.system_messages.is_empty() {
            return Ok(());
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

    // ── Conversion to OpenAI format ──

    pub fn to_openai_messages(&self) -> Vec<Message> {
        let mut out = self.system_messages.clone();
        for turn in &self.turns {
            out.push(turn.user.clone());
            for step in &turn.steps {
                out.push(step.assistant.clone());
                out.extend(step.tool_results.clone());
            }
        }
        out
    }

    // ── Flat view ──

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

    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

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

    pub fn last_assistant_tool_calls(&self) -> Option<&Vec<dsx_types::ToolCall>> {
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
                    let note = format!(
                        "[RESTORE] Tool '{}' had no result when session was saved — not executed.\n[HINT] Do NOT retry.",
                        name
                    );
                    step.tool_results.push(Message::tool(&id, &note));
                    repairs.push(format!("injected [RESTORE] for orphan tool_use {}", id));
                }
            }
        }

        (assembler, repairs)
    }

    // ── Remove last incomplete step ──

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

    pub fn remove_last_step(&mut self) {
        if let Some(turn) = self.turns.last_mut() {
            turn.steps.pop();
            self.dirty = true;
        }
    }
}
