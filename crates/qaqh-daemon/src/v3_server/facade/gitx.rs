//! Gitx 域：骨架期只有签名与拒绝语义（真实现接 `qaqh-gitx` 在 `v4.7`）。
//!
//! `Commit` 是**有副作用**的能力，所以即使当前一律 `Unavailable`，作用域检查也
//! 放在能力检查之前——将来接上真实现时这条次序不会被"顺手优化"掉。

use std::sync::Arc;

use qaqh_ringing_v3::{GitxRequest, GitxResponse, RpcError};

use super::super::error::not_wired;
use super::super::state::ServerState;
use super::CallContext;

/// Gitx 门面。
pub(crate) struct GitxFacade {
    #[allow(dead_code)]
    state: Arc<ServerState>,
}

impl GitxFacade {
    /// 构造。
    pub(crate) fn new(state: Arc<ServerState>) -> Self {
        Self { state }
    }

    /// 处理一条 Gitx 请求。
    pub(crate) async fn handle(
        &self,
        context: &CallContext,
        request: GitxRequest,
    ) -> Result<GitxResponse, RpcError> {
        if matches!(request, GitxRequest::Commit { .. }) {
            context.identity.require_owner("Gitx::Commit")?;
        }
        Err(not_wired(
            "Gitx 域在 v4.6 P3-4 只落地签名与拒绝语义；真实现随 v4.7 接 qaqh-gitx",
        ))
    }
}

#[cfg(test)]
mod smoke {
    use super::*;
    use crate::v3_server::facade::test_support;

    #[tokio::test]
    async fn gitx_is_reached_and_reports_unavailable() {
        let rig = test_support::rig();
        let context = test_support::owner_context(&rig);
        let error = rig
            .facades
            .gitx
            .handle(
                &context,
                GitxRequest::Status {
                    session_id: qaqh_core_vocab::SessionId::new("session-0001").expect("id"),
                },
            )
            .await
            .expect_err("骨架期必须如实拒绝");
        assert_eq!(error.code, qaqh_ringing_v3::ErrorCode::Unavailable);
    }
}
