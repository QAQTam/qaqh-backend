//! 门面单测的共享脚手架（仅测试可见）。
//!
//! 只做三件事：造一份状态、造一个主设备上下文、造一个已配对设备上下文。
//! 守卫测试用的是**真双端 + 真多路复用**，这里只服务门面自身的 smoke 断言。

use std::sync::Arc;

use qaqh_ringing_v3::{ClientCredential, ClientHello, ClientKind};
use tokio::sync::{mpsc, watch};

use super::super::identity::{ConnectionIdentity, Scope};
use super::super::state::{ServerConfig, ServerState};
use super::{CallContext, SkeletonBackend};

/// 门面单测的整套装置。
pub(crate) struct Rig {
    pub(crate) state: Arc<ServerState>,
    pub(crate) backend: SkeletonBackend,
    pub(crate) owner: ConnectionIdentity,
    pub(crate) device: ConnectionIdentity,
}

/// 造一套装置。
pub(crate) fn rig() -> Rig {
    let state = ServerState::new(ServerConfig::default(), "qaqh-test");
    let backend = SkeletonBackend::new(Arc::clone(&state));
    // 两条身份都先经真凭据表认证，保证单测走的是与生产同一条路径。
    let digest = qaqh_ringing_runtime::device_registry::token_digest("phone");
    state
        .identity
        .register_device_digest(&digest, "test-phone", "手机", Scope::Device, 0);
    let owner = state
        .identity
        .authenticate(&ClientHello::new(
            "owner-window",
            ClientKind::Desktop,
            ClientCredential::LocalOsUser {
                user: "qaqh-test".to_string(),
            },
        ))
        .expect("主设备认证");
    let device = state
        .identity
        .authenticate(&ClientHello::new(
            "phone-window",
            ClientKind::Mobile,
            ClientCredential::PairedDevice {
                token: "phone".to_string(),
            },
        ))
        .expect("设备认证");
    Rig {
        state,
        backend,
        owner,
        device,
    }
}

/// 主设备上下文。
pub(crate) fn owner_context(rig: &Rig) -> CallContext {
    context_for(&rig.owner)
}

/// 已配对设备上下文。
pub(crate) fn device_context(rig: &Rig) -> CallContext {
    context_for(&rig.device)
}

fn context_for(identity: &ConnectionIdentity) -> CallContext {
    let (blob_pushes, _) = mpsc::unbounded_channel();
    CallContext {
        identity: Arc::new(identity.clone()),
        blob_pushes,
        attached_sessions: Arc::new(std::sync::Mutex::new(std::collections::BTreeSet::new())),
    }
}

/// 造一个"永远不会被吊销"的哨兵（给只需要身份的纯函数用）。
#[allow(dead_code)]
pub(crate) fn never_revoked() -> watch::Receiver<bool> {
    watch::channel(false).1
}
