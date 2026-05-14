mod agent;
mod exec;
pub use exec::{exec_command, exec_with_sudo, spawn_exec_async, spawn_exec_async_with_sudo, spawn_exec_pty};

mod meta;
use meta::*;

mod ask;
use ask::*;

mod status;
use status::*;

mod explore;
use explore::*;

pub mod plan;
use plan::*;

pub mod task;
use task::*;

mod memory;
use memory::*;

mod web;
mod git;
use git::*;

mod file;

use crate::types::{SafetyLevel, TaskPhase, ToolDef, AnthropicTool};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::OnceLock;

/// Global cancel flag — set by TUI when Esc pressed, checked by long-running tools.
pub static CANCEL: AtomicBool = AtomicBool::new(false);

/// Current session seed — set by TUI, read by memory management tools.
pub static CURRENT_SESSION: OnceLock<String> = OnceLock::new();
pub static AUTO_MODE: AtomicBool = AtomicBool::new(false);

/// Called by TUI when a session starts to set the current seed for memory tools.
pub fn set_current_session(seed: &str) {
    let _ = CURRENT_SESSION.set(seed.to_string());
}


// ── ToolEntry + ToolManager ──

pub struct ToolEntry {
    pub name: &'static str,
    pub def: fn() -> ToolDef,
    pub safety: fn(args: &str) -> SafetyLevel,
    pub exec: fn(args: &str) -> String,
}

pub struct ToolManager {
    entries: Vec<ToolEntry>,
    by_name: HashMap<String, usize>,
}

impl ToolManager {
    pub fn new() -> Self {
        Self { entries: Vec::new(), by_name: HashMap::new() }
    }

    pub fn register(&mut self, e: ToolEntry) {
        let idx = self.entries.len();
        self.by_name.insert(e.name.to_string(), idx);
        self.entries.push(e);
    }

    pub fn all_defs(&self) -> Vec<ToolDef> {
        self.entries.iter().map(|e| (e.def)()).collect()
    }

    pub fn all_anthropic_defs(&self) -> Vec<AnthropicTool> {
        self.entries.iter().map(|e| {
            let td = (e.def)();
            AnthropicTool {
                name: td.function.name,
                description: td.function.description,
                input_schema: td.function.parameters,
            }
        }).collect()
    }

    pub fn classify(&self, name: &str, args: &str) -> (SafetyLevel, String) {
        self.by_name.get(name)
            .and_then(|&i| self.entries.get(i))
            .map(|e| {
                let lvl = (e.safety)(args);
                let prompt = match lvl {
                    SafetyLevel::Danger => format!("⚠ DANGER"),
                    SafetyLevel::Confirm => format!("Allow {}?", name),
                    SafetyLevel::Safe => String::new(),
                };
                (lvl, prompt)
            })
            .unwrap_or_else(|| (SafetyLevel::Confirm, format!("Allow {}?", name)))
    }

    pub fn execute(&self, name: &str, args: &str) -> String {
        use std::sync::mpsc;
        use std::time::Duration;
        let _name_owned = name.to_string();
        let args_owned = args.to_string();
        let Some(&idx) = self.by_name.get(name) else {
            return format!("[ERROR] Unknown tool: {}\n[HINT] Available: file (read/write/edit/list/search), exec (execute/explore), web (fetch/search/context7), agent (git/plan/task/mem/status/ask)", name);
        };
        let exec_fn = self.entries[idx].exec;
        let _name_c = name.to_string();
        let args_for_timeout = args_owned.clone();
        let args_c = args_owned;
        let (tx, rx) = mpsc::channel::<Result<String, Box<dyn std::any::Any + Send>>>();
        std::thread::spawn(move || {
            let _ = tx.send(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (exec_fn)(&args_c)
            })));
        });
        // Respect exec action's timeout_secs parameter; other tools get a sensible default
        let timeout = if name == "exec" {
            let action = parse_arg(&args_for_timeout, "action");
            if action == "explore" { 30 }
            else {
                parse_opt_u64(&args_for_timeout, "timeout_secs")
                    .filter(|&n| n > 0 && n <= 3600)
                    .unwrap_or(30).max(30)
            }
        } else { 15 };
        let timeout = Duration::from_secs(timeout);
        match rx.recv_timeout(timeout) {
            Ok(Ok(output)) => output,
            Ok(Err(panic_info)) => {
                let msg = if let Some(s) = panic_info.downcast_ref::<String>() { s.clone() }
                    else if let Some(s) = panic_info.downcast_ref::<&str>() { s.to_string() }
                    else { "unknown panic".to_string() };
                format!(
                    "[ERROR] Tool '{}' panicked\n                     [HINT] The tool crashed. Do NOT retry the same call.\n                     [HINT] Report this to the user and suggest an alternative approach.\n                     Panic: {}", name, msg)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                format!(
                    "[ERROR] Tool '{}' timed out after {}s\n                     [HINT] The operation took too long. Try a more specific query or use exec for long commands.",
                    name, timeout.as_secs())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                format!("[ERROR] Tool '{}' crashed (thread panicked)", name)
            }
        }
    }
}

static TOOLS: OnceLock<ToolManager> = OnceLock::new();

fn manager() -> &'static ToolManager {
    TOOLS.get_or_init(|| {
        let mut tm = ToolManager::new();
        tm.register(ToolEntry {
            name: "file", def: file::file_def, safety: file::classify_file, exec: file::exec_file,
        });
        tm.register(ToolEntry {
            name: "exec", def: exec::exec_def, safety: exec::classify_exec_action, exec: exec::exec_exec,
        });
        tm.register(ToolEntry {
            name: "web", def: web::web_def, safety: web::classify_web, exec: web::exec_web,
        });
        tm.register(ToolEntry {
            name: "agent", def: agent::agent_def, safety: agent::classify_agent, exec: agent::exec_agent,
        });
        tm
    })
}
// ── Public API (compatible with existing callers) ──

pub fn all_tools() -> Vec<ToolDef> {
    manager().all_defs()
}

pub fn all_anthropic_tools() -> Vec<AnthropicTool> {
    manager().all_anthropic_defs()
}

/// Return all tools. Phase enforcement is done at execution time
/// (phase_check_tool in tui.rs blocks destructive tools in read-only modes).
pub fn tools_for_phase(_phase: TaskPhase) -> Vec<ToolDef> {
    all_tools()
}

/// Return reduced tool set for Flash model (cheaper, lower tool-calling willingness).
/// Cuts to 4 consolidated tools with action-based sub-selection.
pub fn tools_for_flash() -> Vec<ToolDef> {
    let keep: &[&str] = &["file", "exec", "web", "agent"];
    manager().all_defs().into_iter()
        .filter(|t| keep.contains(&t.function.name.as_str()))
        .collect()
}

pub fn classify_tool(name: &str, args: &str) -> (SafetyLevel, String) {
    manager().classify(name, args)
}

pub fn execute_tool(name: &str, args: &str) -> String {
    CANCEL.store(false, std::sync::atomic::Ordering::Relaxed);
    manager().execute(name, args)
}

/// Wrap a tool result with the tool name for traceability in multi-tool turns.
pub fn wrap_tool_result(name: &str, result: &str) -> String {
    format!("── {} ──\n{}", name, result)
}

// ── Helpers ──

fn parse_arg(args: &str, key: &str) -> String {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v.get(key).and_then(|v| v.as_str()).map(String::from))
        .unwrap_or_default()
}

fn parse_opt(args: &str, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v.get(key).and_then(|v| v.as_str()).map(String::from))
}

fn parse_arg_or(args: &str, key: &str, default: &str) -> String {
    parse_opt(args, key).unwrap_or_else(|| default.to_string())
}

fn parse_opt_u64(args: &str, key: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(args).ok()?;
    let val = v.get(key)?;
    val.as_u64().or_else(|| {
        val.as_str().and_then(|s| s.parse::<u64>().ok())
    })
}

fn parse_opt_bool(args: &str, key: &str) -> Option<bool> {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v.get(key).and_then(|v| v.as_bool()))
}

/// Compress consecutive lines that share the same prefix pattern.
/// e.g. "cc -O2 -c a.c\ncc -O2 -c b.c\ncc -O2 -c c.c" → "cc -O2 -c ... (3 similar lines)"
#[allow(dead_code)]
fn args_to_json(_default: &str) -> String {
    "{}".to_string()
}
