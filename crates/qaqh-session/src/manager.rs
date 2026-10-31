//! SessionManager — session directories, listing index and locks.
//!
//! Stores each session as:
//!   {sessions_dir}/{seed}/
//!
//! A central `index.jsonl` enables fast listing. Message bytes are **not** this
//! module's business any more: the committed facts in the batch journal are the
//! only conversation store (2026-10 hard cut).

use qaqh_types::SessionMeta;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use crate::canonical::{CANONICAL_IDENTITY_FILE, CanonicalSessionIdentity};
use crate::store;

static INSTANCE: OnceLock<Arc<SessionManager>> = OnceLock::new();

#[derive(Debug)]
pub struct SessionManager {
    sessions_dir: PathBuf,
    active_path: PathBuf,
    session_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// 本进程已占用的 seed（BUG-2026-09-13-24）。
    ///
    /// 磁盘目录与索引只能表达「其它进程/历史是否用过这个 id」，无法表达
    /// 「本进程刚分配、目录已建但调用方尚未确认接受」的中间态——没有它
    /// 就会出现「分配器认为空闲、保险丝认为被占」的自相矛盾。
    claimed_sessions: Mutex<std::collections::HashSet<String>>,
}

impl SessionManager {
    /// Initialize the global singleton. Must be called once at startup.
    pub fn init(data_dir: PathBuf) {
        let sessions_dir = data_dir.join("sessions");
        let _ = std::fs::create_dir_all(&sessions_dir);

        let mgr = Self {
            active_path: data_dir.join(".active_session"),
            session_locks: Mutex::new(HashMap::new()),
            claimed_sessions: Mutex::new(std::collections::HashSet::new()),
            sessions_dir,
        };
        // Workspace 注册表与 session 存储同根（组织语义，与运行环境 workspace 解耦）。
        crate::grouping::WorkspaceStore::init(data_dir);
        INSTANCE
            .set(Arc::new(mgr))
            .expect("SessionManager already initialized");
    }

    /// 构造一个不与全局单例耦合的实例（测试/嵌入用：PR-3-1 注入化的
    /// 显式入口，测试不再靠重建字段字面量绕过封装）。
    ///
    /// 依赖 workspace 账户的子路径（`delete` / `set_cwd`）需要
    /// [`Self::init_for_test`] 先初始化进程级 store；纯文件路径不依赖。
    #[doc(hidden)]
    pub fn new_for_test(sessions_dir: PathBuf, active_path: PathBuf) -> Self {
        Self {
            sessions_dir,
            active_path,
            session_locks: Mutex::new(HashMap::new()),
            claimed_sessions: Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// 进程级依赖初始化（workspace 账户 / 会话目录），供测试与嵌入方在
    /// 自建实例前调用一次；重复调用是 no-op（`OnceLock` 语义）。
    #[doc(hidden)]
    pub fn init_for_test(data_dir: PathBuf) {
        static ONCE: std::sync::Once = std::sync::Once::new();
        let sessions_dir = data_dir.join("sessions");
        let _ = std::fs::create_dir_all(&sessions_dir);
        // 进程级单例：并发/多次调用只初始化一次（同进程测试共享）。
        ONCE.call_once(|| {
            let _ = std::fs::create_dir_all(&data_dir);
            crate::grouping::WorkspaceStore::init(data_dir)
        });
    }

    /// Access the global instance.
    ///
    /// PR-3-1 注入化：仅 daemon `main` 装配点与 §10.3 白名单测试可调用；
    /// 其余代码一律经构造时注入的 `Arc<SessionManager>` 句柄访问会话存储。
    #[doc(hidden)]
    pub fn global() -> Arc<Self> {
        INSTANCE
            .get()
            .expect("SessionManager not initialized — call init() first")
            .clone()
    }

    /// Non-panicking accessor for optional recovery paths (e.g. timeline
    /// rebuild in contexts where the daemon may not have initialized the
    /// session store yet). 同 [`global()`]：生产代码应经注入句柄访问。
    #[doc(hidden)]
    pub fn try_global() -> Option<Arc<Self>> {
        INSTANCE.get().cloned()
    }

    // ── Session listing ──

    /// List all sessions sorted by updated_at descending.
    pub fn list(&self) -> Vec<SessionMeta> {
        let mut metas = store::read_index(&self.sessions_dir);

        // Fallback: scan directories if index is empty
        if metas.is_empty()
            && let Ok(entries) = std::fs::read_dir(&self.sessions_dir)
        {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let meta = store::read_meta(&path);
                if let Some(meta) = meta {
                    metas.push(meta);
                }
            }
        }

        metas.sort_by_key(|m| std::cmp::Reverse(m.updated_at));
        metas
    }

    /// Delete a session: removes the session directory and its index entry.
    pub fn delete(&self, session_id: &str) -> Result<(), String> {
        let dir = self
            .session_dir(session_id)
            .ok_or_else(|| format!("Session not found: {session_id}"))?;
        // 未绑定工作区的会话把默认工作根建在 `data_dir/workspace/<id>`
        // （`qaqh_types::platform::default_session_workspace_dir`）。删除会话
        // 必须连它一起清，否则长驻 daemon 每删一个无工作区会话就留一个孤儿
        // 目录（无界增长）。判定在删除 meta 之前取：只有从未绑过工作区的会话
        // 才可能是我们建的目录，用户显式选的目录绝不代删。
        let user_bound_workspace = self
            .load_meta(session_id)
            .and_then(|meta| meta.cwd)
            .is_some_and(|cwd| !cwd.trim().is_empty() && cwd.trim() != ".");

        std::fs::remove_dir_all(&dir).map_err(|e| format!("Failed to delete session: {e}"))?;

        if !user_bound_workspace {
            let scratch = qaqh_types::platform::default_session_workspace_dir(session_id);
            if let Err(error) = std::fs::remove_dir_all(&scratch)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                log::warn!(
                    "SessionManager: cannot remove default workspace '{}': {error}",
                    scratch.display()
                );
            }
        }

        store::remove_from_index(&self.sessions_dir, session_id);
        // 同步清理 workspace 账户（会话删除后不留悬空引用）。
        crate::grouping::WorkspaceStore::global().remove_session(session_id);
        // D-4：释放 per-seed 锁槽位与占用登记。两者都以 seed 为键、只在
        // 创建/首次取锁时插入，删除路径若不回收，长驻 daemon 每删一个会话就
        // 永久多留一条（无界增长；`release_session_claim` 清的是另一个 map）。
        // 此处已释放全部其它锁，再取 `session_locks` 不引入反向获取顺序。
        self.session_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
        self.release_session_claim(session_id);

        log::info!("SessionManager: deleted session {session_id}");
        Ok(())
    }

    /// Whether a session directory exists on disk.
    pub fn exists(&self, session_id: &str) -> bool {
        if self.session_dir(session_id).is_some() {
            return true;
        }
        false
    }

    /// Load only metadata (fast, no message parsing). JSON remains primary
    /// until the DB-primary readiness gate is explicitly promoted.
    pub fn load_meta(&self, session_id: &str) -> Option<SessionMeta> {
        if let Some(dir) = self.session_dir(session_id)
            && let Some(meta) = store::read_meta(&dir)
        {
            return Some(meta);
        }
        None
    }

    /// 解析会话运行环境工作目录（PR-3-3 宿主注入的解析权威）：meta.cwd 优先，
    /// 旧 `workspace.txt` 惰性迁移（原子写 meta + 删 txt，两进程竞争幂等）。
    /// 宿主（agent loop / service）经注入句柄调用后把值注入 workspace。
    pub fn workspace_cwd(&self, session_id: &str) -> Option<String> {
        // Journal first; the index entry covers the window between allocation
        // and the first committed fact (creation order: dir -> index -> journal).
        let cwd = self
            .load_meta(session_id)
            .and_then(|meta| meta.cwd)
            .or_else(|| {
                store::read_index(&self.sessions_dir)
                    .into_iter()
                    .find(|meta| meta.session_id == session_id)
                    .and_then(|meta| meta.cwd)
            })?;
        if cwd.is_empty() {
            return None;
        }
        Some(crate::grouping::repair_legacy_backslash_cwd(&cwd))
    }

    /// 会话选定的 BYOK profile 名（journal 里的 `profile`）。`None`/空 = 跟随全局
    /// `active_profile`。宿主（agent loop / service）经注入句柄调用。
    pub fn session_profile(&self, session_id: &str) -> Option<String> {
        self.load_meta(session_id)?
            .profile
            .filter(|name| !name.is_empty())
    }

    pub fn session_effort(&self, session_id: &str) -> Option<String> {
        self.load_meta(session_id)?
            .effort
            .filter(|eff| !eff.is_empty())
    }

    /// Mark a session's cleanup policy without adding it to the session index.
    ///
    /// V2 subagents are durable canonical sessions but remain hidden from the
    /// ordinary session list. Their close path must therefore retain the
    /// directory while `ephemeral` stays false.
    pub fn set_ephemeral(&self, session_id: &str, ephemeral: bool) {
        self.with_meta_locked(session_id, true, |dir, meta| {
            if meta.session_id.is_empty() {
                meta.session_id = session_id.to_string();
            }
            meta.ephemeral = ephemeral;
            meta.updated_at = Self::now_epoch();
            let _ = store::write_meta(dir, meta);
        });
    }

    /// Synchronously create a new session directory and its index entry
    /// on disk, so that the session exists before the agent process starts.
    /// This prevents the race where the frontend receives a seed from
    /// `session.new` but the session directory isn't created until the
    /// agent writes it asynchronously during boot.
    ///
    /// ⚠ BUG-2026-09-13-24：本方法**无条件覆盖**既有 meta（created_at/cwd
    /// 等）。调用方必须先确认 seed 未被占用（[`Self::is_session_taken`]/
    /// [`Self::allocate_session`]），否则会把新会话写进旧会话目录。
    pub fn persist_new_session(&self, session_id: &str) {
        self.persist_new_session_with_cwd(session_id, None);
    }

    /// 仅当 `seed` 未被占用时创建新会话目录 + 初始 meta。
    ///
    /// 与 [`Self::persist_new_session_with_cwd`] 的差异：占用即返回 `false`
    /// 并且**一个字节都不写**（不覆盖既有 meta、不追加 messages.jsonl）。
    /// 检查与落盘在 per-seed 锁内串行，并可选的 `claimed` 钩子在同一把锁
    /// 内二次校验（供调用方维护进程内占用集，见 `QaqhService`）。
    pub fn persist_new_session_if_absent(&self, session_id: &str, cwd: Option<&str>) -> bool {
        self.persist_new_session_if_absent_with(session_id, cwd, |_| true)
    }

    /// [`Self::persist_new_session_if_absent`] 的可注入版本：`claimed` 在
    /// 锁内、落盘前被调用，返回 `false` 视为「已被占用」，本次创建放弃。
    pub fn persist_new_session_if_absent_with(
        &self,
        session_id: &str,
        cwd: Option<&str>,
        claimed: impl FnOnce(&str) -> bool,
    ) -> bool {
        match self.create_new_session(session_id, cwd, claimed, None, true) {
            Ok(()) => true,
            Err(error) => {
                log::warn!("[session] create_session: seed {session_id} refused: {error}");
                false
            }
        }
    }

    fn create_new_session(
        &self,
        session_id: &str,
        cwd: Option<&str>,
        claimed: impl FnOnce(&str) -> bool,
        identity: Option<&CanonicalSessionIdentity>,
        index_session: bool,
    ) -> Result<(), String> {
        if session_id.is_empty() {
            return Err("refusing to create a session with an empty seed".to_string());
        }
        let lock = self.session_lock(session_id);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        if self.session_dir(session_id).is_some() {
            return Err("session directory already exists; refusing to overwrite".to_string());
        }
        if !claimed(session_id) {
            return Err("seed is already claimed elsewhere; refusing to overwrite".to_string());
        }

        let dir = self.session_path_dir(session_id);
        std::fs::create_dir_all(&self.sessions_dir)
            .map_err(|error| format!("create sessions dir failed: {error}"))?;
        std::fs::create_dir(&dir)
            .map_err(|error| format!("create session dir {} failed: {error}", dir.display()))?;

        let cleanup_identity_dir = |error: String| -> Result<(), String> {
            if identity.is_some() {
                let _ = std::fs::remove_dir_all(&dir);
            }
            Err(error)
        };

        if let Some(identity) = identity {
            if identity.session_id.as_str() != session_id {
                return cleanup_identity_dir(format!(
                    "canonical identity session_id {} does not match directory {session_id}",
                    identity.session_id
                ));
            }
            if let Err(error) = CanonicalSessionIdentity::install(&dir, identity) {
                return cleanup_identity_dir(format!(
                    "install canonical identity for {session_id} failed: {error}"
                ));
            }
        }

        // 目录 + identity 就是分配阶段的全部产物：`SessionCreated` 是创世事实，
        // 由运行时在 materialize 阶段独占提交（它掌握 model / parent /
        // ephemeral）。这里写 metadata 会抢在创世之前，投影会拒收
        // （"session creation must be first"）。
        //
        // 索引条目同样由 journal 派生：genesis 提交后 materialize 会
        // `refresh_index`，列表页自然出现该会话。
        if index_session && let Some(cwd) = cwd {
            let canonical = crate::grouping::canonical_cwd(std::path::Path::new(cwd));
            if let Some(store) = crate::grouping::WorkspaceStore::try_global() {
                store.attach_by_cwd(session_id, &canonical);
            }
        }
        Ok(())
    }

    /// Allocate a canonical `SessionId` and create `sessions/{session_id}`.
    ///
    /// This is the beta creation path: identity is allocated first, the
    /// directory name is the `SessionId`, and `canonical-identity.json` is
    /// installed before meta or any actor can observe the session.
    pub fn allocate_session(&self, cwd: Option<&str>) -> Result<CanonicalSessionIdentity, String> {
        self.allocate_session_with_index(cwd, true)
    }

    /// Allocate a canonical child session hidden from the ordinary session list.
    ///
    /// Subagent sessions still satisfy `seed == session_id == directory` and
    /// carry the same identity sidecar, but `meta.ephemeral` follows the
    /// caller's V2 lifecycle policy and the session index is not polluted.
    pub fn allocate_agent_session(
        &self,
        cwd: Option<&str>,
    ) -> Result<CanonicalSessionIdentity, String> {
        self.allocate_session_with_index(cwd, false)
    }

    fn allocate_session_with_index(
        &self,
        cwd: Option<&str>,
        index_session: bool,
    ) -> Result<CanonicalSessionIdentity, String> {
        let mut last_error = String::from("no attempt was made");
        for _ in 0..Self::SESSION_ALLOCATION_ATTEMPTS {
            let identity = CanonicalSessionIdentity::new();
            let session_id = identity.session_id.as_str().to_string();
            match self.create_new_session(
                &session_id,
                cwd,
                |candidate| self.claim_session(candidate),
                Some(&identity),
                index_session,
            ) {
                Ok(()) => return Ok(identity),
                Err(error) => {
                    self.release_session_claim(&session_id);
                    last_error = error.clone();
                    log::warn!(
                        "[session] allocate_session: candidate {session_id} failed: {error}; retrying"
                    );
                }
            }
        }
        Err(format!(
            "allocate_session: exhausted {} canonical identity attempts (last error: {last_error})",
            Self::SESSION_ALLOCATION_ATTEMPTS,
        ))
    }

    /// 同上，但记录创建时工作目录（workspace 归属基础）：
    /// canonicalize 成功存 canonical 路径，失败存原样字符串；
    /// cwd 命中某 workspace 路径时自动 attach（D1 双轨自动侧）。
    pub fn persist_new_session_with_cwd(&self, session_id: &str, cwd: Option<&str>) {
        self.with_meta_locked(session_id, true, |dir, meta| {
            let now = Self::now_epoch();
            meta.session_id = session_id.to_string();
            meta.created_at = now;
            meta.updated_at = now;
            meta.cwd = cwd.map(|c| crate::grouping::canonical_cwd(std::path::Path::new(c)));
            let _ = store::write_meta(dir, meta);
            store::upsert_index(&self.sessions_dir, meta);
            if let Some(cwd) = meta.cwd.as_deref() {
                crate::grouping::WorkspaceStore::global().attach_by_cwd(session_id, cwd);
            }
        });
    }

    /// Currently active session id, if any.
    pub fn active_session(&self) -> Option<String> {
        std::fs::read_to_string(&self.active_path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Set the active session seed (persisted to disk).
    pub fn set_active_session(&self, session_id: &str) {
        if let Some(parent) = self.active_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&self.active_path, session_id).is_err() {
            log::error!("SessionManager: failed to write active session file");
        }
    }

    /// Clear the active session marker.
    pub fn clear_active(&self) {
        let _ = std::fs::remove_file(&self.active_path);
    }

    // ── Helpers ──

    /// Maximum canonical-identity (UUIDv7) allocation attempts; exhaustion is a
    /// hard error with no counter fallback.
    /// 128-bit 的 UUIDv7 空间下连续 64 次命中既有会话是病态事件，但一旦
    /// 发生必须收敛而不是无限重试。
    const SESSION_ALLOCATION_ATTEMPTS: usize = 64;

    /// 尝试把 `seed` 登记为本进程占用。返回 `false` 表示已被本进程占用。
    pub fn claim_session(&self, session_id: &str) -> bool {
        self.claimed_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.to_string())
    }

    /// 释放本进程占用登记（会话删除/分配失败回滚）。
    pub fn release_session_claim(&self, session_id: &str) {
        self.claimed_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
    }

    /// 该 seed 是否已被占用：
    /// **本进程占用登记** ∪ **磁盘会话目录** ∪ **索引条目**。
    ///
    /// 目录检查覆盖「刚 persist 但 meta 尚未可读」的窗口；索引检查覆盖
    /// 「目录已删、索引尚未 compact」的幽灵 seed——复用幽灵 id 会让新会话
    /// 继承旧索引条目的身份。
    pub fn is_session_taken(&self, session_id: &str) -> bool {
        if session_id.is_empty() {
            return true;
        }
        self.claimed_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(session_id)
            || self.session_dir(session_id).is_some()
            || store::read_index(&self.sessions_dir)
                .iter()
                .any(|m| m.session_id == session_id)
    }

    /// Current UNIX epoch.
    pub fn now_epoch() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    /// Load-modify scaffolding for the per-session critical section.
    ///
    /// Takes the per-seed lock, resolves the session dir (creating it when
    /// `create_dir`), loads (or defaults) the meta, then runs `f` with the
    /// lock **held**. Write-back (`store::write_meta`) and index sync
    /// (`store::upsert_index`) deliberately stay per call-site: error policy
    /// differs by path (CK-PERSIST `persist_tool_mode` returns `Result` while
    /// fire-and-forget paths swallow I/O errors, and index sync is
    /// conditional in `set_cwd`).
    fn with_meta_locked<R>(
        &self,
        session_id: &str,
        create_dir: bool,
        f: impl FnOnce(&PathBuf, &mut SessionMeta) -> R,
    ) -> R {
        let lock = self.session_lock(session_id);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let dir = self.session_path_dir(session_id);
        if create_dir {
            let _ = std::fs::create_dir_all(&dir);
        }
        let mut meta = self.load_meta(session_id).unwrap_or_default();
        f(&dir, &mut meta)
    }

    // ── Private ──

    fn session_lock(&self, session_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.session_locks.lock().unwrap_or_else(|e| e.into_inner());
        locks
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    pub fn session_path_dir(&self, session_id: &str) -> PathBuf {
        self.sessions_dir.join(session_id)
    }

    /// Data root that contains `sessions/`, `team/`, `quota/` and other
    /// canonical aggregates.
    pub fn data_dir(&self) -> &std::path::Path {
        self.sessions_dir
            .parent()
            .unwrap_or(self.sessions_dir.as_path())
    }

    /// Resolve a canonical `SessionId` to its session directory.
    ///
    /// The directory name equals the canonical id, so this returns
    /// `sessions/{session_id}` directly once its identity sidecar confirms the
    /// match. Legacy seed-keyed directories predating that invariant are found
    /// by scanning for the sidecar; missing identities are never created.
    pub fn session_dir_for_id(&self, session_id: &str) -> Result<Option<PathBuf>, String> {
        let direct = self.session_path_dir(session_id);
        if direct.is_dir()
            && direct.join(CANONICAL_IDENTITY_FILE).exists()
            && CanonicalSessionIdentity::open(&direct)
                .map_err(|error| {
                    format!("read canonical identity at {}: {error}", direct.display())
                })?
                .session_id
                .as_str()
                == session_id
        {
            return Ok(Some(direct));
        }

        let entries = std::fs::read_dir(&self.sessions_dir).map_err(|error| {
            format!("read sessions dir {}: {error}", self.sessions_dir.display())
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("read session dir entry: {error}"))?;
            let path = entry.path();
            if !path.is_dir() || !path.join(CANONICAL_IDENTITY_FILE).exists() {
                continue;
            }
            let identity = CanonicalSessionIdentity::open(&path).map_err(|error| {
                format!("read canonical identity at {}: {error}", path.display())
            })?;
            if identity.session_id.as_str() == session_id {
                return Ok(Some(path));
            }
        }
        Ok(None)
    }

    fn session_dir(&self, session_id: &str) -> Option<PathBuf> {
        let dir = self.session_path_dir(session_id);
        if dir.exists() && dir.is_dir() {
            Some(dir)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod session_lock_gc_tests {
    //! D-4 / 安全审查 P0-4：`session_locks` 必须在会话删除路径回收。

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    static TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    fn manager() -> (PathBuf, SessionManager) {
        let root = std::env::temp_dir().join(format!(
            "qaqh-session-lockgc-{}-{}-{}",
            std::process::id(),
            TEST_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
        ));
        let sessions_dir = root.join("sessions");
        std::fs::create_dir_all(&sessions_dir).expect("create test sessions");
        let manager = SessionManager {
            sessions_dir,
            active_path: root.join(".active_session"),
            session_locks: Mutex::new(HashMap::new()),
            claimed_sessions: Mutex::new(std::collections::HashSet::new()),
        };
        // `delete()` 会经 `WorkspaceStore::global()` 摘除账户；该单例是进程级
        // `OnceLock`，其它用例可能已初始化 → 重复初始化会 panic，忽略即可。
        let _ = std::panic::catch_unwind(|| {
            crate::grouping::WorkspaceStore::init(root.clone());
        });
        (root, manager)
    }

    fn lock_len(manager: &SessionManager) -> usize {
        manager
            .session_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    #[test]
    fn session_locks_shrinks_after_delete() {
        let (_root, manager) = manager();
        let baseline = lock_len(&manager);

        // 创建会话 → `persist_new_session_if_absent_with` 经 `session_lock`
        // 插入一条；删除会话必须把它回收，而不是每删一个留一条。
        let identity = manager.allocate_session(None).expect("allocate session");
        let session_id = identity.session_id.as_str().to_string();
        assert_eq!(
            lock_len(&manager),
            baseline + 1,
            "创建会话应在 session_locks 里留下一条"
        );

        manager.delete(&session_id).expect("delete session");
        assert_eq!(
            lock_len(&manager),
            baseline,
            "删除会话后 session_locks 必须回落到创建前水位"
        );
    }
}

#[cfg(test)]
#[cfg(test)]
mod session_collision_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    static TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    fn manager() -> (PathBuf, SessionManager) {
        let root = std::env::temp_dir().join(format!(
            "qaqh-session-seed-{}-{}-{}",
            std::process::id(),
            TEST_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
        ));
        let sessions_dir = root.join("sessions");
        std::fs::create_dir_all(&sessions_dir).expect("create test sessions");
        let manager = SessionManager {
            sessions_dir,
            active_path: root.join(".active_session"),
            session_locks: Mutex::new(HashMap::new()),
            claimed_sessions: Mutex::new(std::collections::HashSet::new()),
        };
        // `attach_by_cwd` 需要 WorkspaceStore（进程内单例：其它用例可能
        // 已初始化过 → 忽略重复初始化）。
        let _ = std::panic::catch_unwind(|| {
            crate::grouping::WorkspaceStore::init(root.clone());
        });
        (root, manager)
    }

    /// 占用即拒绝：同一个 canonical id 不得被二次创建，且首个会话的目录与
    /// identity 一个字节都不动。
    #[test]
    fn create_session_refuses_to_overwrite_existing_session() {
        let (root, manager) = manager();

        let identity = manager.allocate_session(Some("D:/kept")).expect("allocate");
        let session_id = identity.session_id.as_str().to_string();
        let dir = manager.session_path_dir(&session_id);
        let identity_bytes =
            std::fs::read(dir.join("canonical-identity.json")).expect("identity sidecar survives");

        assert!(
            !manager.persist_new_session_if_absent_with(&session_id, Some("D:/new-cwd"), |_| true),
            "existing canonical id must be rejected"
        );
        assert_eq!(
            std::fs::read(dir.join("canonical-identity.json")).expect("identity survives"),
            identity_bytes,
            "rejected create must not rewrite the existing session"
        );

        // 调用方占用钩子返回 false 时同样中止创建，且不留下目录。
        let rejected = CanonicalSessionIdentity::new();
        let rejected_id = rejected.session_id.as_str().to_string();
        assert!(
            !manager.persist_new_session_if_absent_with(&rejected_id, None, |_| false),
            "caller-side claim rejection must abort creation"
        );
        assert!(
            !manager.session_path_dir(&rejected_id).exists(),
            "rejected session must not be materialized"
        );

        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn allocate_session_uses_canonical_id_for_directory_identity_and_meta() {
        let (root, manager) = manager();

        let identity = manager.allocate_session(None).expect("allocate session");
        let session_id = identity.session_id.as_str();
        let dir = manager.session_path_dir(session_id);

        assert_eq!(session_id.len(), 36, "new session seed must be UUIDv7");
        assert!(dir.is_dir(), "session directory must use the SessionId");
        assert_eq!(
            CanonicalSessionIdentity::open(&dir).expect("read identity"),
            identity
        );
        // 分配阶段只产出目录 + identity sidecar：`SessionCreated` 由运行时
        // materialize 独占提交，在那之前没有 journal 可回放，元数据必须是 None。
        assert!(
            manager.load_meta(session_id).is_none(),
            "allocation must not invent metadata before the genesis fact"
        );
        assert_eq!(
            manager
                .session_dir_for_id(session_id)
                .expect("resolve session")
                .expect("resolved directory"),
            dir
        );

        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn allocate_agent_session_keeps_child_hidden_from_session_index() {
        let (root, manager) = manager();

        let identity = manager
            .allocate_agent_session(None)
            .expect("allocate child session");
        let session_id = identity.session_id.as_str();
        assert!(
            store::read_index(&manager.sessions_dir)
                .iter()
                .all(|entry| entry.session_id != session_id),
            "child session must not pollute the ordinary session index"
        );
        assert_eq!(
            CanonicalSessionIdentity::open(manager.session_path_dir(session_id))
                .expect("read child identity"),
            identity
        );

        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    /// 并发回归（BUG-2026-09-13-24）：并发调用 canonical `allocate_session`
    /// 必须每个线程都成功创建自己的会话，且互不重复——不得出现「两个线程
    /// 都通过占用检查 → 一个落盘失败 → 调用方直接报错」的窗口。
    #[test]
    fn concurrent_allocate_session_never_fails() {
        let (root, manager) = manager();
        let manager = std::sync::Arc::new(manager);

        let threads = 4;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(threads));
        let mut handles = Vec::new();
        for _ in 0..threads {
            let manager = manager.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                manager
                    .allocate_session(Some("D:/concurrent"))
                    .map(|identity| identity.session_id.as_str().to_string())
            }));
        }
        let created: Vec<String> = handles
            .into_iter()
            .map(|handle| handle.join().expect("join allocation thread"))
            .map(|result| result.expect("every concurrent allocation must succeed"))
            .collect();

        assert_eq!(
            created.len(),
            threads,
            "every concurrent allocation must succeed: {created:?}"
        );
        for session_id in &created {
            assert!(
                !session_id.is_empty(),
                "created allocation must carry a session id"
            );
            assert_eq!(
                manager
                    .session_dir_for_id(session_id)
                    .expect("resolve created session")
                    .expect("created session has a directory")
                    .file_name()
                    .and_then(|name| name.to_str()),
                Some(session_id.as_str()),
                "created directory must be named by its canonical id"
            );
        }
        // 并发分配必须互不重复。
        let unique: std::collections::HashSet<&String> = created.iter().collect();
        assert_eq!(
            unique.len(),
            created.len(),
            "concurrent allocations must be unique"
        );

        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    /// canonical 分配端到端：分配结果一定未被占用，且落盘时确实创建了
    /// 新会话（而不是复用/写穿既有目录）。
    #[test]
    fn allocate_session_never_returns_an_occupied_session() {
        let (root, manager) = manager();

        let mut allocated: Vec<String> = Vec::new();
        for _ in 0..32 {
            let identity = manager
                .allocate_session(None)
                .expect("allocation must succeed in a nearly empty id space");
            let session_id = identity.session_id.as_str().to_string();
            assert!(
                !allocated.contains(&session_id),
                "allocated session ids must be unique within the process"
            );
            assert!(
                manager.session_dir(&session_id).is_some(),
                "allocated session must be materialized on disk"
            );
            assert!(
                manager.is_session_taken(&session_id),
                "an allocated session must be claimed (never re-handed out)"
            );
            allocated.push(session_id);
        }

        std::fs::remove_dir_all(root).expect("remove test directory");
    }
}
