//! `RingingV3` 强类型二进制协议契约（`ADR-0042` 铁律 1–4、7）。
//!
//! 本 crate 只描述**线缆形状**：帧、握手、四类逻辑流与全量 RPC 的强类型 DTO。
//! 它不依赖任何传输实现（没有 `axum`、没有 `tokio`、没有 `iroh`），
//! 因此 `v4.7` 的本地 `UDS/Pipe + yamux` 与 `v4.8` 的远端 `QUIC/iroh` 可以
//! **共用同一份契约**，两端各自只实现传输适配层。
//!
//! ## 七条零遗留铁律在本 crate 的落地形状
//!
//! 1. **零 `serde_json`**：依赖树里出现即失败。动态参数只有 [`JsonText`]（原始文本）
//!    与 [`bytes::Bytes`]（裸字节）两条出路。
//! 2. **零 `skip_serializing_if`**：`postcard` 是位置编码，`Option::None` 跳过字段
//!    会让后面所有字段错位——这是"看起来能用、特定数据才炸"的经典陷阱，直接封杀。
//! 3. **零 `#[serde(tag)]` / `flatten` / `untagged`**：全部 DTO 走 **externally-tagged**
//!    枚举（变体序号即线缆），字段顺序即线缆顺序。
//! 4. **零 `#[serde(alias)]` 与版本前缀**：业务域 DTO 永久不带 `V3`；只有本 crate
//!    （线缆封装层）带版本，握手靠 ALPN + 指纹强校验而不是字符串版本号。
//! 5. **自报身份一律不信**：DTO 里没有 `owner_id` / `client_id` 这类"客户端说自己是谁"
//!    的字段——身份只由握手凭据 + 服务端会话表决定（`ADR-0042` 铁律 5）。
//! 6. **机器生成协议指纹**：[`PROTOCOL_FINGERPRINT`] 由 `build.rs` 从类型定义源码
//!    计算（见 build.rs 头注），源码里没有可手改的字面量。
//! 7. **显式排除**：不纳入 `InspectModelContext`，不带 `local_readonly_path`，
//!    不把后端内部 `opaque` 锚点带上客户端线缆。
//!
//! ## 编码形状速查
//!
//! ```text
//! frame      = u32_le(len) ++ postcard(payload)   // len = payload 字节数，≤ MAX_FRAME_BYTES
//! payload    = HandshakeMessage | StreamFrame
//! ```
//!
//! 握手成功后连接上流动的就是 [`stream::StreamFrame`]：四类逻辑流（Control /
//! SessionEvents / ShellPty / BlobTransfer）共享同一条多路复用连接，彼此的因果
//! 独立域物理隔离（`ADR-0042` 铁律 4：绝不按 `DeliveryClass` 拆流）。

#![cfg_attr(not(test), forbid(unsafe_code))]
#![deny(missing_docs)]

pub mod control;
pub mod endpoint;
pub mod frame;
pub mod handshake;
pub mod stream;

include!(concat!(env!("OUT_DIR"), "/contract.rs"));

#[cfg(test)]
mod tests;

/// 本地/远端握手 ALPN（传输层协商用；失配即拒，不做降级）。
pub const RINGING_V3_ALPN: &[u8] = b"qaqh-ringing/3";

/// 磁盘发现文件名（无版本前缀，`ADR-0042` 铁律"去版本化"）。
pub const DAEMON_ENDPOINT_FILE: &str = "daemon.endpoint";

/// 单帧字节上限（防恶意/异常超大帧把对端拖进 OOM）。
///
/// 16 MiB 的取值依据：`ADR-0030` 泵批帽 256 KiB 的 64 倍余量，足以承载
/// 大块 Blob 分片与首屏快照，又远低于"能把进程打爆"的量级。超限一律
/// [`frame::FrameError::TooLarge`]（Fail-Closed，不截断、不静默丢弃）。
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

pub use control::*;
pub use endpoint::*;
pub use frame::*;
pub use handshake::*;
pub use stream::*;
