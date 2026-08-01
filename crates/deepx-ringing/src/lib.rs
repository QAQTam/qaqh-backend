//! # deepx-ringing — Ringing 线协议层（Wire）
//!
//! 四层架构（Domain / Projection / Wire / Transport）的 Wire 层：
//!
//! - `envelope`：`RingingEventEnvelope` / `RingingCommandEnvelope` / `RingingCommandAck` /
//!   `RingingEventBatch`
//! - `snapshot`：`RingingChannelSnapshot`
//! - `content`：`RingingContentRef`（大内容外置引用）
//! - `worker`：daemon ↔ agent worker 边界的 framed envelope
//! - `capability`：客户端 open/能力协商（`Ringing_v2` 等）
//! - `protocol`：线协议标识（`schema: "deepx.Ringing"`, `version: 2`）
//!
//! ## 架构硬规则
//!
//! - 本 crate 依赖 `deepx-domain`（wire → domain），**不得**依赖 `deepx-proto`（legacy）。
//! - Wire 不决定业务可靠性；envelope 的 `delivery` 由领域事件定义填充。
//! - 本 crate 不含任何传输实现（HTTP/SSE/WebSocket/pipe 在 transport 层）。

pub mod capability;
pub mod command;
pub mod content;
pub mod envelope;
pub mod event;
pub mod protocol;
pub mod reset;
pub mod snapshot;
pub mod worker;

pub use capability::{CapabilityName, ClientOpenRequest, ClientOpenResponse};
pub use command::{
    RingingCommand, RingingControlCommand, RingingConversationCommand, RingingToolCommand,
};
pub use content::RingingContentRef;
pub use envelope::{
    RingingCommandAck, RingingCommandAckStatus, RingingCommandEnvelope, RingingCommandState,
    RingingCommandStatus, RingingEventBatch, RingingEventEnvelope,
};
pub use event::{RingingControlEvent, RingingConversationEvent, RingingEvent, RingingToolEvent};
pub use protocol::{
    CLIENT_SESSION_HEADER, MAX_SAFE_INTEGER, RINGING_BASE_PATH, RINGING_SCHEMA, RINGING_VERSION,
    is_safe_integer,
};
pub use reset::RingingResetRequired;
pub use snapshot::{RingingChannelSnapshot, RingingSessionBootstrap};
pub use worker::{
    RingingWorkerCommandEnvelope, RingingWorkerEventEnvelope, WIRE_RINGING_DOMAIN_V2,
    WORKER_FRAME_MAX_BYTES, WorkerDirection,
};
