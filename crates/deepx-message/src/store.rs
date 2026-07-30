use crate::effect::{Effect, PendingTool, ToolExecRequest, ToolExecutorFn};
use deepx_session::SessionManager;
use deepx_types::{Message, ToolDef};

/// Truncate tool result for LLM context. Tools return full output for the user,
/// but long results are trimmed here before storage to keep KV-cache prefixes
/// stable across turns.
///
/// - JSON results: truncates only the `content` field, preserves metadata.
/// - Truncation markers tell the model how to retrieve the omitted portion.
/// - Line-oriented tools snap to a preceding newline; others cut at a UTF-8 boundary.
fn is_line_oriented_tool(tool_name: &str) -> bool {
    matches!(tool_name, "read" | "search" | "diff") || tool_name.starts_with("file")
}

fn truncation_hint(tool_name: &str) -> &'static str {
    match tool_name {
        "read" => "Call read again with the same path and a later start_line/end_line range.",
        "search" => "Call search again with a narrower pattern, glob, or path.",
        "exec" => "Call exec again with narrower argv or a filtering command.",
        "web" => "Call web again with a narrower URL or query.",
        _ => "Call this tool again with narrower arguments to retrieve the omitted portion.",
    }
}

fn truncation_marker(tool_name: &str, total_chars: usize) -> String {
    format!(
        "[truncated: {total_chars} total chars. {}]",
        truncation_hint(tool_name)
    )
}

fn truncate_tool_result(tool_name: &str, result: &str) -> String {
    // ── JSON-aware truncation ──
    if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(result) {
        if let Some(content) = v.get("content").and_then(|c| c.as_str()) {
            let limit = 4000usize;
            if content.len() > limit {
                let cut = if is_line_oriented_tool(tool_name) {
                    let head = &content[..content.floor_char_boundary(limit)];
                    head.rfind('\n').map(|n| n + 1).unwrap_or(head.len())
                } else {
                    content.floor_char_boundary(limit)
                };
                let truncated = format!(
                    "{}...\n{}",
                    &content[..cut],
                    truncation_marker(tool_name, content.len())
                );
                v.as_object_mut()
                    .unwrap()
                    .insert("content".into(), serde_json::Value::String(truncated));
                v.as_object_mut()
                    .unwrap()
                    .insert("truncated".into(), serde_json::Value::Bool(true));
            }
            return v.to_string();
        }
        // No content field — return as-is
        return result.to_string();
    }

    // ── Plain string truncation (legacy) ──
    let limit: Option<usize> = if is_line_oriented_tool(tool_name) {
        Some(4000)
    } else if tool_name.starts_with("web") {
        Some(4000)
    } else if tool_name.starts_with("exec") {
        Some(4000)
    } else if tool_name.starts_with("task") || tool_name.starts_with("plan") {
        Some(4000)
    } else if tool_name.starts_with("git") || tool_name.starts_with("process") {
        Some(4000)
    } else {
        None
    };
    let Some(limit) = limit else {
        return result.to_string();
    };
    if result.len() <= limit {
        return result.to_string();
    }

    let cut = if is_line_oriented_tool(tool_name) {
        // Snap to previous newline so code lines stay intact.
        let head = &result[..result.floor_char_boundary(limit)];
        head.rfind('\n').map(|n| n + 1).unwrap_or(head.len())
    } else {
        result.floor_char_boundary(limit)
    };

    let mut out = result[..cut].to_string();
    out.push_str(&format!(
        "\n... {}",
        truncation_marker(tool_name, result.len())
    ));
    out
}

/// Fold a completed-turn tool result into a compact status summary.
/// Deterministic: same input → same output, preserving KV-cache prefix stability.
///
/// - `skills` remains complete because its activation instructions are active
///   context. High-volume read/search results are converted into a stable
///   retrieval reference once they are no longer in the active step.
/// - JSON results: preserve metadata, replace content with a fold marker.
/// - Plain results: keep only the first non-empty line (status + key metadata).
fn fold_completed_tool_result(tool_name: &str, result: &str) -> String {
    // Active skill instructions are an explicit part of the system context and
    // must not be folded into an opaque marker.
    if tool_name == "skills" {
        return result.to_string();
    }

    if matches!(tool_name, "read" | "search" | "diff") {
        return fold_retrieval_result(tool_name, result);
    }

    if matches!(tool_name, "edit" | "edit_block" | "write") {
        return fold_file_mutation_result(tool_name, result);
    }

    // ── JSON-aware folding ──
    if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(result) {
        let hint = if tool_name.starts_with("web") {
            "[web content folded]"
        } else if tool_name.starts_with("exec") {
            "[stdout folded]"
        } else if tool_name.starts_with("file") {
            "[output folded]"
        } else {
            "[details folded]"
        };
        if v.get("content").is_some() {
            v.as_object_mut().unwrap().insert(
                "content".into(),
                serde_json::Value::String(hint.to_string()),
            );
        }
        return v.to_string();
    }

    // ── Plain string folding (legacy) ──
    let hint = if tool_name.starts_with("web") {
        " [web content folded]"
    } else if tool_name.starts_with("exec") {
        " [stdout folded]"
    } else if tool_name.starts_with("file") {
        " [output folded]"
    } else {
        " [details folded]"
    };

    // Keep first non-empty line — it always carries [OK]/[ERROR]/[PARTIAL] + metadata.
    let first = result.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    if first.is_empty() {
        return hint[1..].to_string(); // strip leading space
    }
    let cap = first.floor_char_boundary(first.len().min(400));
    format!("{}{}", &first[..cap], hint)
}

/// Fold a historical read/search result into deterministic metadata plus an
/// explicit recovery action. This keeps old tool output out of the stable
/// prefix while preserving the information an agent needs to re-open it.
fn fold_retrieval_result(tool_name: &str, result: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(result) {
        let mut folded = serde_json::Map::new();
        for key in [
            "status",
            "path",
            "start_line",
            "end_line",
            "total_lines",
            "hash",
            "unchanged",
            "truncated",
        ] {
            if let Some(value) = value.get(key) {
                folded.insert(key.to_string(), value.clone());
            }
        }
        let hint = if tool_name == "read" {
            "[read content folded; call read with path and start_line/end_line to retrieve code]"
        } else if tool_name == "diff" {
            "[diff folded; call diff with the same paths to retrieve changes]"
        } else {
            "[search matches folded; call search with the same or narrower query to retrieve matches]"
        };
        folded.insert("content".to_string(), serde_json::Value::String(hint.to_string()));
        return serde_json::Value::Object(folded).to_string();
    }

    let first = result.lines().find(|line| !line.trim().is_empty()).unwrap_or("");
    let cap = first.floor_char_boundary(first.len().min(400));
    let hint = if tool_name == "read" {
        " [read content folded; call read again with a path and range]"
    } else if tool_name == "diff" {
        " [diff folded; call diff again with the same paths]"
    } else {
        " [search matches folded; call search again with a narrower query]"
    };
    format!("{}{}", &first[..cap], hint)
}

/// Preserve a small, verifiable edit receipt instead of only reporting that a
/// write succeeded. The full diff remains in persistence/UI; later turns get
/// the changed path/range and a deterministic instruction to verify it.
fn fold_file_mutation_result(tool_name: &str, result: &str) -> String {
    let first = result.lines().find(|line| !line.trim().is_empty()).unwrap_or("");
    let cap = first.floor_char_boundary(first.len().min(400));
    let action = match tool_name {
        "edit" | "edit_block" => "edit",
        "write" => "write",
        _ => "change",
    };
    format!(
        "{}\n[{} diff folded; verify the affected range with read before making dependent changes]",
        &first[..cap], action
    )
}

#[derive(Debug, Clone)]
pub struct Step {
    pub assistant: Message,
    pub tool_results: Vec<Message>,
}

impl Step {
    pub fn new(assistant: Message) -> Self {
        Self {
            assistant,
            tool_results: Vec::new(),
        }
    }

    pub fn assistant_tool_ids(&self) -> Vec<String> {
        self.assistant
            .content
            .iter()
            .filter_map(|b| {
                if let deepx_types::ContentBlock::ToolUse { id, .. } = b {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn tool_result_has_id(&self, id: &str) -> bool {
        self.tool_results.iter().any(|tr| tr.content.iter().any(|b| matches!(b, deepx_types::ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id)))
    }

    pub fn has_tool_call(&self, id: &str) -> bool {
        self.assistant_tool_ids().iter().any(|tid| tid == id)
    }

    pub fn all_tools_satisfied(&self) -> bool {
        let ids = self.assistant_tool_ids();
        if ids.is_empty() {
            return true;
        }
        ids.iter().all(|id| self.tool_result_has_id(id))
    }

    pub fn pending_tools(&self) -> Vec<PendingTool> {
        self.assistant
            .content
            .iter()
            .filter_map(|b| {
                if let deepx_types::ContentBlock::ToolUse { id, name, input } = b {
                    Some(PendingTool {
                        id: id.clone(),
                        name: name.clone(),
                        args: input.clone(),
                    })
                } else {
                    None
                }
            })
            .collect()
    }

    fn tool_name_for_result(&self, tool_call_id: &str) -> Option<&str> {
        self.assistant.content.iter().find_map(|block| {
            if let deepx_types::ContentBlock::ToolUse { id, name, .. } = block {
                (id == tool_call_id).then_some(name.as_str())
            } else {
                None
            }
        })
    }

    fn has_tool_use(&self) -> bool {
        self.assistant
            .content
            .iter()
            .any(|b| matches!(b, deepx_types::ContentBlock::ToolUse { .. }))
    }
}

#[derive(Debug, Clone)]
pub struct Turn {
    pub user: Message,
    pub steps: Vec<Step>,
}

impl Turn {
    pub fn new(user: Message) -> Self {
        Self {
            user,
            steps: Vec::new(),
        }
    }

    pub fn find_step_for_mut(&mut self, tool_call_id: &str) -> Option<&mut Step> {
        self.steps
            .iter_mut()
            .find(|s| s.has_tool_call(tool_call_id))
    }
}

#[allow(clippy::type_complexity)]
pub struct MessageStore {
    seed: String,
    system_messages: Vec<Message>,
    turns: Vec<Turn>,
    cancelled: bool,
    tool_executor: Option<ToolExecutorFn>,
    /// Number of earliest turns that have been compacted (skipped in LLM context).
    compact_skip: usize,
    /// True once the store is backed by a separate compact checkpoint instead
    /// of treating `messages.jsonl` as its active context.
    has_compact_context: bool,
    /// Next message ID to assign (monotonic per session).
    next_msg_id: u64,
    /// Next externally visible turn sequence. Unlike `turns.len()`, this never
    /// moves backwards when old turns are replaced by a compact checkpoint.
    next_turn_seq: u64,
    /// If true, save_msg is a no-op — used during from_messages replay.
    replaying: bool,
    /// Messages assigned msg_id but not yet flushed to disk.
    pending_save: Vec<Message>,
    /// If true, skip all disk persistence. Used by subagents (disposable workers).
    ephemeral: bool,
}

impl std::fmt::Debug for MessageStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageStore")
            .field("seed", &self.seed)
            .field("turns", &self.turns.len())
            .field("cancelled", &self.cancelled)
            .field("has_executor", &self.tool_executor.is_some())
            .field("compact_skip", &self.compact_skip)
            .field("next_msg_id", &self.next_msg_id)
            .finish()
    }
}

impl Clone for MessageStore {
    fn clone(&self) -> Self {
        Self {
            seed: self.seed.clone(),
            system_messages: self.system_messages.clone(),
            turns: self.turns.clone(),
            cancelled: self.cancelled,
            tool_executor: None,
            compact_skip: self.compact_skip,
            has_compact_context: self.has_compact_context,
            next_msg_id: self.next_msg_id,
            next_turn_seq: self.next_turn_seq,
            replaying: false,
            pending_save: Vec::new(),
            ephemeral: self.ephemeral,
        }
    }
}

impl MessageStore {
    pub fn new(seed: &str) -> Self {
        Self {
            seed: seed.to_string(),
            system_messages: Vec::new(),
            turns: Vec::new(),
            cancelled: false,
            tool_executor: None,
            compact_skip: 0,
            has_compact_context: false,
            next_msg_id: 1,
            next_turn_seq: 1,
            replaying: false,
            pending_save: Vec::new(),
            ephemeral: false,
        }
    }

    /// Create a MessageStore that never persists to disk (subagent / disposable worker).
    pub fn new_ephemeral(seed: &str) -> Self {
        let mut s = Self::new(seed);
        s.ephemeral = true;
        s
    }

    pub fn seed(&self) -> &str {
        &self.seed
    }

    pub fn switch_seed(&mut self, new_seed: &str) {
        self.seed = new_seed.to_string();
        self.system_messages.clear();
        self.turns.clear();
        self.cancelled = false;
        self.compact_skip = 0;
        self.has_compact_context = false;
        self.next_msg_id = 1;
        self.next_turn_seq = 1;
        self.replaying = false;
        self.pending_save.clear();
    }

    pub fn cancel(&mut self) {
        self.cancelled = true;
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// Assign msg_id and buffer for batched persistence.
    /// Flushed to disk via [`flush_meta`].
    /// No-op in ephemeral mode.
    fn save_msg(&mut self, msg: &Message) {
        if self.ephemeral {
            return;
        }
        let mut m = msg.clone();
        m.msg_id = Some(self.next_msg_id);
        self.next_msg_id += 1;
        if !self.replaying {
            self.pending_save.push(m);
        }
    }

    /// Write buffered messages to JSONL, then update meta.json + index.
    /// No-op if the session seed has not been initialized yet (empty seed),
    /// or if ephemeral mode is enabled.
    pub fn flush_meta(&mut self, model: &str, effort: &str) {
        if self.seed.is_empty() || self.ephemeral {
            return;
        }
        let turn_count = self.turns.len();
        if !self.pending_save.is_empty() {
            SessionManager::global().save_append(
                &self.seed,
                &self.pending_save,
                model,
                Some(effort),
                self.compact_skip,
                turn_count,
            );
            self.pending_save.clear();
            if self.has_compact_context {
                SessionManager::global().update_compact_context(&self.seed, &self.to_vec());
            }
        } else {
            SessionManager::global().update_meta(
                &self.seed,
                model,
                Some(effort),
                self.compact_skip,
                turn_count,
            );
        }
    }

    pub fn push_system(&mut self, msg: Message) -> Effect {
        debug_assert_eq!(msg.role, "system", "push_system requires role=system");
        // Guard: skip if an identical system message already exists.
        // This prevents double-injection when lifecycle paths are called
        // multiple times (e.g. create_session after a failed resume).
        let new_text = msg
            .content
            .iter()
            .find_map(|b| match b {
                deepx_types::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or("");
        if !new_text.is_empty()
            && self.system_messages.iter().any(|m| {
                m.content.iter().any(|b| match b {
                    deepx_types::ContentBlock::Text { text } => text == new_text,
                    _ => false,
                })
            })
        {
            return Effect::None;
        }
        self.system_messages.push(msg.clone());
        self.save_msg(&msg);
        Effect::None
    }

    /// Remove all system messages whose first text block starts with the
    /// given prefix. Used to replace catalog messages without relying on
    /// the [DEEPX_SKILL_V1] format.
    pub fn remove_system_messages_by_prefix(&mut self, prefix: &str) {
        self.system_messages.retain(|message| {
            let text = message
                .content
                .iter()
                .find_map(|block| match block {
                    deepx_types::ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            !text.starts_with(prefix)
        });
    }

    pub fn push_user(&mut self, text: &str) -> Effect {
        if !self.replaying {
            if let Some(turn) = self.turns.last_mut() {
                if let Some(step) = turn.steps.last_mut() {
                    auto_complete_unfulfilled(
                        step,
                        "[CANCELLED] Tool was not executed (user interrupted).",
                    );
                }
            }
        }
        let msg = Message::user(text);
        self.turns.push(Turn::new(msg.clone()));
        self.save_msg(&msg);
        Effect::None
    }

    /// Add an image block to the last user message (the most recent turn's user message).
    pub fn push_image_to_last_user(&mut self, mime_type: &str, data: &str) {
        if let Some(turn) = self.turns.last_mut() {
            turn.user.content.push(deepx_types::ContentBlock::Image {
                mime_type: mime_type.to_string(),
                data: data.to_string(),
            });
        }
    }

    pub fn push_assistant(&mut self, msg: Message) -> Effect {
        debug_assert_eq!(
            msg.role, "assistant",
            "push_assistant requires role=assistant"
        );

        let turn = match self.turns.last_mut() {
            Some(t) => t,
            None => {
                log::error!(
                    "push_assistant: no turn exists — assistant response without user input. Dropping."
                );
                return Effect::None;
            }
        };

        if !self.replaying {
            if let Some(step) = turn.steps.last_mut() {
                auto_complete_unfulfilled(
                    step,
                    "[AUTO] Tool was not executed before next assistant response.",
                );
            }
        }

        let step = Step::new(msg.clone());
        let has_tools = step.has_tool_use();
        turn.steps.push(step);
        self.save_msg(&msg);

        if has_tools {
            Effect::None
        } else {
            Effect::TurnComplete
        }
    }

    pub fn push_tool_result(&mut self, tool_call_id: &str, result: &str, success: bool) -> Effect {
        self.push_tool_result_inner(tool_call_id, result, success);

        if let Some(turn) = self.turns.last() {
            if let Some(step) = turn.steps.last() {
                if step.all_tools_satisfied() {
                    return if step.pending_tools().is_empty() {
                        Effect::TurnComplete
                    } else {
                        Effect::None
                    };
                }
            }
        }
        Effect::None
    }

    pub fn push_tool_results_batch(&mut self, results: &[(String, String, bool)]) -> Effect {
        for (tc_id, result, success) in results {
            self.push_tool_result_inner(tc_id, result, *success);
        }

        if let Some(turn) = self.turns.last() {
            if let Some(step) = turn.steps.last() {
                if step.all_tools_satisfied() {
                    return if step.pending_tools().is_empty() {
                        Effect::TurnComplete
                    } else {
                        Effect::None
                    };
                }
            }
        }
        Effect::None
    }

    fn push_tool_result_inner(&mut self, tool_call_id: &str, result: &str, success: bool) {
        // Look up tool name from any step that owns this tool_call_id.
        let _tool_name: Option<String> = self.turns.iter().rev().find_map(|turn| {
            turn.steps.iter().rev().find_map(|step| {
                step.assistant.content.iter().find_map(|b| {
                    if let deepx_types::ContentBlock::ToolUse { id, name, .. } = b {
                        if id == tool_call_id {
                            Some(name.clone())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                })
            })
        });
        let final_result = result.to_string();
        let tool_msg = Message::tool(tool_call_id, &final_result, success);

        for turn in self.turns.iter_mut().rev() {
            if let Some(step) = turn.find_step_for_mut(tool_call_id) {
                if !step.tool_result_has_id(tool_call_id) {
                    step.tool_results.push(tool_msg.clone());
                    self.save_msg(&tool_msg);
                }
                return;
            }
        }
        if let Some(turn) = self.turns.last_mut() {
            if let Some(step) = turn.steps.last_mut() {
                if step.has_tool_call(tool_call_id) {
                    log::warn!(
                        "push_tool_result: tool_result {} matched by last-step fallback — appending",
                        tool_call_id
                    );
                    step.tool_results.push(tool_msg.clone());
                    self.save_msg(&tool_msg);
                    return;
                }
                log::error!(
                    "push_tool_result: orphan tool_result {} — last step does not own this call_id, dropped",
                    tool_call_id
                );
                return;
            }
        }
        log::error!(
            "push_tool_result: orphan tool_result {} — nowhere to place, dropped",
            tool_call_id
        );
    }

    pub fn replace_tool_result(&mut self, tool_call_id: &str, result: &str, success: bool) {
        // Same truncation for replace path.
        let _tool_name: Option<String> = self.turns.iter().rev().find_map(|turn| {
            turn.steps.iter().rev().find_map(|step| {
                step.assistant.content.iter().find_map(|b| {
                    if let deepx_types::ContentBlock::ToolUse { id, name, .. } = b {
                        if id == tool_call_id {
                            Some(name.clone())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                })
            })
        });
        let final_result = result.to_string();

        for turn in self.turns.iter_mut().rev() {
            if let Some(step) = turn.find_step_for_mut(tool_call_id) {
                step.tool_results.retain(|tr| !tr.content.iter().any(|b| matches!(b, deepx_types::ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == tool_call_id)));
                step.tool_results
                    .push(Message::tool(tool_call_id, &final_result, success));
                return;
            }
        }
        log::error!(
            "replace_tool_result: tool_call_id {} not found in any turn",
            tool_call_id
        );
    }

    pub fn build_context_for_gate(&mut self, annotations: &[String]) -> Vec<Message> {
        let mut full: Vec<Message> = {
            let mut v = Vec::new();
            v.extend(self.system_messages.clone());
            let total_turns = self.turns.len();
            for (i, turn) in self.turns.iter().enumerate() {
                if i < self.compact_skip {
                    continue;
                }
                v.push(turn.user.clone());
                let _is_last_turn = i == total_turns - 1;
                let _total_steps = turn.steps.len();
                for (_si, step) in turn.steps.iter().enumerate() {
                    v.push(step.assistant.clone());
                    for tr in &step.tool_results {
                        let mut msg = tr.clone();
                        for block in &mut msg.content {
                            if let deepx_types::ContentBlock::ToolResult {
                                tool_use_id,
                                content,
                                ..
                            } = block
                            {
                                let tool_name =
                                    step.tool_name_for_result(tool_use_id).unwrap_or("");
                                // Unified rule (same for all steps) — keeps
                                // the prefix stable across rounds within a turn.
                                // Reasonix applies truncation at storage time;
                                // we apply it here consistently regardless of
                                // step position.
                                if tool_name == "read" {
                                    // read self-limits to head 50 + tail 30 lines
                                    // (~4K chars); pass through unchanged.
                                } else if tool_name == "search" {
                                    // search results are structured text; keep
                                    // as-is — they are bounded by the tool.
                                } else if tool_name.starts_with("exec") {
                                    *content = truncate_tool_result(tool_name, content);
                                } else {
                                    *content = fold_completed_tool_result(tool_name, content);
                                }
                            }
                        }
                        v.push(msg);
                    }
                }
            }
            v
        };

        if !annotations.is_empty() {
            let ann_text = annotations.join("\n");
            if let Some(last_user) = full.iter_mut().rev().find(|m| m.role == "user") {
                let existing = last_user.content.iter_mut().find_map(|b| {
                    if let deepx_types::ContentBlock::Text { text } = b {
                        Some(text)
                    } else {
                        None
                    }
                });
                if let Some(text) = existing {
                    let original = text.clone();
                    *text = format!("[Environment]\n{}\n\n[UserMessage]\n{}", ann_text, original);
                } else {
                    last_user
                        .content
                        .push(deepx_types::ContentBlock::text(&ann_text));
                }
            }
        }

        full
    }

    /// Get pending tools from the last step (for manual execution with streaming).
    pub fn get_last_step_pending(&self) -> Vec<PendingTool> {
        let step = match self.turns.last().and_then(|t| t.steps.last()) {
            Some(s) => s,
            None => return Vec::new(),
        };
        let tool_ids = step.assistant_tool_ids();
        step.pending_tools()
            .into_iter()
            .filter(|t| tool_ids.contains(&t.id) && !step.tool_result_has_id(&t.id))
            .collect()
    }

    /// Push a tool result directly (for manual execution).
    pub fn push_tool_result_direct(&mut self, tool_call_id: &str, result: &str, success: bool) {
        self.push_tool_result_inner(tool_call_id, result, success);
    }

    /// Execute all pending tools in the current step. When `tool_executor` is None
    /// (e.g. during session restore), returns early without injecting errors.
    pub fn execute_tools_batch(&mut self) -> Effect {
        let executor = match &self.tool_executor {
            Some(e) => e,
            None => {
                log::warn!("execute_tools_batch: no tool executor set — skipping tool execution");
                return Effect::None;
            }
        };

        let pending: Vec<PendingTool> = {
            let step = match self.turns.last().and_then(|t| t.steps.last()) {
                Some(s) => s,
                None => return Effect::None,
            };
            let tool_ids = step.assistant_tool_ids();
            step.pending_tools()
                .into_iter()
                .filter(|t| tool_ids.contains(&t.id) && !step.tool_result_has_id(&t.id))
                .collect()
        };

        if pending.is_empty() {
            return Effect::None;
        }

        let mut reports: Vec<(String, String, bool)> = Vec::new();
        for tool in &pending {
            let req = ToolExecRequest {
                id: tool.id.clone(),
                name: tool.name.clone(),
                args: tool.args.clone(),
            };
            let report = executor(req);
            reports.push((tool.id.clone(), report.content, report.success));
        }
        for (tc_id, content, success) in reports {
            self.push_tool_result_inner(&tc_id, &content, success);
        }

        // Tools executed; caller re-evaluates (build context → gate → push_assistant)
        Effect::None
    }

    pub fn last_step_tool_results(&self) -> Vec<(String, String, String, bool)> {
        let step = match self.turns.last().and_then(|t| t.steps.last()) {
            Some(s) => s,
            None => return Vec::new(),
        };
        let mut results = Vec::new();
        for tr in &step.tool_results {
            if let Some((tc_id, result_text, ok)) = tr.content.iter().find_map(|b| {
                if let deepx_types::ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    success,
                } = b
                {
                    Some((tool_use_id.clone(), content.clone(), *success))
                } else {
                    None
                }
            }) {
                let tool_name = step
                    .assistant
                    .content
                    .iter()
                    .find_map(|b| {
                        if let deepx_types::ContentBlock::ToolUse { id, name, .. } = b {
                            if id == &tc_id {
                                Some(name.clone())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                results.push((tc_id, tool_name, result_text, ok));
            }
        }
        results
    }

    pub fn tool_call_args(&self, tool_call_id: &str) -> Option<serde_json::Value> {
        let step = self.turns.last().and_then(|t| t.steps.last())?;
        step.assistant.content.iter().find_map(|b| {
            if let deepx_types::ContentBlock::ToolUse { id, input, .. } = b {
                if id == tool_call_id {
                    Some(input.clone())
                } else {
                    None
                }
            } else {
                None
            }
        })
    }

    /// Find the tool_call_id of a named tool in the most recent assistant step.
    pub fn find_last_step_tool_call(&self, tool_name: &str) -> Option<String> {
        let step = self.turns.last()?.steps.last()?;
        step.assistant.content.iter().find_map(|block| match block {
            deepx_types::ContentBlock::ToolUse { name, id, .. } if name == tool_name => {
                Some(id.clone())
            }
            _ => None,
        })
    }

    pub fn has_pending_tools(&self) -> bool {
        self.turns
            .last()
            .and_then(|t| t.steps.last())
            .map(|s| !s.all_tools_satisfied())
            .unwrap_or(false)
    }

    pub fn turn_count(&self) -> usize {
        self.turns.len()
    }

    pub fn message_count(&self) -> usize {
        self.system_messages.len()
            + self
                .turns
                .iter()
                .map(|t| {
                    1 + t
                        .steps
                        .iter()
                        .map(|s| 1 + s.tool_results.len())
                        .sum::<usize>()
                })
                .sum::<usize>()
    }

    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    pub fn system_messages(&self) -> &[Message] {
        &self.system_messages
    }

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

    pub fn set_tool_executor(&mut self, executor: ToolExecutorFn) {
        self.tool_executor = Some(executor);
    }

    /// Save all messages (full rewrite). Used for undo or compact.
    /// No-op if the session seed has not been initialized yet.
    pub fn snapshot_full(&mut self, model: &str, effort: &str) {
        if self.seed.is_empty() || self.ephemeral {
            return;
        }
        let msgs = self.to_vec();
        let turn_count = self.turns.len();
        if self.has_compact_context {
            SessionManager::global().save_compact_context(&self.seed, &msgs);
        } else {
            SessionManager::global().save_full(
                &self.seed,
                &msgs,
                model,
                Some(effort),
                self.compact_skip,
                turn_count,
            );
        }
        self.pending_save.clear();
    }

    /// Reconstruct the internal turn/step structure by replaying saved messages
    /// through `push_user` / `push_assistant` / `push_tool_result`.
    pub fn from_messages(seed: &str, msgs: &[Message], compact_skip: usize) -> (Self, Vec<String>) {
        let mut store = Self::new(seed);
        store.compact_skip = compact_skip;
        store.replaying = true;
        let mut repairs = Vec::new();
        let mut i = 0;

        // Keep the base prompt plus distinct protected skill injections.
        // Other duplicate system messages from old persistence bugs are dropped.
        let mut has_system = false;
        let mut protected_system_texts = std::collections::HashSet::new();
        while i < msgs.len() && msgs[i].role == "system" {
            let text = msgs[i]
                .content
                .iter()
                .find_map(|block| match block {
                    deepx_types::ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            let protected_skill =
                text.starts_with("[DEEPX_SKILL_V1]") && text.contains("[END_DEEPX_SKILL_V1]");
            if !has_system || (protected_skill && protected_system_texts.insert(text.to_string())) {
                store.system_messages.push(msgs[i].clone());
                has_system = true;
            } else {
                repairs.push(
                    "dropped duplicate system message (msg_id collision or prior bug)".into(),
                );
            }
            if let Some(mid) = msgs[i].msg_id {
                store.next_msg_id = store.next_msg_id.max(mid + 1);
            }
            i += 1;
        }

        while i < msgs.len() {
            match msgs[i].role.as_str() {
                "user" => {
                    let text = msgs[i]
                        .content
                        .iter()
                        .find_map(|b| {
                            if let deepx_types::ContentBlock::Text { text } = b {
                                Some(text.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    store.push_user(&text);
                    i += 1;
                }
                "assistant" => {
                    store.push_assistant(msgs[i].clone());
                    i += 1;
                }
                "tool" => {
                    let (tc_id, result, success) = msgs[i]
                        .content
                        .iter()
                        .find_map(|b| {
                            if let deepx_types::ContentBlock::ToolResult {
                                tool_use_id,
                                content,
                                success,
                            } = b
                            {
                                Some((tool_use_id.clone(), content.clone(), *success))
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    store.push_tool_result(&tc_id, &result, success);
                    i += 1;
                }
                _ => {
                    i += 1;
                }
            }
        }

        for turn in store.turns.iter_mut() {
            for step in turn.steps.iter_mut() {
                let missing_ids: Vec<(String, String)> = {
                    let tool_ids = step.assistant_tool_ids();
                    tool_ids
                        .iter()
                        .filter(|id| !step.tool_result_has_id(id))
                        .map(|id| {
                            let name = step
                                .assistant
                                .content
                                .iter()
                                .find_map(|b| {
                                    if let deepx_types::ContentBlock::ToolUse {
                                        id: tid,
                                        name,
                                        ..
                                    } = b
                                    {
                                        if tid == id {
                                            Some(name.clone())
                                        } else {
                                            None
                                        }
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or_default();
                            (id.clone(), name)
                        })
                        .collect()
                };
                if missing_ids.is_empty() {
                    continue;
                }
                for (id, name) in missing_ids {
                    let note = format!(
                        "[RESTORE] Tool \"{name}\" had no result when session was saved — not executed.\n[HINT] Do NOT retry."
                    );
                    step.tool_results.push(Message::tool(&id, &note, false));
                    repairs.push(format!("injected [RESTORE] for orphan tool_use {}", id));
                }
            }
        }

        // Restore next_msg_id: max(msg_id) + 1, or 1 if empty.
        let max_id = msgs.iter().filter_map(|m| m.msg_id).max().unwrap_or(0);
        store.next_msg_id = store.next_msg_id.max(max_id + 1);
        store.next_turn_seq = store.next_turn_seq.max(store.turns.len() as u64 + 1);
        store.replaying = false;

        (store, repairs)
    }

    /// Mark a restored store as being driven by a compact checkpoint.  New
    /// messages still append to the immutable archive, while the checkpoint
    /// is refreshed with the active model context.
    pub fn set_compact_context_active(&mut self, active: bool) {
        self.has_compact_context = active;
    }

    /// Keep IDs monotonic against the raw archive when restoring a compact
    /// checkpoint (the checkpoint intentionally omits archived messages).
    pub fn ensure_next_msg_id(&mut self, next: u64) {
        self.next_msg_id = self.next_msg_id.max(next);
    }

    /// Allocate a session-monotonic turn ID. Compaction may shrink the active
    /// model view, but must never make a future TurnStart reuse an archived ID.
    pub fn allocate_turn_id(&mut self) -> String {
        let seq = self.next_turn_seq;
        self.next_turn_seq = self.next_turn_seq.saturating_add(1);
        format!("t{seq}")
    }

    /// Restore the turn allocator against the immutable archive.
    pub fn ensure_next_turn_seq(&mut self, next: u64) {
        self.next_turn_seq = self.next_turn_seq.max(next.max(1));
    }

    pub fn remove_last_step_if_incomplete(&mut self) -> bool {
        if let Some(turn) = self.turns.last_mut() {
            if let Some(step) = turn.steps.last() {
                if !step.all_tools_satisfied() {
                    turn.steps.pop();
                    return true;
                }
            }
        }
        false
    }

    pub fn truncate_before_turn(&mut self, turn_id: &str) -> bool {
        let idx: usize = match turn_id
            .strip_prefix('t')
            .and_then(|n| n.parse::<usize>().ok())
        {
            Some(n) if n > 0 => n.saturating_sub(1),
            _ => return false,
        };
        if idx >= self.turns.len() {
            return false;
        }
        self.turns.truncate(idx);
        // After truncation, need full rewrite on next save.
        true
    }

    /// Compact: keep `keep` recent turns in LLM context, physically remove older ones.
    /// Inserts the summary as a synthetic user turn before the kept turns
    /// so that `to_vec()` serializes correctly without duplicating compacted data.
    /// Sets `compact_skip` to 0 because all turns now present are live.
    pub fn apply_compact(&mut self, summary: &str, keep: usize) {
        // 防御：确保至少保留 1 个 turn，防止 caller 传入 keep=0 导致清空全部历史。
        let keep = keep.max(1);
        let skip = self.turns.len().saturating_sub(keep);
        if skip == 0 {
            return;
        }

        // Remove old compact markers
        self.system_messages.retain(|m| !m.content.iter().any(|b| matches!(b, deepx_types::ContentBlock::Text { text } if text.starts_with("[COMPACT"))));

        // Build compact summary as a synthetic user turn (no steps).
        // The next LLM sees only the handoff summary; the original user
        // message is intentionally omitted — it's redundant with the
        // summary and, after a manual compact, the user will send a fresh
        // message to resume work.
        let compact_text = format!(
            "[Compacted {} turns]\n{}",
            skip,
            summary.trim(),
        );
        let compact_turn = Turn::new(Message::user(&compact_text));

        // Physically remove compacted turns, keep only the most recent `keep`.
        let kept = self.turns.split_off(skip);
        self.turns = kept;
        // Prepend compact summary as a synthetic turn before the kept turns.
        self.turns.insert(0, compact_turn);
        // No skipping needed — compacted data is physically gone.
        self.compact_skip = 0;
        self.has_compact_context = true;
    }

    /// Get the text of any previous compaction summary (for incremental update mode).
    /// Format: `[Compacted N turns]\n{summary}` — everything after the first newline.
    /// Searches turns[0] since compact summary is stored as a synthetic turn.
    pub fn previous_compact_summary(&self) -> Option<String> {
        self.turns.first().and_then(|turn| {
            turn.user.content.iter().find_map(|b| {
                if let deepx_types::ContentBlock::Text { text } = b {
                    if text.starts_with("[Compacted") {
                        let after_header = text.find('\n').map(|n| n + 1).unwrap_or(0);
                        let summary = text[after_header..].trim();
                        if summary.len() > 20 {
                            Some(summary.to_string())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
        })
    }

    /// Compute context composition stats from the current message store.
    /// This reflects the actual state (post-compact), unlike the API dump which lags.
    /// Returns (chat_text_tok, thinking_tok, tool_calls_tok, tool_results_tok, tools_schema_tok, system_prompt_tok, thinking_blocks, tool_call_blocks).
    /// All token fields use `deepx_types::count_tokens` (CJK-aware heuristic), NOT raw char length.
    #[allow(clippy::too_many_arguments)]
    pub fn compute_context_stats(
        &self,
        tool_defs: Option<&[ToolDef]>,
    ) -> (u64, u64, u64, u64, u64, u64, u64, u64) {
        let mut chat_text = 0u64;
        let mut thinking = 0u64;
        let mut tool_calls = 0u64;
        let mut tool_results = 0u64;
        let mut tools_schema = 0u64;
        let mut system_prompt = 0u64;
        let mut thinking_blocks = 0u64;
        let mut tool_call_blocks = 0u64;

        // Count tool definitions (sent as JSON schema to the LLM)
        if let Some(defs) = tool_defs {
            for td in defs {
                if let Ok(json) = serde_json::to_string(td) {
                    tools_schema += deepx_types::count_tokens(&json) as u64;
                }
            }
        }

        for m in &self.system_messages {
            for b in &m.content {
                if let deepx_types::ContentBlock::Text { text } = b {
                    system_prompt += deepx_types::count_tokens(text) as u64;
                }
            }
        }
        for (i, turn) in self.turns.iter().enumerate() {
            if i < self.compact_skip {
                continue;
            }
            let is_last_turn = i == self.turns.len() - 1;
            for m in [&turn.user] {
                for b in &m.content {
                    match b {
                        deepx_types::ContentBlock::Text { text } => {
                            chat_text += deepx_types::count_tokens(text) as u64;
                        }
                        deepx_types::ContentBlock::Reasoning { reasoning } => {
                            thinking += deepx_types::count_tokens(reasoning) as u64;
                            thinking_blocks += 1;
                        }
                        deepx_types::ContentBlock::ToolUse { .. } => {
                            // Tool call JSON ≈ token count of serialized form
                            let json = serde_json::to_string(b).unwrap_or_default();
                            tool_calls += deepx_types::count_tokens(&json) as u64;
                            tool_call_blocks += 1;
                        }
                        deepx_types::ContentBlock::ToolResult { content, .. } => {
                            tool_results += deepx_types::count_tokens(content) as u64;
                        }
                        deepx_types::ContentBlock::Image { .. } => {
                            // Image token count uses the MiMo formula (roughly ~256-1024 tokens depending on resolution).
                            // Use a conservative estimate of 512 tokens per image.
                            chat_text += 512;
                        }
                    }
                }
            }
            for (si, step) in turn.steps.iter().enumerate() {
                let is_last_step_of_last_turn = is_last_turn && si == turn.steps.len() - 1;
                for b in &step.assistant.content {
                    match b {
                        deepx_types::ContentBlock::Text { text } => {
                            chat_text += deepx_types::count_tokens(text) as u64;
                        }
                        deepx_types::ContentBlock::Reasoning { reasoning } => {
                            thinking += deepx_types::count_tokens(reasoning) as u64;
                            thinking_blocks += 1;
                        }
                        deepx_types::ContentBlock::ToolUse { .. } => {
                            let json = serde_json::to_string(b).unwrap_or_default();
                            tool_calls += deepx_types::count_tokens(&json) as u64;
                            tool_call_blocks += 1;
                        }
                        _ => {}
                    }
                }
                for tr in &step.tool_results {
                    for b in &tr.content {
                        if let deepx_types::ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } = b
                        {
                            // Match tool name by id
                            let tool_name = step
                                .assistant
                                .content
                                .iter()
                                .find_map(|blk| {
                                    if let deepx_types::ContentBlock::ToolUse { id, name, .. } = blk
                                    {
                                        if id == tool_use_id {
                                            Some(name.as_str())
                                        } else {
                                            None
                                        }
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or("");

                            let effective = if is_last_step_of_last_turn {
                                let keep_full = tool_name == "read"
                                    || tool_name == "search"
                                    || tool_name == "skills"
                                    || tool_name.starts_with("exec");
                                if keep_full {
                                    truncate_tool_result(tool_name, content)
                                } else {
                                    fold_completed_tool_result(tool_name, content)
                                }
                            } else {
                                fold_completed_tool_result(tool_name, content)
                            };
                            tool_results += deepx_types::count_tokens(&effective) as u64;
                        }
                    }
                }
            }
        }
        (
            chat_text,
            thinking,
            tool_calls,
            tool_results,
            tools_schema,
            system_prompt,
            thinking_blocks,
            tool_call_blocks,
        )
    }
}

fn auto_complete_unfulfilled(step: &mut Step, reason: &str) {
    let missing: Vec<(String, String)> = {
        let tool_ids = step.assistant_tool_ids();
        tool_ids
            .iter()
            .filter(|id| !step.tool_result_has_id(id))
            .map(|id| {
                let name = step
                    .assistant
                    .content
                    .iter()
                    .find_map(|b| {
                        if let deepx_types::ContentBlock::ToolUse { id: tid, name, .. } = b {
                            if tid == id {
                                Some(name.clone())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                (id.clone(), name)
            })
            .collect()
    };
    if !missing.is_empty() {
        log::warn!(
            "auto-complete: {} unfulfilled tool(s) — {}",
            missing.len(),
            reason
        );
        for (id, name) in missing {
            step.tool_results.push(Message::tool(
                &id,
                &format!("{} Tool \"{name}\" was not executed.", reason),
                false,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepx_types::ContentBlock;

    fn assistant_with_tools(tools: &[(&str, &str)]) -> Message {
        Message {
            msg_id: None,
            role: "assistant".to_string(),
            name: None,
            content: tools
                .iter()
                .map(|(id, name)| ContentBlock::ToolUse {
                    id: (*id).to_string(),
                    name: (*name).to_string(),
                    input: serde_json::json!({}),
                })
                .collect(),
        }
    }

    fn context_result(context: &[Message], id: &str) -> String {
        context
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } if tool_use_id == id => Some(content.clone()),
                _ => None,
            })
            .expect("tool result must be present in gate context")
    }

    #[test]
    fn current_step_projects_each_result_using_its_own_tool_name() {
        let mut store = MessageStore::new_ephemeral("test");
        store.push_user("inspect files");
        store.push_assistant(assistant_with_tools(&[
            ("write-1", "write"),
            ("read-1", "read"),
            ("exec-1", "exec"),
        ]));
        store.push_tool_result_direct("write-1", "WRITE_RESULT", true);
        store.push_tool_result_direct("read-1", "READ_RESULT", true);
        store.push_tool_result_direct("exec-1", "EXEC_RESULT", true);

        let context = store.build_context_for_gate(&[]);

        assert_eq!(
            context_result(&context, "write-1"),
            "WRITE_RESULT\n[write diff folded; verify the affected range with read before making dependent changes]"
        );
        assert_eq!(context_result(&context, "read-1"), "READ_RESULT");
        assert_eq!(context_result(&context, "exec-1"), "EXEC_RESULT");
    }

    #[test]
    fn completed_step_projects_each_result_using_its_own_tool_name() {
        let mut store = MessageStore::new_ephemeral("test");
        store.push_user("first turn");
        store.push_assistant(assistant_with_tools(&[
            ("write-1", "write"),
            ("read-1", "read"),
        ]));
        store.push_tool_result_direct("write-1", "WRITE_RESULT", true);
        store.push_tool_result_direct("read-1", "READ_RESULT", true);
        store.push_user("second turn");

        let context = store.build_context_for_gate(&[]);

        assert_eq!(
            context_result(&context, "write-1"),
            "WRITE_RESULT\n[write diff folded; verify the affected range with read before making dependent changes]"
        );
        assert_eq!(
            context_result(&context, "read-1"),
            "READ_RESULT"
        );
    }

    #[test]
    fn historical_read_preserves_full_content() {
        let mut store = MessageStore::new_ephemeral("test");
        store.push_user("inspect file");
        store.push_assistant(assistant_with_tools(&[("read-1", "read")]));
        store.push_tool_result_direct(
            "read-1",
            &serde_json::json!({
                "status": "ok",
                "path": "src/lib.rs",
                "start_line": 40,
                "end_line": 80,
                "total_lines": 300,
                "content": "sensitive and lengthy source body"
            })
            .to_string(),
            true,
        );
        store.push_user("continue");

        let folded: serde_json::Value = serde_json::from_str(&context_result(
            &store.build_context_for_gate(&[]),
            "read-1",
        ))
        .expect("folded read remains valid JSON");
        assert_eq!(folded["path"], "src/lib.rs");
        assert_eq!(folded["start_line"], 40);
        assert!(!folded["content"].as_str().unwrap().contains("folded"));
        assert!(folded.to_string().contains("lengthy source body"));
    }

    #[test]
    fn historical_edit_keeps_verification_receipt_not_full_diff() {
        let mut store = MessageStore::new_ephemeral("test");
        store.push_user("make edit");
        store.push_assistant(assistant_with_tools(&[("edit-1", "edit")]));
        store.push_tool_result_direct(
            "edit-1",
            "[OK] src/lib.rs:42 +3 -2 | edit_file\n\n@@ -42,2 +42,3 @@\n-full diff body",
            true,
        );

        assert_eq!(
            context_result(&store.build_context_for_gate(&[]), "edit-1"),
            "[OK] src/lib.rs:42 +3 -2 | edit_file\n[edit diff folded; verify the affected range with read before making dependent changes]"
        );
    }

    #[test]
    fn skill_result_remains_complete_across_steps_and_turns() {
        let activation = format!(
            "[DEEPX_SKILL_V1]\nname: large\n--- instructions ---\n{}\n[END_DEEPX_SKILL_V1]",
            "instruction\n".repeat(600)
        );
        let mut store = MessageStore::new_ephemeral("test");
        store.push_user("activate it");
        store.push_assistant(assistant_with_tools(&[("skill-1", "skills")]));
        store.push_tool_result_direct("skill-1", &activation, true);

        assert_eq!(
            context_result(&store.build_context_for_gate(&[]), "skill-1"),
            activation
        );
        store.push_user("continue");
        assert_eq!(
            context_result(&store.build_context_for_gate(&[]), "skill-1"),
            activation
        );
    }

    #[test]
    fn skills_resource_action_remains_complete_across_steps_and_turns() {
        let resource = "reference line\n".repeat(600);
        let mut store = MessageStore::new_ephemeral("test");
        store.push_user("read its reference");
        store.push_assistant(assistant_with_tools(&[("resource-1", "skills")]));
        store.push_tool_result_direct("resource-1", &resource, true);

        assert_eq!(
            context_result(&store.build_context_for_gate(&[]), "resource-1"),
            resource
        );
        store.push_user("continue");
        assert_eq!(
            context_result(&store.build_context_for_gate(&[]), "resource-1"),
            resource
        );
    }

    #[test]
    fn restore_keeps_distinct_protected_skill_system_messages() {
        let skill = "[DEEPX_SKILL_V1]\nname: review\n[END_DEEPX_SKILL_V1]";
        let messages = vec![
            Message::system("base prompt"),
            Message::system(skill),
            Message::system(skill),
            Message::system("stale duplicate prompt"),
            Message::user("hello"),
        ];
        let (store, repairs) = MessageStore::from_messages("test", &messages, 0);
        assert_eq!(store.system_messages().len(), 2);
        assert!(store.system_messages().iter().any(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Text { text } if text == skill))
        }));
        assert_eq!(repairs.len(), 2);
    }

    #[test]
    fn truncated_read_result_explicitly_instructs_follow_up_range() {
        let content = "line\n".repeat(1_100);
        let raw = serde_json::json!({"content": content}).to_string();

        let truncated: serde_json::Value =
            serde_json::from_str(&truncate_tool_result("read", &raw)).expect("valid JSON result");
        let content = truncated["content"].as_str().expect("content string");

        assert!(truncated["truncated"].as_bool().unwrap_or(false));
        assert!(content
            .contains("Call read again with the same path and a later start_line/end_line range."));
    }

    #[test]
    fn plain_read_result_is_truncated_at_line_boundary_with_follow_up_hint() {
        let result = "line\n".repeat(1_100);
        let truncated = truncate_tool_result("read", &result);

        assert!(truncated.len() < result.len());
        assert!(truncated
            .contains("Call read again with the same path and a later start_line/end_line range."));
    }

    #[test]
    fn find_last_step_tool_call_returns_correct_id() {
        let mut store = MessageStore::new_ephemeral("test");
        store.push_user("hello");

        let assistant = Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: vec![
                ContentBlock::Text {
                    text: "Let me ask...".into(),
                },
                ContentBlock::ToolUse {
                    id: "call_abc".into(),
                    name: "ask_user".into(),
                    input: serde_json::json!({"question": "Which?"}),
                },
            ],
        };
        store.push_assistant(assistant);

        let id = store.find_last_step_tool_call("ask_user").unwrap();
        assert_eq!(id, "call_abc");
        assert!(store.find_last_step_tool_call("bogus").is_none());
    }

    #[test]
    fn find_last_step_tool_call_no_turns_returns_none() {
        let store = MessageStore::new_ephemeral("test");
        assert!(store.find_last_step_tool_call("ask_user").is_none());
    }

    #[test]
    fn compact_does_not_reuse_archived_turn_ids() {
        let mut store = MessageStore::new_ephemeral("test");
        for index in 1..=5 {
            assert_eq!(store.allocate_turn_id(), format!("t{index}"));
            store.push_user(&format!("turn {index}"));
        }

        store.apply_compact("checkpoint", 2);

        assert_eq!(store.turn_count(), 3); // summary + two kept turns
        assert_eq!(store.allocate_turn_id(), "t6");
    }

    #[test]
    fn repeated_compact_replaces_the_old_summary_in_gate_context() {
        let mut store = MessageStore::new_ephemeral("test");
        for index in 1..=5 {
            store.push_user(&format!("turn {index}"));
        }
        store.apply_compact("first checkpoint body", 2);
        store.push_user("work after first checkpoint");
        store.push_user("latest work");

        store.apply_compact("second checkpoint body", 2);

        let serialized = serde_json::to_string(&store.build_context_for_gate(&[])).unwrap();
        assert!(serialized.contains("second checkpoint body"));
        assert!(!serialized.contains("first checkpoint body"));
        assert_eq!(serialized.matches("[Compacted ").count(), 1);
    }

    #[test]
    fn restored_turn_allocator_can_follow_immutable_archive_count() {
        let messages = vec![Message::user("active summary"), Message::user("recent")];
        let (mut store, _) = MessageStore::from_messages("test", &messages, 0);
        store.ensure_next_turn_seq(31);

        assert_eq!(store.allocate_turn_id(), "t31");
    }
}
