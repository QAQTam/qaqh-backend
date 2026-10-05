//! # qaqh-ringing — Ringing 线协议层（Wire）
//!
//! 四层架构（Domain / Projection / Wire / Transport）的 Wire 层：
//!
//! - `envelope`：`RingingCommandAck`（v1 命令信封与事件信封/batch 已删除，
//!   事件面由 `v2` typed envelope 承担）
//! - `snapshot`：`RingingChannelSnapshot`
//! - `content`：`RingingContentRef`（大内容外置引用）
//! - `worker`：daemon ↔ agent worker 边界的 framed envelope
//! - `protocol`：线协议标识（`schema: "qaqh.Ringing"`）与安全整数常量
//! - `v2`：并存的 Ringing v2 wire 契约（`/ringing/v2`、canonical cursor、
//!   typed projection envelope、bootstrap/reset/driver/interaction）
//!
//! ## 架构硬规则
//!
//! - 本 crate 依赖 `qaqh-domain`（wire → domain）；legacy proto crate 已删除（PR-3-5），依赖关系由编译图天然禁止。
//! - Wire 不决定业务可靠性；envelope 的 `delivery` 由领域事件定义填充。
//! - 本 crate 不含任何传输实现（HTTP/SSE/WebSocket/pipe 在 transport 层）。

pub mod command;
pub mod content;
pub mod envelope;
pub mod event;
pub mod protocol;
pub mod snapshot;
pub mod v2;
pub mod worker;

pub use command::{
    RingingCommand, RingingControlCommand, RingingConversationCommand, RingingToolCommand,
};
pub use content::RingingContentRef;
pub use envelope::{
    RingingCommandAck, RingingCommandAckStatus, RingingCommandState, RingingCommandStatus,
};
pub use event::{RingingControlEvent, RingingConversationEvent, RingingEvent, RingingToolEvent};
pub use protocol::{
    CLIENT_SESSION_HEADER, MAX_SAFE_INTEGER, RINGING_SCHEMA, RINGING_VERSION, is_safe_integer,
};
pub use snapshot::RingingChannelSnapshot;
pub use v2::*;
pub use worker::{
    RingingTimelineIntentEnvelope, RingingWorkerCommandEnvelope, RingingWorkerEventEnvelope,
};
