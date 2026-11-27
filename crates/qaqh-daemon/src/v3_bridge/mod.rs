//! Production RingingV3 backend over QaqhService, SQLite, and local IPC.

mod blob;
mod config_account;
mod fs;
mod gitx;
mod marketplace;
mod projection;
mod receipts;
mod session;
mod shell;
mod streams;
mod system;
mod workspace;

#[cfg(test)]
mod regression_tests;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;

use qaqh_core_vocab::{ContentHash, ContentRef, JsonText, SessionId};
use qaqh_ringing_runtime::{
    DeviceRegistry, PendingCommandStore, RingingHub, pairing::PairingTable,
};
use qaqh_ringing_v3::{
    AccountRequest, AccountResponse, ConfigRequest, ConfigResponse, ErrorCode, FsRequest,
    FsResponse, GitxRequest, GitxResponse, MarketplaceRequest, MarketplaceResponse, PairingRequest,
    PairingResponse, RpcError, SessionRequest, SessionResponse, ShellRequest, ShellResponse,
    SystemRequest, SystemResponse, ToolCatalogRequest, ToolCatalogResponse, WorkspaceRequest,
    WorkspaceResponse,
};
use qaqh_runtime::QaqhService;
use tokio::io::AsyncWrite;
use tokio::sync::watch;

use crate::v3_server::facade::{
    BlobFacade, BlobPush, CallContext, ConfigAccountFacade, FsFacade, GitxFacade,
    MarketplaceFacade, SessionFacade, ShellFacade, SystemFacade, V3Backend,
};
use crate::v3_server::identity::Scope;
use crate::v3_server::session_runtime::SessionRuntime;
use crate::v3_server::state::ServerState;

#[derive(Debug)]
enum BlockingError {
    Rpc(RpcError),
    Message(String),
}

impl From<RpcError> for BlockingError {
    fn from(error: RpcError) -> Self {
        Self::Rpc(error)
    }
}

impl From<String> for BlockingError {
    fn from(error: String) -> Self {
        Self::Message(error)
    }
}

impl From<qaqh_runtime::QaqhError> for BlockingError {
    fn from(error: qaqh_runtime::QaqhError) -> Self {
        Self::Message(error.to_string())
    }
}

impl From<qaqh_config::ConfigError> for BlockingError {
    fn from(error: qaqh_config::ConfigError) -> Self {
        Self::Message(error.to_string())
    }
}

impl From<qaqh_session::db::DbError> for BlockingError {
    fn from(error: qaqh_session::db::DbError) -> Self {
        Self::Message(error.to_string())
    }
}

type ProjectorTask = (JoinHandle<()>, watch::Receiver<bool>);
type ShellSession = (String, Option<SessionId>);

struct ProjectorTasks {
    tasks: Mutex<HashMap<SessionId, ProjectorTask>>,
}

impl Drop for ProjectorTasks {
    fn drop(&mut self) {
        for (_, (task, _)) in self
            .tasks
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .drain()
        {
            task.abort();
        }
    }
}

#[derive(Clone)]
pub(crate) struct RealBackend {
    pub(crate) service: QaqhService,
    pub(crate) state: Arc<ServerState>,
    pub(crate) hub: Arc<RingingHub>,
    pub(crate) pairings: Arc<Mutex<PairingTable>>,
    pub(crate) devices: Arc<DeviceRegistry>,
    pub(crate) shutdown: watch::Sender<bool>,
    pub(crate) receipts: Arc<Mutex<PendingCommandStore>>,
    projectors: Arc<ProjectorTasks>,
    shell_sessions: Arc<Mutex<HashMap<String, ShellSession>>>,
}

impl RealBackend {
    pub(crate) async fn new(
        service: QaqhService,
        state: Arc<ServerState>,
        hub: Arc<RingingHub>,
        pairings: Arc<Mutex<PairingTable>>,
        devices: Arc<DeviceRegistry>,
        shutdown: watch::Sender<bool>,
        receipts: Arc<Mutex<PendingCommandStore>>,
    ) -> Result<Self, String> {
        let backend = Self {
            service,
            state,
            hub,
            pairings,
            devices,
            shutdown,
            receipts,
            projectors: Arc::new(ProjectorTasks {
                tasks: Mutex::new(HashMap::new()),
            }),
            shell_sessions: Arc::new(Mutex::new(HashMap::new())),
        };
        backend.sync_devices().await?;
        backend.sync_sessions().await?;
        Ok(backend)
    }

    async fn sync_devices(&self) -> Result<(), String> {
        let records = self.devices.list().await?;
        for record in records {
            let scope = match record.scope {
                qaqh_ringing_runtime::Scope::View => Scope::Device,
                qaqh_ringing_runtime::Scope::Interact => Scope::Device,
                qaqh_ringing_runtime::Scope::Admin => Scope::Owner,
            };
            self.state.identity.register_device_digest(
                &record.token_digest,
                &record.device_id,
                &record.name,
                scope,
                record.created_at_ms,
            );
        }
        Ok(())
    }

    async fn sync_sessions(&self) -> Result<(), String> {
        let service = self.service.clone();
        let sessions = Self::blocking(move || service.list_sessions(true))
            .await
            .map_err(|error| error.message)?;
        let owner = format!("os:{}", self.state.identity.os_user());
        for entry in sessions {
            let session_id = SessionId::new(entry.meta.session_id)
                .map_err(|error| format!("invalid persisted session id: {error}"))?;
            let runtime = self.state.register_session(session_id, &owner);
            if let Some(title) = entry.meta.title {
                runtime.rename(title);
            }
        }
        Ok(())
    }

    pub(crate) async fn ensure_session_runtime(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Arc<SessionRuntime>>, RpcError> {
        if let Some(session) = self.state.session(session_id) {
            return Ok(Some(session));
        }
        let raw = session_id.as_str().to_string();
        let row = self
            .service
            .owners()
            .db()
            .read(move |conn| qaqh_session::db::store::session_row(conn, &raw))
            .await
            .map_err(|error| internal(format!("lookup persisted session `{session_id}`: {error}")))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let owner = format!("os:{}", self.state.identity.os_user());
        let runtime = self.state.register_session(session_id.clone(), &owner);
        if !row.title.is_empty() {
            runtime.rename(row.title);
        }
        Ok(Some(runtime))
    }

    pub(crate) async fn session_for(
        &self,
        context: &CallContext,
        session_id: &SessionId,
    ) -> Result<Arc<SessionRuntime>, RpcError> {
        let session = self
            .ensure_session_runtime(session_id)
            .await?
            .ok_or_else(|| {
                crate::v3_server::error::not_found(format!("没有会话 `{session_id}`"))
            })?;
        if !session.can_access(&context.identity.device) {
            return Err(crate::v3_server::error::denied(format!(
                "设备无权访问会话 `{session_id}`"
            )));
        }
        Ok(session)
    }

    pub(crate) fn ensure_projector(&self, session_id: &SessionId) {
        let mut projectors = self
            .projectors
            .tasks
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if projectors
            .get(session_id)
            .is_some_and(|(task, _)| !task.is_finished())
        {
            return;
        }
        projectors.remove(session_id);
        let session_id = session_id.clone();
        let service = self.service.clone();
        let state = Arc::clone(&self.state);
        let hub = Arc::clone(&self.hub);
        let task_session_id = session_id.clone();
        let (ready, ready_rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            if let Err(error) = projection::run(service, state, hub, &task_session_id, ready).await
            {
                log::error!("v3 projector {task_session_id} stopped: {error}");
            }
        });
        projectors.insert(session_id, (task, ready_rx));
    }

    pub(crate) async fn ensure_projector_ready(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RpcError> {
        self.ensure_projector(session_id);
        let mut ready = self
            .projectors
            .tasks
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(session_id)
            .map(|(_, ready)| ready.clone())
            .ok_or_else(|| internal(format!("projector missing for session `{session_id}`")))?;
        ready.wait_for(|ready| *ready).await.map_err(|_| {
            internal(format!(
                "projector stopped before session `{session_id}` was ready"
            ))
        })?;
        Ok(())
    }

    async fn blocking<T, E, F>(operation: F) -> Result<T, RpcError>
    where
        T: Send + 'static,
        E: Into<BlockingError> + Send + 'static,
        F: FnOnce() -> Result<T, E> + Send + 'static,
    {
        tokio::task::spawn_blocking(operation)
            .await
            .map_err(|error| internal(format!("blocking backend task: {error}")))?
            .map_err(|error| match error.into() {
                BlockingError::Rpc(error) => error,
                BlockingError::Message(message) => internal(message),
            })
    }

    pub(crate) fn shell_session(&self, shell_id: &str) -> Option<ShellSession> {
        self.shell_sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(shell_id)
            .cloned()
    }

    pub(crate) fn set_shell_session(
        &self,
        owner: String,
        shell_id: String,
        session_id: Option<SessionId>,
    ) {
        self.shell_sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(shell_id, (owner, session_id));
    }

    pub(crate) fn remove_shell_session(&self, shell_id: &str) {
        self.shell_sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(shell_id);
    }

    pub(crate) async fn json_call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        let service = self.service.clone();
        let method = method.to_string();
        Self::blocking(move || {
            qaqh_runtime::ringing::service_methods::dispatch(&service, &method, &params)
                .map_err(|error| error.to_string())
        })
        .await
    }

    pub(crate) async fn put_blob(
        &self,
        session_id: &SessionId,
        bytes: Vec<u8>,
    ) -> Result<ContentRef, RpcError> {
        let session_id = session_id.as_str().to_string();
        let len = bytes.len() as u64;
        let hash = self
            .hub
            .put_content_async(&session_id, "application/octet-stream", bytes, false)
            .await
            .map_err(|error| internal(format!("store blob: {error}")))?;
        let hash = ContentHash::from_hex(&hash)
            .map_err(|error| internal(format!("invalid stored content hash: {error}")))?;
        let _ = self.state.push_bus.send(BlobPush {
            session_id: SessionId::new(session_id).expect("validated session id"),
            hash,
            len,
        });
        Ok(ContentRef::Blob { hash, len })
    }

    pub(crate) async fn content_ref_for_text(
        &self,
        session_id: &SessionId,
        text: String,
    ) -> Result<ContentRef, RpcError> {
        if text.len() > self.state.config.inline_budget_bytes {
            self.put_blob(session_id, text.into_bytes()).await
        } else {
            Ok(ContentRef::Inline { text })
        }
    }

    pub(crate) async fn push_blob<W: AsyncWrite + Unpin + Send>(
        &self,
        writer: &mut W,
        hash: ContentHash,
        expected_len: u64,
    ) -> Result<u64, RpcError> {
        blob::push(self, writer, hash, expected_len).await
    }
}

impl SystemFacade for RealBackend {
    async fn handle_system(
        &self,
        context: &CallContext,
        request: SystemRequest,
    ) -> Result<SystemResponse, RpcError> {
        system::handle(self, context, request).await
    }
}

impl SessionFacade for RealBackend {
    async fn handle_session(
        &self,
        context: &CallContext,
        request: SessionRequest,
    ) -> Result<SessionResponse, RpcError> {
        session::handle_session(self, context, request).await
    }

    async fn handle_command(
        &self,
        context: &CallContext,
        request: qaqh_ringing_v3::CommandRequest,
    ) -> Result<qaqh_ringing_v3::CommandResponse, RpcError> {
        session::handle_command(self, context, request).await
    }

    async fn handle_catalog(
        &self,
        context: &CallContext,
        request: ToolCatalogRequest,
    ) -> Result<ToolCatalogResponse, RpcError> {
        session::handle_catalog(self, context, request).await
    }

    async fn handle_workspace(
        &self,
        context: &CallContext,
        request: WorkspaceRequest,
    ) -> Result<WorkspaceResponse, RpcError> {
        workspace::handle(self, context, request).await
    }
}

impl ShellFacade for RealBackend {
    async fn handle_shell(
        &self,
        context: &CallContext,
        request: ShellRequest,
    ) -> Result<ShellResponse, RpcError> {
        shell::handle(self, context, request).await
    }
}

impl ConfigAccountFacade for RealBackend {
    async fn handle_config(
        &self,
        context: &CallContext,
        request: ConfigRequest,
    ) -> Result<ConfigResponse, RpcError> {
        config_account::handle_config(self, context, request).await
    }

    async fn handle_account(
        &self,
        context: &CallContext,
        request: AccountRequest,
    ) -> Result<AccountResponse, RpcError> {
        config_account::handle_account(self, context, request).await
    }

    async fn handle_pairing(
        &self,
        context: &CallContext,
        request: PairingRequest,
    ) -> Result<PairingResponse, RpcError> {
        config_account::handle_pairing(self, context, request).await
    }
}

impl MarketplaceFacade for RealBackend {
    async fn handle_marketplace(
        &self,
        context: &CallContext,
        request: MarketplaceRequest,
    ) -> Result<MarketplaceResponse, RpcError> {
        marketplace::handle(self, context, request).await
    }
}

impl GitxFacade for RealBackend {
    async fn handle_gitx(
        &self,
        context: &CallContext,
        request: GitxRequest,
    ) -> Result<GitxResponse, RpcError> {
        gitx::handle(self, context, request).await
    }
}

impl FsFacade for RealBackend {
    async fn handle_fs(
        &self,
        context: &CallContext,
        request: FsRequest,
    ) -> Result<FsResponse, RpcError> {
        fs::handle(self, context, request).await
    }
}

impl BlobFacade for RealBackend {
    async fn push_blob<W: AsyncWrite + Unpin + Send>(
        &self,
        writer: &mut W,
        hash: ContentHash,
        expected_len: u64,
    ) -> Result<u64, RpcError> {
        self.push_blob(writer, hash, expected_len).await
    }
}

impl V3Backend for RealBackend {
    fn request_shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    async fn lookup_receipt(
        &self,
        device: &crate::v3_server::identity::DeviceKey,
        command_id: &str,
        fingerprint: &str,
    ) -> Result<Option<qaqh_ringing_v3::ControlReply>, RpcError> {
        receipts::lookup(self, device, command_id, fingerprint).await
    }

    async fn reserve_receipt(
        &self,
        device: &crate::v3_server::identity::DeviceKey,
        command_id: &str,
        fingerprint: &str,
    ) -> Result<(), RpcError> {
        receipts::reserve(self, device, command_id, fingerprint).await
    }

    async fn persist_receipt(
        &self,
        device: &crate::v3_server::identity::DeviceKey,
        command_id: &str,
        reply: &qaqh_ringing_v3::ControlReply,
    ) -> Result<(), RpcError> {
        receipts::persist(self, device, command_id, reply).await
    }
}

pub(crate) fn local_ipc_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        let _ = data_dir;
        std::path::PathBuf::from(format!(r"\\.\pipe\qaqh-daemon-{}", std::process::id()))
    }
    #[cfg(not(windows))]
    {
        data_dir.join("daemon.sock")
    }
}

pub(crate) fn internal(message: impl Into<String>) -> RpcError {
    RpcError {
        code: ErrorCode::Internal,
        message: message.into(),
        retryable: false,
    }
}

pub(crate) fn bad_request(message: impl Into<String>) -> RpcError {
    RpcError {
        code: ErrorCode::BadRequest,
        message: message.into(),
        retryable: false,
    }
}

pub(crate) fn json_text(value: &serde_json::Value) -> Result<JsonText, RpcError> {
    serde_json::to_string(value)
        .map(JsonText::new)
        .map_err(|error| internal(format!("serialize backend JSON: {error}")))
}
