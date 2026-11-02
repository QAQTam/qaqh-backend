//! `qaqh-ringing-runtime` — Ringing daemon 的传输 / 投影 / 队列层。
//!
//! 2026-10-30 从 `qaqh-runtime` 拆出（原 `qaqh-runtime/src/ringing/` 的传输部分
//! 加上 `agent_graph`）。拆出时它是自洽的：
//!
//! - 内部只依赖 `qaqh-session` / `qaqh-domain` / `qaqh-types` / `qaqh-ringing` /
//!   `qaqh-memwatch`；
//! - 不引用 `QaqhService`、registry 或任何 RPC 分发逻辑。
//!
//! 留在 `qaqh-runtime::ringing` 的只剩 `service_methods`（方法表 + DTO +
//! `dispatch`），它需要 `QaqhService`，所以留在 core。
//!
//! ## 布局
//!
//! - `projection`：持久化消息 → UI turn 投影
//! - `hub`：daemon 侧 Ringing 运行时聚合入口 `RingingHub`
//! - `v2`：v2 投影/订阅面（bootstrap、stream、lease 绑定）
//! - `pending_store` / `content_store` / `lease_store` / `device_registry`：
//!   命令回执、外置内容、租约与设备登记
//! - `agent_graph`：从 canonical fact log 读会话事实与 agent 图（原属 runtime）

#![cfg_attr(not(test), forbid(unsafe_code))]

pub mod agent_graph;
pub mod attachment;
pub mod content_store;
pub mod device_registry;
pub mod hub;
pub mod lease_store;
pub mod pending_store;
pub mod v2;

pub use attachment::hydrate_attachment_previews;
pub use content_store::CONTENT_STORE_THRESHOLD_BYTES;
pub use device_registry::{DeviceRecord, DeviceRegistry, Scope};
pub use hub::RingingHub;
pub use lease_store::RingingLeaseStore;
pub use pending_store::{ExistingCommandReceipt, PendingCommandStore};
pub use v2::{
    V2BootstrapSnapshot, V2Envelope, V2HubError, V2ProjectionHub, V2StreamItem, V2Subscription,
};
