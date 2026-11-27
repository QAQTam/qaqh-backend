//! Config / Account / Pairing 三个域合并成一个门面（同一把"本机配置与凭据"的锁）。
//!
//! ## 骨架期哪些是真的
//!
//! - `Pairing::ListDevices` / `Pairing::RevokeDevice`：**真实现**。设备表就在身份表里，
//!   吊销即置哨兵（踢流由连接层落实）。这是守卫⑤要用的能力，不实现就没法证明。
//! - `Pairing::CreateInvite` / `Redeem`：`Unavailable`。真配对要挑战-应答 + 二维码 +
//!   令牌安全落盘，属于 `v4.7` 的活；在这里返回一个假邀请码只会让人以为能连。
//! - `Config::*` / `Account::*`：`Unavailable`（要接 `qaqh-config` / `qaqh-dsaccount`）。

use std::sync::Arc;

use qaqh_ringing_v3::{
    AccountRequest, AccountResponse, ConfigRequest, ConfigResponse, DeviceView, PairingRequest,
    PairingResponse, RpcError,
};

use super::super::error::not_wired;
use super::super::state::ServerState;
use super::CallContext;

/// Config / Account / Pairing 门面骨架实现。
pub(crate) struct SkeletonConfigAccount {
    state: Arc<ServerState>,
}

impl SkeletonConfigAccount {
    /// 构造。
    pub(crate) fn new(state: Arc<ServerState>) -> Self {
        Self { state }
    }

    /// 处理一条 Config 请求。
    pub(crate) async fn handle_config(
        &self,
        context: &CallContext,
        request: ConfigRequest,
    ) -> Result<ConfigResponse, RpcError> {
        if matches!(request, ConfigRequest::Set { .. } | ConfigRequest::Reload) {
            context.identity.require_owner("Config::Set/Reload")?;
        }
        Err(not_wired(
            "Config 域在 v4.6 P3-4 只落地签名与拒绝语义；真实现随 v4.7 接 qaqh-config",
        ))
    }

    /// 处理一条 Account 请求。
    pub(crate) async fn handle_account(
        &self,
        context: &CallContext,
        request: AccountRequest,
    ) -> Result<AccountResponse, RpcError> {
        if matches!(
            request,
            AccountRequest::Select { .. } | AccountRequest::Logout { .. }
        ) {
            context.identity.require_owner("Account::Select/Logout")?;
        }
        Err(not_wired(
            "Account 域在 v4.6 P3-4 只落地签名与拒绝语义；真实现随 v4.7 接 qaqh-dsaccount",
        ))
    }

    /// 处理一条 Pairing 请求。
    pub(crate) async fn handle_pairing(
        &self,
        context: &CallContext,
        request: PairingRequest,
    ) -> Result<PairingResponse, RpcError> {
        match request {
            PairingRequest::ListDevices => {
                context.identity.require_owner("Pairing::ListDevices")?;
                let online_devices: std::collections::BTreeSet<String> = self
                    .state
                    .sessions()
                    .iter()
                    .flat_map(|session| {
                        session
                            .attached()
                            .into_keys()
                            .collect::<Vec<_>>()
                    })
                    .collect();
                // 已吊销的设备不再出现在配对表里：吊销＝解绑，不是"打个标记还挂着"。
                let devices = self
                    .state
                    .identity
                    .devices()
                    .into_iter()
                    .filter(|device| !device.revoked)
                    .map(|device| DeviceView {
                        device_id: device.device_id,
                        label: device.label,
                        paired_at_ms: device.paired_at_ms,
                        online: online_devices.contains(&device.key),
                    })
                    .collect();
                Ok(PairingResponse::Devices(devices))
            }
            PairingRequest::RevokeDevice { device_id } => {
                context.identity.require_owner("Pairing::RevokeDevice")?;
                let Some(device_key) = self.state.identity.revoke_device_with_key(&device_id)
                else {
                    return Err(super::super::error::not_found(format!(
                        "没有设备 `{device_id}`"
                    )));
                };
                for session in self.state.sessions() {
                    session.evict_device(&device_key);
                }
                Ok(PairingResponse::Revoked)
            }
            PairingRequest::CreateInvite { .. } | PairingRequest::Redeem { .. } => {
                context.identity.require_owner("Pairing::CreateInvite/Redeem")?;
                Err(not_wired(
            "配对挑战-应答与令牌落盘随 v4.7 接线；本切片不发放假邀请码",
                ))
            }
        }
    }
}

#[cfg(test)]
mod smoke {
    use super::*;
    use crate::v3_server::facade::test_support;
    use crate::v3_server::identity::Scope;

    #[tokio::test]
    async fn device_list_and_revoke_are_real() {
        let rig = test_support::rig();
        let context = test_support::owner_context(&rig);
        rig.state
            .identity
            .register_device("tablet", "平板", Scope::Device);
        let listed = rig
            .backend
            .config_account
            .handle_pairing(&context, PairingRequest::ListDevices)
            .await
            .expect("列出设备");
        let devices = match listed {
            PairingResponse::Devices(devices) => devices,
            other => panic!("形状不符：{other:?}"),
        };
        let tablet = devices
            .iter()
            .find(|device| device.label == "平板")
            .expect("平板必须在列表里");
        let revoked = rig
            .backend
            .config_account
            .handle_pairing(
                &context,
                PairingRequest::RevokeDevice {
                    device_id: tablet.device_id.clone(),
                },
            )
            .await
            .expect("吊销");
        assert!(matches!(revoked, PairingResponse::Revoked));
        assert!(rig.state.identity.is_revoked("tablet"));
    }

    #[tokio::test]
    async fn device_scope_cannot_revoke_or_list_devices() {
        let rig = test_support::rig();
        let device = test_support::device_context(&rig);
        let error = rig
            .backend
            .config_account
            .handle_pairing(&device, PairingRequest::ListDevices)
            .await
            .expect_err("已配对设备不得枚举设备表");
        assert_eq!(error.code, qaqh_ringing_v3::ErrorCode::PermissionDenied);
    }

    #[tokio::test]
    async fn invite_is_refused_instead_of_faked() {
        let rig = test_support::rig();
        let context = test_support::owner_context(&rig);
        let error = rig
            .backend
            .config_account
            .handle_pairing(&context, PairingRequest::CreateInvite { ttl_ms: 60_000 })
            .await
            .expect_err("骨架期不得发放假邀请码");
        assert_eq!(error.code, qaqh_ringing_v3::ErrorCode::Unavailable);
    }
}
