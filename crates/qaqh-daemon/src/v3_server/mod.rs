//! `RingingV3` 服务端（**物理隔离目录**：`ADR-0042` 铁律 3/4/6/7）。
//!
//! ## 这个目录的隔离红线
//!
//! 本目录（以及 `crates/qaqh-client/src/v3/`、`crates/qaqh-v3-transport/src/`）里
//! **一行都不许出现**：
//!
//! ```text
//! axum_server   RINGING_V2   v2_stream   serde_json
//! skip_serializing_if   serde(tag|flatten|untagged)   serde(alias)
//! legacy_   compat_
//! ```
//!
//! 这不是风格洁癖：V3 的全部意义就是**不再有 V2 的兼容层**。一旦允许"顺手引用一下
//! 老路由"或"临时塞个 `serde_json::Value`"，双栈期就会永远结束不了
//! （`v4.6` 之前 40 天的 V2 迁移拖延就是这么来的）。闸门由 CI 抓，见
//! `docs/adr/0042` 与 `V4_6_EXECUTION_PLAN.md` §三.4。
//!
//! ## 分层
//!
//! ```text
//! qaqh-ringing-v3    线缆形状（零 tokio、零 serde_json）
//!      ↑
//! qaqh-v3-transport  多路复用（MuxTransport + yamux + duplex 闭环）
//!      ↑
//! v3_server          身份 → 门面 → 事件总线 → 四条逻辑流
//! ```
//!
//! ## 十一域一网打尽
//!
//! [`V3Server::dispatch`] 是唯一的控制面入口，十一个域一个不少
//! （`System` / `Session` / `Command` / `Shell` / `Marketplace` / `Config` /
//! `Account` / `Pairing` / `Gitx` / `Fs` / `ToolCatalog`）。

mod bus;
mod connection;
mod endpoint;
mod error;
mod facade;
mod framing;
mod identity;
mod state;
mod util;

#[cfg(test)]
mod harness;
#[cfg(test)]
mod invariants;

pub(crate) use util::{lock, now_ms};

use std::sync::Arc;

use qaqh_ringing_v3::{ControlRequest, ControlResponse};
use qaqh_v3_transport::MuxTransport;

use facade::{CallContext, Facades};
use state::{ServerConfig, ServerState};

/// V3 服务端（`B` 是具体传输后端，**泛型静态分发，不做 `dyn` 装箱**）。
pub(crate) struct V3Server<B: MuxTransport> {
    transport: B,
    state: Arc<ServerState>,
    facades: Facades,
}

impl<B: MuxTransport> V3Server<B> {
    /// 以新状态构造（`os_user` 是本地 IPC 的授权依据）。
    pub(crate) fn new(
        transport: B,
        config: ServerConfig,
        os_user: impl Into<String>,
    ) -> Arc<Self> {
        Self::with_state(transport, ServerState::new(config, os_user))
    }

    /// 以既有状态构造（守卫用它把状态握在手里，才能伪造"洪峰产生方"）。
    pub(crate) fn with_state(transport: B, state: Arc<ServerState>) -> Arc<Self> {
        Arc::new(Self {
            transport,
            facades: Facades::new(Arc::clone(&state)),
            state,
        })
    }

    /// 全局状态（句柄）。
    pub(crate) fn state(&self) -> Arc<ServerState> {
        Arc::clone(&self.state)
    }

    /// 传输后端（守卫用）。
    pub(crate) fn transport(&self) -> &B {
        &self.transport
    }

    /// 控制面唯一分派入口：**十一域全覆盖，编译期静态分派**。
    pub(crate) async fn dispatch(
        &self,
        context: &CallContext,
        request: ControlRequest,
    ) -> ControlResponse {
        let outcome = match request {
            ControlRequest::System(request) => self
                .facades
                .system
                .handle(context, request)
                .await
                .map(ControlResponse::System),
            ControlRequest::Session(request) => self
                .facades
                .session
                .handle_session(context, request)
                .await
                .map(ControlResponse::Session),
            ControlRequest::Command(request) => self
                .facades
                .session
                .handle_command(context, request)
                .await
                .map(ControlResponse::Command),
            ControlRequest::Shell(request) => self
                .facades
                .shell
                .handle(context, request)
                .await
                .map(ControlResponse::Shell),
            ControlRequest::Marketplace(request) => self
                .facades
                .marketplace
                .handle(context, request)
                .await
                .map(ControlResponse::Marketplace),
            ControlRequest::Config(request) => self
                .facades
                .config_account
                .handle_config(context, request)
                .await
                .map(ControlResponse::Config),
            ControlRequest::Account(request) => self
                .facades
                .config_account
                .handle_account(context, request)
                .await
                .map(ControlResponse::Account),
            ControlRequest::Pairing(request) => self
                .facades
                .config_account
                .handle_pairing(context, request)
                .await
                .map(ControlResponse::Pairing),
            ControlRequest::Gitx(request) => self
                .facades
                .gitx
                .handle(context, request)
                .await
                .map(ControlResponse::Gitx),
            ControlRequest::Fs(request) => self
                .facades
                .fs
                .handle(context, request)
                .await
                .map(ControlResponse::Fs),
            ControlRequest::ToolCatalog(request) => self
                .facades
                .session
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
