//! # qaqh-ringing — Ringing 线协议层（Wire）
//!
//! Ringing wire command and transport-independent DTO definitions:
//!
//! - `envelope`：`RingingCommandAck` 与业务命令状态
//! - `worker`：daemon → agent command envelope
//! - `protocol`：线协议标识（`schema: "qaqh.Ringing"`）与安全整数常量
//! - `v2`：Ringing v2 命令信封、能力协商、配对与设备授权 DTO
//!
//! ## 架构硬规则
//!
//! - Public command payloads come from `qaqh-ringing-contract`.
//! - Wire 不决定业务可靠性。
//! - 本 crate 不含任何传输实现（HTTP/SSE/WebSocket/pipe 在 transport 层）。

#![cfg_attr(not(test), forbid(unsafe_code))]
pub mod command;
pub mod envelope;
pub mod protocol;
pub mod v2;
pub mod worker;

pub use command::{ConversationSendMessage, RingingCommand};
pub use envelope::{
    RingingCommandAck, RingingCommandAckStatus, RingingCommandState, RingingCommandStatus,
};
pub use protocol::{CLIENT_SESSION_HEADER, MAX_SAFE_INTEGER, RINGING_SCHEMA, is_safe_integer};
pub use v2::*;
pub use worker::{RingingWorkerCommandEnvelope, WorkerActor};
