use crate::agent::AgentState;
use crate::tokenizer;
use crate::tokenizer::TokenBreakdown;
use dsx_types::Message;

/// CompactLevel: simplified — only None and L1 (output truncation).
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub enum CompactLevel {
    None = 0,
    L1 = 1,
}

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
        // Check before mutable borrow
        let has_unfulfilled = self.has_unfulfilled_tool_calls();
        let turn = self.turns.last_mut().ok_or(AssemblerError::NoUserPending)?;
        // Previous step must be satisfied before starting a new one
        if has_unfulfilled {
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

    /// Output messages in OpenAI-compatible format.
    /// System messages are prepended, tool results are separate "tool" role messages.
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

    /// Read-only access to all turns for compaction.
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

        // Repair: inject neutral [RESTORE] note for orphan tool_uses
        // (satisfies API tool_call/tool_result alternation without faking errors)
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

    // ── Build: layered context assembly ──

    /// Build the final Anthropic-formatted output with token tracking.
    /// Layer order: 0=system_prompt, 1=long_mem, 2=semantic, 3=episodic (compacted L1).
    pub fn build(&self, system_prompt: &str, long_mem_block: &str, semantic_block: &str, _context_limit: u32) -> (String, Vec<Message>, TokenBreakdown) {
        let mut bd = TokenBreakdown::default();
        bd.system = tokenizer::count_tokens(system_prompt);
        bd.long_mem = if long_mem_block.is_empty() { 0 } else { tokenizer::count_tokens(long_mem_block) };
        bd.semantic = if semantic_block.is_empty() { 0 } else { tokenizer::count_tokens(semantic_block) };
        let mut episodic_msgs = self.to_openai_messages();
        episodic_msgs.retain(|m| m.role != "system");
        let _episodic_tokens = estimate_message_tokens(&episodic_msgs);
        if !episodic_msgs.is_empty() {
            apply_l1_truncation(&mut episodic_msgs);
        }
        bd.episodic = estimate_message_tokens(&episodic_msgs);
        let mut system_parts: Vec<&str> = Vec::with_capacity(4);
        if !system_prompt.is_empty() { system_parts.push(system_prompt); }
        if !long_mem_block.is_empty() { system_parts.push(long_mem_block); }
        if !semantic_block.is_empty() { system_parts.push(semantic_block); }
        let system = system_parts.join("\n\n");
        bd.total = bd.system + bd.long_mem + bd.semantic + bd.episodic;
        (system, episodic_msgs, bd)
    }
}

/// Apply L1 truncation to OpenAI-format messages (truncate long "tool" role outputs).
fn apply_l1_truncation(msgs: &mut [Message]) {
    for msg in msgs.iter_mut() {
        if msg.role != "tool" { continue; }
        if let Some(ref mut content) = msg.content {
            let line_count = content.lines().count();
            if line_count > 100 {
                let lines: Vec<&str> = content.lines().collect();
                let head = lines[..50].join("\n");
                let tail = lines[lines.len().saturating_sub(50)..].join("\n");
                *content = format!("{}\n... ({} lines trimmed) ...\n{}", head, line_count - 100, tail);
            }
            if content.len() > 15000 {
                let head_end = char_boundary(content, 5000);
                let tail_start = char_boundary(content, content.len().saturating_sub(2000));
                let head = content[..head_end].to_string();
                let tail = content[tail_start..].to_string();
                *content = format!("{}\n\n... ({} chars trimmed) ...\n\n{}", head, content.len() - 7000, tail);
            }
        }
    }
}

/// Rough token estimation for OpenAI-format messages.
fn estimate_message_tokens(msgs: &[Message]) -> u32 {
    let mut t = 0u32;
    for msg in msgs {
        t += 4;
        if let Some(ref content) = msg.content {
            t += tokenizer::count_tokens(content);
        }
        if let Some(ref rc) = msg.reasoning_content {
            t += tokenizer::count_tokens(rc);
        }
        if let Some(ref tcs) = msg.tool_calls {
            for tc in tcs {
                t += tokenizer::count_tokens(&tc.function.name);
                t += tokenizer::count_tokens(&tc.function.arguments);
                t += 8;
            }
        }
    }
    t
}

// ── Helpers ──

fn char_boundary(s: &str, pos: usize) -> usize {
    if pos >= s.len() { return s.len(); }
    let mut p = pos;
    while p > 0 && !s.is_char_boundary(p) { p -= 1; }
    p
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max { return s.to_string(); }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) { end -= 1; }
    format!("{}…", &s[..end])
}

// ── prepare_and_compact ──

/// Build the compacted+prepped Anthropic-formatted context for next API request.
pub fn prepare_and_compact(state: &mut AgentState) -> (String, Vec<Message>, TokenBreakdown) {
    let prompt = crate::config::system_prompt(&state.config.prompt_lang);

    // Prepend tool definitions to system prompt (stable prefix for caching)
    let mut prompt_with_tools = prompt;
    if !state.tool_defs.is_empty() {
        prompt_with_tools.push_str("\n\n## Available Tools\n");
        for td in &state.tool_defs {
            prompt_with_tools.push_str(&format!("- {}: {}\n", td.function.name, td.function.description));
        }
        prompt_with_tools.push_str("Use these tools by their exact name.");
    }

    let semantic_block = state.semantic_memory.render();
    let long_mem_block = state.long_mem.render();

    let (mut system, messages, breakdown) = state.ctx.build(
        &prompt_with_tools,
        &semantic_block,
        &long_mem_block,
        state.config.context_limit,
    );

    state.token_estimate = breakdown.total;
    state.token_breakdown = Some(breakdown);
    state.health.context_tokens = state.tokens_used();
    state.health.context_tier = crate::health::ContextTier::from_tokens(
        state.health.context_tokens, state.config.context_limit,
    );

    // Insert pitfall guide as a system-prompt suffix block
    let pitfall_block = state.pitfall_guide.render(3);
    if !pitfall_block.is_empty() {
        system.push_str("\n\n");
        system.push_str(&pitfall_block);
    }

    // Append skill bodies to system prompt
    if !state.active_skill_bodies.is_empty() {
        system.push_str("\n\n## Active Skills\n");
        for (name, body) in &state.active_skill_bodies {
            system.push_str(&format!("### {}\n{}\n---\n", name, body));
        }
    }

    // Phase prompt suffix (model info + mode description)
    let phase = crate::router::read_phase();
    if let Some(suffix) = crate::router::phase_prompt_suffix(phase, &state.config.prompt_lang) {
        system.push_str(suffix);
    }

    // Notify on first turn that modes have changed (for backward compat)
    if state.ctx.turn_count() == 0 {
        system.push_str("\n\n注意：status 工具只支持 plan/coding/debug 三种模式，explore/chat 已移除。");
    }

    (system, messages, breakdown)
}
