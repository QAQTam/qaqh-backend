use crate::agent::AgentState;
use crate::tokenizer;
use crate::tokenizer::TokenBreakdown;
use dsx_types::Message;

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

}

/// A single conversation turn: one user message + a chain of assistant steps.
#[derive(Debug, Clone)]
pub(crate) struct Turn {
    pub(crate) user: Message,
    /// Accumulated annotations for this turn (tagged notes like [health] ...).
    /// Rendered as part of the trailing context note in prepare_and_compact
    /// so they never mutate the stored user message.
    pub(crate) annotations: Vec<String>,
    pub(crate) steps: Vec<Step>,
}

impl Turn {
    fn new(user: Message) -> Self {
        Self {
            user,
            annotations: Vec::new(),
            steps: Vec::new(),
        }
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
    dirty: bool,
}

impl ContextAssembler {
    // ── Construction ──

    pub fn new() -> Self {
        Self {
            system_messages: Vec::new(),
            turns: Vec::new(),
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

    /// Record a per-turn annotation. Stored on the current turn and rendered in the
    /// trailing context note during prepare_and_compact(). Does NOT mutate user messages.
    /// If no turn exists yet, the annotation is silently dropped.
    pub fn annotate(&mut self, tag: &str, msg: &str) {
        if let Some(turn) = self.turns.last_mut() {
            turn.annotations.push(format!("[{}] {}", tag, msg));
            self.dirty = true;
        }
    }

    /// No-op — annotations are read from the Turn and rendered in the trailing
    /// context note during prepare_and_compact().
    pub fn flush_annotations(&mut self) {}

    /// Returns true if the current turn has any annotations.
    pub fn has_pending_annotations(&self) -> bool {
        self.turns.last().map_or(false, |t| !t.annotations.is_empty())
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

    // ── Build: conversation messages with truncation ──

    /// Return conversation messages (user/assistant/tool), system messages stripped.
    pub fn build(&self, _context_limit: u32) -> Vec<Message> {
        let mut msgs = self.to_openai_messages();
        msgs.retain(|m| m.role != "system");
        msgs
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

// ── build_context ──

/// Build context for the next API request.
///
/// # Cache-friendly design for DeepSeek KV cache
///
/// DeepSeek caches complete prefix units. Common prefixes across requests
/// are auto-detected and cached independently. We exploit this by layering
/// the system prompt: STABLE content first, DYNAMIC content last.
///
/// ```
/// System prompt layers (in order):
///   1. Base prompt          ← static (changes only when language switches)
///   2. Tool definitions     ← stable per session
///   3. Phase prompt         ← stable per phase (changes occasionally)
///   4. Active skills        ← dynamic (per user input)
///   5. Turn annotations     ← dynamic (per round)
///
/// Messages: pure conversation — user/assistant/tool only, no injected suffix.
/// ```
///
/// DeepSeek's common-prefix detection will independently cache layers 1-3
/// (which are identical across consecutive requests), so only layers 4-5
/// and new conversation messages miss cache.
pub fn build_context(state: &mut AgentState) -> (String, Vec<Message>, TokenBreakdown) {
    debug_assert_eq!(
        state.exec_pending, 0,
        "build_context called with {} exec(s) still pending",
        state.exec_pending,
    );

    let base_prompt = crate::config::system_prompt(&state.config.prompt_lang);
    let base_tokens = tokenizer::count_tokens(&base_prompt);

    // === System prompt: layered, stable→dynamic ===
    let mut system = base_prompt;

    // Layer 2: Tool definitions (stable per session)
    let tool_help = tool_help_text(&state.tool_defs);
    let tool_help_tokens = if tool_help.is_empty() { 0 } else {
        system.push_str("\n\n## Available Tools\n");
        system.push_str(&tool_help);
        tokenizer::count_tokens(&tool_help)
    };

    // Layer 3: Phase prompt (stable per phase)
    let phase = crate::router::read_phase();
    let phase_tokens = if let Some(suffix) = crate::router::phase_prompt_suffix(phase, &state.config.prompt_lang) {
        system.push_str("\n\n");
        system.push_str(suffix);
        tokenizer::count_tokens(suffix)
    } else { 0 };

    // Layer 4: Active skills (dynamic, per user input)
    let mut skill_tokens = 0u32;
    if !state.active_skill_bodies.is_empty() {
        let mut text = String::new();
        for (name, body) in &state.active_skill_bodies {
            let s = format!("### {}\n{}---\n", name, body);
            skill_tokens += tokenizer::count_tokens(&s);
            text.push_str(&s);
        }
        system.push_str("\n\n## Active Skills\n");
        system.push_str(&text);
    }

    // Layer 5: Turn annotations (dynamic, per round — health/gate/system notes)
    let mut annotation_tokens = 0u32;
    if !state.turn_annotations.is_empty() {
        let ann = state.turn_annotations.join("\n");
        annotation_tokens = tokenizer::count_tokens(&ann);
        system.push_str("\n\n## Notes\n");
        system.push_str(&ann);
    }
    // Always append the static reminder (stable, small)
    system.push_str("\n\n注意：status 工具只支持 plan/coding/debug 三种模式，explore/chat 已移除。");

    // Clear per-turn annotations for next round
    state.turn_annotations.clear();

    // === Messages: pure conversation, no suffix ===
    let messages = state.ctx.build(state.config.context_limit);

    // === Token breakdown ===
    let mut bd = TokenBreakdown::default();
    bd.system = base_tokens + tool_help_tokens;
    bd.episodic = estimate_message_tokens(&messages);
    bd.total = bd.system + bd.episodic;

    state.token_estimate = bd.total;
    state.token_breakdown = Some(bd);
    state.health.context_tokens = state.tokens_used();
    state.health.context_tier = crate::health::ContextTier::from_tokens(
        state.health.context_tokens, state.config.context_limit,
    );

    // Predict KV cache hit rate
    let report = state.cache_analyzer.record(&system, &messages);
    state.predicted_cache_hit_pct = report.hit_rate;

    log::info!(
        "context (tokens): base={} tools={} phase={} skills={} annotations={} messages={} total={}",
        base_tokens, tool_help_tokens, phase_tokens,
        skill_tokens, annotation_tokens, bd.episodic, bd.total,
    );

    (system, messages, bd)
}

/// Render tool definitions as a stable system-prompt help block for the model.
/// Stable within a session — only changes when tools are reloaded.
fn tool_help_text(defs: &[dsx_types::ToolDef]) -> String {
    if defs.is_empty() { return String::new(); }
    let mut out = String::new();
    for td in defs {
        out.push_str(&format!("- `{}`", td.function.name));
        if !td.function.description.is_empty() {
            out.push_str(&format!(": {}", td.function.description));
        }
        // Include parameter names from schema
        if let Some(params) = td.function.parameters.get("properties").and_then(|p| p.as_object()) {
            if !params.is_empty() {
                let names: Vec<&str> = params.keys().map(|k| k.as_str()).collect();
                out.push_str(&format!(" ({})", names.join(", ")));
            }
        }
        out.push('\n');
    }
    out
}
