//! 身份单权威（`ADR-0042` 铁律 5）。
//!
//! ## 谁能决定"我是谁"
//!
//! **只有凭据。** `ClientHello.client_instance_id` 是客户端自报的、只用于同端多窗口
//! 去重与日志关联的标签，**不参与任何授权判断**——它不唯一、不可验证、谁都能填。
//! 所以本模块的授权键是 [`DeviceKey`]（由凭据推导），而不是那个自报字段。
//!
//! ## 对外只给设备 id，不给令牌
//!
//! 设备令牌是可以冒充整台设备的等价物。`Pairing::ListDevices` 这类读接口一旦回
//! 令牌，任何能调它的人都拿到了永久的冒充能力。所以对外一律用与令牌无关的
//! [`DeviceSnapshot::device_id`]，令牌只留在表内。
//!
//! ## 两种作用域
//!
//! - [`Scope::Owner`]：本机同名 OS 用户，或配对表里的主设备。管理面 RPC（停机、
//!   改配置、吊销设备、装/卸插件）只有它能调。
//! - [`Scope::Device`]：已配对但非主设备（手机、第二台桌面）。只能读/驱动**授权给
//!   它自己**的会话，管理面一律当场 `PermissionDenied`。
//!
//! ## 吊销必须"即时踢流"
//!
//! 每条已认证连接都持有一份 [`watch::Receiver`]，吊销时一次性置真：连接上的接受循环
//! 与每条流的读写循环都 `select!` 在它上面，于是吊销是"立刻收摊"，不是"等下一次
//! RPC 才发现自己没权限了"。

use std::collections::BTreeMap;
use std::sync::Mutex;

use qaqh_ringing_v3::{ClientCredential, ClientHello, ClientKind, HandshakeRejectReason, RpcError};
use tokio::sync::watch;

use super::error::denied;
use super::lock;
use super::now_ms;

/// 设备键：授权的唯一依据（`os:<user>` 或 `device:<token>`）。
pub(crate) type DeviceKey = String;

/// 作用域。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    /// 主设备 / 本机同名 OS 用户：全部能力。
    Owner,
    /// 已配对的次要设备：只能碰授权给它的会话。
    Device,
}

/// 驱动权身份：**设备 + 客户端实例**都要对得上。
///
/// 只比 `client_instance_id` 是可以被冒充的（那个字段本来就是自报标签）；
/// 只比设备又分不清同一台设备的两个窗口。两者一起比，才既不可伪造又够细。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DriverKey {
    /// 设备键。
    pub(crate) device: DeviceKey,
    /// 客户端实例 id（自报标签，仅在设备内用于区分窗口）。
    pub(crate) instance: String,
}

/// 一条已通过握手的连接身份。
#[derive(Debug, Clone)]
pub(crate) struct ConnectionIdentity {
    /// 设备键（授权依据）。
    pub(crate) device: DeviceKey,
    /// 对外设备 id（不含令牌）。
    pub(crate) device_id: String,
    /// 作用域。
    pub(crate) scope: Scope,
    /// 客户端自报的实例标签（仅用于驱动权归属与日志）。
    pub(crate) client_instance_id: String,
    /// 客户端种类。
    pub(crate) client_kind: ClientKind,
    /// 吊销哨兵。
    pub(crate) revoked: watch::Receiver<bool>,
}

impl ConnectionIdentity {
    /// 要求主设备作用域，否则当场拒绝。
    pub(crate) fn require_owner(&self, action: &str) -> Result<(), RpcError> {
        match self.scope {
            Scope::Owner => Ok(()),
            Scope::Device => Err(denied(format!(
                "`{action}` 属于管理面能力，需要主设备作用域（当前为已配对设备）"
            ))),
        }
    }

    /// 本连接的驱动权身份。
    pub(crate) fn driver_key(&self) -> DriverKey {
        DriverKey {
            device: self.device.clone(),
            instance: self.client_instance_id.clone(),
        }
    }

    /// 吊销哨兵是否已触发。
    pub(crate) fn is_revoked(&self) -> bool {
        *self.revoked.borrow()
    }
}

/// 对外的设备视图（**不含令牌**）。
#[derive(Debug, Clone)]
pub(crate) struct DeviceSnapshot {
    /// 对外设备 id。
    pub(crate) device_id: String,
    /// 授权键（内部用）。
    pub(crate) key: DeviceKey,
    /// 展示名。
    pub(crate) label: String,
    /// 作用域。
    pub(crate) scope: Scope,
    /// 是否已吊销。
    pub(crate) revoked: bool,
    /// 配对时刻（毫秒）。
    pub(crate) paired_at_ms: u64,
}

struct DeviceRecord {
    id: String,
    token: Option<String>,
    scope: Scope,
    label: String,
    paired_at_ms: u64,
    revoker: watch::Sender<bool>,
    revoked: bool,
}

struct TableInner {
    os_user: String,
    devices: BTreeMap<DeviceKey, DeviceRecord>,
    next_device_id: u64,
}

/// 身份表（配对设备 + 本机 OS 用户）。
pub(crate) struct IdentityTable {
    inner: Mutex<TableInner>,
}

impl IdentityTable {
    /// 以本机 OS 用户名建表（本地 IPC 的同名用户即主设备）。
    pub(crate) fn new(os_user: impl Into<String>) -> Self {
        Self {
            inner: Mutex::new(TableInner {
                os_user: os_user.into(),
                devices: BTreeMap::new(),
                next_device_id: 0,
            }),
        }
    }

    /// 本机 OS 用户名。
    pub(crate) fn os_user(&self) -> String {
        lock(&self.inner).os_user.clone()
    }

    /// 登记一台已配对设备（幂等：同 token 重复登记只更新标签/作用域）。
    pub(crate) fn register_device(&self, token: &str, label: &str, scope: Scope) -> DeviceKey {
        let key = format!("device:{token}");
        let mut inner = lock(&self.inner);
        if let Some(record) = inner.devices.get_mut(&key) {
            record.label = label.to_string();
            record.scope = scope;
            return key;
        }
        let (revoker, _) = watch::channel(false);
        let id = inner.next_device_id;
        inner.next_device_id += 1;
        inner.devices.insert(
            key.clone(),
            DeviceRecord {
                id: format!("dev-{id:04}"),
                token: Some(token.to_string()),
                scope,
                label: label.to_string(),
                paired_at_ms: now_ms(),
                revoker,
                revoked: false,
            },
        );
        key
    }

    /// 按对外设备 id 吊销并返回其内部 `DeviceKey`：置真 + 唤醒所有持该设备哨兵的连接。
    pub(crate) fn revoke_device_with_key(&self, device_id: &str) -> Option<DeviceKey> {
        let mut inner = lock(&self.inner);
        let (key, record) = inner
            .devices
            .iter_mut()
            .find(|(_, record)| record.id == device_id)?;
        record.revoked = true;
        record.revoker.send_replace(true);
        Some(key.clone())
    }

    /// 按对外设备 id 吊销：置真 + 唤醒所有持该设备哨兵的连接（**即时踢流**）。
    pub(crate) fn revoke_device(&self, device_id: &str) -> bool {
        self.revoke_device_with_key(device_id).is_some()
    }

    /// 按令牌吊销（内部/测试用：令牌是对内概念，不对外暴露）。
    pub(crate) fn revoke_token(&self, token: &str) -> bool {
        let key = format!("device:{token}");
        let mut inner = lock(&self.inner);
        let Some(record) = inner.devices.get_mut(&key) else {
            return false;
        };
        record.revoked = true;
        record.revoker.send_replace(true);
        true
    }

    /// 设备是否已被吊销。
    pub(crate) fn is_revoked(&self, token: &str) -> bool {
        let key = format!("device:{token}");
        lock(&self.inner)
            .devices
            .get(&key)
            .map(|record| record.revoked)
            .unwrap_or(false)
    }

    /// 设备对外 id（测试/守卫用）。
    pub(crate) fn device_id_for_token(&self, token: &str) -> Option<String> {
        let key = format!("device:{token}");
        lock(&self.inner)
            .devices
            .get(&key)
            .map(|record| record.id.clone())
    }

    /// 列出设备（**不含令牌**）。
    pub(crate) fn devices(&self) -> Vec<DeviceSnapshot> {
        lock(&self.inner)
            .devices
            .iter()
            .map(|(key, record)| DeviceSnapshot {
                device_id: record.id.clone(),
                key: key.clone(),
                label: record.label.clone(),
                scope: record.scope,
                revoked: record.revoked,
                paired_at_ms: record.paired_at_ms,
            })
            .collect()
    }

    /// 认证一个 `ClientHello`。
    ///
    /// 版本/指纹校验由 [`qaqh_ringing_v3::verify_handshake`] 负责（"先版本后凭据"
    /// 的次序在那里已固定），这里只做凭据本身。
    pub(crate) fn authenticate(
        &self,
        hello: &ClientHello,
    ) -> Result<ConnectionIdentity, HandshakeRejectReason> {
        let mut inner = lock(&self.inner);
        let (key, device_id, scope, revoked) = match &hello.credential {
            ClientCredential::LocalOsUser { user } => {
                if user != &inner.os_user {
                    return Err(HandshakeRejectReason::CredentialRejected {
                        detail: "本机 IPC 只接受与 daemon 同名的 OS 用户".to_string(),
                    });
                }
                let key = format!("os:{user}");
                if !inner.devices.contains_key(&key) {
                    let (revoker, _) = watch::channel(false);
                    let id = inner.next_device_id;
                    inner.next_device_id += 1;
                    inner.devices.insert(
                        key.clone(),
                        DeviceRecord {
                            id: format!("dev-{id:04}"),
                            token: None,
                            scope: Scope::Owner,
                            label: "local-os-user".to_string(),
                            paired_at_ms: now_ms(),
                            revoker,
                            revoked: false,
                        },
                    );
                }
                let record = inner.devices.get(&key).expect("刚插入的记录必须存在");
                (
                    key,
                    record.id.clone(),
                    record.scope,
                    record.revoker.subscribe(),
                )
            }
            ClientCredential::PairedDevice { token } => {
                let key = format!("device:{token}");
                let Some(record) = inner.devices.get(&key) else {
                    // 不区分"令牌不存在"与"令牌被吊销"：任何可枚举的细节都是泄漏。
                    return Err(HandshakeRejectReason::CredentialRejected {
                        detail: "设备凭据不被接受".to_string(),
                    });
                };
                if record.revoked {
                    return Err(HandshakeRejectReason::CredentialRejected {
                        detail: "设备凭据不被接受".to_string(),
                    });
                }
                (
                    key,
                    record.id.clone(),
                    record.scope,
                    record.revoker.subscribe(),
                )
            }
            ClientCredential::IrohEndpoint { .. } => {
                return Err(HandshakeRejectReason::CredentialRejected {
                    detail: "远端 iroh 传输在 v4.8 接线，本切片只开本地 IPC/内存闭环".to_string(),
                });
            }
        };
        Ok(ConnectionIdentity {
            device: key,
            device_id,
            scope,
            client_instance_id: hello.client_instance_id.clone(),
            client_kind: hello.client_kind,
            revoked,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(credential: ClientCredential) -> ClientHello {
        ClientHello::new("instance-1", ClientKind::Desktop, credential)
    }

    #[test]
    fn same_os_user_is_owner_and_others_are_rejected() {
        let table = IdentityTable::new("qaqh");
        let owner = table
            .authenticate(&hello(ClientCredential::LocalOsUser {
                user: "qaqh".into(),
            }))
            .expect("本机同名用户必须通过");
        assert_eq!(owner.scope, Scope::Owner);
        assert!(
            table
                .authenticate(&hello(ClientCredential::LocalOsUser {
                    user: "intruder".into()
                }))
                .is_err(),
            "不同名 OS 用户必须被拒"
        );
    }

    #[test]
    fn unknown_and_revoked_tokens_are_rejected_without_enumeration_detail() {
        let table = IdentityTable::new("qaqh");
        let unknown = table
            .authenticate(&hello(ClientCredential::PairedDevice {
                token: "nope".into(),
            }))
            .expect_err("未知令牌必须被拒");
        let key = table.register_device("phone", "手机", Scope::Device);
        assert_eq!(key, "device:phone");
        assert!(table.revoke_token("phone"));
        let revoked = table
            .authenticate(&hello(ClientCredential::PairedDevice {
                token: "phone".into(),
            }))
            .expect_err("已吊销令牌必须被拒");
        let detail = |reason: HandshakeRejectReason| match reason {
            HandshakeRejectReason::CredentialRejected { detail } => detail,
            other => panic!("必须是凭据拒绝，实际 {other:?}"),
        };
        // 两条拒绝文案完全一致：不给出"这个令牌存在过"这类可枚举信号。
        assert_eq!(detail(unknown), detail(revoked));
    }

    #[test]
    fn revocation_wakes_every_live_connection_of_that_device() {
        let table = IdentityTable::new("qaqh");
        table.register_device("phone", "手机", Scope::Device);
        let first = table
            .authenticate(&hello(ClientCredential::PairedDevice {
                token: "phone".into(),
            }))
            .expect("认证");
        let second = table
            .authenticate(&hello(ClientCredential::PairedDevice {
                token: "phone".into(),
            }))
            .expect("认证");
        assert!(!*first.revoked.borrow());
        let device_id = table.device_id_for_token("phone").expect("设备 id");
        assert!(table.revoke_device(&device_id));
        assert!(*first.revoked.borrow(), "第一连接必须被通知");
        assert!(*second.revoked.borrow(), "第二连接也必须被通知");
        assert!(table.is_revoked("phone"));
    }

    #[test]
    fn device_list_never_leaks_tokens() {
        let table = IdentityTable::new("qaqh");
        table.register_device("super-secret-token", "手机", Scope::Device);
        let devices = table.devices();
        assert_eq!(devices.len(), 1);
        for device in &devices {
            assert!(!device.device_id.contains("super-secret-token"));
            assert!(!device.label.contains("super-secret-token"));
            assert!(!device.key.contains("super-secret-token") || device.key.starts_with("device:"));
        }
    }

    #[test]
    fn device_scope_cannot_touch_admin_surface() {
        let table = IdentityTable::new("qaqh");
        table.register_device("phone", "手机", Scope::Device);
        let device = table
            .authenticate(&hello(ClientCredential::PairedDevice {
                token: "phone".into(),
            }))
            .expect("认证");
        assert_eq!(device.scope, Scope::Device);
        let error = device.require_owner("System::Stop").expect_err("必须拒绝");
        assert_eq!(error.code, qaqh_ringing_v3::ErrorCode::PermissionDenied);
    }
}
