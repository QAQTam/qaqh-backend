//! 按域切分的服务端门面（`ADR-0042` 铁律 4："细粒度异步门面 + 泛型静态分发"）。
//!
//! ## 为什么按域切而不是一个大 impl
//!
//! 一个 `handle(ControlRequest) -> ControlResponse` 大概能省几百行 match，代价是：
//! 每个域都不得不认识其它域的存在，锁的范围也只能"为了安全"取最粗的那把。
//! 按域切之后，每个门面只持自己需要的状态，`shell` 那条热路径**一次中央锁都不碰**。
//!
//! ## 为什么没有 `dyn`
//!
//! `V3Server<B: MuxTransport>` 直接持有具体门面类型，`dispatch` 是编译期确定的
//! match 分派。装箱会把"哪条路径走哪个后端"推到运行期，还会在每次调用上多一层
//! 虚调用与一次分配——在 60fps 的渲染预算面前，这些都不该出现在地基上。
//!
//! ## 骨架期的诚实边界
//!
//! `Fs` / `Gitx` / `Marketplace` / `Config` / `Account` 在 `P3-4` 只落地**签名与
//! 拒绝语义**（`Unavailable` + 明确文案）：它们的真实现要接 `qaqh-workspace` /
//! `qaqh-gitx` / `qaqh-marketplace` / `qaqh-config` 的真实能力，那是 `v4.7` 切端
//! 的活。**留空壳返回假数据**才是这一层最该避免的错误。

use std::sync::Arc;

use qaqh_core_vocab::ContentHash;
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
    /// 本连接的大内容推送通道（`BlobTransfer` 流由连接层负责开流）。
    pub(crate) blob_pushes: mpsc::UnboundedSender<BlobPush>,
    /// 本连接附着过的会话（连接收摊时据此撤销附着与驱动权）。
    pub(crate) attached_sessions: Arc<std::sync::Mutex<std::collections::BTreeSet<qaqh_core_vocab::SessionId>>>,
}

/// 一次 blob 推送请求。
#[derive(Debug, Clone, Copy)]
pub(crate) struct BlobPush {
    /// 内容哈希。
    pub(crate) hash: ContentHash,
    /// 字节长度（客户端预留进度用）。
    pub(crate) len: u64,
}

/// 门面集合（`V3Server<B>` 的持有物，全部具体类型）。
pub(crate) struct Facades {
    pub(crate) system: system::SystemFacade,
    pub(crate) session: session::SessionFacade,
    pub(crate) shell: shell::ShellFacade,
    pub(crate) config_account: config_account::ConfigAccountFacade,
    pub(crate) marketplace: marketplace::MarketplaceFacade,
    pub(crate) gitx: gitx::GitxFacade,
    pub(crate) fs: fs::FsFacade,
    pub(crate) blob: blob::BlobFacade,
}

impl Facades {
    /// 从全局状态构造。
    pub(crate) fn new(state: Arc<ServerState>) -> Self {
        Self {
            system: system::SystemFacade::new(Arc::clone(&state)),
            session: session::SessionFacade::new(Arc::clone(&state)),
            shell: shell::ShellFacade::new(Arc::clone(&state)),
            config_account: config_account::ConfigAccountFacade::new(Arc::clone(&state)),
            marketplace: marketplace::MarketplaceFacade::new(Arc::clone(&state)),
            gitx: gitx::GitxFacade::new(Arc::clone(&state)),
            fs: fs::FsFacade::new(Arc::clone(&state)),
            blob: blob::BlobFacade::new(state),
        }
    }
}
