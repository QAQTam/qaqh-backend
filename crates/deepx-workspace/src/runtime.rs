//! Global tool runtime state and ToolManager lifecycle.

use deepx_types::ToolDef;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

/// Unified runtime security context used for session binding and admission.
#[derive(Clone)]
pub struct RuntimeContext {
    pub active_session: String,
    pub permission_level: u8,
}

#[cfg(not(test))]
static RUNTIME_CTX: Mutex<Option<RuntimeContext>> = Mutex::new(None);

#[cfg(test)]
thread_local! {
    static RUNTIME_CTX: std::cell::RefCell<Option<RuntimeContext>> = const { std::cell::RefCell::new(None) };
}

static TOOL_MANAGER: OnceLock<Mutex<crate::ToolManager>> = OnceLock::new();

/// Agent operating mode: 0=Normal, 1=Plan, 2=Code.
static AGENT_MODE: AtomicU8 = AtomicU8::new(0);

pub fn set_context(session: &str, permission_level: u8) {
    #[cfg(test)]
    {
        RUNTIME_CTX.with(|ctx| {
            *ctx.borrow_mut() = Some(RuntimeContext {
                active_session: session.to_string(),
                permission_level,
            });
        });
        return;
    }
    #[cfg(not(test))]
    if let Ok(mut guard) = RUNTIME_CTX.lock() {
        *guard = Some(RuntimeContext {
            active_session: session.to_string(),
            permission_level,
        });
    }
}

pub fn clear_context() {
    #[cfg(test)]
    {
        RUNTIME_CTX.with(|ctx| *ctx.borrow_mut() = None);
        return;
    }
    #[cfg(not(test))]
    if let Ok(mut guard) = RUNTIME_CTX.lock() {
        *guard = None;
    }
}

pub fn context() -> Option<RuntimeContext> {
    #[cfg(test)]
    {
        return RUNTIME_CTX.with(|ctx| ctx.borrow().clone());
    }
    #[cfg(not(test))]
    RUNTIME_CTX.lock().ok()?.clone()
}

/// Fail closed if the proof is missing a session or no longer matches the runtime.
pub(crate) fn verify_active_session(authorized_session: &str) -> Result<(), String> {
    if authorized_session.is_empty() {
        return Err("missing session in authorization".to_string());
    }

    #[cfg(test)]
    {
        return RUNTIME_CTX.with(|runtime| {
            let context = runtime.borrow();
            let context = context
                .as_ref()
                .ok_or_else(|| "no active session".to_string())?;
            if authorized_session != context.active_session {
                return Err("session mismatch".to_string());
            }
            Ok(())
        });
    }

    #[cfg(not(test))]
    {
        let guard = RUNTIME_CTX
            .lock()
            .map_err(|_| "runtime context poisoned".to_string())?;
        let context = guard
            .as_ref()
            .ok_or_else(|| "no active session".to_string())?;
        if authorized_session != context.active_session {
            return Err("session mismatch".to_string());
        }
        Ok(())
    }
}

pub fn set_mode(mode: u8) {
    AGENT_MODE.store(mode, Ordering::SeqCst);
}

pub(crate) fn is_plan_mode() -> bool {
    AGENT_MODE.load(Ordering::SeqCst) == 1
}

/// Initialize the process-global tool manager.
pub fn init_tools(
    session_seed: &str,
    extra_registrars: &[crate::registration::ToolRegistrar],
    allowed_tools: Vec<String>,
) {
    let mut manager = crate::registration::build_tool_manager(extra_registrars);
    manager.apply_init(allowed_tools, session_seed);
    let _ = TOOL_MANAGER.set(Mutex::new(manager));
    crate::file_cache::clear();
    crate::file_state::clear();
    log::info!("deepx: tool manager inited ({} tools)", all_tools().len());
    crate::agentfs_bridge::init_bridge(session_seed);
}

pub(crate) fn with_manager<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut crate::ToolManager) -> R,
{
    let mgr = TOOL_MANAGER.get()?;
    let mut guard = match mgr.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            log::warn!("[TOOLS] ToolManager Mutex poisoned — recovering with into_inner()");
            poisoned.into_inner()
        }
    };
    Some(f(&mut guard))
}

#[cfg(test)]
pub(crate) fn register_test_handler(handler: crate::ToolHandler) {
    with_manager(|manager| manager.register(handler));
}

/// Return the canonical workspace root used for authorization and execution.
pub(crate) fn active_workspace_root() -> PathBuf {
    let workspace = crate::CURRENT_WORKSPACE
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let root = if workspace.is_empty() || workspace == "." {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(workspace)
    };
    crate::permission::resolve_target_path(root)
}

#[cfg(test)]
pub(crate) fn register_test_handler_with_placement(
    handler: crate::ToolHandler,
    placement: crate::ToolPlacement,
) {
    with_manager(|manager| manager.register_with_placement(handler, placement));
}

pub fn all_tools() -> Vec<ToolDef> {
    with_manager(|manager| manager.filtered_defs()).unwrap_or_default()
}

pub fn all_tool_names() -> Vec<String> {
    with_manager(|manager| {
        manager
            .all_defs()
            .iter()
            .map(|definition| definition.function.name.clone())
            .collect()
    })
    .unwrap_or_default()
}

pub fn global_stats() -> crate::ToolStats {
    with_manager(|manager| manager.stats()).unwrap_or_default()
}

pub fn files_read() -> Vec<String> {
    global_stats().files_read
}

pub fn files_written() -> Vec<String> {
    global_stats().files_written
}

pub fn cancel_current_tool() {
    with_manager(|manager| manager.cancel_tool(None));
}

pub fn shutdown_tools() {
    log::info!("deepx: tool manager shut down");
}
