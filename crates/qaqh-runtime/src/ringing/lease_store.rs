//! Ringing client connection & ownership store.
//!
//! §2/§3/§4（2026-10-23 多端通信审查）后的职责划分：
//!
//! - **连接（lease）= 在线 presence**：短 TTL、renew 续期、过期 GC。只服务
//!   在线提示、连接回收与吊销断流（SSE 逐事件复查 `is_active_session`），
//!   **不再是读写授权条件**。
//! - **会话归属（attach）挂在稳定身份名下**：device 身份的 `instance_key`
//!   即 `device_id`，admin 为 open 时自报的 `client_instance_id`。归属独立于
//!   任何单条连接的寿命——重新 open（换 `client_session_id`）、第二/第三个
//!   窗口、短时离线都不丢授权；重连后凭原身份直接恢复读写，无需重放
//!   attach。显式设备吊销仍立即摘除该身份下全部连接与归属。
//! - 归属无淘汰（跟随设备生命周期）；容量治理留给后续批次。

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const RENEW_TTL_MS: u64 = 30_000;

fn lease_ttl_ms() -> u64 {
    std::env::var("QAQH_TEST_LEASE_TTL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(RENEW_TTL_MS)
}

/// Ringing 客户端连接与会话归属。
///
/// 键说明：`client_session_id`（服务端签发、header 携带）标识一条连接；
/// `instance_key` 是稳定身份（device token 反推的 `device_id`，admin 为
/// 自报 id）。归属只按 `instance_key` 记账，连接随时可换。
#[derive(Debug, Default)]
pub struct RingingLeaseStore {
    /// `client_session_id` → 连接 presence。同身份多连接并存（§3）。
    connections: HashMap<String, Connection>,
    /// `client_session_id` → `instance_key` 索引。
    instance_of: HashMap<String, String>,
    /// `instance_key` → 会话归属（§2/§4：授权主体是稳定身份）。
    ownership: HashMap<String, HashSet<String>>,
}

#[derive(Debug, Clone)]
struct Connection {
    expiry: Instant,
}

impl RingingLeaseStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一条新连接。同一身份的既有连接**不受影响**（§3：多窗口互不失效），
    /// 各连接独立按 TTL 过期。open 是低频协商路径，顺带做一次过期 GC。
    pub fn open(&mut self, client_session_id: String, instance_key: String) {
        self.expire_connections();
        self.instance_of
            .insert(client_session_id.clone(), instance_key.clone());
        self.connections.insert(
            client_session_id,
            Connection {
                expiry: Instant::now() + Duration::from_millis(lease_ttl_ms()),
            },
        );
    }

    /// attach 建立会话归属——**记在身份名下**，不随连接过期/更换失效（§4：
    /// attach 只表达"关注哪些会话"，授权由身份持有）。要求连接本身存活
    /// （客户端 open 之后的正常动作）；空 session 拒绝。
    pub fn attach_session(&mut self, client_session_id: &str, session_id: &str) -> bool {
        if !self.is_active_session(client_session_id) || session_id.is_empty() {
            return false;
        }
        let Some(instance) = self.instance_of.get(client_session_id).cloned() else {
            return false;
        };
        self.ownership
            .entry(instance)
            .or_default()
            .insert(session_id.to_string());
        true
    }

    /// 摘除该连接所属身份对某会话的归属（session close/delete 路径）。
    pub fn detach_session(&mut self, client_session_id: &str, session_id: &str) {
        let Some(instance) = self.instance_of.get(client_session_id).cloned() else {
            return;
        };
        if let Some(sessions) = self.ownership.get_mut(&instance) {
            sessions.remove(session_id);
            if sessions.is_empty() {
                self.ownership.remove(&instance);
            }
        }
    }

    /// 该连接所属身份的会话归属快照（SSE 回放过滤用；BUG-2026-09-12-12）。
    ///
    /// 调用方在一次短临界区内取快照后锁外过滤，与事件数解耦。不要求连接
    /// 活跃——presence 由调用方另行判定（吊销断流路径）。
    pub fn owned_sessions(&self, client_session_id: &str) -> HashSet<String> {
        self.instance_of
            .get(client_session_id)
            .and_then(|instance| self.ownership.get(instance))
            .cloned()
            .unwrap_or_default()
    }

    /// 归属判定（§2）：**只查身份归属，不再要求连接活跃**——授权独立于连接
    /// 寿命。调用入口（`require_lease_on_session` 等）自行做 presence 检查。
    /// 连接完全未知（从未 open 或已被 GC）时无从解析身份，返回 false。
    pub fn owns_session(&self, client_session_id: &str, session_id: &str) -> bool {
        self.instance_of
            .get(client_session_id)
            .is_some_and(|instance| {
                self.ownership
                    .get(instance)
                    .is_some_and(|sessions| sessions.contains(session_id))
            })
    }

    /// 续租（按 `client_session_id` 反查）；过期/未知连接返回 false。
    /// 过期即摘除连接与索引——但**保留归属**（§2：重开连接后授权仍在）。
    pub fn renew(&mut self, client_session_id: &str) -> bool {
        let alive = matches!(
            self.connections.get(client_session_id),
            Some(connection) if connection.expiry >= Instant::now()
        );
        if !alive {
            // 索引悬挂（理论上由 open/expire 维护消除）：按未知处理并清理。
            self.connections.remove(client_session_id);
            self.instance_of.remove(client_session_id);
            return false;
        }
        if let Some(connection) = self.connections.get_mut(client_session_id) {
            connection.expiry = Instant::now() + Duration::from_millis(lease_ttl_ms());
        }
        true
    }

    /// 过期连接 GC（open 是低频协商路径，承担清理）。归属不受影响。
    fn expire_connections(&mut self) {
        let now = Instant::now();
        let expired: Vec<String> = self
            .connections
            .iter()
            .filter(|(_, connection)| connection.expiry < now)
            .map(|(cs, _)| cs.clone())
            .collect();
        for cs in expired {
            self.connections.remove(&cs);
            self.instance_of.remove(&cs);
        }
    }

    /// 活跃校验（按 `instance_key`；该身份下任一连接存活即为活跃）。
    pub fn is_active(&self, instance_key: &str) -> bool {
        self.instance_of
            .iter()
            .filter(|(_, instance)| instance.as_str() == instance_key)
            .any(|(cs, _)| {
                self.connections
                    .get(cs)
                    .is_some_and(|connection| connection.expiry >= Instant::now())
            })
    }

    pub fn is_active_session(&self, client_session_id: &str) -> bool {
        self.connections
            .get(client_session_id)
            .is_some_and(|connection| connection.expiry >= Instant::now())
    }

    pub fn instance_for_session(&self, client_session_id: &str) -> Option<String> {
        if !self.is_active_session(client_session_id) {
            return None;
        }
        self.instance_of.get(client_session_id).cloned()
    }

    /// Test helper: force expiry for a given instance id.
    pub fn set_expiry_for_test(&mut self, client_instance_id: &str, expiry: Instant) {
        for (cs, instance) in self.instance_of.iter() {
            if instance == client_instance_id
                && let Some(connection) = self.connections.get_mut(cs)
            {
                connection.expiry = expiry;
            }
        }
    }

    /// 吊销设备：摘除其名下**全部**连接与归属（§3：吊销统一关闭其下所有
    /// 连接）。显式切断在途 SSE 与后续请求，不依赖短 TTL 自愈留窗口。
    pub fn revoke_device(&mut self, device_id: &str) -> bool {
        let mine: Vec<String> = self
            .instance_of
            .iter()
            .filter(|(_, instance)| instance.as_str() == device_id)
            .map(|(cs, _)| cs.clone())
            .collect();
        if mine.is_empty() && !self.ownership.contains_key(device_id) {
            return false;
        }
        for cs in &mine {
            self.connections.remove(cs);
            self.instance_of.remove(cs);
        }
        self.ownership.remove(device_id);
        !mine.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn lease_lifecycle_ttl_renew_expiry() {
        let mut store = RingingLeaseStore::new();
        store.open("cs-1".into(), "ci-1".into());
        assert!(store.is_active("ci-1"));
        assert!(store.is_active_session("cs-1"));
        assert!(!store.is_active_session("unknown"));
        assert!(store.renew("cs-1"));
        store.set_expiry_for_test("ci-1", Instant::now() - Duration::from_secs(1));
        assert!(!store.is_active_session("cs-1"));
        assert!(!store.renew("cs-1"));
    }

    #[test]
    fn same_identity_keeps_multiple_connections_and_ownership() {
        let mut store = RingingLeaseStore::new();
        store.open("cs-1".into(), "device-1".into());
        assert!(store.attach_session("cs-1", "seed-a"));
        // §3：同身份第二连接（第二窗口）不失效第一连接，也不清归属。
        store.open("cs-2".into(), "device-1".into());
        assert!(store.is_active_session("cs-1"));
        assert!(store.is_active_session("cs-2"));
        // §4：归属在身份名下——两条连接都能读到同一归属。
        assert!(store.owns_session("cs-1", "seed-a"));
        assert!(store.owns_session("cs-2", "seed-a"));
        assert_eq!(store.owned_sessions("cs-2"), store.owned_sessions("cs-1"));
    }

    #[test]
    fn reconnect_restores_access_without_reattach() {
        let mut store = RingingLeaseStore::new();
        store.open("cs-1".into(), "device-1".into());
        assert!(store.attach_session("cs-1", "seed-a"));

        // §2：连接过期（移动端后台挂起）只摘 presence，归属保留。
        store.set_expiry_for_test("device-1", Instant::now() - Duration::from_secs(1));
        assert!(!store.is_active_session("cs-1"));
        assert!(!store.renew("cs-1"));

        // 重开连接：无需重放 attach，读写授权立即恢复。
        store.open("cs-2".into(), "device-1".into());
        assert!(store.owns_session("cs-2", "seed-a"));
    }

    #[test]
    fn revoke_device_drops_all_connections_and_ownership() {
        let mut store = RingingLeaseStore::new();
        store.open("cs-1".into(), "device-1".into());
        store.open("cs-2".into(), "device-1".into());
        store.open("cs-other".into(), "device-2".into());
        assert!(store.attach_session("cs-1", "seed-a"));
        assert!(store.attach_session("cs-2", "seed-b"));
        assert!(store.attach_session("cs-other", "seed-a"));

        assert!(store.revoke_device("device-1"));
        assert!(!store.is_active_session("cs-1"));
        assert!(!store.is_active_session("cs-2"));
        assert!(!store.owns_session("cs-1", "seed-a"));
        // 未吊销设备不受影响。
        assert!(store.is_active_session("cs-other"));
        assert!(store.owns_session("cs-other", "seed-a"));
        // 幂等。
        assert!(!store.revoke_device("device-1"));
    }

    #[test]
    fn attach_requires_a_live_connection_and_other_devices_are_isolated() {
        let mut store = RingingLeaseStore::new();
        store.open("cs-1".into(), "device-1".into());
        store.open("cs-2".into(), "device-2".into());
        assert!(store.attach_session("cs-1", "seed-a"));
        assert!(!store.attach_session("", "seed-a"), "空 session 拒绝");

        // 身份隔离：device-2 看不到 device-1 的归属。
        assert!(!store.owns_session("cs-2", "seed-a"));

        // 连接过期后不能再 attach（presence 前置），但既有归属仍在。
        store.set_expiry_for_test("device-1", Instant::now() - Duration::from_secs(1));
        assert!(!store.attach_session("cs-1", "seed-b"));
        assert!(store.owns_session("cs-1", "seed-a"));

        // detach 摘的是身份级归属。
        store.open("cs-3".into(), "device-1".into());
        store.detach_session("cs-3", "seed-a");
        assert!(!store.owns_session("cs-3", "seed-a"));
    }
}
