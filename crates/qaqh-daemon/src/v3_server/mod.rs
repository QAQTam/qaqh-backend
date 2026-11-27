//! `RingingV3` 服务端（**物理隔离目录**：`ADR-0042` 铁律 3/4/6/7）。
//!
//! ## 这个目录的隔离红线
//!
//! 本目录（以及 `crates/qaqh-client/src/v3/`、`crates/qaqh-v3-transport/src/`）
//! 严禁引入旧 HTTP 服务端、旧 V2 流/版本标识、动态 JSON Value 解析库、空值跳过或
//! 标签/扁平化/别名等 serde 形状属性以及旧代际兼容前缀。
//! 闸门由 CI 抓，见 `docs/adr/0042` 与 `V4_6_EXECUTION_PLAN.md` §三.4。
//!
//! ## 分层
//!
//! ```text
//! qaqh-ringing-v3    线缆形状（零 tokio、零 JSON 库）
//!      ↑
//! qaqh-v3-transport  多路复用（MuxTransport + yamux + duplex 闭环 + 单源帧 I/O）
//!      ↑
//! v3_server          身份 → 按域细粒度 Trait 门面 → 事件总线 → 四条逻辑流
//! ```
//!
//! ## 十一域一网打尽
//!
//! [`V3Server::dispatch`] 是唯一的控制面入口，十一个域一个不少
//! （`System` / `Session` / `Command` / `Shell` / `Marketplace` / `Config` /
//! `Account` / `Pairing` / `Gitx` / `Fs` / `ToolCatalog`）。

mod blob_store;
mod bus;
mod connection;
mod endpoint;
mod error;
mod facade;
mod identity;
mod receipts;
mod session_runtime;
mod shell_table;
mod state;
mod util;

#[cfg(test)]
mod harness;
#[cfg(test)]
mod invariants;
#[cfg(test)]
mod iroh_loopback;

pub(crate) use util::{lock, now_ms};

use std::sync::Arc;

use qaqh_ringing_v3::{ControlRequest, ControlResponse};
use qaqh_v3_transport::MuxTransport;

use facade::{CallContext, SkeletonBackend, V3Backend};
use state::{ServerConfig, ServerState};

/// V3 服务端（传输 `T` 与后端门面 `B` 均走**泛型静态分发，不做 `dyn` 装箱**）。
pub(crate) struct V3Server<T: MuxTransport, B: V3Backend = SkeletonBackend> {
    transport: T,
    state: Arc<ServerState>,
    backend: B,
}

impl<T: MuxTransport> V3Server<T, SkeletonBackend> {
    /// 以新状态构造（`os_user` 是本地 IPC 的授权依据）。
    #[allow(dead_code)]
    pub(crate) fn new(
        transport: T,
        config: ServerConfig,
        os_user: impl Into<String>,
    ) -> Arc<Self> {
        Self::with_state(transport, ServerState::new(config, os_user))
    }

    /// 以既有状态构造（守卫用它把状态握在手里，才能在进程内发事件）。
    pub(crate) fn with_state(transport: T, state: Arc<ServerState>) -> Arc<Self> {
        let backend = SkeletonBackend::new(Arc::clone(&state));
        Arc::new(Self {
            transport,
            state,
            backend,
        })
    }
}

impl<T: MuxTransport, B: V3Backend> V3Server<T, B> {
    /// 控制面唯一分派入口：**十一域全覆盖，编译期静态分派**。
    pub(crate) async fn dispatch(
        &self,
        context: &CallContext,
        request: ControlRequest,
    ) -> ControlResponse {
        let outcome = match request {
            ControlRequest::System(request) => self
                .backend
                .handle_system(context, request)
                .await
                .map(ControlResponse::System),
            ControlRequest::Session(request) => self
                .backend
                .handle_session(context, request)
                .await
                .map(ControlResponse::Session),
            ControlRequest::Command(request) => self
                .backend
                .handle_command(context, request)
                .await
                .map(ControlResponse::Command),
            ControlRequest::Shell(request) => self
                .backend
                .handle_shell(context, request)
                .await
                .map(ControlResponse::Shell),
            ControlRequest::Marketplace(request) => self
                .backend
                .handle_marketplace(context, request)
                .await
                .map(ControlResponse::Marketplace),
            ControlRequest::Config(request) => self
                .backend
                .handle_config(context, request)
                .await
                .map(ControlResponse::Config),
            ControlRequest::Account(request) => self
                .backend
                .handle_account(context, request)
                .await
                .map(ControlResponse::Account),
            ControlRequest::Pairing(request) => self
                .backend
                .handle_pairing(context, request)
                .await
                .map(ControlResponse::Pairing),
            ControlRequest::Gitx(request) => self
                .backend
                .handle_gitx(context, request)
                .await
                .map(ControlResponse::Gitx),
            ControlRequest::Fs(request) => self
                .backend
                .handle_fs(context, request)
                .await
                .map(ControlResponse::Fs),
            ControlRequest::ToolCatalog(request) => self
                .backend
                .handle_catalog(context, request)
                .await
                .map(ControlResponse::ToolCatalog),
        };
        match outcome {
            Ok(response) => response,
            // 失败也走同一条控制面回执：前端不需要逐域写错误处理。
            Err(error) => ControlResponse::Error(error),
        }
    }
}
