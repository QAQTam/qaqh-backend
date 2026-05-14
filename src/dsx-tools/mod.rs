//! ToolManager IPC 服务框架。
//!
//! 提供两大组件：
//! 1. **ToolManager** — 工具注册 + 安全分类 + 请求路由
//! 2. **向后兼容** — 保留 `execute_tool()` 垫片供 TUI 渐进迁移
//!
//! 子模块通过 `pub fn register(mgr: &mut ToolManager)` 注册各自 handler。

pub mod exec;
pub use exec::{exec_command, exec_with_sudo, spawn_exec_async, spawn_exec_async_with_sudo, spawn_exec_pty};
pub mod explore;
pub mod file;
mod git;
mod safety;
mod web;

// D02 module — depends on crate::session/crate::skills, enabled in monolith build
pub mod meta;

// D03 modules — depend on crate::session/crate::router/crate::health, disabled until standalone build
// pub mod ask;
// pub mod memory;
// pub mod plan;
// pub mod status;
// pub mod task;

pub mod ipc;
pub mod registration;

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::types::{SafetyLevel, TaskPhase, ToolDef, AnthropicTool};

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
    /// 从旧式 name+args 构建（兼容垫片使用）。
    pub fn from_str(name: &str, action: &str, args: &str) -> Self {
        let v: serde_json::Value = serde_json::from_str(args).unwrap_or(serde_json::Value::Null);
        let args = if v.is_null() {
            // 尝试解析为旧式 JSON 参数
            serde_json::from_str(args).unwrap_or(serde_json::json!({}))
        } else {
            v
        };
        Self {
            id: String::new(),
            name: name.to_string(),
            action: action.to_string(),
            args,
            tx_progress: None,
        }
    }

    /// 便捷方法：读取命名参数。
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
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v.get(key).and_then(|v| v.as_str()).map(String::from))
        .unwrap_or_default()
}

pub fn parse_arg_or(args: &str, key: &str, default: &str) -> String {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v.get(key).and_then(|v| v.as_str()).map(String::from))
        .unwrap_or_else(|| default.to_string())
}

pub fn parse_opt(args: &str, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v.get(key).and_then(|v| v.as_str()).map(String::from))
}

pub fn parse_opt_bool(args: &str, key: &str) -> Option<bool> {
    let v: serde_json::Value = serde_json::from_str(args).ok()?;
    let val = v.get(key)?;
    val.as_bool().or_else(|| val.as_str().and_then(|s| s.parse::<bool>().ok()))
}

// ── 工具句柄 ──

/// 一个工具操作（name/action）的完整定义。
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
            function: crate::types::ToolFunction {
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
        self.allowed = Some(allowed_tools);
        let _ = CURRENT_SESSION.set(session_seed.to_string());
        AUTO_MODE.store(auto_mode, std::sync::atomic::Ordering::Relaxed);
    }

    // ── 定义查询 ──

    pub fn all_defs(&self) -> Vec<ToolDef> {
        // 按 name 分组，每个 name 只暴露第一个 action 的定义（保持兼容）
        let mut seen = std::collections::HashSet::new();
        let mut defs = Vec::new();
        // 按工具名固定顺序排列
        for name in &["explore", "exec", "read_file", "write_file", "edit_file", "edit_file_diff", "list_dir", "search", "list_skills", "read_skill_ref", "glance", "web", "git"] {
            for (key, handler) in &self.handlers {
                if key.name == *name && seen.insert(key.name.clone()) {
                    defs.push(handler.to_tool_def());
                }
            }
        }
        // 未在排序列表中的工具
        for (key, handler) in &self.handlers {
            if seen.insert(key.name.clone()) {
                defs.push(handler.to_tool_def());
            }
        }
        defs
    }

    pub fn all_anthropic_defs(&self) -> Vec<AnthropicTool> {
        // 先用 Vec<(usize, AnthropicTool)> 收集，保持原始注册顺序
        let mut tools: Vec<AnthropicTool> = Vec::new();
        for def in self.all_defs() {
            tools.push(AnthropicTool {
                name: def.function.name,
                description: def.function.description,
                input_schema: def.function.parameters,
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

    // ── 安全分类（兼容垫片用）──

    pub fn classify_tool(&self, name: &str, args: &str, action: &str) -> (SafetyLevel, String) {
        let key = ToolKey::new(name, action);
        let args_val: serde_json::Value = serde_json::from_str(args).unwrap_or(serde_json::Value::Null);
        match self.handlers.get(&key) {
            Some(h) => {
                let ctx = ToolCallCtx {
                    id: String::new(),
                    name: name.to_string(),
                    action: action.to_string(),
                    args: args_val,
                    tx_progress: None,
                };
                match (h.safety)(&ctx) {
                    SafetyVerdict::Allow => (SafetyLevel::Safe, String::new()),
                    SafetyVerdict::Confirm(p) => (SafetyLevel::Confirm, p),
                    SafetyVerdict::Block(p) => (SafetyLevel::Danger, p),
                }
            }
            None => (SafetyLevel::Confirm, format!("Allow {}?", name)),
        }
    }

    // ── 执行（兼容垫片入口）──

    /// 同步执行（向后兼容）。IPC 模式下请走 `handle_frame`。
    pub fn execute(&self, name: &str, args: &str, action: &str) -> String {
        CANCEL.store(false, std::sync::atomic::Ordering::Relaxed);
        let key = ToolKey::new(name, action);
        let Some(handler) = self.handlers.get(&key) else {
            // 尝试不带 action（旧式单层调用）
            let fallback = self.handlers.iter()
                .find(|(k, _)| k.name == name)
                .map(|(_, h)| h);
            return match fallback {
                Some(h) => self.execute_handler(h, name, action, args),
                None => format!("[ERROR] Unknown tool: {}/{}\n[HINT] Available tools: explore, exec, read_file, write_file, edit_file, edit_file_diff, list_dir, search, list_skills, read_skill_ref, glance, web, git", name, action),
            };
        };
        self.execute_handler(handler, name, action, args)
    }

    fn execute_handler(&self, handler: &ToolHandler, name: &str, action: &str, args: &str) -> String {
        use std::sync::mpsc;
        let args_val: serde_json::Value = serde_json::from_str(args).unwrap_or(serde_json::Value::Null);

        // 提取超时值（在 args_val 被 move 之前）
        let timeout_override = args_val.get("timeout_secs")
            .and_then(|v| v.as_u64())
            .filter(|&n| n > 0 && n <= 3600);

        // 安全检查
        let ctx = ToolCallCtx {
            id: String::new(),
            name: name.to_string(),
            action: action.to_string(),
            args: args_val,
            tx_progress: None,
        };
        match (handler.safety)(&ctx) {
            SafetyVerdict::Block(reason) => {
                return format!("[ERROR] Blocked: {}", reason);
            }
            SafetyVerdict::Confirm(_) => { /* 兼容模式下自动允许 */ }
            SafetyVerdict::Allow => {}
        }

        // 超时计算
        let timeout = timeout_override
            .map(Duration::from_secs)
            .unwrap_or(handler.default_timeout);

        let (tx, rx) = mpsc::channel::<Result<ToolResult, Box<dyn std::any::Any + Send>>>();
        let handler_fn = handler.handler; // fn pointer is Copy + 'static
        let ctx = ToolCallCtx {
            id: String::new(),
            name: name.to_string(),
            action: action.to_string(),
            args: ctx.args,
            tx_progress: None,
        };
        std::thread::spawn(move || {
            let _ = tx.send(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (handler_fn)(ctx)
            })));
        });

        match rx.recv_timeout(timeout) {
            Ok(Ok(tr)) => tr.content,
            Ok(Err(panic_info)) => {
                let msg = if let Some(s) = panic_info.downcast_ref::<String>() { s.clone() }
                    else if let Some(s) = panic_info.downcast_ref::<&str>() { s.to_string() }
                    else { "unknown panic".to_string() };
                format!("[ERROR] Tool '{}/{}' panicked\n[HINT] Report this to the user. Panic: {}", name, action, msg)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                format!("[ERROR] Tool '{}/{}' timed out after {}s", name, action, timeout.as_secs())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                format!("[ERROR] Tool '{}/{}' crashed (thread panicked)", name, action)
            }
        }
    }

    // ── IPC 路由 ──

    /// 处理一个入站 CallReq 帧，返回对应的出站帧。
    pub fn handle_req(&mut self, id: String, name: &str, action: &str, args: serde_json::Value, timeout_secs: Option<u64>) -> ipc::OutboundFrame {
        // 授权检查
        if let Some(ref allowed) = self.allowed {
            if !allowed.contains(&name.to_string()) {
                return ipc::OutboundFrame::ToolError {
                    id, error: format!("Tool '{}' not in allowed list", name), code: "FORBIDDEN".into(),
                };
            }
        }

        let key = ToolKey::new(name, action);
        let Some(handler) = self.handlers.get(&key) else {
            return ipc::OutboundFrame::ToolError {
                id, error: format!("Unknown tool: {}/{}", name, action), code: "UNKNOWN_TOOL".into(),
            };
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
            SafetyVerdict::Confirm(prompt) => {
                return ipc::OutboundFrame::ToolConfirmReq {
                    id, tool_name: name.into(), action: action.into(),
                    danger_level: "confirm".into(), prompt,
                };
            }
            SafetyVerdict::Allow => {}
        }

        // 超时
        let timeout = timeout_secs
            .filter(|&n| n > 0 && n <= 3600)
            .map(Duration::from_secs)
            .unwrap_or(handler.default_timeout);

        // 执行（带进度通道、Panic 捕获、超时）
        let (tx_progress, rx_progress) = std::sync::mpsc::channel::<String>();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel_flag_for_handler = cancel_flag.clone();
        self.inflight_tasks.insert(id.clone(), cancel_flag);

        let ctx = ToolCallCtx {
            id: id.clone(),
            name: name.to_string(),
            action: action.to_string(),
            args,
            tx_progress: Some(tx_progress),
        };

        // 先消费所有已有的进度消息（同步模式下 pre-collect）
        let mut progress_lines = Vec::new();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (handler.handler)(ctx)
        }));

        // 消费剩余进度
        while let Ok(line) = rx_progress.try_recv() {
            progress_lines.push(line);
        }

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

// ── 全局管理器 ──

static TOOLS: OnceLock<std::sync::Mutex<ToolManager>> = OnceLock::new();

fn manager_mutex() -> &'static std::sync::Mutex<ToolManager> {
    TOOLS.get_or_init(|| {
        std::sync::Mutex::new(registration::build_tool_manager())
    })
}

// ── 公开 API（兼容现有调用者）──

pub fn all_tools() -> Vec<ToolDef> {
    manager_mutex().lock().unwrap().all_defs()
}

pub fn all_anthropic_tools() -> Vec<AnthropicTool> {
    manager_mutex().lock().unwrap().all_anthropic_defs()
}

pub fn tools_for_phase(_phase: TaskPhase) -> Vec<ToolDef> {
    all_tools()
}

pub fn tools_for_flash() -> Vec<ToolDef> {
    let keep: &[&str] = &["explore", "exec", "read_file", "write_file", "edit_file", "edit_file_diff", "list_dir", "search", "list_skills", "read_skill_ref", "glance", "web"];
    all_tools().into_iter()
        .filter(|t| keep.contains(&t.function.name.as_str()))
        .collect()
}

pub fn classify_tool(name: &str, action: &str, args: &str) -> (SafetyLevel, String) {
    manager_mutex().lock().unwrap().classify_tool(name, args, action)
}

pub fn execute_tool(name: &str, args: &str) -> String {
    // 兼容垫片：从 JSON 中提取 action 字段，若无则默认为首个 action
    let action: String = serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v.get("action").and_then(|v| v.as_str()).map(String::from))
        .unwrap_or_default();
    manager_mutex().lock().unwrap().execute(name, args, &action)
}

/// Wrap a tool result with the tool name for traceability in multi-tool turns.
pub fn wrap_tool_result(name: &str, result: &str) -> String {
    format!("── {} ──\n{}", name, result)
}

// ── 兼容函数：旧调用点直接引用 exec::exec_command 等 ──

// 这些由 exec.rs 中的 pub use 导出
