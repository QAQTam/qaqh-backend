//! Self-learning capture: extract patterns from completed turns.

use crate::tui::app::App;
use crate::types::Message;
use crate::memory;
use crate::session;
use crate::health::HealthLevel;
use crate::orchestrator::turn_scorer;

// ── Memory extraction ──

pub fn auto_extract_memory(app: &mut App, final_msg: &Message) {
    let round_num = app.messages.iter()
        .filter(|m| m.role == "user" && m.content.as_deref().map_or(false, |c| !c.starts_with('[')))
        .count();

    // 1. Extract file changes and errors from tool_results (consume them)
    let mut all_files: Vec<String> = Vec::new();
    let mut all_tools: Vec<String> = Vec::new();
    let mut had_errors = false;
    // Per-file operations for learning log: (tool_name, file_path, result)
    let mut file_ops: Vec<(String, String, String)> = Vec::new();
    for (tool_name, result) in app.tool_results.drain(..) {
        all_tools.push(tool_name.clone());
        let files = memory::extract_files_from_tool(&tool_name, &result);
        for f in &files {
            let added = result.lines().count() as u32;
            app.long_mem.add_file_change(f, added, 0);
            file_ops.push((tool_name.clone(), f.clone(), result.clone()));
        }
        all_files.extend(files);
        if result.starts_with("[ERROR]") || result.starts_with("[FAIL]") {
            app.health.record_error(&tool_name, &result);
            had_errors = true;
        }
    }

    // 1b. Update context health stats + sync app-level counters
    app.health.context_tokens = app.tokens_used();
    app.health.context_limit = app.config.context_limit;
    // Only sync tool_only_turns when errors occurred — emotion should reflect
    // failure patterns, not successful tool-only turns (which are normal workflow).
    if had_errors {
        app.health.consecutive_tool_only_turns = app.consecutive_tool_turns;
    } else {
        app.health.consecutive_tool_only_turns = 0;
    }

    // Idle detection: if in chat mode with no tools, increment idle counter
    if all_tools.is_empty() && (app.current_task_phase == crate::types::TaskPhase::Chat
        || app.current_task_phase == crate::types::TaskPhase::Coding) {
        app.health.idle_chat_turns = app.health.idle_chat_turns.saturating_add(1);
    } else {
        app.health.idle_chat_turns = 0;
    }

    // Explore-before-read: track turns since last read, warn at 3+
    app.turns_since_last_read = app.turns_since_last_read.saturating_add(1);
    if app.turns_since_last_read == 3 && app.has_explored {
        app.status_msg = format!(
            "{} turns without reading files. Context may be missing. Consider read_file().",
            app.turns_since_last_read);
    }

    // 1c. Record turn + assess health
    app.health.record_turn(had_errors);
    let assessment = app.health.assess();
    let report = app.health.render_health();
    crate::health::update_health_report(report);

    // Health status — store for TUI display, inject as system note for AI awareness
    let level_tag = if assessment.level == HealthLevel::Red { "RED" }
        else if assessment.level == HealthLevel::Yellow { "YLW" }
        else { "OK" };
    let turn = app.health.turn;
    app.health_status_line = format!(
        "[{} {} {} | tier:{:?} | {}% | t{}]",  // TUI keeps turn; API message below omits it for cache
        level_tag,
        assessment.emotion.emoji(),
        assessment.emotion.label(),
        app.health.context_tier,
        (assessment.success_rate * 100.0) as u32,
        turn,
    );
    // Append health status to the last message (preserves user/assistant alternation)
    let _health_text = format!(
        "\n[HEALTH: {} {} {} | tier:{:?} | {}%]",
        level_tag,
        assessment.emotion.emoji(),
        assessment.emotion.label(),
        app.health.context_tier,
        (assessment.success_rate * 100.0) as u32,
    );
    app.ctx.annotate("HEALTH", &format!("{} {} {}", level_tag, assessment.emotion.emoji(), assessment.emotion.label()));
    app.ctx.flush_annotations();

    // Pre-extract for learning capture (before interrupt moves from assessment)
    let assess_emo = assessment.emotion;
    let assess_label = assessment.emotion.label().to_string();

    if let Some(interrupt) = assessment.interrupt {
        app.ctx.annotate("HEALTH", &interrupt);
        app.ctx.flush_annotations();
        if assessment.level == HealthLevel::Red {
            app.status_msg = format!("[HEALTH: RED] {}", assessment.advice);
        }
    }

    // 1d. Pitfall extraction — scan AI's response for lesson keywords
    if had_errors {
        if let Some(ref content) = final_msg.content {
            let pitfall_kws = ["lesson:", "lesson is", "下次", "需要注意",
                "avoid:", "watch out:", "记住", "教训", "root cause:",
                "根因", "因为", "所以以后", "key takeaway", "takeaway:",
                "learned:", "学到了", "避坑"];
            if pitfall_kws.iter().any(|kw| content.to_lowercase().contains(kw)) {
                let desc = content.lines().next().unwrap_or("");
                let lesson = content.lines()
                    .filter(|l| pitfall_kws.iter().any(|kw| l.to_lowercase().contains(kw)))
                    .take(3)
                    .collect::<Vec<_>>()
                    .join("; ");
                if !lesson.is_empty() {
                    let top_kind = app.health.error_counts.iter()
                        .max_by_key(|(_, &c)| c)
                        .map(|(&k, _)| k)
                        .unwrap_or(crate::health::ErrorKind::Unknown);
                    let tool = all_tools.last().cloned().unwrap_or_default();
                    app.pitfall_guide.upsert(
                        assessment.emotion,
                        top_kind,
                        &tool,
                        desc,
                        &lesson,
                        &all_files,
                    );
                    crate::session::save_pitfalls(&app.pitfall_guide);
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
    app.short_mem.append_round(round_num, &tool_names, &all_files, &summary);

    // 2b. Capture reasoning → learning log
    capture_learning_entry(app, final_msg, round_num, &tool_names, &all_files,
                             &file_ops, had_errors, assess_emo, &assess_label);

    // 3. Write memory to disk
    if !app.session_seed.is_empty() {
        session::write_memory(&app.session_seed, "long", &app.long_mem.render());
        session::write_memory_preserving_notes(
            &app.session_seed, "short", &app.short_mem.render());
        session::write_semantic_memory(&app.session_seed, &app.semantic_memory);
    }
}

/// Capture AI reasoning + outcome into learning.md for cross-session self-improvement.
/// Extracts per-file "why" from the assistant content alongside each file operation.
pub fn capture_learning_entry(app: &App, final_msg: &Message, round: usize,
                               tools: &[String], files: &[String],
                               file_ops: &[(String, String, String)],
                               had_errors: bool,
                               emo: crate::health::AgentEmotion, label: &str) {
    if app.session_seed.is_empty() { return; }

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
            "read_file" => per_file_lines.push(format!("**读:** {} → {}", path, why)),
            "write_file" => per_file_lines.push(format!("**写:** {} → {}", path, why)),
            "edit_file" => per_file_lines.push(format!("**改:** {} → {}", path, why)),
            _ => per_file_lines.push(format!("**{}:** {} → {}", tool_name, path, why)),
        }
    }
    // For plan tools, extract plan name
    for tool_name in tools {
        if tool_name.starts_with("plan_") && !per_file_lines.iter().any(|l| l.contains("plan")) {
            per_file_lines.push(format!("**计划:** {}", tool_name));
        }
    }

    // Detect lesson from response content
    let lesson = final_msg.content.as_ref().and_then(|content| {
        let kws = ["lesson:", "lesson is", "下次", "需要注意", "avoid:", "watch out:",
                   "root cause:", "根因", "学到了", "key takeaway:"];
        kws.iter().find_map(|kw| {
            content.find(kw).map(|pos| {
                content[pos..].chars().take(150).collect::<String>()
            })
        })
    });

    let outcome = if tools.is_empty() || tools == &["text".to_string()] {
        "💬 text-only"
    } else if had_errors {
        "❌ failed"
    } else {
        "✅ success"
    };

    let tools_str = if tools.is_empty() { "-".into() } else { tools.join(", ") };
    let files_str = if files.is_empty() { "-".into() } else { files.join(", ") };
    let emo_str = emo.emoji();

    // Classify entry type
    let tag = if had_errors {
        "[错误]"
    } else if !file_ops.is_empty() {
        "[操作]"
    } else {
        "[思考]"
    };

    // Build entry
    let mut entry = format!(
        "### t{} | {} {} {}\n**方向:** {}\n",
        round, emo_str, label, tag, direction,
    );
    for line in &per_file_lines {
        entry.push_str(line);
        entry.push('\n');
    }
    entry.push_str(&format!("**执行:** {} → {}\n**结果:** {}\n**教训:** {}\n",
        tools_str, files_str, outcome,
        lesson.unwrap_or_else(|| "-".into()),
    ));

    session::append_learning(&app.session_seed, &entry);
}
