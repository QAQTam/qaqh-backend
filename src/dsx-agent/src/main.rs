//! dsx-agent standalone binary entry point.
//!
//! In the new architecture, dsx-agent is an independent process that:
//!   - Receives UserInput from TUI via stdin/stdout IPC
//!   - Sends API requests to HP via TCP IPC
//!   - Dispatches tool calls to Tools via stdin/stdout IPC
//!   - Maintains AgentState (context, memory, health, session)
//!
//! Phase 2 incremental: skeleton that initialises AgentState and
//! enters the event loop. Full IPC wiring is added in Phase 3/4.

use anyhow::Result;

mod orchestrator;
mod agent;
mod config;
mod context;
mod conversation;
mod dsc_log;
mod memory;
mod prompt;
mod router;
mod session;
mod skills;
mod tokenizer;

// Phase 2 bridge: dsx-types as a sub-module for code sharing.
// Phase 4: replace with `extern crate dsx_types;` when dsx-agent becomes a separate crate.
#[path = "../../dsx-types"]
mod dsx_types;
pub use dsx_types as types;

// These are used via crate:: in orchestrator files.
// In the standalone binary, they must be declared here.
// Re-exported for backwards compatibility with orchestrator code.
use crate::agent::AgentState;

// Placeholder for future HP TCP client.
// In Phase 3, this becomes a real TCP connection to dsx-hp.
struct HealthClient;

impl HealthClient {
    async fn connect(_address: &str) -> Result<Self> {
        // TODO: Phase 3 — TCP connection to dsx-hp
        Ok(Self)
    }
}

// Placeholder for future Tools subprocess manager.
// In Phase 3, this becomes a stdin/stdout subprocess manager.
struct ToolClient;

impl ToolClient {
    async fn spawn() -> Result<Self> {
        // TODO: Phase 3 — spawn dsx-tools subprocess
        Ok(Self)
    }
}

// Placeholder for future TUI IPC bridge.
// In Phase 3, this becomes a stdin/stdout connection to dsx-tui.
struct TuiBridge;

impl TuiBridge {
    async fn listen() -> Result<()> {
        // TODO: Phase 3 — read UserInput from stdin, write StreamAction to stdout
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dsc_log::init();
    log::info!("dsx-agent starting");

    let config = config::Config::load()?;
    let mut state = AgentState::new(config);

    // ── Session initialization ──

    let seed = session::generate_seed();
    state.session_seed = seed.clone();
    state.session_start = session::now_epoch();
    dsc_log::set_session(&seed);
    // Phase 3: sync session seed with tool binary: tools::set_current_session(&seed);

    // Restore interrupted session if one exists (Phase 3: move to agent_loop)
    if let Some((session_file, is_live)) = session::load_session_or_live(&seed) {
        if is_live {
            log::warn!("found interrupted session {}, attempting resume", seed);
            // TODO Phase 3: restore stream state and offer resume to user
            // For now, log and finalize the interrupted session
            if session_file.messages.len() > 1 {
                session::finalize_session(
                    &seed,
                    &session_file.messages,
                    &state.config.model,
                    state.config.effort.as_deref(),
                    None,
                );
            }
        } else {
            log::info!("session {} exists on disk ({} messages)", seed, session_file.messages.len());
        }
    }

    log::info!("dsx-agent ready (standalone mode, IPC wiring pending Phase 3)");
    log::info!("session_seed={}", state.session_seed);
    log::info!("model={}", state.config.model);
    log::info!("context_limit={}", state.config.context_limit);

    // Phase 3: wire IPC channels here.
    // let hp = HealthClient::connect("localhost:9090").await?;
    // let tools = ToolClient::spawn().await?;
    // let tui = TuiBridge::listen().await?;

    // Phase 4: enter agent event loop.
    // agent_loop::run(state, hp, tools, tui).await

    Ok(())
}
