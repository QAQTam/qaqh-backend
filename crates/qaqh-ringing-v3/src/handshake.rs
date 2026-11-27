//! 握手契约（`ADR-0042` 铁律 2/5：严格同版 Fail-Closed + 身份单权威）。
//!
//! ## 严格同版，桌面与移动端一视同仁
//!
//! 客户端在 [`ClientHello`] 里自报 `protocol_rev` 与 `protocol_fingerprint`，
//! 服务端拿自己的 [`crate::PROTOCOL_REV`] / [`crate::PROTOCOL_FINGERPRINT`] 逐项比对，
//! 任一失配立即 [`HandshakeRejectReason::RevMismatch`] /
//! [`HandshakeRejectReason::FingerprintMismatch`]。
//!
//! **没有任何"跨小版本宽松解码"**：那是 `ADR-0042` r4 §四明确否决的选项——
//! postcard 是位置编码，形状差一个字段就是整帧乱码，宽松解码只会把
//! "读串了"伪装成"能用"。宁可让用户升级，也不产出一屏错乱数据。
//!
//! ## 身份不接受自报
//!
//! [`ClientHello`] 只带**凭据**（本地 OS 用户 / 远端 `iroh` 公钥 EndpointId），
//! 不带 `client_id` 之类"我说我是谁"。会话归属由服务端据凭据查表决定
//! （铁律 5：身份单权威不可伪造）。

use serde::{Deserialize, Serialize};

use crate::{PROTOCOL_FINGERPRINT, PROTOCOL_REV};

/// 客户端种类（只影响提示与配对 UX，不参与鉴权判断）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClientKind {
    /// 桌面（GPUI 原生壳）。
    Desktop,
    /// 移动端（iOS / Android 原生壳）。
    Mobile,
    /// 无头脚本 / CI。
    Headless,
}

/// 客户端凭据：**唯一**的身份来源。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientCredential {
    /// 本地 IPC：操作系统层面的同名用户（UDS SO_PEERCRED / Pipe Owner DACL 的产物）。
    LocalOsUser {
        /// 凭据里的用户标识（服务端仍会自行核对，不单方面采信）。
        user: String,
    },
    /// 远端：`iroh` EndpointId（公钥）——加密身份即凭据。
    IrohEndpoint {
        /// 32 字节公钥。
        endpoint_id: [u8; 32],
    },
    /// 已完成配对的设备令牌。
    PairedDevice {
        /// 设备令牌（不可伪造的随机串，服务端持有同一份）。
        token: String,
    },
}

/// 客户端首帧。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    /// 客户端进程实例 id（仅用于同端多窗口去重与日志关联，**不是身份**）。
    pub client_instance_id: String,
    /// 客户端种类。
    pub client_kind: ClientKind,
    /// 身份凭据。
    pub credential: ClientCredential,
    /// 客户端理解的协议修订号。
    pub protocol_rev: u32,
    /// 客户端理解的协议指纹。
    pub protocol_fingerprint: [u8; 32],
}

impl ClientHello {
    /// 以"本 crate 的版本"构造 hello（服务端自测与同仓客户端用）。
    pub fn new(
        client_instance_id: impl Into<String>,
        client_kind: ClientKind,
        credential: ClientCredential,
    ) -> Self {
        Self {
            client_instance_id: client_instance_id.into(),
            client_kind,
            credential,
            protocol_rev: PROTOCOL_REV,
            protocol_fingerprint: PROTOCOL_FINGERPRINT,
        }
    }
}

/// 服务端首帧。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerHello {
    /// 接受：给出服务端实例身份（客户端据此确认"我说的是哪个 daemon"）。
    Accepted {
        /// 服务端进程实例 id。
        server_instance_id: String,
        /// 服务端构建 id（与 `daemon.endpoint` 里的 `build_id` 同源）。
        build_id: String,
        /// 服务端协议修订号（必然等于客户端的）。
        protocol_rev: u32,
    },
    /// 拒绝：带机器可读原因，连接随即关闭（不进入业务帧阶段）。
    Rejected(HandshakeReject),
}

/// 拒绝原因（每个都对应一个客户端可执行动作）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HandshakeRejectReason {
    /// 协议修订号不同（客户端太旧或太新）。
    RevMismatch {
        /// 客户端自报的修订号。
        client: u32,
        /// 服务端修订号。
        server: u32,
    },
    /// 协议指纹不同——**同修订号也可能不同形状**（改字段不 bump rev 是作弊，
    /// 指纹会把它抓出来）。
    FingerprintMismatch {
        /// 客户端自报的指纹。
        client: [u8; 32],
        /// 服务端指纹。
        server: [u8; 32],
    },
    /// 凭据不被接受（未配对设备、被吊销令牌、越权 OS 用户）。
    CredentialRejected {
        /// 人类可读说明（不含任何可用于枚举的细节）。
        detail: String,
    },
}

/// 拒绝包。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeReject {
    /// 原因。
    pub reason: HandshakeRejectReason,
    /// 服务端协议修订号（让客户端能打印"你该升级到哪一版"）。
    pub server_rev: u32,
    /// 服务端协议指纹。
    pub server_fingerprint: [u8; 32],
}

/// 服务端侧握手校验（纯函数，便于在传输层之外单测）。
///
/// 顺序刻意固定：**先版本后凭据**——版本失配时连凭据都不想看（不泄露
/// "这个令牌是否存在"这类信息）。
pub fn verify_handshake(hello: &ClientHello, server_instance_id: &str, build_id: &str) -> ServerHello {
    if hello.protocol_rev != PROTOCOL_REV {
        return ServerHello::Rejected(HandshakeReject {
            reason: HandshakeRejectReason::RevMismatch {
                client: hello.protocol_rev,
                server: PROTOCOL_REV,
            },
            server_rev: PROTOCOL_REV,
            server_fingerprint: PROTOCOL_FINGERPRINT,
        });
    }
    if hello.protocol_fingerprint != PROTOCOL_FINGERPRINT {
        return ServerHello::Rejected(HandshakeReject {
            reason: HandshakeRejectReason::FingerprintMismatch {
                client: hello.protocol_fingerprint,
                server: PROTOCOL_FINGERPRINT,
            },
            server_rev: PROTOCOL_REV,
            server_fingerprint: PROTOCOL_FINGERPRINT,
        });
    }
    ServerHello::Accepted {
        server_instance_id: server_instance_id.to_string(),
        build_id: build_id.to_string(),
        protocol_rev: PROTOCOL_REV,
    }
}
