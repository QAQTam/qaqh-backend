//! Fs 域：**骨架期只有签名与拒绝语义**。
//!
//! `Fs` 的真实现必须挂在会话授权根上（`ADR-0042` 铁律 7 显式否决了
//! `local_readonly_path` 这类"客户端指定路径"的后门）：范围由**所属会话授权**
//! 决定，不由请求里的 `path` 决定。

use std::sync::Arc;

use qaqh_ringing_v3::{FsRequest, FsResponse, RpcError};

use super::super::error::{denied, not_found, not_wired};
use super::super::state::ServerState;
use super::CallContext;

/// Fs 门面骨架实现。
pub(crate) struct SkeletonFs {
    state: Arc<ServerState>,
}

impl SkeletonFs {
    /// 构造。
    pub(crate) fn new(state: Arc<ServerState>) -> Self {
        Self { state }
    }

    /// 处理一条 Fs 请求。
    pub(crate) async fn handle(
        &self,
        context: &CallContext,
        request: FsRequest,
    ) -> Result<FsResponse, RpcError> {
        let session_id = match &request {
            FsRequest::Stat { session_id, .. }
            | FsRequest::List { session_id, .. }
            | FsRequest::ReadRange { session_id, .. }
            | FsRequest::Write { session_id, .. } => session_id,
        };
        // 写路径额外要求主设备作用域：即使将来接上真实现，这条判断也不会放宽。
        if matches!(request, FsRequest::Write { .. }) {
            context.identity.require_owner("Fs::Write")?;
        }
        let session = self
            .state
            .session(session_id)
            .ok_or_else(|| not_found(format!("没有会话 `{session_id}`")))?;
        if !session.can_access(&context.identity.device) {
            return Err(denied(format!("设备无权访问会话 `{session_id}` 文件域")));
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
        let session = rig.state.create_session(&context.identity.device);
        let error = rig
            .backend
            .fs
            .handle(
                &context,
                FsRequest::Stat {
                    session_id: session.id.clone(),
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
        let owner = test_support::owner_context(&rig);
        let device = test_support::device_context(&rig);
        let session = rig.state.create_session(&owner.identity.device);
        let error = rig
            .backend
            .fs
            .handle(
                &device,
                FsRequest::Write {
                    session_id: session.id.clone(),
                    path: "x.txt".to_string(),
                    bytes: bytes::Bytes::from_static(b"x"),
                },
            )
            .await
            .expect_err("设备作用域不得写文件");
        assert_eq!(error.code, qaqh_ringing_v3::ErrorCode::PermissionDenied);
    }
}
