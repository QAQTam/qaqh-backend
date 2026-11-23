//! 设备注册表（移动端 M0，spec-daemon-auth-devices §5）。
//!
//! v4.4 P1-3（MUST 铁闸 2）：状态权威是 `qaqh.db` 的 `paired_devices` 表。
//! 旧的 `<data_dir>/ringing-devices.json` 全量覆写与「损坏即空启动」(L-20)
//! 一并物理删除；启动吸入只发生在版本位 11→12 的那一个事务里
//! （`qaqh-session/src/db/sidecar.rs`）。
//!
//! **只存 `device_token` 的 SHA256 摘要，绝不含明文**——token 明文仅在 `/pair`
//! 响应中出现一次，丢失即重新扫码。daemon 重启不得失效 device_token（与 admin
//! token 每次轮换的行为显式区分）。
//!
//! 方法全 async：调用点在 axum 中间件/handler（Tokio worker），DB 等待不得
//! 占住 worker。内存 `Mutex<DeviceRegistry>` 随文件权威一起退场——并发裁决
//! 收敛到 SQLite 串行写执行器。

use std::sync::Arc;

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

    fn as_db_str(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Interact => "interact",
            Self::Admin => "admin",
        }
    }

    fn from_db_str(raw: &str) -> Result<Self, String> {
        match raw {
            "view" => Ok(Self::View),
            "interact" => Ok(Self::Interact),
            "admin" => Ok(Self::Admin),
            other => Err(format!("corrupt device scope {other:?}")),
        }
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

/// touch 节流窗口：距上次命中不足 60s 不产生写（与旧内存实现同律）。
const TOUCH_MIN_DELTA_MS: i64 = 60_000;

/// 设备注册表（`paired_devices` 门面）。
pub struct DeviceRegistry {
    db: Arc<qaqh_session::db::Db>,
}

// `Db` 不暴露 Debug(执行器句柄无诊断价值),门面只印存在性。
impl std::fmt::Debug for DeviceRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceRegistry").finish_non_exhaustive()
    }
}

fn to_record(row: qaqh_session::db::devices::DeviceRow) -> Result<DeviceRecord, String> {
    Ok(DeviceRecord {
        device_id: row.device_id,
        name: row.name,
        platform: row.platform,
        scope: Scope::from_db_str(&row.scope)?,
        token_digest: row.token_digest,
        created_at_ms: row.created_at_ms.max(0) as u64,
        last_seen_ms: row.last_seen_ms.max(0) as u64,
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn new_device_id() -> String {
    qaqh_ringing_contract::ulid::generate_ulid()
}

/// 设备 token 明文：两个 ULID 拼接（UUIDv7 载荷），约 148 bit 随机熵。
fn new_device_token() -> String {
    format!(
        "{}{}",
        qaqh_ringing_contract::ulid::generate_ulid(),
        qaqh_ringing_contract::ulid::generate_ulid()
    )
}

pub fn token_digest(token: &str) -> String {
    qaqh_types::sha256_hex(token.as_bytes())
}

impl DeviceRegistry {
    pub fn new(db: Arc<qaqh_session::db::Db>) -> Self {
        Self { db }
    }

    /// 签发新设备：返回 `(device_id, device_token 明文 /*仅此一次*/)`。
    /// 落库失败即 `Err`——不得静默返回一个没进权威表的设备。
    pub async fn issue(
        &self,
        name: &str,
        platform: &str,
        scope: Scope,
    ) -> Result<(String, String), String> {
        let device_id = new_device_id();
        let token = new_device_token();
        let now = now_ms();
        let row = qaqh_session::db::devices::DeviceRow {
            device_id: device_id.clone(),
            name: name.to_string(),
            platform: platform.to_string(),
            scope: scope.as_db_str().to_string(),
            token_digest: token_digest(&token),
            created_at_ms: now as i64,
            last_seen_ms: now as i64,
        };
        self.db
            .write(move |conn| qaqh_session::db::devices::insert(conn, &row))
            .await
            .map_err(|error| format!("device issue persist: {error}"))?;
        Ok((device_id, token))
    }

    /// 鉴权热路径：由 `SHA256(token)` 摘要反查设备。
    /// DB/数据损坏返回 `Err`——鉴权侧必须 fail-closed(401)，不得当作
    /// 「无此设备」继续走后续逻辑，更不得回退旧文件。
    pub async fn lookup_by_digest(&self, digest: &str) -> Result<Option<DeviceRecord>, String> {
        let digest = digest.to_string();
        let row = self
            .db
            .read(move |conn| qaqh_session::db::devices::get_by_digest(conn, &digest))
            .await
            .map_err(|error| format!("device lookup: {error}"))?;
        row.map(to_record).transpose()
    }

    pub async fn get(&self, device_id: &str) -> Result<Option<DeviceRecord>, String> {
        let device_id = device_id.to_string();
        let row = self
            .db
            .read(move |conn| qaqh_session::db::devices::get_by_id(conn, &device_id))
            .await
            .map_err(|error| format!("device get: {error}"))?;
        row.map(to_record).transpose()
    }

    /// 吊销：删注册表项。返回被吊销设备的 `device_id`（存在时），供 lease 联动失效。
    pub async fn revoke(&self, device_id: &str) -> Result<Option<String>, String> {
        let device_id = device_id.to_string();
        let removed = self
            .db
            .write(move |conn| qaqh_session::db::devices::delete(conn, &device_id))
            .await
            .map_err(|error| format!("device revoke: {error}"))?;
        Ok(removed.map(|row| row.device_id))
    }

    /// 惰性更新 `last_seen_ms`（鉴权命中时调用）。60s 节流窗口内不产生写。
    pub async fn touch(&self, device_id: &str) {
        let device_id = device_id.to_string();
        let result = self
            .db
            .write(move |conn| {
                qaqh_session::db::devices::touch(conn, &device_id, now_ms() as i64, TOUCH_MIN_DELTA_MS)
            })
            .await;
        if let Err(error) = result {
            // last_seen 是展示字段：写失败不反转鉴权结果，但必须留痕。
            log::warn!("[ringing] device touch failed: {error}");
        }
    }

    pub async fn list(&self) -> Result<Vec<DeviceRecord>, String> {
        let rows = self
            .db
            .read(|conn| qaqh_session::db::devices::list(conn))
            .await
            .map_err(|error| format!("device list: {error}"))?;
        rows.into_iter().map(to_record).collect()
    }

    pub async fn len(&self) -> Result<usize, String> {
        Ok(self.list().await?.len())
    }

    pub async fn is_empty(&self) -> Result<bool, String> {
        Ok(self.len().await? == 0)
    }
}
