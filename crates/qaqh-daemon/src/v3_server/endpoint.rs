//! `daemon.endpoint` 的**产生端**：启动期生成、原子写入、退出时清理。
//!
//! 读取端在客户端（`qaqh-client/src/v3/endpoint.rs`），形状与校验规则都在
//! [`qaqh_ringing_v3::DaemonEndpoint`]（P3-3 冻结）。本文件只负责"把它写对"。
//!
//! ## 载体是本地 IPC，且只有本地 IPC
//!
//! 本切片只落 `EndpointTransport::LocalIpc`。`Iroh` 变体留到 `v4.8`——现在写一个
//! 假的 `endpoint_id` 只会让客户端以为能连远端。
//!
//! ## Unix：socket 文件权限就是身份边界
//!
//! socket 目录 `0700` + socket 文件 `0600`，再由 daemon 侧用 `SO_PEERCRED`
//! 核对对端 uid（实现随 `v4.7` 的监听器落地；本文件只把路径与权限约定写清楚）。
//! V3 的 `ClientCredential::LocalOsUser` 正是这份内核级凭据的产物——**不是**客户端
//! 自报的用户名。
//!
//! ## Windows：Owner-Only DACL + `reject_remote_clients`
//!
//! 命名管道必须显式构造只含**当前用户 SID** 的 DACL（不给 `Everyone`、不给
//! `Authenticated Users`），并设置 `PIPE_REJECT_REMOTE_CLIENTS`：默认安全描述符在
//! 域环境下会把管道暴露给网络登录，这不是"配置问题"而是"实现问题"。
//! 平台分支在 `v4.7` 落地，本文件不写半吊子 `cfg(windows)` 占位代码。
//!
//! ## 严格同版，绝不宽松解码
//!
//! `build_id` / `protocol_rev` / `protocol_fingerprint` / `channel` 任一项失配，
//! 客户端必须拒绝连接并提示重启 daemon（`ADR-0042` r4 §四否决了跨小版本宽松解码）。
//! 磁盘上的旧 `daemon.endpoint` 因此不是"能凑合用的线索"，而是**必须被拒绝的陷阱**。

use std::path::{Path, PathBuf};

use qaqh_ringing_v3::{
    DAEMON_ENDPOINT_FILE, DaemonEndpoint, EndpointChannel, PROTOCOL_FINGERPRINT, PROTOCOL_REV,
    write_daemon_endpoint,
};

use super::state::ServerConfig;
use super::util::now_ms;

/// 生成本机 IPC 的发现记录。
pub(crate) fn local_ipc_endpoint(
    config: &ServerConfig,
    socket_path: impl Into<String>,
) -> DaemonEndpoint {
    DaemonEndpoint {
        build_id: config.build_id.clone(),
        channel: EndpointChannel(config.channel.clone()),
        local_ipc: Some(socket_path.into()),
        iroh: None,
        protocol_rev: PROTOCOL_REV,
        protocol_fingerprint: PROTOCOL_FINGERPRINT,
        written_at_ms: now_ms(),
    }
}

/// 原子发布（先写 `.tmp` 再 rename：半截文件永远不该被读到）。
pub(crate) fn publish(data_dir: &Path, endpoint: &DaemonEndpoint) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(data_dir)?;
    write_daemon_endpoint(data_dir, endpoint)
}

/// 退出时清理（**只删自己写的那个文件**；daemon 崩溃留下的陈旧文件由客户端的
/// 版本校验拒绝，而不是靠这里"尽力删"）。
pub(crate) fn retire(data_dir: &Path) -> std::io::Result<()> {
    let path = data_dir.join(DAEMON_ENDPOINT_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_ringing_v3::{EndpointReject, read_daemon_endpoint};

    fn temp_dir(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "qaqh-v3-endpoint-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("建临时目录");
        path
    }

    #[test]
    fn published_endpoint_round_trips_through_the_client_reader() {
        let dir = temp_dir("roundtrip");
        let config = ServerConfig {
            build_id: "build-abc".to_string(),
            channel: "dev".to_string(),
            ..ServerConfig::default()
        };
        let endpoint = local_ipc_endpoint(&config, "/run/qaqh/daemon.sock");
        publish(&dir, &endpoint).expect("发布");
        let read = read_daemon_endpoint(&dir, "dev").expect("读取并校验");
        assert_eq!(read.build_id, "build-abc");
        assert_eq!(read.protocol_rev, PROTOCOL_REV);
        assert_eq!(read.protocol_fingerprint, PROTOCOL_FINGERPRINT);
        assert_eq!(read.local_ipc.as_deref(), Some("/run/qaqh/daemon.sock"));
        assert!(read.iroh.is_none());
        retire(&dir).expect("清理");
        assert!(matches!(
            read_daemon_endpoint(&dir, "dev"),
            Err(EndpointReject::Unreadable(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_channel_is_rejected_not_negotiated() {
        let dir = temp_dir("channel");
        let config = ServerConfig {
            channel: "preview".to_string(),
            ..ServerConfig::default()
        };
        let endpoint = local_ipc_endpoint(&config, "/run/qaqh/daemon.sock");
        publish(&dir, &endpoint).expect("发布");
        match read_daemon_endpoint(&dir, "stable") {
            Err(EndpointReject::ChannelRejected { found }) => assert_eq!(found, "preview"),
            other => panic!("必须拒绝跨渠道连接，实际 {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
