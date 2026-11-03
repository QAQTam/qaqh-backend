//! SessionManager — 会话目录与 id 分配(纯路径职责)。
//!
//! 业务状态(title/cwd/血缘/活动回合)在 SQLite(`OwnerDirectory`);本模块
//! 只管理磁盘上的会话目录(工作区侧文件如 todo/PLAN 的宿主)与进程内
//! 占用登记。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use qaqh_ringing_contract::ulid::generate_ulid;

static INSTANCE: OnceLock<Arc<SessionManager>> = OnceLock::new();

#[derive(Debug)]
pub struct SessionManager {
    sessions_dir: PathBuf,
    active_path: PathBuf,
    session_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// 本进程已占用的 seed(BUG-2026-09-13-24)。
    claimed_sessions: Mutex<HashSet<String>>,
}

impl SessionManager {
    /// Initialize the global singleton. Must be called once at startup.
    pub fn init(data_dir: PathBuf) {
        let sessions_dir = data_dir.join("sessions");
        let _ = std::fs::create_dir_all(&sessions_dir);
        let mgr = Self {
            active_path: data_dir.join(".active_session"),
            session_locks: Mutex::new(HashMap::new()),
            claimed_sessions: Mutex::new(HashSet::new()),
            sessions_dir,
        };
        INSTANCE
            .set(Arc::new(mgr))
            .expect("SessionManager already initialized");
    }

    /// 构造一个不与全局单例耦合的实例(测试/嵌入用)。
    #[doc(hidden)]
    pub fn new_for_test(sessions_dir: PathBuf, active_path: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&sessions_dir);
        Self {
            sessions_dir,
            active_path,
            session_locks: Mutex::new(HashMap::new()),
            claimed_sessions: Mutex::new(HashSet::new()),
        }
    }

    #[doc(hidden)]
    pub fn global() -> Arc<Self> {
        INSTANCE
            .get()
            .expect("SessionManager not initialized — call init() first")
            .clone()
    }

    #[doc(hidden)]
    pub fn try_global() -> Option<Arc<Self>> {
        INSTANCE.get().cloned()
    }

    /// Delete a session: removes the session directory and workspace account.
    pub fn delete(&self, session_id: &str) -> Result<(), String> {
        let dir = self.session_path_dir(session_id);
        if !dir.is_dir() {
            return Err(format!("Session not found: {session_id}"));
        }
        std::fs::remove_dir_all(&dir).map_err(|e| format!("Failed to delete session: {e}"))?;
        // 默认工作根随会话删除(用户显式绑定的工作区绝不代删——它不在
        // sessions 目录里,删不到)。
        let scratch = qaqh_types::platform::default_session_workspace_dir(session_id);
        if scratch.starts_with(self.data_dir().join("workspace"))
            && let Err(error) = std::fs::remove_dir_all(&scratch)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!(
                "SessionManager: cannot remove default workspace '{}': {error}",
                scratch.display()
            );
        }
        self.session_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
        self.release_session_claim(session_id);
        log::info!("SessionManager: deleted session {session_id}");
        Ok(())
    }

    /// Currently active session id, if any.
    pub fn active_session(&self) -> Option<String> {
        std::fs::read_to_string(&self.active_path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    pub fn set_active_session(&self, session_id: &str) {
        if let Some(parent) = self.active_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&self.active_path, session_id).is_err() {
            log::error!("SessionManager: failed to write active session file");
        }
    }

    pub fn clear_active(&self) {
        let _ = std::fs::remove_file(&self.active_path);
    }

    /// 尝试把 `seed` 登记为本进程占用。返回 `false` 表示已被本进程占用。
    pub fn claim_session(&self, session_id: &str) -> bool {
        self.claimed_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.to_string())
    }

    /// 释放本进程占用登记(会话删除/分配失败回滚)。
    pub fn release_session_claim(&self, session_id: &str) {
        self.claimed_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
    }

    /// 该 seed 是否已被占用:本进程占用登记 ∪ 磁盘会话目录。
    pub fn is_session_taken(&self, session_id: &str) -> bool {
        if session_id.is_empty() {
            return true;
        }
        self.claimed_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(session_id)
            || self.session_path_dir(session_id).is_dir()
    }

    /// Current UNIX epoch.
    pub fn now_epoch() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    pub fn session_path_dir(&self, session_id: &str) -> PathBuf {
        self.sessions_dir.join(session_id)
    }

    /// Data root that contains `sessions/` and other aggregates.
    pub fn data_dir(&self) -> &std::path::Path {
        self.sessions_dir
            .parent()
            .unwrap_or(self.sessions_dir.as_path())
    }

    /// 分配一个新会话 id 并建目录(目录名即 id)。id 生成自 ULID;
    /// 占用登记防进程内碰撞,目录已存在则重试。
    pub fn allocate_session(&self) -> Result<String, String> {
        for _ in 0..64 {
            let session_id = format!("ses_{}", generate_ulid());
            let lock = self.session_lock(&session_id);
            let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            if !self.claim_session(&session_id) {
                continue;
            }
            let dir = self.session_path_dir(&session_id);
            match std::fs::create_dir(&dir) {
                Ok(()) => return Ok(session_id),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    self.release_session_claim(&session_id);
                    continue;
                }
                Err(error) => {
                    self.release_session_claim(&session_id);
                    return Err(format!("create session dir failed: {error}"));
                }
            }
        }
        Err("allocate_session: exhausted 64 attempts".to_string())
    }

    /// Resolve a session id to its directory(目录名即 id)。
    pub fn session_dir_for_id(&self, session_id: &str) -> Result<Option<PathBuf>, String> {
        let direct = self.session_path_dir(session_id);
        Ok(direct.is_dir().then_some(direct))
    }

    // ── Private ──

    fn session_lock(&self, session_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.session_locks.lock().unwrap_or_else(|e| e.into_inner());
        locks
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

#[cfg(test)]
mod session_lock_gc_tests {
    //! D-4 / 安全审查 P0-4:`session_locks` 必须在会话删除路径回收。

    use super::*;

    #[test]
    fn delete_releases_the_per_seed_lock_slot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = SessionManager::new_for_test(
            dir.path().join("sessions"),
            dir.path().join(".active_session"),
        );
        let session_id = "0198f1a0-0000-7000-8000-0000000000aa";
        std::fs::create_dir_all(manager.session_path_dir(session_id)).expect("mkdir");
        let lock = manager.session_lock(session_id);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        drop(_guard);
        manager.delete(session_id).expect("delete");
        let locks = manager
            .session_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert!(!locks.contains_key(session_id));
    }
}
