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

/// HTTP backend that forwards prepared calls to a `deepx-workspace serve`
/// instance (local process or WSL). On any transport/HTTP failure it falls
/// back to the in-process handler so a flapping tool service never blocks
/// the agent loop (failure mode: slow path, not hard failure).
#[derive(Debug, Clone)]
pub struct HttpToolExecutionBackend {
    pub endpoint: String,
    pub token: String,
}

impl HttpToolExecutionBackend {
    pub fn new(endpoint: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            token: token.into(),
        }
    }
}

#[derive(serde::Serialize)]
struct HttpExecuteRequest<'a> {
    session_id: &'a str,
    workspace: &'a str,
    name: &'a str,
    #[serde(skip_serializing_if = "String::is_empty")]
    action: String,
    args: &'a serde_json::Value,
    call_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_secs: Option<u64>,
}

#[derive(serde::Deserialize)]
struct HttpExecuteResponse {
    #[serde(flatten)]
    result: ToolResult,
}

impl ToolExecutionBackend for HttpToolExecutionBackend {
    fn execute(&self, request: BackendRequest) -> ToolResult {
        let ctx = &request.ctx;
        let payload = HttpExecuteRequest {
            session_id: &request.session_id,
            workspace: &request.host_workspace.to_string_lossy(),
            name: &ctx.name,
            action: ctx.action.clone(),
            args: &ctx.args,
            call_id: &ctx.id,
            timeout_secs: ctx.timeout_secs,
        };
        let body = match serde_json::to_vec(&payload) {
            Ok(body) => body,
            Err(error) => {
                log::warn!("[workspace-backend] serialize execute request: {error}; fallback local");
                return (request.local_handler)(request.ctx);
            }
        };

        // 与 serve 的 executor 串行语义匹配：HTTP 读超时按工具自身超时放大，
        // 避免代理层比工具更早放弃；连接/写超时保持有界（服务不可达快速回退）。
        let timeout = ctx.timeout_secs.unwrap_or(30).saturating_add(30).min(3600);
        let url = format!("{}/execute", self.endpoint.trim_end_matches('/'));
        let result = ureq::Agent::config_builder()
            .timeout_connect(Some(std::time::Duration::from_secs(5)))
            .timeout_per_call(Some(std::time::Duration::from_secs(timeout)))
            .build()
            .new_agent()
            .post(&url)
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
            .send(body);

        match result {
            Ok(mut response) => {
                let parsed = response
                    .body_mut()
                    .read_json::<HttpExecuteResponse>()
                    .map_err(|e| e.to_string());
                match parsed {
                    Ok(resp) => resp.result,
                    Err(error) => {
                        log::warn!("[workspace-backend] invalid execute response: {error}; fallback local");
                        (request.local_handler)(request.ctx)
                    }
                }
            }
            Err(error) => {
                log::warn!(
                    "[workspace-backend] {} unavailable ({error}); fallback local",
                    self.endpoint
                );
                (request.local_handler)(request.ctx)
            }
        }
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
