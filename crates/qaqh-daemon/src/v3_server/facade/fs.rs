//! Fs 域：**骨架期只有签名与拒绝语义**。
//!
//! `Fs` 的真实现必须挂在会话授权根上（`ADR-0042` 铁律 7 显式否决了
//! `local_readonly_path` 这类"客户端指定路径"的后门）：范围由**会话授权**决定，
//! 不由请求里的 `path` 决定。那套授权根要在 `v4.7` 接 `qaqh-workspace` 时才有真身，
//! 所以这里如实 `Unavailable`——返回假 `Stat` 才是这一层最该避免的错误。

use std::sync::Arc;

use qaqh_ringing_v3::{FsRequest, FsResponse, RpcError};

use super::super::error::not_wired;
use super::super::state::ServerState;
use super::CallContext;

/// Fs 门面。
pub(crate) struct FsFacade {
    #[allow(dead_code)]
    state: Arc<ServerState>,
}

impl FsFacade {
    /// 构造。
    pub(crate) fn new(state: Arc<ServerState>) -> Self {
        Self { state }
    }

    /// 处理一条 Fs 请求。
    pub(crate) async fn handle(
        &self,
        _context: &CallContext,
        request: FsRequest,
    ) -> Result<FsResponse, RpcError> {
        // 写路径额外要求主设备作用域：即使将来接上真实现，这条判断也不会放宽。
        if matches!(request, FsRequest::Write { .. }) {
            _context.identity.require_owner("Fs::Write")?;
        }
        Err(not_wired(
            "Fs 域在 v4.6 P3-4 只落地签名与拒绝语义；真实现随 v4.7 会话授权根接线",
        ))
    }
}

#[cfg(test)]
mod smoke {
    use super::*;
    use crate::v3_server::facade::test_support;

    #[tokio::test]
    async fn fs_is_reached_through_dispatch_and_says_unavailable() {
        let rig = test_support::rig();
        let context = test_support::owner_context(&rig);
        let error = rig
            .facades
            .fs
            .handle(
                &context,
                FsRequest::Stat {
                    path: "README.md".to_string(),
                },
            )
            .await
            .expect_err("骨架期必须如实拒绝");
        assert_eq!(error.code, qaqh_ringing_v3::ErrorCode::Unavailable);
        assert!(!error.retryable, "能力缺失不可重试（重试只会得到同一个答案）");
    }

    #[tokio::test]
    async fn fs_write_requires_owner_before_capability_is_checked() {
        let rig = test_support::rig();
        let device = test_support::device_context(&rig);
        let error = rig
            .facades
            .fs
            .handle(
                &device,
                FsRequest::Write {
                    path: "x.txt".to_string(),
                    bytes: bytes::Bytes::from_static(b"x"),
                },
            )
            .await
            .expect_err("设备作用域不得写文件");
        assert_eq!(error.code, qaqh_ringing_v3::ErrorCode::PermissionDenied);
    }
}
