//! Self-learning capture: extract patterns from completed turns.

use crate::agent::AgentState;
use crate::types::Message;
use crate::memory;
use crate::session;
use crate::health::HealthLevel;
use crate::orchestrator::turn_scorer;

// ── Memory extraction ──

pub fn auto_extract_memory(state: &mut AgentState, final_msg: &Message) {
    let round_num = state.messages.iter()
        .filter(|m| m.role == "user" && m.content.as_deref().map_or(false, |c| !c.starts_with('[')))
        .count();

    // 1. Extract file changes and errors from tool_results (consume them)
    let mut all_files: Vec<String> = Vec::new();
    let mut all_tools: Vec<String> = Vec::new();
    let mut had_errors = false;
    // Per-file operations for learning log: (tool_name, file_path, result)
    let mut file_ops: Vec<(String, String, String)> = Vec::new();
    for (tool_name, result) in state.tool_results.drain(..) {
        all_tools.push(tool_name.clone());
        let files = memory::extract_files_from_tool(&tool_name, &result);
        for f in &files {
            let added = result.lines().count() as u32;
            state.long_mem.add_file_change(f, added, 0);
            file_ops.push((tool_name.clone(), f.clone(), result.clone()));
        }
        all_files.extend(files);
        if result.starts_with("[ERROR]") || result.starts_with("[FAIL]") {
            state.health.record_error(&tool_name, &result);
            had_errors = true;
        }
    }

    // 1b. Update context health stats + sync state-level counters
    state.health.context_tokens = state.tokens_used();
    state.health.context_limit = state.config.context_limit;
    if had_errors {
        state.health.consecutive_tool_only_turns = state.consecutive_tool_turns;
    } else {
        state.health.consecutive_tool_only_turns = 0;
    }

    // Idle detection: if in chat mode with no tools, increment idle counter
    if all_tools.is_empty() && (state.current_task_phase == crate::types::TaskPhase::Chat
        || state.current_task_phase == crate::types::TaskPhase::Coding) {
        state.health.idle_chat_turns = state.health.idle_chat_turns.saturating_add(1);
    } else {
        state.health.idle_chat_turns = 0;
    }

    // Explore-before-read: track turns since last read, warn at 3+
    state.turns_since_last_read = state.turns_since_last_read.saturating_add(1);
    if state.turns_since_last_read == 3 && state.has_explored {
        // Warning stored in pending_notes for TUI pickup
        let note = format!(
            "{} turns without reading files. Context may be missing. Consider read_file().",
            state.turns_since_last_read);
        state.pending_notes.push(note);
    }

    // 1c. Record turn + assess health
    state.health.record_turn(had_errors);
    let assessment = state.health.assess();
    let report = state.health.render_health();
    crate::health::update_health_report(report);

    // Health status — store for TUI display, inject as system note for AI awareness
    let level_tag = if assessment.level == HealthLevel::Red { "RED" }
        else if assessment.level == HealthLevel::Yellow { "YLW" }
        else { "OK" };
    let turn = state.health.turn;
    state.health_status_line = format!(
        "[{} {} {} | tier:{:?} | {}% | t{}]",
        level_tag,
        assessment.emotion.emoji(),
        assessment.emotion.label(),
        state.health.context_tier,
        (assessment.success_rate * 100.0) as u32,
        turn,
    );
    state.ctx.annotate("HEALTH", &format!("{} {} {}", level_tag, assessment.emotion.emoji(), assessment.emotion.label()));
    state.ctx.flush_annotations();

    // Pre-extract for learning capture (before interrupt moves from assessment)
    let assess_emo = assessment.emotion;
    let assess_label = assessment.emotion.label().to_string();

    if let Some(interrupt) = assessment.interrupt {
        state.ctx.annotate("HEALTH", &interrupt);
        state.ctx.flush_annotations();
    }

    // 1d. Pitfall extraction — scan AI's response for lesson keywords
    if had_errors {
        if let Some(ref content) = final_msg.content {
            let pitfall_kws = ["lesson:", "lesson is", "next time", "avoid:", "watch out:",
                "root cause:", "key takeaway:", "takeaway:",
                "learned:", "记住", "教训", "因为", "所以以后",
                "需要注意", "避坑"];
            if pitfall_kws.iter().any(|kw| content.to_lowercase().contains(kw)) {
                let desc = content.lines().next().unwrap_or("");
                let lesson = content.lines()
                    .filter(|l| pitfall_kws.iter().any(|kw| l.to_lowercase().contains(kw)))
                    .take(3)
                    .collect::<Vec<_>>()
                    .join("; ");
                if !lesson.is_empty() {
                    let top_kind = state.health.error_counts.iter()
                        .max_by_key(|(_, &c)| c)
                        .map(|(&k, _)| k)
                        .unwrap_or(crate::health::ErrorKind::Unknown);
                    let tool = all_tools.last().cloned().unwrap_or_default();
                    state.pitfall_guide.upsert(
                        assessment.emotion,
                        top_kind,
                        &tool,
                        desc,
                        &lesson,
                        &all_files,
                    );
                    crate::session::save_pitfalls(&state.pitfall_guide);
                }
            }
        }
    }

    // 2. Append round to short-term memory
    let tool_names: Vec<String> = match &final_msg.tool_calls {
        Some(tcs) => tcs.iter().map(|tc| tc.function.name.clone()).collect(),
        None if all_tools.is_empty() => vec!["text".into()],
        _ => all_tools,
    };
    all_files.truncate(5);
    let summary = final_msg.content.as_deref().unwrap_or("")
        .lines().next().unwrap_or("")
        .chars().take(100).collect::<String>();
    state.short_mem.append_round(round_num, &tool_names, &all_files, &summary);

    // 2b. Capture reasoning → learning log
    capture_learning_entry(state, final_msg, round_num, &tool_names, &all_files,
                             &file_ops, had_errors, assess_emo, &assess_label);

    // 3. Write memory to disk
    if !state.session_seed.is_empty() {
        session::write_memory(&state.session_seed, "long", &state.long_mem.render());
        session::write_memory_preserving_notes(
            &state.session_seed, "short", &state.short_mem.render());
        session::write_semantic_memory(&state.session_seed, &state.semantic_memory);
    }
}

/// Capture AI reasoning + outcome into learning.md for cross-session self-improvement.
pub fn capture_learning_entry(state: &AgentState, final_msg: &Message, round: usize,
                               tools: &[String], files: &[String],
                               file_ops: &[(String, String, String)],
                               had_errors: bool,
                               emo: crate::health::AgentEmotion, label: &str) {
    if state.session_seed.is_empty() { return; }

    // Extract thinking direction: reasoning first, fallback to content
    let direction = final_msg.reasoning_content.as_ref()
        .and_then(|r| r.lines().find(|l| l.len() > 20 && !l.starts_with("//") && !l.starts_with('#')))
        .or_else(|| final_msg.content.as_ref().and_then(|c| c.lines().find(|l| l.len() > 20)))
        .map(|s| s.chars().take(200).collect::<String>())
        .unwrap_or_else(|| "(no content)".to_string());

    // Extract per-file "why" from assistant content
    let assistant_text = final_msg.content.as_deref().unwrap_or("");
    let reasoning_text = final_msg.reasoning_content.as_deref().unwrap_or("");
    let combined = format!("{}\n{}", assistant_text, reasoning_text);

    let mut per_file_lines: Vec<String> = Vec::new();
    for (tool_name, path, _result) in file_ops {
        let why = turn_scorer::extract_why(&combined, path, tool_name);
        match tool_name.as_str() {
            "read_file" => per_file_lines.push(format!("**Read:** {} -> {}", path, why)),
            "write_file" => per_file_lines.push(format!("**Write:** {} -> {}", path, why)),
            "edit_file" => per_file_lines.push(format!("**Edit:** {} -> {}", path, why)),
            _ => per_file_lines.push(format!("**{}:** {} -> {}", tool_name, path, why)),
        }
    }
    // For plan tools, extract plan name
    for tool_name in tools {
        if tool_name.starts_with("plan_") && !per_file_lines.iter().any(|l| l.contains("plan")) {
            per_file_lines.push(format!("**Plan:** {}", tool_name));
        }
    }

    // Detect lesson from response content
    let lesson = final_msg.content.as_ref().and_then(|content| {
        let kws = ["lesson:", "lesson is", "next time", "avoid:", "watch out:",
                   "root cause:", "key takeaway:"];
        kws.iter().find_map(|kw| {
            content.find(kw).map(|pos| {
                content[pos..].chars().take(150).collect::<String>()
            })
        })
    });

    let outcome = if tools.is_empty() || tools == &["text".to_string()] {
        "text-only"
    } else if had_errors {
        "failed"
    } else {
        "success"
    };

    let tools_str = if tools.is_empty() { "-".into() } else { tools.join(", ") };
    let files_str = if files.is_empty() { "-".into() } else { files.join(", ") };
    let emo_str = emo.emoji();

    // Classify entry type
    let tag = if had_errors {
        "[Error]"
    } else if !file_ops.is_empty() {
        "[Operation]"
    } else {
        "[Thought]"
    };

    // Build entry
    let mut entry = format!(
        "### t{} | {} {} {}\n**Direction:** {}\n",
        round, emo_str, label, tag, direction,
    );
    for line in &per_file_lines {
        entry.push_str(line);
        entry.push('\n');
    }
    entry.push_str(&format!("**Exec:** {} -> {}\n**Result:** {}\n**Lesson:** {}\n",
        tools_str, files_str, outcome,
        lesson.unwrap_or_else(|| "-".into()),
    ));

    session::append_learning(&state.session_seed, &entry);
}
