//! dsx-tools standalone binary — IPC service process.
//!
//! Reads JSON-LP frames from stdin, dispatches to ToolManager,
//! writes response frames to stdout.
//!
//! ## Protocol
//!
//! All frames are newline-delimited JSON (JSON-LP) over stdin/stdout.
//! See `tools::ipc` for frame type definitions.
//!
//! ## Usage
//!
//! ```text
//! dsx-tools
//! ```
//!
//! No CLI arguments. The Agent subprocess manager spawns this binary
//! and communicates via stdio.
//!
//! ## Startup sequence
//!
//! 1. Build ToolManager with all registered handlers
//! 2. Write `Ready` frame with tool definitions to stdout
//! 3. Enter `ipc_main_loop` — read stdin, dispatch, write stdout
//! 4. On EOF or `Shutdown` frame: exit cleanly

// ── Stub modules (satisfy `crate::` references in shared sub-modules) ──

/// Stub for `crate::api::StreamEvent` — used by exec.rs async spawn fns
/// which are never called in standalone mode.
mod api {
    pub enum StreamEvent {
        ExecDone(String, String),
        ExecStarted(String, u32),
        ExecProgress(String),
    }
}

/// Stub for `crate::memory::SemanticMemory` — used by dsx-types/session.rs.
mod memory {
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    pub struct SemanticMemory;
}

// ── Shared modules ──

/// Shared type definitions (dsx-types).
#[path = "../dsx-types/mod.rs"]
mod types;

/// ToolManager + IPC framework (dsx-tools).
#[path = "../dsx-tools/mod.rs"]
mod tools;

fn main() {
    // Build the fully-populated ToolManager
    let mut manager = tools::registration::build_tool_manager();

    // Enter the IPC event loop (reads stdin, writes stdout)
    tools::ipc::ipc_main_loop(&mut manager);
}
