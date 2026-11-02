//! Ringing 服务面（方法表 + wire DTO + `dispatch`）。
//!
//! 传输 / 投影 / 队列层已于 2026-10-30 外移到 `qaqh-ringing-runtime`
//! （`v2`、`content_store`、`pending_store`、`lease_store`、`hub`、`projection`、
//! `device_registry`、`attachment`、
//! `agent_graph`）。这里 `pub use` 它，保持
//! `crate::ringing::V2ProjectionHub`、`qaqh_runtime::ringing::*` 等既有路径不变。
//!
//! 留在本模块的只有需要 `QaqhService` 的那一层：方法表（`METHODS` /
//! `TYPED_METHODS`）、逐方法 DTO、以及 `dispatch`。

pub use qaqh_ringing_runtime::*;

pub mod service_methods;
