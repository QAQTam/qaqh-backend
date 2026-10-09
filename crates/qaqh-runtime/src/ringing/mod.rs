//! Ringing daemon 运行时（Projection/队列 层）。
//!
//! - `projection`：持久化消息 → UI turn 投影
//! - `hub`：daemon 侧 Ringing 运行时聚合入口 `RingingHub`

pub mod attachment;
pub(crate) mod compact_mirror;
pub mod content_store;
pub mod device_registry;
pub mod driver_watch;
pub mod hub;
pub mod lease_store;
pub mod orphan_seal;
pub mod pending_store;
pub mod persistence_policy;
pub mod projection;
pub mod service_methods;
pub mod timeline_hub;
pub(crate) mod timeline_rebuild;
pub mod v2;

pub use attachment::hydrate_attachment_previews;
pub(crate) use compact_mirror::CompactMirror;
pub use content_store::CONTENT_STORE_THRESHOLD_BYTES;
pub use device_registry::{DeviceRecord, DeviceRegistry, Scope};
pub use driver_watch::RingingDriverWatch;
pub use lease_store::RingingLeaseStore;
pub use pending_store::{ExistingCommandReceipt, PendingCommandStore};
pub use v2::{
    V2BootstrapSnapshot, V2Envelope, V2HubError, V2ProjectionHub, V2StreamItem, V2Subscription,
};

// PR-2-3 模块规则 1（R-4）：ringing/ 的对外消费面收敛为上方 re-export
// 白名单；`ringing/` 之外的模块禁止深路径引用（如 `ringing::content_store::*`），
// 需要新条目时先在此登记再使用。评审检查单：ringing/ 之外 `crate::ringing::`
// 命中必须全为白名单路径；`crates/qaqh-runtime/src/agent/` 内命中必须为 0。
