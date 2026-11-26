//! `daemon.endpoint` 无版本发现契约（`ADR-0042` 铁律 2："去版本化"）。
//!
//! ## 为什么是 postcard 而不是 JSON
//!
//! 发现文件是**同仓同版**的两端之间的事（不是对外 API）。用 JSON 会把
//! `serde_json` 拖进这条链，等于在协议地基上留一条 `Value` 逃生口；
//! 用 postcard 则与线缆同一套编码，且**天生不支持字段别名与版本容错**——
//! 正是我们要的"零兼容层"。
//!
//! ## 过期守护进程必须被拒绝，而不是被猜测
//!
//! 磁盘上的 `daemon.endpoint` 可能指向**上一版 daemon** 的 socket。
//! 客户端连上去之前先比对 `protocol_rev` + `protocol_fingerprint` + `build_id`：
//! 失配就是 [`EndpointReject`]，客户端应报"请重启 daemon"而不是硬连——
//! 硬连的后果是 postcard 把旧形状按新形状解码，产出一屏貌似正常实则错位的数据。

use serde::{Deserialize, Serialize};

use crate::DAEMON_ENDPOINT_FILE;
use crate::PROTOCOL_FINGERPRINT;
use crate::PROTOCOL_REV;

/// 传输载体（本地与远端同构：差异只在 [`DaemonEndpoint::transport`]）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndpointTransport {
    /// 本地 IPC：Unix domain socket / Windows named pipe（Owner-Only DACL）。
    LocalIpc {
        /// socket 或 pipe 的绝对路径（`\\\\.\\pipe\\...` 或 `/run/...`）。
        path: String,
    },
    /// 远端：`iroh` 端点（QUIC，公钥寻址 + NAT 打洞 + 加密 relay）。
    Iroh {
        /// 32 字节 EndpointId。
        endpoint_id: [u8; 32],
        /// relay 地址（自建/公共；`None` = 仅直连/局域网）。
        relay_url: Option<String>,
    },
}

/// 发布渠道（`stable` / `preview` / `dev`；决定客户端是否接受跨渠道连接）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointChannel(pub String);

/// 磁盘发现文件的内容（**永久无版本前缀**）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonEndpoint {
    /// 构建 id（每次构建不同；用于识别"同一个二进制"）。
    pub build_id: String,
    /// 发布渠道。
    pub channel: EndpointChannel,
    /// 传输载体。
    pub transport: EndpointTransport,
    /// 协议修订号。
    pub protocol_rev: u32,
    /// 协议指纹。
    pub protocol_fingerprint: [u8; 32],
    /// 写入时刻（毫秒 Unix 时间；只用于展示与清理陈旧文件，**不参与校验**）。
    pub written_at_ms: u64,
}

/// 拒绝原因（每个都对应一条明确的用户处置动作）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EndpointReject {
    /// 文件不存在或不可读（daemon 未启动 / 数据目录不对）。
    #[error("`{DAEMON_ENDPOINT_FILE}` 不可读：{0}")]
    Unreadable(String),
    /// 文件内容不是合法的 postcard `DaemonEndpoint`（旧版本写的、被截断、被篡改）。
    #[error("`{DAEMON_ENDPOINT_FILE}` 解码失败（多半是旧版 daemon 写的）：{0}")]
    Undecodable(String),
    /// 协议修订号不符（daemon 版本不对）。
    #[error("daemon 协议修订 {found} 与本端 {expected} 不同，请重启 daemon 或升级本端")]
    RevMismatch {
        /// 文件里的修订号。
        found: u32,
        /// 本端修订号。
        expected: u32,
    },
    /// 协议指纹不符（同修订号但形状不同——有人改了协议没 bump rev）。
    #[error("daemon 协议指纹与本端不一致（协议形状不同，禁止连接）")]
    FingerprintMismatch,
    /// 渠道不被接受（例如稳定版客户端拒绝连 dev daemon）。
    #[error("daemon 渠道 `{found}` 不被本端接受")]
    ChannelRejected {
        /// 文件里的渠道。
        found: String,
    },
}

impl DaemonEndpoint {
    /// 编码为磁盘字节（postcard，单一非 JSON 格式）。
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        postcard::to_stdvec(self).map_err(|error| error.to_string())
    }

    /// 从磁盘字节解码（**严格**：尾部多余字节即失败）。
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let (endpoint, rest) = postcard::take_from_bytes::<Self>(bytes)
            .map_err(|error| format!("postcard: {error}"))?;
        if !rest.is_empty() {
            return Err(format!(
                "尾部残留 {} 字节（发现文件必须恰好一个结构）",
                rest.len()
            ));
        }
        Ok(endpoint)
    }

    /// 校验：本端是否**允许**连接这个 daemon。
    ///
    /// `accepted_channel` 之外一律拒绝——渠道是产品决策，不做"就近降级"。
    pub fn validate(&self, accepted_channel: &str) -> Result<(), EndpointReject> {
        if self.protocol_rev != PROTOCOL_REV {
            return Err(EndpointReject::RevMismatch {
                found: self.protocol_rev,
                expected: PROTOCOL_REV,
            });
        }
        if self.protocol_fingerprint != PROTOCOL_FINGERPRINT {
            return Err(EndpointReject::FingerprintMismatch);
        }
        if self.channel.0 != accepted_channel {
            return Err(EndpointReject::ChannelRejected {
                found: self.channel.0.clone(),
            });
        }
        Ok(())
    }
}

/// 从磁盘读取并校验发现文件（两步合一，避免调用方忘掉校验）。
pub fn read_daemon_endpoint(
    data_dir: &std::path::Path,
    accepted_channel: &str,
) -> Result<DaemonEndpoint, EndpointReject> {
    let path = data_dir.join(DAEMON_ENDPOINT_FILE);
    let bytes = std::fs::read(&path).map_err(|error| {
        EndpointReject::Unreadable(format!("{}: {error}", path.display()))
    })?;
    let endpoint = DaemonEndpoint::decode(&bytes).map_err(EndpointReject::Undecodable)?;
    endpoint.validate(accepted_channel)?;
    Ok(endpoint)
}

/// 原子写发现文件（先写临时文件再 rename；daemon 启动期调用）。
pub fn write_daemon_endpoint(
    data_dir: &std::path::Path,
    endpoint: &DaemonEndpoint,
) -> std::io::Result<std::path::PathBuf> {
    let path = data_dir.join(DAEMON_ENDPOINT_FILE);
    let bytes = endpoint
        .encode()
        .map_err(|error| std::io::Error::other(format!("encode daemon.endpoint: {error}")))?;
    let staging = data_dir.join(format!("{DAEMON_ENDPOINT_FILE}.tmp"));
    std::fs::write(&staging, &bytes)?;
    std::fs::rename(&staging, &path)?;
    Ok(path)
}
