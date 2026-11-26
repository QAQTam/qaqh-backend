//! Marketplace 域：骨架期只有签名与拒绝语义（真实现接 `qaqh-marketplace` 在 `v4.7`）。
//!
//! 安装/卸载是**有副作用且可执行第三方代码**的能力，所以作用域检查放在能力检查之前。

use std::sync::Arc;

use qaqh_ringing_v3::{MarketplaceRequest, MarketplaceResponse, RpcError};

use super::super::error::not_wired;
use super::super::state::ServerState;
use super::CallContext;

/// Marketplace 门面。
pub(crate) struct MarketplaceFacade {
    #[allow(dead_code)]
    state: Arc<ServerState>,
}

impl MarketplaceFacade {
    /// 构造。
    pub(crate) fn new(state: Arc<ServerState>) -> Self {
        Self { state }
    }

    /// 处理一条 Marketplace 请求。
    pub(crate) async fn handle(
        &self,
        context: &CallContext,
        request: MarketplaceRequest,
    ) -> Result<MarketplaceResponse, RpcError> {
        if matches!(
            request,
            MarketplaceRequest::Install { .. } | MarketplaceRequest::Uninstall { .. }
        ) {
            context.identity.require_owner("Marketplace::Install/Uninstall")?;
        }
        Err(not_wired(
            "Marketplace 域在 v4.6 P3-4 只落地签名与拒绝语义；真实现随 v4.7 接 qaqh-marketplace",
        ))
    }
}

#[cfg(test)]
mod smoke {
    use super::*;
    use crate::v3_server::facade::test_support;

    #[tokio::test]
    async fn marketplace_reaches_dispatch_and_refuses_honestly() {
        let rig = test_support::rig();
        let context = test_support::owner_context(&rig);
        let error = rig
            .facades
            .marketplace
            .handle(
                &context,
                MarketplaceRequest::Search {
                    query: "screenshot".to_string(),
                    limit: 10,
                },
            )
            .await
            .expect_err("骨架期必须如实拒绝");
        assert_eq!(error.code, qaqh_ringing_v3::ErrorCode::Unavailable);
    }

    #[tokio::test]
    async fn install_requires_owner_scope() {
        let rig = test_support::rig();
        let device = test_support::device_context(&rig);
        let error = rig
            .facades
            .marketplace
            .handle(
                &device,
                MarketplaceRequest::Install {
                    id: "evil".to_string(),
                    version: None,
                },
            )
            .await
            .expect_err("已配对设备不得装插件");
        assert_eq!(error.code, qaqh_ringing_v3::ErrorCode::PermissionDenied);
    }
}
