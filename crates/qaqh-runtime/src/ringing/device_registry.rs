//! 设备注册表（移动端 M0，spec-daemon-auth-devices §5）。
//!
//! 持久化于 `<data_dir>/ringing-devices.json`，与 [`RingingDriverWatch`] 同范式：
//! JSON、`.json.tmp` + rename 原子写、损坏即空启动。
//!
//! **只存 `device_token` 的 SHA256 摘要，绝不含明文**——token 明文仅在 `/pair`
//! 响应中出现一次，丢失即重新扫码。daemon 重启不得失效 device_token（与 admin
//! token 每次轮换的行为显式区分）。
//!
//! [`RingingDriverWatch`]: crate::ringing::driver_watch::RingingDriverWatch

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 设备授权档位。`Ord` 由声明序给出（`View < Interact < Admin`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    View,
    Interact,
    Admin,
}

impl Scope {
    pub fn at_least(self, min: Scope) -> bool {
        self >= min
    }
}

/// 单个已配对设备的注册项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    /// pair 时服务端生成（ULID）。身份由 token 反推，此 id 用于 lease 绑定与归因。
    pub device_id: String,
    /// 仅展示 + 归因，**非信任**字段。
    pub name: String,
    /// `"android"` | `"harmonyos"` | ...
    pub platform: String,
    /// 实际授予档位（可与请求不同，桌面端可降档）。
    pub scope: Scope,
    /// `SHA256(device_token)` 的十六进制摘要；绝不含明文。
    pub token_digest: String,
    pub created_at_ms: u64,
    /// 每次鉴权命中时惰性更新，供设备管理 UI。
    pub last_seen_ms: u64,
}

/// 设备注册表。`by_digest` 做鉴权热路径的 O(1) 反查。
#[derive(Debug, Default)]
pub struct DeviceRegistry {
    by_digest: HashMap<String, String>,
    by_id: HashMap<String, DeviceRecord>,
    persistence_path: Option<PathBuf>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn new_device_id() -> String {
    qaqh_session::canonical::generate_ulid()
}

/// 设备 token 明文：两个 ULID 拼接（UUIDv7 载荷），约 148 bit 随机熵。
fn new_device_token() -> String {
    format!(
        "{}{}",
        qaqh_session::canonical::generate_ulid(),
        qaqh_session::canonical::generate_ulid()
    )
}

pub fn token_digest(token: &str) -> String {
    qaqh_types::sha256_hex(token.as_bytes())
}

impl DeviceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 从 `<data_dir>/ringing-devices.json` 载入。文件缺失或损坏即空启动。
    pub fn new_persistent() -> Self {
        let path = qaqh_types::platform::data_dir().join("ringing-devices.json");
        let mut registry = Self {
            persistence_path: Some(path.clone()),
            ..Self::new()
        };
        registry.reload(&path);
        registry
    }

    fn reload(&mut self, path: &Path) {
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        match serde_json::from_slice::<Vec<DeviceRecord>>(&bytes) {
            Ok(records) => {
                for record in records {
                    self.by_digest
                        .insert(record.token_digest.clone(), record.device_id.clone());
                    self.by_id.insert(record.device_id.clone(), record);
                }
            }
            Err(_) => log::warn!(
                "[ringing] device registry is unreadable; starting empty ({})",
                path.display()
            ),
        }
    }

    fn persist(&self) {
        let Some(path) = &self.persistence_path else {
            return;
        };
        let mut records: Vec<&DeviceRecord> = self.by_id.values().collect();
        records.sort_by(|a, b| a.device_id.cmp(&b.device_id));
        let Ok(bytes) = serde_json::to_vec(&records) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// 签发新设备：返回 `(device_id, device_token 明文 /*仅此一次*/)`。
    pub fn issue(&mut self, name: &str, platform: &str, scope: Scope) -> (String, String) {
        let device_id = new_device_id();
        let token = new_device_token();
        let now = now_ms();
        let record = DeviceRecord {
            device_id: device_id.clone(),
            name: name.to_string(),
            platform: platform.to_string(),
            scope,
            token_digest: token_digest(&token),
            created_at_ms: now,
            last_seen_ms: now,
        };
        self.by_digest
            .insert(record.token_digest.clone(), record.device_id.clone());
        self.by_id.insert(device_id.clone(), record);
        self.persist();
        (device_id, token)
    }

    /// 鉴权热路径：由 `SHA256(token)` 摘要反查设备。
    pub fn lookup_by_digest(&self, digest: &str) -> Option<&DeviceRecord> {
        self.by_digest
            .get(digest)
            .and_then(|device_id| self.by_id.get(device_id))
    }

    pub fn get(&self, device_id: &str) -> Option<&DeviceRecord> {
        self.by_id.get(device_id)
    }

    /// 吊销：删注册表项。返回被吊销设备的 `device_id`（存在时），供 lease 联动失效。
    pub fn revoke(&mut self, device_id: &str) -> Option<String> {
        let record = self.by_id.remove(device_id)?;
        self.by_digest.remove(&record.token_digest);
        self.persist();
        Some(record.device_id)
    }

    /// 惰性更新 `last_seen_ms`（鉴权命中时调用）。仅在明显变旧时落盘以省 IO。
    pub fn touch(&mut self, device_id: &str) {
        let Some(record) = self.by_id.get_mut(device_id) else {
            return;
        };
        let now = now_ms();
        if now.saturating_sub(record.last_seen_ms) < 60_000 {
            record.last_seen_ms = now;
            return;
        }
        record.last_seen_ms = now;
        self.persist();
    }

    pub fn list(&self) -> Vec<DeviceRecord> {
        let mut records: Vec<DeviceRecord> = self.by_id.values().cloned().collect();
        records.sort_by(|a, b| a.created_at_ms.cmp(&b.created_at_ms));
        records
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}
