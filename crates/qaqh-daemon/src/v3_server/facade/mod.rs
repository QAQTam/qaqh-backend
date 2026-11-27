//! 按域切分的纯 `async` 门面 Trait 集合（`ADR-0042` 铁律 6 / `V4_6_DECISION_PACK` D4）。
//!
//! ## 为什么既按域切又走 Trait 静态分发
//!
//! - **按锁边界与生命周期切分**为 8 个纯 `async` Trait，避免单个上帝 Facade 把所有域绑进同一把锁；
//! - **`V3Server<T, B: V3Backend>` 泛型静态分发**（RPITIT，无 `dyn` 装箱）：`v4.6` 用
//!   [`SkeletonBackend`] 落地契约与不变量，`v4.7` 接真 `QaqhService` 时只需实现这组 Trait，
//!   不必重写分发层与连接层。

use std::future::Future;
use std::sync::Arc;

use qaqh_core_vocab::{ContentHash, SessionId};
use qaqh_ringing_v3::{
    AccountRequest, AccountResponse, CommandRequest, CommandResponse, ConfigRequest,
    ConfigResponse, FsRequest, FsResponse, GitxRequest, GitxResponse, MarketplaceRequest,
    MarketplaceResponse, PairingRequest, PairingResponse, RpcError, SessionRequest,
    SessionResponse, ShellRequest, ShellResponse, SystemRequest, SystemResponse,
    ToolCatalogRequest, ToolCatalogResponse,
};
use tokio::io::AsyncWrite;
use tokio::sync::mpsc;

use super::identity::ConnectionIdentity;
use super::state::ServerState;

pub(crate) mod blob;
pub(crate) mod config_account;
pub(crate) mod fs;
pub(crate) mod gitx;
pub(crate) mod marketplace;
pub(crate) mod session;
pub(crate) mod shell;
pub(crate) mod system;

#[cfg(test)]
pub(crate) mod test_support;

/// 一次控制面调用的上下文。
pub(crate) struct CallContext {
    /// 已认证身份（**授权只看它**，不看 DTO 里的自报字段）。
    pub(crate) identity: Arc<ConnectionIdentity>,
    /// 本连接的大内容推送通道（`BlobTransfer` 流由连接层负责开流与鉴权）。
    pub(crate) blob_pushes: mpsc::UnboundedSender<BlobPush>,
    /// 本连接附着过的会话（连接收摊时据此撤销附着与驱动权，并约束推送范围）。
    pub(crate) attached_sessions: Arc<std::sync::Mutex<std::collections::BTreeSet<SessionId>>>,
}

/// 一次 blob 推送请求（绑定产出它的 `session_id`，连接层据此核验权限与附着）。
#[derive(Debug, Clone)]
pub(crate) struct BlobPush {
    /// 所属会话 id（严禁向未附着/无权访问该会话的连接广播）。
    pub(crate) session_id: SessionId,
    /// 内容哈希。
    pub(crate) hash: ContentHash,
    /// 字节长度（客户端预留进度用）。
    pub(crate) len: u64,
}

/// System 域纯 `async` 门面。
pub(crate) trait SystemFacade {
    fn handle_system(
        &self,
        context: &CallContext,
        request: SystemRequest,
    ) -> impl Future<Output = Result<SystemResponse, RpcError>> + Send;
}

/// Session / Command / ToolCatalog 域纯 `async` 门面。
pub(crate) trait SessionFacade {
    fn handle_session(
        &self,
        context: &CallContext,
        request: SessionRequest,
    ) -> impl Future<Output = Result<SessionResponse, RpcError>> + Send;

    fn handle_command(
        &self,
        context: &CallContext,
        request: CommandRequest,
    ) -> impl Future<Output = Result<CommandResponse, RpcError>> + Send;

    fn handle_catalog(
        &self,
        context: &CallContext,
        request: ToolCatalogRequest,
    ) -> impl Future<Output = Result<ToolCatalogResponse, RpcError>> + Send;
}

/// Shell 域纯 `async` 门面（独立并发句柄表，热路径不碰中央会话锁）。
pub(crate) trait ShellFacade {
    fn handle_shell(
        &self,
        context: &CallContext,
        request: ShellRequest,
    ) -> impl Future<Output = Result<ShellResponse, RpcError>> + Send;
}

/// Config / Account / Pairing 域纯 `async` 门面。
pub(crate) trait ConfigAccountFacade {
    fn handle_config(
        &self,
        context: &CallContext,
        request: ConfigRequest,
    ) -> impl Future<Output = Result<ConfigResponse, RpcError>> + Send;

    fn handle_account(
        &self,
        context: &CallContext,
        request: AccountRequest,
    ) -> impl Future<Output = Result<AccountResponse, RpcError>> + Send;

    fn handle_pairing(
        &self,
        context: &CallContext,
        request: PairingRequest,
    ) -> impl Future<Output = Result<PairingResponse, RpcError>> + Send;
}

/// Marketplace 域纯 `async` 门面。
pub(crate) trait MarketplaceFacade {
    fn handle_marketplace(
        &self,
        context: &CallContext,
        request: MarketplaceRequest,
    ) -> impl Future<Output = Result<MarketplaceResponse, RpcError>> + Send;
}

/// Gitx 域纯 `async` 门面。
pub(crate) trait GitxFacade {
    fn handle_gitx(
        &self,
        context: &CallContext,
        request: GitxRequest,
    ) -> impl Future<Output = Result<GitxResponse, RpcError>> + Send;
}

/// Fs 域纯 `async` 门面。
pub(crate) trait FsFacade {
    fn handle_fs(
        &self,
        context: &CallContext,
        request: FsRequest,
    ) -> impl Future<Output = Result<FsResponse, RpcError>> + Send;
}

/// Blob 域纯 `async` 门面。
pub(crate) trait BlobFacade {
    fn push_blob<W: AsyncWrite + Unpin + Send>(
        &self,
        writer: &mut W,
        hash: ContentHash,
        expected_len: u64,
    ) -> impl Future<Output = Result<u64, RpcError>> + Send;
}

/// 顶层后端聚合 bound（`V3Server<T, B>` 静态分派约束，零 `dyn`）。
pub(crate) trait V3Backend:
    SystemFacade
    + SessionFacade
    + ShellFacade
    + ConfigAccountFacade
    + MarketplaceFacade
    + GitxFacade
    + FsFacade
    + BlobFacade
    + Send
    + Sync
    + 'static
{
}

impl<T> V3Backend for T where
    T: SystemFacade
        + SessionFacade
        + ShellFacade
        + ConfigAccountFacade
        + MarketplaceFacade
        + GitxFacade
        + FsFacade
        + BlobFacade
        + Send
        + Sync
        + 'static
{
}

/// 骨架期后端集合（实现全部 8 个细粒度门面 Trait）。
pub(crate) struct SkeletonBackend {
    pub(crate) system: system::SkeletonSystem,
    pub(crate) session: session::SkeletonSession,
    pub(crate) shell: shell::SkeletonShell,
    pub(crate) config_account: config_account::SkeletonConfigAccount,
    pub(crate) marketplace: marketplace::SkeletonMarketplace,
    pub(crate) gitx: gitx::SkeletonGitx,
    pub(crate) fs: fs::SkeletonFs,
    pub(crate) blob: blob::SkeletonBlob,
}

impl SkeletonBackend {
    /// 从全局状态构造。
    pub(crate) fn new(state: Arc<ServerState>) -> Self {
        Self {
            system: system::SkeletonSystem::new(Arc::clone(&state)),
            session: session::SkeletonSession::new(Arc::clone(&state)),
            shell: shell::SkeletonShell::new(Arc::clone(&state)),
            config_account: config_account::SkeletonConfigAccount::new(Arc::clone(&state)),
            marketplace: marketplace::SkeletonMarketplace::new(Arc::clone(&state)),
            gitx: gitx::SkeletonGitx::new(Arc::clone(&state)),
            fs: fs::SkeletonFs::new(Arc::clone(&state)),
            blob: blob::SkeletonBlob::new(state),
        }
    }
}

impl SystemFacade for SkeletonBackend {
    async fn handle_system(
        &self,
        context: &CallContext,
        request: SystemRequest,
    ) -> Result<SystemResponse, RpcError> {
        self.system.handle(context, request).await
    }
}

impl SessionFacade for SkeletonBackend {
    async fn handle_session(
        &self,
        context: &CallContext,
        request: SessionRequest,
    ) -> Result<SessionResponse, RpcError> {
        self.session.handle_session(context, request).await
    }

    async fn handle_command(
        &self,
        context: &CallContext,
        request: CommandRequest,
    ) -> Result<CommandResponse, RpcError> {
        self.session.handle_command(context, request).await
    }

    async fn handle_catalog(
        &self,
        context: &CallContext,
        request: ToolCatalogRequest,
    ) -> Result<ToolCatalogResponse, RpcError> {
        self.session.handle_catalog(context, request).await
    }
}

impl ShellFacade for SkeletonBackend {
    async fn handle_shell(
        &self,
        context: &CallContext,
        request: ShellRequest,
    ) -> Result<ShellResponse, RpcError> {
        self.shell.handle(context, request).await
    }
}

impl ConfigAccountFacade for SkeletonBackend {
    async fn handle_config(
        &self,
        context: &CallContext,
        request: ConfigRequest,
    ) -> Result<ConfigResponse, RpcError> {
        self.config_account.handle_config(context, request).await
    }

    async fn handle_account(
        &self,
        context: &CallContext,
        request: AccountRequest,
    ) -> Result<AccountResponse, RpcError> {
        self.config_account.handle_account(context, request).await
    }

    async fn handle_pairing(
        &self,
        context: &CallContext,
        request: PairingRequest,
    ) -> Result<PairingResponse, RpcError> {
        self.config_account.handle_pairing(context, request).await
    }
}

impl MarketplaceFacade for SkeletonBackend {
    async fn handle_marketplace(
        &self,
        context: &CallContext,
        request: MarketplaceRequest,
    ) -> Result<MarketplaceResponse, RpcError> {
        self.marketplace.handle(context, request).await
    }
}

impl GitxFacade for SkeletonBackend {
    async fn handle_gitx(
        &self,
        context: &CallContext,
        request: GitxRequest,
    ) -> Result<GitxResponse, RpcError> {
        self.gitx.handle(context, request).await
    }
}

impl FsFacade for SkeletonBackend {
    async fn handle_fs(
        &self,
        context: &CallContext,
        request: FsRequest,
    ) -> Result<FsResponse, RpcError> {
        self.fs.handle(context, request).await
    }
}

impl BlobFacade for SkeletonBackend {
    async fn push_blob<W: AsyncWrite + Unpin + Send>(
        &self,
        writer: &mut W,
        hash: ContentHash,
        expected_len: u64,
    ) -> Result<u64, RpcError> {
        self.blob.push(writer, hash, expected_len).await
    }
}
