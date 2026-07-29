//! Tool execution backend routing.
//!
//! Authorization, admission, inflight tracking, finalization, and auditing stay
//! in the host Agent Worker. Backends only own the phase that invokes an
//! already-prepared tool call.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};

use crate::{ToolCallCtx, ToolResult};

/// Where a registered tool must execute.
///
/// Existing tools default to [`HostOnly`](Self::HostOnly). Workspace tools are
/// opted in explicitly as their remote execution contract is implemented.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolPlacement {
    #[default]
    HostOnly,
    Workspace,
}

/// Local handler function retained by a prepared call.
pub type ToolHandlerFn = fn(ToolCallCtx) -> ToolResult;

/// An authorized and prepared call passed to an execution backend.
pub struct BackendRequest {
    pub session_id: String,
    pub host_workspace: PathBuf,
    pub authorized_resources: Vec<PathBuf>,
    pub local_handler: ToolHandlerFn,
    pub ctx: ToolCallCtx,
}

/// Executes the data-plane phase of an already-authorized tool call.
pub trait ToolExecutionBackend: Send + Sync {
    fn execute(&self, request: BackendRequest) -> ToolResult;
}

/// Default backend that preserves the current in-process handler behavior.
#[derive(Debug, Default)]
pub struct LocalToolExecutionBackend;

impl ToolExecutionBackend for LocalToolExecutionBackend {
    fn execute(&self, request: BackendRequest) -> ToolResult {
        (request.local_handler)(request.ctx)
    }
}

fn backend_slot() -> &'static RwLock<Arc<dyn ToolExecutionBackend>> {
    static WORKSPACE_BACKEND: OnceLock<RwLock<Arc<dyn ToolExecutionBackend>>> = OnceLock::new();
    WORKSPACE_BACKEND
        .get_or_init(|| RwLock::new(Arc::new(LocalToolExecutionBackend)))
}

fn active_workspace_backend() -> Arc<dyn ToolExecutionBackend> {
    backend_slot()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn swap_workspace_backend(
    backend: Arc<dyn ToolExecutionBackend>,
) -> Arc<dyn ToolExecutionBackend> {
    let mut current = backend_slot()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::mem::replace(&mut *current, backend)
}

/// Install the backend used by tools registered with [`ToolPlacement::Workspace`].
///
/// Calls already in flight retain the backend they acquired before the swap.
pub fn install_workspace_backend(backend: Arc<dyn ToolExecutionBackend>) {
    drop(swap_workspace_backend(backend));
}

/// Restore in-process execution for workspace tools.
pub fn use_local_workspace_backend() {
    install_workspace_backend(Arc::new(LocalToolExecutionBackend));
}

pub(crate) fn execute(
    placement: ToolPlacement,
    session_id: String,
    host_workspace: PathBuf,
    authorized_resources: Vec<PathBuf>,
    local_handler: ToolHandlerFn,
    ctx: ToolCallCtx,
) -> ToolResult {
    let request = BackendRequest {
        session_id,
        host_workspace,
        authorized_resources,
        local_handler,
        ctx,
    };
    match placement {
        ToolPlacement::HostOnly => LocalToolExecutionBackend.execute(request),
        ToolPlacement::Workspace => active_workspace_backend().execute(request),
    }
}

#[cfg(test)]
pub(crate) struct WorkspaceBackendTestGuard {
    previous: Option<Arc<dyn ToolExecutionBackend>>,
}

#[cfg(test)]
impl Drop for WorkspaceBackendTestGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            drop(swap_workspace_backend(previous));
        }
    }
}

#[cfg(test)]
pub(crate) fn replace_workspace_backend_for_test(
    backend: Arc<dyn ToolExecutionBackend>,
) -> WorkspaceBackendTestGuard {
    WorkspaceBackendTestGuard {
        previous: Some(swap_workspace_backend(backend)),
    }
}
