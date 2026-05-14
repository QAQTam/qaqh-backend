//! Chat mode keyboard handler.

use crossterm::event::{self, KeyCode, KeyModifiers};
use tokio::sync::mpsc;

use crate::api::{self, StreamEvent};
use crate::config;

use super::super::app::App;
use super::super::utils::{char_boundary, prev_char_boundary, next_char_boundary};
use super::super::{Mode, SubMenuState};

pub(crate) fn handle_chat_key(
    app: &mut App,
    key: event::KeyEvent,
    tx: mpsc::Sender<StreamEvent>,
) {
    // ── Slash menu navigation ──
    if app.slash_open {
        // ── Submenu handling (priority over main menu) ──
        if app.slash_submenu.is_some() {
            match key.code {
                KeyCode::Esc => {
                    app.slash_submenu = None;
                    return;
                }
                KeyCode::Up => {
                    if let Some(ref mut sub) = app.slash_submenu {
                        sub.idx = sub.idx.saturating_sub(1);
                    }
                    return;
                }
                KeyCode::Down => {
                    if let Some(ref mut sub) = app.slash_submenu {
                        if sub.idx + 1 < sub.items.len() {
                            sub.idx += 1;
                        }
                    }
                    return;
                }
                KeyCode::Enter => {
                    if let Some(sub) = app.slash_submenu.take() {
                        if let Some((val, _)) = sub.items.get(sub.idx) {
                            app.input = format!("{} {} ", sub.parent_cmd, val);
                            app.input_cursor = app.input.len();
                            app.status_msg = format!("\u{00BB} {} {}", sub.parent_cmd, val);
                        }
                    }
                    app.slash_open = false;
                    return;
                }
                _ => return, // Block all other keys while submenu is open
            }
        }

        // ── Space opens submenu ──
        if key.code == KeyCode::Char(' ') {
            if let Some((cmd, _)) = app.slash_items.get(app.slash_idx) {
                if let Some(items) = build_submenu(cmd) {
                    app.slash_submenu = Some(SubMenuState {
                        parent_cmd: cmd.clone(),
                        items,
                        idx: 0,
                    });
                }
            }
            return;
        }

        match key.code {
            KeyCode::Esc => {
                app.slash_open = false;
                app.input.clear();
                app.input_cursor = 0;
                // Also consume this Esc event so it doesn't fall through to global exit
                app.esc_just_closed_menu = true;
                return;
            }
            KeyCode::Up => {
                app.slash_idx = app.slash_idx.saturating_sub(1);
                return;
            }
            KeyCode::Down => {
                if app.slash_idx + 1 < app.slash_items.len() {
                    app.slash_idx += 1;
                }
                return;
            }
            KeyCode::Enter | KeyCode::Tab => {
                if let Some((cmd, _)) = app.slash_items.get(app.slash_idx) {
                    let label = cmd.clone();
                    app.input = format!("{} ", cmd);
                    app.input_cursor = app.input.len();
                    app.status_msg = format!("\u{00BB} {} \u{2014} type args and Enter", label);
                }
                app.slash_open = false;
                return;
            }
            KeyCode::Char(c) => {
                let pos = char_boundary(&app.input, app.input_cursor);
                app.input.insert(pos, c);
                app.input_cursor = pos + c.len_utf8();
                app.slash_filter = app.input.clone();
                update_slash_items(app);
                app.slash_idx = 0;
                if app.slash_items.is_empty() {
                    app.slash_open = false;
                }
                return;
            }
            KeyCode::Backspace => {
                if app.input_cursor > 0 {
                    let prev = prev_char_boundary(&app.input, app.input_cursor);
                    app.input.remove(prev);
                    app.input_cursor = prev;
                    app.slash_filter = app.input.clone();
                    update_slash_items(app);
                    app.slash_idx = 0;
                }
                return;
            }
            _ => return,
        }
    }

    // ── Setup wizard key handling (before normal chat keys) ──
    if app.mode == Mode::ApiConfig {
        match key.code {
            KeyCode::Esc => {
                std::process::exit(0);
            }
            KeyCode::Enter => {
                let mut key = app.input.trim().to_string();
                if key.len() >= 10 {
                    // Normalize: strip doubled prefix, handle mixed-case
                    let key_lower = key.to_lowercase();
                    while key_lower.starts_with("sk-sk-") {
                        key = key.replacen("sk-", "", 1);
                        break;
                    }
                    let key2 = if key.to_lowercase().starts_with("sk-") {
                        key
                    } else {
                        format!("sk-{}", key)
                    };
                    app.config.api_key = key2;
                    app.status_msg = "Verifying...".into();
                    let cfg = app.config.clone();
                    let tx2 = tx.clone();
                    tokio::spawn(async move {
                        use tokio::net::TcpStream;
                        // Step 1: test network reachability
                        let net_ok = TcpStream::connect("api.deepseek.com:443")
                            .await
                            .map(|_| ())
                            .is_ok();
                        if !net_ok {
                            let _ = tx2.send(StreamEvent::BalanceResult("netfail".into())).await;
                            return;
                        }
                        // Step 2: test API key validity with specific error
                        let result = match api::list_models(&cfg).await {
                            Ok(_) => "ok".to_string(),
                            Err(e) => {
                                let err_str = format!("{}", e);
                                if err_str.contains("401") {
                                    "fail_auth".into()
                                } else {
                                    format!(
                                        "fail_{}",
                                        err_str.chars().take(40).collect::<String>()
                                    )
                                }
                            }
                        };
                        let _ = tx2.send(StreamEvent::BalanceResult(result)).await;
                    });
                } else {
                    app.status_msg = "Key must be at least 10 characters".into();
                }
            }
            KeyCode::Char(c) => {
                let pos = char_boundary(&app.input, app.input_cursor);
                app.input.insert(pos, c);
                app.input_cursor += 1;
            }
            KeyCode::Backspace => {
                if app.input_cursor > 0 {
                    let prev = prev_char_boundary(&app.input, app.input_cursor);
                    app.input.remove(prev);
                    app.input_cursor -= 1;
                }
            }
            _ => {}
        }
        return; // Block other key handling during setup
    }

    match key.code {
        KeyCode::Esc => {
            if !app.input.is_empty() {
                app.input.clear();
                app.input_cursor = 0;
            }
        }
        KeyCode::Enter => {
            if key.modifiers == KeyModifiers::CONTROL {
                let pos = char_boundary(&app.input, app.input_cursor);
                app.input.insert(pos, '\n');
                app.input_cursor = pos + 1;
            } else {
                app.send_message(tx);
            }
        }
        KeyCode::Char(c) if key.modifiers == KeyModifiers::CONTROL => {
            match c {
                'd' | '\x04' => {
                    if app.auto_scroll {
                        app.scroll_offset = app.last_max_scroll;
                        app.auto_scroll = false;
                    }
                    app.scroll_offset = app.scroll_offset.saturating_add(12);
                }
                'u' | '\x15' => {
                    if app.auto_scroll {
                        app.scroll_offset = app.last_max_scroll;
                        app.auto_scroll = false;
                    }
                    app.scroll_offset = app.scroll_offset.saturating_sub(12);
                }
                _ => {}
            }
            return;
        }
        KeyCode::Char(c) => {
            let pos = char_boundary(&app.input, app.input_cursor);
            app.input.insert(pos, c);
            app.input_cursor = pos + c.len_utf8();
            // Open slash menu when user types / at start
            if c == '/' && app.input.trim() == "/" {
                app.slash_open = true;
                app.slash_filter = app.input.clone();
                update_slash_items(app);
                app.slash_idx = 0;
            }
        }
        KeyCode::Backspace => {
            if app.input_cursor > 0 {
                let prev = prev_char_boundary(&app.input, app.input_cursor);
                app.input.remove(prev);
                app.input_cursor = prev;
            }
        }
        KeyCode::Delete => {
            if app.input_cursor < app.input.len() {
                app.input.remove(app.input_cursor);
            }
        }
        KeyCode::Left => app.input_cursor = prev_char_boundary(&app.input, app.input_cursor),
        KeyCode::Right => app.input_cursor = next_char_boundary(&app.input, app.input_cursor),
        KeyCode::Home => app.input_cursor = 0,
        KeyCode::End => {
            app.input_cursor = app.input.len();
            app.auto_scroll = true;
        }
        KeyCode::Up => {
            if app.auto_scroll {
                app.scroll_offset = app.last_max_scroll;
                app.auto_scroll = false;
            }
            app.scroll_offset = app.scroll_offset.saturating_sub(3);
        }
        KeyCode::Down => {
            if app.auto_scroll {
                app.scroll_offset = app.last_max_scroll;
                app.auto_scroll = false;
            }
            app.scroll_offset = app.scroll_offset.saturating_add(3);
        }
        KeyCode::PageUp => {
            app.exec_scroll_offset = app.exec_scroll_offset.saturating_add(20);
        }
        KeyCode::PageDown => {
            app.exec_scroll_offset = app.exec_scroll_offset.saturating_sub(20);
        }
        KeyCode::Tab => {}
        _ => {}
    }
}

pub(crate) fn update_slash_items(app: &mut App) {
    let filter = app.slash_filter.trim().to_lowercase();
    let cmds = config::all_commands(&app.config.prompt_lang);
    app.slash_items = cmds
        .iter()
        .filter(|(cmd, _)| cmd.to_lowercase().contains(&filter))
        .map(|(c, d)| (c.to_string(), d.to_string()))
        .collect();
}

pub(crate) fn build_submenu(cmd: &str) -> Option<Vec<(String, String)>> {
    match cmd {
        "/effort" => Some(vec![
            ("high".into(), "High reasoning effort".into()),
            ("max".into(), "Maximum reasoning effort".into()),
        ]),
        "/model" => Some(vec![
            ("deepseek-v4-flash".into(), "Fast, cost-effective".into()),
            ("deepseek-v4-pro".into(), "High capability".into()),
        ]),
        "/lang" => Some(vec![
            ("zh".into(), "中文 UI + 中文提示词".into()),
            ("en".into(), "English UI + English prompt".into()),
        ]),
        _ => None,
    }
}
