//! 线协议标识与常量（PLAN 固定命名）。

/// 线协议 schema 标识。
pub const RINGING_SCHEMA: &str = "deepx.Ringing";

/// 线协议版本。
pub const RINGING_VERSION: u32 = 1;

/// SSE 事件流帧格式：`id: <server_epoch>:<channel>:<stream_seq>`。
pub const SSE_EVENT_ID_SEPARATOR: char = ':';

