//! `RingingV3` 多路复用**传输层**（`ADR-0042` 铁律 4 / r5 §yamux 工程约束）。
//!
//! ## 这一层为什么独立成 crate
//!
//! `qaqh-ringing-v3` 只描述线缆形状，且**刻意不依赖 `tokio`**（P3-3 冻结的依赖面）。
//! 但"把四条逻辑流搬到一条多路复用连接上"这件事天然要说 `AsyncRead`/`AsyncWrite`，
//! 而**两端**（`qaqh-daemon` 的 `v3_server` 与 `qaqh-client` 的 `v3`）都要说同一套话。
//!
//! 于是传输抽象只有两个可能的位置：抄两份（两套 trait，迟早语义漂移），
//! 或者放在两端都能看见的一层。这里选后者——本 crate 只做这件事：
//!
//! ```text
//! qaqh-ringing-v3   (线缆形状，零 tokio)
//!        ↑
//! qaqh-v3-transport (MuxTransport trait + yamux 后端 + duplex 闭环)   ← 本 crate
//!        ↑                          ↑
//!   v3_server                 client/v3
//! ```
//!
//! ## 铁律：唯一 driver 持续推进
//!
//! `yamux` 的 `Connection` 是**单所有者**状态机：`poll_new_outbound` /
//! `poll_next_inbound` 都要 `&mut self`。这既是限制也是保证——只要整个连接只有
//! 一个任务在驱动它，四条逻辑流之间就不存在"两个 driver 抢着 flush 导致定序错乱"。
//! 本 crate 把这条约束固化成类型：`Connection` 只存在于 driver task 内部，
//! 外部拿到的只有 `YamuxTransport`（可克隆句柄），**没有任何办法**绕过 driver 直接动连接。
//!
//! ## 与 `v4.8` `iroh` 的对齐面
//!
//! [`MuxTransport`] 的四个方法名与 `iroh::Connection` 逐字对齐
//! （`open_bi` / `accept_bi` / `open_uni` / `accept_uni`），
//! 这样 `v4.8` 接远端时是"加一个 impl"，而不是"改一遍所有门面签名"。
//! `yamux` 规范里**没有单向流**，所以本后端的 `open_uni`/`accept_uni` 一律
//! [`MuxError::Unsupported`]——这是能力差异，不是缺实现，必须在 `v4.8` 之前就知道。

// Windows Named Pipe 的 Owner-Only DACL 需要局限在单个 FFI 模块内的 unsafe。
#![deny(unsafe_code)]
#![deny(missing_docs)]

mod mux;
mod yamux_backend;

pub mod duplex;
pub mod framing;
mod local_ipc;
#[cfg(unix)]
mod unix_ipc;
#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_ipc;

pub use local_ipc::{LocalIpcListener, bind_local_ipc, connect_local_ipc};

pub use framing::{FrameIoError, read_frame, write_frame};
pub use mux::{MuxConfig, MuxError, MuxRole, MuxStats, MuxTransport};
pub use yamux_backend::{MuxStream, YamuxTransport};

#[cfg(test)]
mod tests;
