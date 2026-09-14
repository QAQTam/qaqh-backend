//! Ringing client session lease store.
//!
//! Extracted from `qaqh-daemon/src/ringing_http.rs:2866` for sharing between
//! the legacy hand-written TCP path and the new axum path. Single source of
//! truth; no drift.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const RENEW_TTL_MS: u64 = 30_000;

fn lease_ttl_ms() -> u64 {
    std::env::var("QAQH_TEST_LEASE_TTL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(RENEW_TTL_MS)
}

/// Ringing 逻辑 client session lease.
///
/// 键 = 客户端自生成的 `client_instance_id`；值记录服务端签发的
/// `client_session_id`，后续请求必须通过 header 携带该 session id。
/// open 时双 id 关联，renew 按 client_session_id 反查续期。
#[derive(Debug, Default)]
pub struct RingingLeaseStore {
    leases: HashMap<String, LeaseEntry>,
    seed_leases: HashMap<String, HashSet<String>>,
    /// `client_session_id` → `client_instance_id` 索引（BUG-2026-09-12-10）：
    /// 让 `is_active_session` / `owns_seed` 等热路径查询保持 O(1)，
    /// 避免每次调用做全表扫描（SSE 每事件都会经过 `owns_seed`）。
    by_session: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct LeaseEntry {
    client_session_id: String,
    expiry: Instant,
}

impl RingingLeaseStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&mut self, client_session_id: String, client_instance_id: String) {
        // 顺带做一次过期 GC（open 是低频协商路径，适合承担清理）。
        self.expire();
        // 重新协商（同 instance 换新 cs）：旧 cs 的归属与索引必须清掉——
        // 否则旧 cs 会成为「僵尸身份」（owns_seed 仍 true 但 lease 已死），
        // 而新 cs 的归属由客户端重放 attach 补上（BUG-2026-09-12-10）。
        if let Some(old) = self.leases.get(&client_instance_id)
            && old.client_session_id != client_session_id
        {
            self.seed_leases.remove(&old.client_session_id);
            self.by_session.remove(&old.client_session_id);
        }
        self.by_session
            .insert(client_session_id.clone(), client_instance_id.clone());
        self.leases.insert(
            client_instance_id,
            LeaseEntry {
                client_session_id,
                expiry: Instant::now() + Duration::from_millis(lease_ttl_ms()),
            },
        );
    }

    pub fn attach_seed(&mut self, client_session_id: &str, seed: &str) -> bool {
        if !self.is_active_session(client_session_id) || seed.is_empty() {
            return false;
        }
        self.seed_leases
            .entry(client_session_id.to_string())
            .or_default()
            .insert(seed.to_string());
        true
    }

    pub fn detach_seed(&mut self, client_session_id: &str, seed: &str) {
        if let Some(seeds) = self.seed_leases.get_mut(client_session_id) {
            seeds.remove(seed);
            if seeds.is_empty() {
                self.seed_leases.remove(client_session_id);
            }
        }
    }

    /// 该会话当前已 attach 的 seed 归属快照（BUG-2026-09-12-12 / issue #31）。
    ///
    /// 用途：SSE 回放过滤原先在一次全局租约锁内逐事件调 `owns_seed`，
    /// 持锁时间随回放事件数线性增长（实测 62–247 ms）。调用方改为在**一次**
    /// 短临界区内取本快照，之后锁外过滤，与事件数解耦。
    ///
    /// 活跃性由调用方在同临界区内用 `is_active_session` 判定（跳过活跃检查
    /// 会让过期/重新协商后的僵尸身份仍能读到归属——见 BUG-2026-09-12-10）。
    pub fn owned_seeds(&self, client_session_id: &str) -> HashSet<String> {
        self.seed_leases
            .get(client_session_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn owns_seed(&mut self, client_session_id: &str, seed: &str) -> bool {
        // BUG-2026-09-12-10：不再在热路径上做 expire() 全表扫描（每次还分配
        // 一个 HashSet）；过期即失效改由 is_active_session 惰性判定，GC 交给
        // open()。同时要求活跃性——否则重新协商后的旧 cs（已不在 leases）
        // 会继续通过归属检查（僵尸身份仍能读 timeline）。
        self.is_active_session(client_session_id)
            && self
                .seed_leases
                .get(client_session_id)
                .is_some_and(|seeds| seeds.contains(seed))
    }

    /// 续租（按 client_session_id 反查）；过期/未知会话返回 false。
    pub fn renew(&mut self, client_session_id: &str) -> bool {
        let Some(instance) = self.by_session.get(client_session_id).cloned() else {
            return false;
        };
        let expired = match self.leases.get_mut(&instance) {
            Some(entry) => {
                if entry.expiry < Instant::now() {
                    true
                } else {
                    entry.expiry = Instant::now() + Duration::from_millis(lease_ttl_ms());
                    false
                }
            }
            // 索引悬挂（理论上由 open/expire 维护消除）：按未知处理并清理。
            None => true,
        };
        if expired {
            self.leases.remove(&instance);
            self.by_session.remove(client_session_id);
            self.seed_leases.remove(client_session_id);
            return false;
        }
        true
    }

    fn expire(&mut self) {
        let now = Instant::now();
        let expired: Vec<(String, String)> = self
            .leases
            .iter()
            .filter(|(_, entry)| entry.expiry < now)
            .map(|(instance, entry)| (instance.clone(), entry.client_session_id.clone()))
            .collect();
        for (instance, session) in expired {
            self.leases.remove(&instance);
            self.by_session.remove(&session);
            self.seed_leases.remove(&session);
        }
    }

    /// 活跃校验（按 client_instance_id；命令/切流端点使用）
    pub fn is_active(&self, client_instance_id: &str) -> bool {
        self.leases
            .get(client_instance_id)
            .is_some_and(|e| e.expiry >= Instant::now())
    }

    pub fn is_active_session(&self, client_session_id: &str) -> bool {
        self.by_session
            .get(client_session_id)
            .and_then(|instance| self.leases.get(instance))
            .is_some_and(|entry| entry.expiry >= Instant::now())
    }

    pub fn instance_for_session(&self, client_session_id: &str) -> Option<String> {
        if !self.is_active_session(client_session_id) {
            return None;
        }
        self.by_session.get(client_session_id).cloned()
    }

    /// Test helper: force expiry for a given instance id.
    pub fn set_expiry_for_test(&mut self, client_instance_id: &str, expiry: Instant) {
        if let Some(entry) = self.leases.get_mut(client_instance_id) {
            entry.expiry = expiry;
        }
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
        assert!(store.is_active("ci-1"));
        assert!(!store.is_active("unknown"));
        assert!(store.renew("cs-1"));
        store.set_expiry_for_test("ci-1", Instant::now() - Duration::from_secs(1));
        assert!(!store.is_active("ci-1"));
        assert!(!store.renew("cs-1"));
    }

    #[test]
    fn renegotiation_drops_old_session_ownership_and_activity_is_required() {
        let mut store = RingingLeaseStore::new();
        store.open("cs-1".into(), "ci-1".into());
        assert!(store.attach_seed("cs-1", "seed-a"));
        assert!(store.owns_seed("cs-1", "seed-a"));

        // 重新协商：同 instance 换新 cs（BUG-2026-09-12-10 修复点 1）
        store.open("cs-2".into(), "ci-1".into());
        assert!(
            !store.owns_seed("cs-1", "seed-a"),
            "旧 cs 在重新协商后必须失去 seed 归属（僵尸身份）"
        );
        assert!(
            !store.owns_seed("cs-2", "seed-a"),
            "新 cs 不自动继承归属，须由客户端重放 attach"
        );
        assert!(store.attach_seed("cs-2", "seed-a"));
        assert!(store.owns_seed("cs-2", "seed-a"));

        // 过期即失效（惰性判定，不依赖热路径 GC；修复点 2）
        store.set_expiry_for_test("ci-1", Instant::now() - Duration::from_secs(1));
        assert!(
            !store.owns_seed("cs-2", "seed-a"),
            "过期 lease 不得再通过归属检查"
        );
        assert!(!store.is_active_session("cs-2"));
    }

    #[test]
    fn sse_replay_is_scoped_to_session_seed_leases() {
        use crate::ringing::hub::ChannelReplay;
        use qaqh_domain::RingingChannel;
        use qaqh_ringing::{RingingEvent, RingingEventEnvelope, RingingResetRequired};
        use std::sync::{Arc, Mutex};

        let leases = Arc::new(Mutex::new(RingingLeaseStore::new()));
        leases.lock().unwrap().open("cs-1".into(), "ci-1".into());
        assert!(leases.lock().unwrap().attach_seed("cs-1", "seed-a"));

        let event_a = RingingEventEnvelope::new(
            "seed-a",
            1,
            1,
            1,
            "event-a",
            RingingEvent::Tool(qaqh_domain::ToolEvent::ToolStarted {
                tool_call_id: "call-a".into(),
                turn_id: "turn-a".into(),
                round_num: 0,
                name: "exec".into(),
            }),
        );
        let event_b = RingingEventEnvelope::new(
            "seed-b",
            2,
            1,
            1,
            "event-b",
            RingingEvent::Tool(qaqh_domain::ToolEvent::ToolStarted {
                tool_call_id: "call-b".into(),
                turn_id: "turn-b".into(),
                round_num: 0,
                name: "exec".into(),
            }),
        );
        let replay = ChannelReplay {
            events: vec![event_a, event_b],
            resets: vec![
                RingingResetRequired::new(RingingChannel::Tool, "seed-a", 1),
                RingingResetRequired::new(RingingChannel::Tool, "seed-b", 2),
            ],
        };
        // replicate filter_replay_for_session logic (owns_seed)
        let mut leases_guard = leases.lock().unwrap();
        let mut filtered = replay;
        filtered
            .events
            .retain(|e| leases_guard.owns_seed("cs-1", &e.seed));
        filtered
            .resets
            .retain(|r| leases_guard.owns_seed("cs-1", &r.seed));
        assert_eq!(filtered.events.len(), 1);
        assert_eq!(filtered.events[0].seed, "seed-a");
        assert_eq!(filtered.resets.len(), 1);
        assert_eq!(filtered.resets[0].seed, "seed-a");
    }
}
