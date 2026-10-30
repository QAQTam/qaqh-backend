//! `RingingHub`：daemon 侧 Ringing 运行时聚合入口。
//!
//! 职责：
//! - 大内容外置存储（`content_store.rs`，会话所有权 + TTL）；
//! - 活交互正文归属与 worker 存活登记（`live_workers`）。
//!
//! 旧 timeline transcript 投影已随纯 v2 单流改造整体火化
//! （canonical fact → v2 projection 是唯一呈现路径）。
//!
//! 由 daemon 与孤儿收尾路径消费。线程安全（Mutex 保护）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use qaqh_session::SessionManager;

use super::content_store::{ContentEntry, ContentQuotaExceeded, ContentStore};

/// Ringing daemon 运行时聚合。
pub struct RingingHub {
    pub(super) epoch: String,
    /// 大内容外置存储（会话所有权 + TTL）。
    pub(super) content_store: Mutex<ContentStore>,
    /// #345：活交互正文在 content store 里的归属（seed → (interaction_id, content_id)）。
    ///
    /// 交互正文是**展示面旁路**（canonical fact 只存 ref）：正文在
    /// [`RingingHub::put_interaction_content`] 入 store 并 pin，交互
    /// resolved / expired 时按此表 unpin。表里没有 = 没有正文（permission，
    /// 或写入时超配额）。
    pub(super) live_interaction_content: Mutex<HashMap<String, (String, String)>>,
    /// B9/H3：当前进程内存活的 worker（registry 维护）。bootstrap 的
    /// force=false 孤儿收尾在 worker 存活时整体跳过，防止误杀活 turn。
    pub(super) live_workers: Mutex<std::collections::HashSet<String>>,
    /// 会话存储句柄（PR-3-1 注入化；conversation snapshot 的持久化读侧）。
    /// None = 测试或未装配（对应旧 `try_global()` 为空语义）。
    pub(super) sessions: Option<Arc<SessionManager>>,
}

impl RingingHub {
    pub fn new(epoch: impl Into<String>) -> Self {
        Self::with_options(epoch.into(), None)
    }

    /// 持久化构造：大内容外置落盘（v1 事件持久化已删除）。
    pub fn with_persistence(epoch: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        Self::with_options(epoch.into(), Some(root.into()))
    }

    /// 注入会话存储句柄（PR-3-1：daemon main 在 `SessionManager::init` 后装配）。
    pub fn with_sessions(mut self, sessions: Arc<SessionManager>) -> Self {
        self.sessions = Some(sessions);
        self
    }

    fn with_options(epoch: String, root: Option<PathBuf>) -> Self {
        let content_store = match root.as_ref() {
            Some(root) => ContentStore::with_root(root.join("content")),
            None => ContentStore::new(),
        };
        Self {
            epoch,
            content_store: Mutex::new(content_store),
            live_interaction_content: Mutex::new(HashMap::new()),
            live_workers: Mutex::new(std::collections::HashSet::new()),
            sessions: None,
        }
    }

    /// 登记 worker 存活（B9/H3）：registry 在 spawn 成功时调用，worker 关闭时
    /// 由 [`RingingHub::mark_worker_dead`] 摘除。
    pub fn mark_worker_live(&self, session_id: &str) {
        self.live_workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.to_string());
    }

    pub fn mark_worker_dead(&self, session_id: &str) {
        self.live_workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
    }

    pub fn memory_components(&self) -> Vec<qaqh_memwatch::ComponentMemory> {
        let mut components = Vec::new();
        if let Ok(content) = self.content_store.lock() {
            components.extend(content.memory_components());
        }
        let live_interactions = self
            .live_interaction_content
            .lock()
            .map(|live| live.len() as u64)
            .unwrap_or_default();
        let live_workers = self
            .live_workers
            .lock()
            .map(|workers| workers.len() as u64)
            .unwrap_or_default();
        components.extend([
            qaqh_memwatch::ComponentMemory {
                name: "ringing.live_interactions".into(),
                item_count: live_interactions,
                payload_bytes: None,
                heap_estimate_bytes: None,
                ..Default::default()
            },
            qaqh_memwatch::ComponentMemory {
                name: "ringing.live_workers".into(),
                item_count: live_workers,
                payload_bytes: None,
                heap_estimate_bytes: None,
                ..Default::default()
            },
        ]);
        components
    }

    /// D-1：会话关闭后丢弃该 seed 的全部常驻内存态（活交互正文表、
    /// live_workers、大内容条目）。
    ///
    /// 调用约束：调用方必须已 join worker，避免存活 worker 继续写回脏状态。
    pub fn forget_session(&self, session_id: &str) {
        if let Ok(mut live) = self.live_interaction_content.lock() {
            live.remove(session_id);
        }
        if let Ok(mut workers) = self.live_workers.lock() {
            workers.remove(session_id);
        }
        if let Ok(mut content) = self.content_store.lock() {
            content.release_session(session_id);
        }
    }

    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    /// 大内容外置：存入（返回 content_id）。
    pub fn put_content(
        &self,
        session_id: &str,
        media_type: &str,
        bytes: Vec<u8>,
        truncated: bool,
    ) -> String {
        self.content_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .put(session_id, media_type, bytes, truncated)
    }

    /// 大内容外置：读取（校验会话所有权 + TTL）。
    pub fn get_content(&self, session_id: &str, content_id: &str) -> Option<ContentEntry> {
        self.content_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id, content_id)
    }

    /// #345：交互正文入 store 并 pin（v2 content 端点按 id 取，不带 seed）。
    ///
    /// 超配额返回 [`ContentQuotaExceeded`]（fail-closed，见 content_store 文档）；
    /// 调用方不得在写入失败时假装交互正文可用。
    pub fn put_interaction_content(
        &self,
        session_id: &str,
        interaction_id: &str,
        media_type: &str,
        bytes: Vec<u8>,
    ) -> Result<String, ContentQuotaExceeded> {
        let content_id = self
            .content_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .put_pinned_for(session_id, media_type, bytes, Some(interaction_id))?;
        self.live_interaction_content
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                session_id.to_string(),
                (interaction_id.to_string(), content_id.clone()),
            );
        Ok(content_id)
    }

    /// #345：按 content_id 读取（**不校验 seed**）。调用方（daemon）拿条目的
    /// `seed` 再校验请求方归属——v2 content 端点的 wire 形态不带 seed 参数。
    pub fn get_content_any(&self, content_id: &str) -> Option<ContentEntry> {
        self.content_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_any(content_id)
    }

    /// 解除某交互正文的 pin（交互 resolved / expired / permission tool finished）。
    ///
    /// 先走内存活表（当前进程路径），再按持久化的 pin_key 兜底——daemon 重启后
    /// 活表为空，但 metadata 里仍记录着 interaction_id，孤儿收尾时也能释放 pin。
    ///
    /// §4.0.5：副作用由事件产生侧调用（actor 桥 / orphan_seal 补终态），
    /// 不再由 publish 承载。
    pub fn release_interaction_content(&self, session_id: &str, interaction_id: &str) {
        let content_id = {
            let mut live = self
                .live_interaction_content
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match live.get(session_id) {
                Some((current, _)) if current == interaction_id => {
                    live.remove(session_id).map(|(_, content_id)| content_id)
                }
                _ => None,
            }
        };
        if let Some(content_id) = content_id {
            self.content_store
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .unpin(&content_id);
        }
        // 重启后 live 表为空，按 pin_key 兜底解除。幂等：已 unpin 时返回 0。
        self.content_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unpin_key(session_id, interaction_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forget_session_drops_per_session_resident_state() {
        let hub = RingingHub::new("forget-seed-test");
        hub.mark_worker_live("s1");
        let content_id = hub.put_content("s1", "text/plain", b"hello".to_vec(), false);
        assert!(hub.get_content("s1", &content_id).is_some());

        hub.forget_session("s1");

        assert!(
            !hub.live_workers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains("s1"),
            "live_workers entry must be dropped"
        );
        assert!(
            hub.get_content("s1", &content_id).is_none(),
            "content_store entry must be released"
        );
    }

    #[test]
    fn persisted_content_survives_hub_restart_and_unpins_by_key() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (plain, pinned) = {
            let hub = RingingHub::with_persistence("epoch-content-1", tmp.path());
            let plain = hub.put_content("s1", "text/plain", b"persisted".to_vec(), false);
            let pinned = hub
                .put_interaction_content(
                    "s1",
                    "int_ask_restart",
                    "application/json",
                    b"{\"kind\":\"ask\"}".to_vec(),
                )
                .expect("pinned content admitted");
            (plain, pinned)
        };

        let hub = RingingHub::with_persistence("epoch-content-2", tmp.path());
        let entry = hub
            .get_content("s1", &plain)
            .expect("plain content survives hub restart");
        assert_eq!(entry.bytes, b"persisted");
        let entry = hub
            .get_content_any(&pinned)
            .expect("pinned content survives hub restart");
        assert!(entry.pinned);
        assert_eq!(entry.owners, vec!["s1".to_string()]);

        // 重启后 live_interaction_content 为空，release 仍按持久化的 pin_key
        // 解除 pin（否则 pending interaction 的正文会永久占额度）。
        hub.release_interaction_content("s1", "int_ask_restart");
        assert!(
            !hub.get_content_any(&pinned)
                .expect("unpinned content remains readable")
                .pinned
        );
    }
}
