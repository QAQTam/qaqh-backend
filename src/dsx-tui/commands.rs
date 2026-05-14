use crate::api::{self, StreamEvent};
use crate::config;
use crate::{context, tokenizer};
use crate::conversation::ContextAssembler;
use crate::tui::app::App;
use tokio::sync::mpsc;

pub fn handle_slash_command(app: &mut App, input: &str, tx: mpsc::Sender<StreamEvent>) {
    let s = input.trim();
    if let Some(msg) = config::handle_reset_command(s, &mut app.config) {
        app.status_msg = msg;
    } else if let Some(msg) = config::handle_reconfig_command(s, &mut app.config) {
        app.status_msg = msg;
    } else if let Some(msg) = config::handle_effort_command(s, &mut app.config) {
        app.status_msg = msg;

    } else if let Some(msg) = config::handle_lang_command(s, &mut app.config) {
        if let Some(sys) = app.messages.first_mut() {
            if sys.role == "system" {
                sys.content = Some(config::system_prompt(&app.config.prompt_lang).into());
            }
        }
        app.status_msg = msg;
    } else if let Some(msg) = config::handle_model_command(s, &mut app.config) {
        app.status_msg = msg;
    } else if let Some(msg) = config::handle_profile_command(s, &mut app.config) {
        app.status_msg = msg;
    } else if s == "/balance" {
        app.status_msg = "Querying...".into();
        let cfg = app.config.clone();
        let btx = tx.clone();
        tokio::spawn(async move {
            let r = api::get_balance(&cfg).await;
            let msg = match r {
                Ok(info) => info.balance_infos.first()
                    .map(|b| format!("{} {}", b.total_balance, b.currency))
                    .unwrap_or_else(|| "N/A".into()),
                Err(e) => format!("err: {}", e),
            };
            let _ = btx.send(StreamEvent::BalanceResult(msg)).await;
        });
    } else if s == "/fix" {
        let before = app.messages.len();
        app.repair_messages();
        let removed = before.saturating_sub(app.messages.len());
        let filled = app.messages.iter().filter(|m| m.role == "tool" && m.content.as_deref().unwrap_or("").contains("was cancelled")).count();
        // Verify fix worked
        match app.health.check_alternation(&app.messages) {
            None => {
                app.health.has_orphan_tool_uses = false;
                app.status_msg = format!("/fix ✓ 删除 {} 个孤儿，填充 {} 个孤儿。请求体已干净。", removed, filled);
            },
            Some(issue) => app.status_msg = format!("/fix ✗ 修复后仍有问题: {}。消息队列可能损坏，输入 /clear 重置。", issue),
        }
    } else if s == "/context" {
        if let Some(ref bd) = app.token_breakdown {
            let pct = app.context_pct();
            let msg = format!("context: {} / {} ({:.0}%)\nsystem:{}  semantic:{}  backbone:{}  messages:{}",
                tokenizer::format_tokens(app.token_estimate),
                tokenizer::format_tokens(app.config.context_limit),
                pct * 100.0,
                tokenizer::format_tokens(bd.system),
                tokenizer::format_tokens(bd.semantic),
                tokenizer::format_tokens(bd.long_mem),
                tokenizer::format_tokens(bd.episodic),
            );
            app.status_msg = msg;
        } else {
            app.status_msg = "context: 0 — send a message first.".into();
        }
    } else if s == "/clear" {
        let sys = app.messages.first().cloned();
        app.ctx = ContextAssembler::new();
        if let Some(sys) = sys { app.ctx.push_system(sys); }
        app.sync_messages();
        app.tool_results.clear();
        app.api_usage = None;
        app.stream_content.clear();
        app.stream_reasoning.clear();
        app.status_msg = "Cleared.".into();
    } else if input.trim() == "/tools" {
        app.tools_enabled = !app.tools_enabled;
        app.status_msg = format!("Tools: {}", if app.tools_enabled { "ON" } else { "OFF" });
    } else if input.trim() == "/auto" {
        app.auto_mode = !app.auto_mode;
        crate::tools::AUTO_MODE.store(app.auto_mode, std::sync::atomic::Ordering::Relaxed);
        app.config.auto_mode = app.auto_mode;
        app.config.save();
        app.status_msg = format!("Auto mode: {}", if app.auto_mode { "ON (AI chooses model/effort via status tool)" } else { "OFF" });
    } else if input.trim() == "/skip-all" {
        app.skip_all = !app.skip_all;
        app.status_msg = format!("Skip-all: {} — all confirmations auto-approved", if app.skip_all { "ON" } else { "OFF" });
    } else if input.trim() == "/dev" {
        app.dev_mode = !app.dev_mode;
        crate::dsc_log::set_level(if app.dev_mode { "debug" } else { "info" });
        app.status_msg = format!("Dev mode: {}", if app.dev_mode { "ON (debug logging + status)" } else { "OFF" });
    } else if input.trim().starts_with("/compact") {
        let rest = input.trim()[8..].trim();
        if rest == "force" || rest == "now" {
            let lvl = context::CompactLevel::L3;
            let tokens = context::force_compact(&mut app.messages, lvl, &app.turn_scores);
            app.token_estimate = tokens;
            app.status_msg = format!("Forced L3: {} tokens", tokenizer::format_tokens(tokens));
        } else if rest == "hard" {
            let lvl = context::CompactLevel::L4;
            let tokens = context::force_compact(&mut app.messages, lvl, &app.turn_scores);
            app.token_estimate = tokens;
            app.status_msg = format!("Forced L4: {} tokens", tokenizer::format_tokens(tokens));
        } else {
            let mut prepared = app.messages.clone();
            let kf: std::collections::HashSet<String> = app.semantic_memory.entries.keys().cloned().collect();
            let (tokens, level) = context::compact_context(&mut prepared, app.config.context_limit, &app.turn_scores, &kf);
            app.messages = prepared;
            app.token_estimate = tokens;
            app.status_msg = match level {
                context::CompactLevel::None => "Context clean.".into(),
                lvl => format!("Compacted {:?}: {} tokens", lvl, tokenizer::format_tokens(tokens)),
            };
        }
    } else {
        app.status_msg = format!("Unknown: {}", input);
    }
    app.maybe_save_session();
}
