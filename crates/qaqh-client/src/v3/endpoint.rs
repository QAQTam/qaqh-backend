//! `daemon.endpoint` 的**读取校验层**：过期守护进程必须被拒绝，不能硬连。
//!
//! 形状与校验规则的真源在 [`qaqh_ringing_v3::DaemonEndpoint`]（P3-3 冻结）：
//! `protocol_rev` / `protocol_fingerprint` / `channel` 三项由
//! [`read_daemon_endpoint`] 一并校验。本文件补上**产品层**那一道：
//! `build_id` 失配说明磁盘上的文件指向**另一个构建**的 daemon——那不是"版本稍旧
//! 但能用"，硬连的后果是 postcard 按新形状解码旧字节，产出一屏貌似正常实则错位
//! 的数据。
//!
//! ## 拒绝之后该做什么
//!
//! 每一个拒绝都对应一个明确动作：文件不可读 → 启动 daemon；修订/指纹不匹配 →
//! 升级本端或重启 daemon；渠道不被接受 → 换渠道；`build_id` 不匹配 → 重启 daemon
//! （它多半是崩溃残留）。**没有一种是"再试一次说不定就好了"**。

use std::path::Path;

use qaqh_ringing_v3::{DaemonEndpoint, EndpointReject, read_daemon_endpoint};

/// 发现层错误。
#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    /// 发现文件被协议层拒绝（不可读 / 解码失败 / 修订 / 指纹 / 渠道）。
    #[error("`daemon.endpoint` 被拒绝：{0}")]
    Rejected(#[from] EndpointReject),
    /// `build_id` 与期望的构建不同（多半是崩溃残留的旧文件）。
    #[error("`daemon.endpoint` 指向另一个构建：文件 `{found}`，期望 `{expected}`")]
    BuildIdMismatch {
        /// 文件里的构建 id。
        found: String,
        /// 调用方期望的构建 id。
        expected: String,
    },
}

/// 读到并校验本机 `daemon.endpoint`。
///
/// `expected_build_id` 给 `None` 表示"只做协议层校验"（客户端不认构建号时用）。
pub fn resolve_daemon_endpoint(
    data_dir: &Path,
    accepted_channel: &str,
    expected_build_id: Option<&str>,
) -> Result<DaemonEndpoint, EndpointError> {
    let endpoint = read_daemon_endpoint(data_dir, accepted_channel)?;
    if let Some(expected) = expected_build_id
        && endpoint.build_id != expected
    {
        return Err(EndpointError::BuildIdMismatch {
            found: endpoint.build_id,
            expected: expected.to_string(),
        });
    }
    Ok(endpoint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_ringing_v3::{
        DAEMON_ENDPOINT_FILE, EndpointChannel, EndpointTransport, PROTOCOL_FINGERPRINT, PROTOCOL_REV,
        write_daemon_endpoint,
    };

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "qaqh-v3-client-endpoint-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("建临时目录");
        path
    }

    fn endpoint(build_id: &str, channel: &str) -> DaemonEndpoint {
        DaemonEndpoint {
            build_id: build_id.to_string(),
            channel: EndpointChannel(channel.to_string()),
            transport: EndpointTransport::LocalIpc {
                path: "/run/qaqh/daemon.sock".to_string(),
            },
            protocol_rev: PROTOCOL_REV,
            protocol_fingerprint: PROTOCOL_FINGERPRINT,
            written_at_ms: 1,
        }
    }

    #[test]
    fn matching_build_and_channel_resolves() {
        let dir = temp_dir("ok");
        write_daemon_endpoint(&dir, &endpoint("build-1", "dev")).expect("写");
        let resolved = resolve_daemon_endpoint(&dir, "dev", Some("build-1")).expect("解析");
        assert_eq!(resolved.build_id, "build-1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_build_id_is_refused_instead_of_guessed() {
        let dir = temp_dir("build");
        write_daemon_endpoint(&dir, &endpoint("build-old", "dev")).expect("写");
        match resolve_daemon_endpoint(&dir, "dev", Some("build-new")) {
            Err(EndpointError::BuildIdMismatch { found, expected }) => {
                assert_eq!(found, "build-old");
                assert_eq!(expected, "build-new");
            }
            other => panic!("必须拒绝另一个构建的 daemon，实际 {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_endpoint_file_reports_the_actual_path() {
        let dir = temp_dir("missing");
        match resolve_daemon_endpoint(&dir, "dev", None) {
            Err(EndpointError::Rejected(EndpointReject::Unreadable(detail))) => {
                assert!(detail.contains(DAEMON_ENDPOINT_FILE));
            }
            other => panic!("必须报不可读，实际 {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrong_channel_is_refused() {
        let dir = temp_dir("channel");
        write_daemon_endpoint(&dir, &endpoint("build-1", "preview")).expect("写");
        assert!(matches!(
            resolve_daemon_endpoint(&dir, "stable", None),
            Err(EndpointError::Rejected(EndpointReject::ChannelRejected { .. }))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
