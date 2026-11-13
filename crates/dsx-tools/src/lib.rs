//! ToolManager IPC 服务框架。
//!
//! 提供：**ToolManager** — 工具注册 + 安全分类 + IPC 请求路由
//!
//! 子模块通过 `pub fn register(mgr: &mut ToolManager)` 注册各自 handler。

pub mod exec;
pub use exec::{exec_command, exec_with_sudo, spawn_exec_async, spawn_exec_async_with_sudo};
pub mod explore;
pub mod file;
mod safety;
mod web;

// D03 modules — tool handlers for plan, task
pub mod plan;
pub mod task;

pub mod ipc;
pub mod registration;
pub mod stubs;

/// Run the tools IPC server (stdin/stdout JSON-LP loop).
pub fn run() {
    ipc::ipc_main_loop(&mut registration::build_tool_manager());
}

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use dsx_types::{ToolDef, AnthropicTool};

pub use safety::SafetyVerdict;

// ── 全局状态 ──

/// 全局取消标志 — TUI 按 Esc 时设置，长工具在循环中检查。
pub static CANCEL: AtomicBool = AtomicBool::new(false);

/// 当前会话 seed — 由 TUI 启动时设置，memory 工具使用。
pub static CURRENT_SESSION: OnceLock<String> = OnceLock::new();
pub static AUTO_MODE: AtomicBool = AtomicBool::new(false);

pub fn set_current_session(seed: &str) {
    let _ = CURRENT_SESSION.set(seed.to_string());
}

// ── 工具键（名称 + 二级操作）──

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ToolKey {
    pub name: String,
    pub action: String,
}

impl ToolKey {
    pub fn new(name: impl Into<String>, action: impl Into<String>) -> Self {
        Self { name: name.into(), action: action.into() }
    }
}

impl std::fmt::Display for ToolKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.name, self.action)
    }
}

// ── handler 上下文与返回值 ──

/// handler 执行上下文：包含请求参数和进度通道。
pub struct ToolCallCtx {
    pub id: String,
    pub name: String,
    pub action: String,
    pub args: serde_json::Value,
    /// 可选进度发送通道 — handler 可在执行期间推送进度帧。
    pub tx_progress: Option<std::sync::mpsc::Sender<String>>,
}

impl ToolCallCtx {
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.args.get(key).and_then(|v| v.as_str())
    }

    pub fn get_u64(&self, key: &str) -> Option<u64> {
        self.args.get(key).and_then(|v| v.as_u64())
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.args.get(key).and_then(|v| v.as_bool())
    }
}

/// handler 返回值。
#[derive(Clone, Debug)]
pub struct ToolResult {
    pub success: bool,
    pub content: String,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self { success: true, content: content.into() }
    }

    pub fn err(content: impl Into<String>) -> Self {
        Self { success: false, content: content.into() }
    }
}

// ── 参数解析工具（供子模块共享）──

/// Parse a string parameter from JSON args (legacy compat).
pub fn parse_arg(args: &str, key: &str) -> String {
    dsx_types::arg::parse_arg(args, key).unwrap_or_default()
}

pub fn parse_arg_or(args: &str, key: &str, default: &str) -> String {
    dsx_types::arg::parse_arg_or(args, key, default)
}

pub fn parse_opt(args: &str, key: &str) -> Option<String> {
    dsx_types::arg::parse_arg(args, key)
}

pub fn parse_opt_bool(args: &str, key: &str) -> Option<bool> {
    let v: serde_json::Value = serde_json::from_str(args).ok()?;
    let val = v.get(key)?;
    val.as_bool().or_else(|| val.as_str().and_then(|s| s.parse::<bool>().ok()))
}

// ── 工具句柄 ──

/// 一个工具操作（name/action）的完整定义。
#[derive(Clone)]
pub struct ToolHandler {
    pub key: ToolKey,
    pub description: &'static str,
    pub input_schema: serde_json::Value,
    pub handler: fn(ToolCallCtx) -> ToolResult,
    pub safety: fn(&ToolCallCtx) -> SafetyVerdict,
    pub default_timeout: Duration,
}

impl ToolHandler {
    pub fn to_tool_def(&self) -> ToolDef {
        ToolDef {
            call_type: "function".into(),
            function: dsx_types::ToolFunction {
                name: self.key.name.to_string(),
                description: self.description.to_string(),
                parameters: self.input_schema.clone(),
            },
        }
    }
}

// ── ToolManager ──

pub struct ToolManager {
    handlers: HashMap<ToolKey, ToolHandler>,
    /// 若 Some，仅允许列表中 name 的工具通过。
    allowed: Option<Vec<String>>,
    /// inflight 任务的取消标志（id → should_cancel）。
    inflight_tasks: HashMap<String, Arc<AtomicBool>>,
}

impl ToolManager {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
            allowed: None,
            inflight_tasks: HashMap::new(),
        }
    }

    // ── 注册 ──

    pub fn register(&mut self, handler: ToolHandler) {
        self.handlers.insert(handler.key.clone(), handler);
    }

    // ── Init（IPC 中由 ToolsInit 帧触发）──

    pub fn apply_init(&mut self, allowed_tools: Vec<String>, session_seed: &str, auto_mode: bool) {
        // Empty list = allow all (no restriction)
        self.allowed = if allowed_tools.is_empty() { None } else { Some(allowed_tools) };
        let _ = CURRENT_SESSION.set(session_seed.to_string());
        AUTO_MODE.store(auto_mode, std::sync::atomic::Ordering::Relaxed);
    }

    // ── 定义查询 ──

    pub fn all_defs(&self) -> Vec<ToolDef> {
        let mut seen = std::collections::HashSet::new();
        let mut defs = Vec::new();
        for (key, handler) in &self.handlers {
            if seen.insert(key.name.clone()) {
                defs.push(handler.to_tool_def());
            }
        }
        defs
    }

    pub fn all_anthropic_defs(&self) -> Vec<AnthropicTool> {
        let mut tools: Vec<AnthropicTool> = Vec::new();
        for def in self.all_defs() {
            tools.push(AnthropicTool {
                name: def.function.name,
                description: def.function.description,
                input_schema: def.function.parameters,
                cache_control: None,
            });
        }
        tools
    }

    pub fn filtered_defs(&self) -> Vec<ToolDef> {
        match &self.allowed {
            Some(allowed) => self.all_defs().into_iter()
                .filter(|d| allowed.contains(&d.function.name))
                .collect(),
            None => self.all_defs(),
        }
    }

    // ── IPC 路由 ──

    /// 处理一个入站 CallReq 帧，返回对应的出站帧。
    pub fn handle_req(&mut self, id: String, name: &str, action: &str, args: serde_json::Value, _timeout_secs: Option<u64>) -> ipc::OutboundFrame {
        // 授权检查
        if let Some(ref allowed) = self.allowed {
            if !allowed.contains(&name.to_string()) {
                return ipc::OutboundFrame::ToolError {
                    id, error: format!("Tool '{}' not in allowed list", name), code: "FORBIDDEN".into(),
                };
            }
        }

        let key = ToolKey::new(name, action);
        let handler = match self.handlers.get(&key) {
            Some(h) => h,
            None => {
                // Fallback: try to find handler by name only (ignore action)
                match self.handlers.iter().find(|(k, _)| k.name == name) {
                    Some((_, h)) => h,
                    None => return ipc::OutboundFrame::ToolError {
                        id, error: format!("Unknown tool: {}/{}", name, action), code: "UNKNOWN_TOOL".into(),
                    },
                }
            }
        };

        // 安全检查
        let ctx = ToolCallCtx {
            id: id.clone(),
            name: name.to_string(),
            action: action.to_string(),
            args: args.clone(),
            tx_progress: None,
        };
        match (handler.safety)(&ctx) {
            SafetyVerdict::Block(reason) => {
                return ipc::OutboundFrame::ToolError {
                    id, error: reason, code: "BLOCKED".into(),
                };
            }
            SafetyVerdict::Allow => {}
        }

        // 注册 inflight 取消标志
        let cancel_flag = Arc::new(AtomicBool::new(false));
        self.inflight_tasks.insert(id.clone(), cancel_flag);

        let ctx = ToolCallCtx {
            id: id.clone(),
            name: name.to_string(),
            action: action.to_string(),
            args,
            tx_progress: None,
        };

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (handler.handler)(ctx)
        }));

        self.inflight_tasks.remove(&id);

        let (content, success, is_error) = match result {
            Ok(tr) => (tr.content, tr.success, if !tr.success { Some(true) } else { None }),
            Err(panic_info) => {
                let msg = if let Some(s) = panic_info.downcast_ref::<String>() { s.clone() }
                    else if let Some(s) = panic_info.downcast_ref::<&str>() { s.to_string() }
                    else { "unknown panic".to_string() };
                (format!("[ERROR] Tool panicked: {}", msg), false, Some(true))
            }
        };

        ipc::OutboundFrame::ToolResultMessage {
            id,
            name: name.into(),
            action: action.into(),
            success,
            content,
            is_error,
        }
    }

    /// 取消工具。
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