//! deepx-tui — Terminal UI for DeepX AI Assistant.
//!
//! Architecture:
//! - Connects to the DeepX daemon via WebSocket (`deepx-client`)
//! - Two screens: SessionList (splash) → Chat (conversation)
//! - Multi-line input with paste confirmation
//! - Inline diff blocks for file edits, streaming exec progress
//!
//! Uses `ratatui` 0.30 + `ratatui-markdown` + `ratatui-textarea`.

mod app;
mod client;
mod diff;
mod events;
mod input;
mod ui;

use anyhow::Result;
use crossterm::{
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io;
use std::time::Duration;
use tokio::sync::mpsc;

use app::App;
use client::TuiClient;
use events::{AppAction, event_to_action};

#[tokio::main]
async fn main() -> Result<()> {
    // ------------------------------------------------------------------
    // Terminal setup
    // ------------------------------------------------------------------
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Run the app — restore terminal on exit
    let result = run(&mut terminal).await;

    // ------------------------------------------------------------------
    // Terminal teardown
    // ------------------------------------------------------------------
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
}

async fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    let mut app = App::new();

    // Get terminal size
    if let Ok(size) = crossterm::terminal::size() {
        app.resize(size.0, size.1);
    }

    // ------------------------------------------------------------------
    // Connect to daemon
    // ------------------------------------------------------------------
    let tui_client = match TuiClient::connect_or_launch().await {
        Ok(tc) => {
            app.connected = true;
            app.status_message = String::from("Connected");
            tc
        }
        Err(e) => {
            app.status_message = format!("Connection failed: {}", e);
            // Draw error screen and wait for quit
            terminal.draw(|f| ui::render(f, &mut app))?;
            // Wait a bit so user can see the error
            tokio::time::sleep(Duration::from_secs(2)).await;
            return Err(e);
        }
    };

    // Request session list
    if let Ok(result) = tui_client.list_sessions().await {
        app.replace_sessions_from_value(result);
    }

    // Subscribe to events (separate from TuiClient's cached receiver)
    let mut client_events = tui_client.client.subscribe();

    // ------------------------------------------------------------------
    // Keyboard event channel (separate thread)
    // ------------------------------------------------------------------
    let (action_tx, mut action_rx) = mpsc::unbounded_channel::<AppAction>();

    std::thread::spawn(move || {
        while let Ok(event) = crossterm::event::read() {
            if let Some(action) = event_to_action(event)
                && action_tx.send(action).is_err()
            {
                break;
            }
        }
    });

    // ------------------------------------------------------------------
    // Main loop
    // ------------------------------------------------------------------
    let mut tick_interval = tokio::time::interval(Duration::from_millis(16)); // ~60fps

    loop {
        // Check for client event before select
        let daemon_msg = client_events.try_recv().ok();

        // Process daemon message if any
        if let Some(msg) = daemon_msg {
            app.handle_control_message(msg);
        }

        // Process pending keyboard actions (non-blocking)
        while let Ok(action) = action_rx.try_recv() {
            match action {
                AppAction::Quit => return Ok(()),
                AppAction::Tick => {} // handled below

                _ => {
                    handle_action(&mut app, &tui_client, action).await;
                }
            }
        }

        // Also await on tick + keyboard (avoid busy loop)
        tokio::select! {
            // Tick for redraw + streaming updates
            _ = tick_interval.tick() => {}

            // Keyboard events
            Some(action) = action_rx.recv() => {
                match action {
                    AppAction::Quit => return Ok(()),
                    _ => {
                        handle_action(&mut app, &tui_client, action).await;
                    }
                }
            }

            // Daemon messages
            Ok(msg) = client_events.recv() => {
                app.handle_control_message(msg);
            }
        }

        // Always redraw after processing events
        terminal.draw(|f| ui::render(f, &mut app))?;
    }
}

/// Handle a keyboard action, possibly making async requests to daemon.
async fn handle_action(app: &mut App, client: &TuiClient, action: AppAction) {
    if let AppAction::Resize(width, height) = &action {
        app.resize(*width, *height);
        return;
    }

    match app.mode {
        app::Mode::SessionList => match action {
            AppAction::SelectUp | AppAction::Char('k') => app.select_up(),
            AppAction::SelectDown | AppAction::Char('j') => app.select_down(),
            AppAction::Confirm => {
                if let Some(session) = app.selected_session() {
                    let seed = session.seed.clone();
                    if let Err(e) = client.attach_session(&seed).await {
                        app.status_message = format!("Attach failed: {}", e);
                        return;
                    }
                    app.open_session(seed);
                    if let Err(e) = client
                        .resume_session(app.current_session_seed.as_deref().unwrap_or_default())
                        .await
                    {
                        app.status_message = format!("Resume failed: {e}");
                    }
                }
            }
            AppAction::NewSession => {
                if let Err(error) = create_and_open_session(app, client).await {
                    app.status_message = format!("Create failed: {error}");
                }
            }
            AppAction::Delete | AppAction::DeleteSession => {
                if let Some(session) = app.selected_session() {
                    let seed = session.seed.clone();
                    if let Err(e) = client.delete_session(&seed).await {
                        app.status_message = format!("Delete failed: {}", e);
                    } else if let Ok(result) = client.list_sessions().await {
                        app.replace_sessions_from_value(result);
                    }
                }
            }
            AppAction::RefreshSessions => match client.list_sessions().await {
                Ok(result) => {
                    app.replace_sessions_from_value(result);
                    app.status_message = String::from("Sessions refreshed");
                }
                Err(error) => {
                    app.status_message = format!("Refresh failed: {error}");
                }
            },
            _ => {}
        },

        app::Mode::Chat => match action {
            AppAction::BackToSessions => {
                if app.paste_state != app::PasteState::None {
                    app.cancel_paste();
                    return;
                }
                if let Some(ref seed) = app.current_session_seed {
                    let _ = client.detach_session(seed).await;
                }
                app.back_to_sessions();
                if let Ok(result) = client.list_sessions().await {
                    app.replace_sessions_from_value(result);
                }
            }
            AppAction::Confirm | AppAction::SendMessage => {
                // If paste is pending, confirm it
                if app.paste_state != app::PasteState::None {
                    if action == AppAction::Confirm {
                        app.confirm_paste();
                    } else {
                        app.cancel_paste();
                    }
                    return;
                }

                // Enter (or Ctrl+Enter) sends the current message.
                let text = app.take_input();
                if !text.trim().is_empty()
                    && let Some(ref seed) = app.current_session_seed
                {
                    match client.send_text(seed, &text).await {
                        Ok(_) => {
                            app.status_message = String::from("Sending...");
                            app.scroll_to_bottom();
                        }
                        Err(e) => {
                            app.status_message = format!("Send failed: {}", e);
                            // Put text back in input
                            app.input.set_text(&text);
                        }
                    }
                }
            }
            AppAction::Newline => {
                app.input_newline();
            }
            AppAction::Char(c) => {
                if app.paste_state != app::PasteState::None {
                    match c.to_ascii_lowercase() {
                        'y' => app.confirm_paste(),
                        'n' => app.cancel_paste(),
                        _ => {}
                    }
                } else {
                    app.input_char(c);
                }
            }
            AppAction::Backspace => {
                app.input_backspace();
            }
            AppAction::Delete => {
                app.input_delete();
            }
            AppAction::CursorLeft => {
                app.input_cursor_left();
            }
            AppAction::CursorRight => {
                app.input_cursor_right();
            }
            AppAction::SelectUp => {
                app.input_cursor_up();
            }
            AppAction::SelectDown => {
                app.input_cursor_down();
            }
            AppAction::Home => {
                app.input_home();
            }
            AppAction::End => {
                app.input_end();
            }
            AppAction::Paste(text) => {
                app.paste_detected(&text);
            }
            AppAction::ConfirmPaste => {
                app.confirm_paste();
            }
            AppAction::CancelPaste => {
                app.cancel_paste();
            }
            AppAction::ScrollUp => app.scroll_up(),
            AppAction::ScrollDown => app.scroll_down(),
            AppAction::PageUp => app.page_up(),
            AppAction::PageDown => app.page_down(),
            AppAction::ScrollBottom => app.scroll_to_bottom(),
            AppAction::NewSession => {
                if let Some(seed) = app.current_session_seed.as_deref() {
                    let _ = client.detach_session(seed).await;
                }
                if let Err(error) = create_and_open_session(app, client).await {
                    app.status_message = format!("Create failed: {error}");
                }
            }
            _ => {}
        },
    }
}

async fn create_and_open_session(app: &mut App, client: &TuiClient) -> Result<()> {
    let seed = client.create_session().await?;
    client.attach_session(&seed).await?;
    app.open_new_session(seed);
    if let Ok(result) = client.list_sessions().await {
        app.replace_sessions_from_value(result);
    }
    Ok(())
}
