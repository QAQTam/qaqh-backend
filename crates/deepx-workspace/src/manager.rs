//! ToolManager: tool registration, lookup, routing, and cancellation.
//!
//! Since v5: per-call execution metadata (ToolExecMeta) and cumulative
//! stats (ToolStats) are returned to the caller instead of being lost
//! to stderr. The caller (agent tools.rs) acts as a forwarding layer
//! that pushes these into UI events.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::{CANCEL, SafetyVerdict, ToolHandler, ToolPlacement};

// ── Execution metadata ──

#[derive(Clone, Debug)]
pub struct ToolExecMeta {
    pub name: String,
    pub elapsed_ms: u64,
    pub output_size: usize,
    pub success: bool,
    pub args_summary: String,
}

#[derive(Clone, Debug)]
pub struct ToolExecReport {
    pub content: String,
    pub success: bool,
    pub meta: ToolExecMeta,
    pub files_affected: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ToolStats {
    pub calls_total: u32,
    pub failures: u32,
    pub files_read: Vec<String>,
    pub files_written: Vec<String>,
}

pub struct ToolManager {
    pub(crate) handlers: BTreeMap<String, ToolHandler>,
    placements: BTreeMap<String, ToolPlacement>,
    allowed: Option<Vec<String>>,
    inflight_tasks: BTreeMap<String, Arc<AtomicBool>>,
    stats_total: u32,
    stats_failures: u32,
    files_read: Vec<String>,
    files_written: Vec<String>,
}

// ── Three-phase execution for parallel tool support ──

/// Prepared tool call, ready for execution without holding the manager lock.
pub(crate) struct PreparedCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) placement: ToolPlacement,
    pub(crate) handler_fn: fn(crate::ToolCallCtx) -> crate::ToolResult,
    pub(crate) ctx: crate::ToolCallCtx,
    pub(crate) audit_args: serde_json::Value,
}

impl ToolManager {
    pub fn new() -> Self {
        Self {
            handlers: BTreeMap::new(),
            placements: BTreeMap::new(),
            allowed: None,
            inflight_tasks: BTreeMap::new(),
            stats_total: 0,
            stats_failures: 0,
            files_read: Vec::new(),
            files_written: Vec::new(),
        }
    }

    pub fn register(&mut self, handler: ToolHandler) {
        self.register_with_placement(handler, ToolPlacement::HostOnly);
    }

    pub fn register_with_placement(
        &mut self,
        handler: ToolHandler,
        placement: ToolPlacement,
    ) {
        let key = handler.key.clone();
        self.handlers.insert(key.clone(), handler);
        self.placements.insert(key, placement);
    }

    pub fn lookup(&self, name: &str) -> Option<&ToolHandler> {
        self.handlers.get(name)
    }

    pub fn apply_init(&mut self, allowed_tools: Vec<String>, session_seed: &str) {
        self.allowed = if allowed_tools.is_empty() {
            None
        } else {
            Some(allowed_tools)
        };
        crate::set_current_session(session_seed);
    }

    pub fn all_defs(&self) -> Vec<deepx_types::ToolDef> {
        self.handlers.values().map(|h| h.to_tool_def()).collect()
    }

    pub fn filtered_defs(&self) -> Vec<deepx_types::ToolDef> {
        match &self.allowed {
            Some(allowed) => self
                .all_defs()
                .into_iter()
                .filter(|d| allowed.contains(&d.function.name))
                .collect(),
            None => self.all_defs(),
        }
    }

    // ── Three-phase execution for parallel tool support ──

    /// Phase 1: validate, safety-check, register inflight. Returns a [`PreparedCall`]
    /// that can be executed without the manager lock.
    pub(crate) fn prepare_req(
        &mut self,
        id: String,
        name: &str,
        action: &str,
        args: serde_json::Value,
        timeout_secs: Option<u64>,
        progress_tx: Option<crate::ExecProgressSender>,
    ) -> Result<PreparedCall, ToolExecReport> {
        if let Some(ref allowed) = self.allowed {
            if !allowed.contains(&name.to_string()) {
                let msg = format!(
                    "[ERROR] Tool '{}' is not in the allowed list for this subagent. Allowed tools: [{}]",
                    name,
                    allowed.join(", ")
                );
                return Err(ToolExecReport {
                    success: false,
                    content: msg.clone(),
                    files_affected: Vec::new(),
                    meta: ToolExecMeta {
                        name: name.to_string(),
                        elapsed_ms: 0,
                        output_size: msg.len(),
                        success: false,
                        args_summary: String::new(),
                    },
                });
            }
        }

        let (handler, placement) = match self.handlers.get(name) {
            Some(handler) => (
                handler.clone(),
                self.placements.get(name).copied().unwrap_or_default(),
            ),
            None => {
                let msg = format!("[ERROR] Unknown tool: {}", name);
                return Err(ToolExecReport {
                    success: false,
                    content: msg.clone(),
                    files_affected: Vec::new(),
                    meta: ToolExecMeta {
                        name: name.to_string(),
                        elapsed_ms: 0,
                        output_size: msg.len(),
                        success: false,
                        args_summary: String::new(),
                    },
                });
            }
        };

        let timeout_secs = timeout_secs.unwrap_or_else(|| handler.default_timeout.as_secs());
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let skill_effects = Arc::new(Mutex::new(Vec::new()));
        let ctx = crate::ToolCallCtx {
            id: id.clone(),
            name: name.to_string(),
            action: action.to_string(),
            args: args.clone(),
            tx_progress: progress_tx.clone(),
            timeout_secs: Some(timeout_secs),
            cancel: cancel_flag.clone(),
            skill_effects: skill_effects.clone(),
        };
        let in_workspace = is_path_in_workspace(&ctx);
        match crate::safety::SafetyPolicy::evaluate(handler.risk.clone(), in_workspace) {
            SafetyVerdict::Block(reason) => {
                let msg = format!("[ERROR] {}", reason);
                return Err(ToolExecReport {
                    success: false,
                    content: msg.clone(),
                    files_affected: Vec::new(),
                    meta: ToolExecMeta {
                        name: name.to_string(),
                        elapsed_ms: 0,
                        output_size: msg.len(),
                        success: false,
                        args_summary: String::new(),
                    },
                });
            }
            SafetyVerdict::Allow => {}
        }

        self.inflight_tasks.insert(id.clone(), cancel_flag.clone());

        let audit_args = args.clone();
        let ctx = crate::ToolCallCtx {
            id: id.clone(),
            name: name.to_string(),
            action: action.to_string(),
            args,
            tx_progress: progress_tx,
            timeout_secs: Some(timeout_secs),
            cancel: cancel_flag,
            skill_effects,
        };

        Ok(PreparedCall {
            id,
            name: name.to_string(),
            placement,
            handler_fn: handler.handler,
            ctx,
            audit_args,
        })
    }

    /// Phase 3: deregister inflight, accumulate stats, build report.
    pub(crate) fn finalize_req(
        &mut self,
        prepared: PreparedCall,
        result: crate::ToolResult,
        elapsed_ms: u64,
    ) -> ToolExecReport {
        self.inflight_tasks.remove(&prepared.id);

        let output_size = result.model_text().len();
        let success = result.is_success();

        self.stats_total += 1;
        if !success {
            self.stats_failures += 1;
        }
        let args_summary = audit_args_summary(&prepared.name, &prepared.audit_args);
        let files_affected = extract_files_affected(&prepared.name, &prepared.audit_args);
        if success {
            match prepared.name.as_str() {
                "read" | "search" | "skills" => {
                    for f in &files_affected {
                        if !self.files_read.contains(f) {
                            self.files_read.push(f.clone());
                        }
                    }
                }
                "apply_patch" | "todo" | "task" => {
                    for f in &files_affected {
                        if !self.files_written.contains(f) {
                            self.files_written.push(f.clone());
                        }
                    }
                }
                "exec" | "git_commit" | "git_add" => {
                    // These mutate the workspace but don't have a single 'path' argument
                }
                _ => {}
            }
        }
        let meta = ToolExecMeta {
            name: prepared.name,
            elapsed_ms,
            output_size,
            success,
            args_summary,
        };
        ToolExecReport {
            success,
            content: result.model_text().to_string(),
            meta,
            files_affected,
        }
    }

    pub fn stats(&self) -> ToolStats {
        ToolStats {
            calls_total: self.stats_total,
            failures: self.stats_failures,
            files_read: self.files_read.clone(),
            files_written: self.files_written.clone(),
        }
    }

    pub fn reset_stats(&mut self) {
        self.stats_total = 0;
        self.stats_failures = 0;
        self.files_read.clear();
        self.files_written.clear();
    }

    pub fn cancel_tool(&mut self, id: Option<&str>) {
        match id {
            Some(specific) => {
                if let Some(flag) = self.inflight_tasks.get(specific) {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            None => {
                CANCEL.store(true, std::sync::atomic::Ordering::SeqCst);
                for flag in self.inflight_tasks.values() {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }
    }
}

/// Extract file paths from tool args.
fn extract_files_affected(_tool_name: &str, args: &serde_json::Value) -> Vec<String> {
    let obj = match args.as_object() {
        Some(o) => o,
        None => return Vec::new(),
    };
    let mut files = Vec::new();
    if let Some(v) = obj.get("path").and_then(|v| v.as_str()) {
        files.push(v.to_string());
    }
    if let Some(arr) = obj.get("paths").and_then(|v| v.as_array()) {
        for v in arr {
            if let Some(s) = v.as_str() {
                files.push(s.to_string());
            }
        }
    }
    for key in ["file_a", "file_b", "dest", "target"] {
        if let Some(v) = obj.get(key).and_then(|v| v.as_str()) {
            files.push(v.to_string());
        }
    }
    files
}

/// Determine whether the tool call is operating within the current workspace.
fn is_path_in_workspace(ctx: &crate::ToolCallCtx) -> bool {
    if let Some(path) = ctx.args.get("path").and_then(|v| v.as_str()) {
        if path.is_empty() || path == "." {
            return true;
        }
        let ws = crate::CURRENT_WORKSPACE
            .read()
            .unwrap_or_else(|e| e.into_inner());
        if ws.is_empty() || *ws == "." {
            return true;
        }
        let abs_path = if std::path::Path::new(path).is_absolute() {
            path.to_string()
        } else {
            std::path::Path::new(&*ws)
                .join(path)
                .to_string_lossy()
                .to_string()
        };
        abs_path.starts_with(&*ws)
    } else {
        // No path arg — assume workspace operation (e.g. task, skills, ask)
        true
    }
}

/// Compact args summary for audit log — path and key values only.
fn audit_args_summary(_tool: &str, args: &serde_json::Value) -> String {
    let obj = match args.as_object() {
        Some(o) => o,
        None => return String::new(),
    };
    // Show path-like args first, then command, then truncate to 80 chars
    let mut parts: Vec<String> = Vec::new();
    for key in [
        "path", "file_a", "file_b", "dest", "target", "command", "pattern", "query", "question",
    ] {
        if let Some(v) = obj.get(key).and_then(|v| v.as_str()) {
            let short = if v.len() > 50 {
                // Show last path segment
                let seg = v.rsplit(&['/', '\\']).next().unwrap_or(v);
                format!("{key}=\"{seg}\"")
            } else {
                format!("{key}=\"{v}\"")
            };
            parts.push(short);
        }
    }
    let s = parts.join(", ");
    if s.len() > 80 {
        let end = s.floor_char_boundary(77);
        format!("{}…", &s[..end])
    } else {
        s
    }
}
