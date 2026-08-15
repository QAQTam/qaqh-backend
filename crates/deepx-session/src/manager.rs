//! SessionManager — unified singleton for session persistence and lifecycle.
//!
//! Stores each session as:
//!   {sessions_dir}/{seed}/
//!     meta.json       — SessionMeta (atomic replace-write)
//!     messages.jsonl  — one JSON line per Message (append-only)
//!
//! A central `index.json` enables fast listing.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use deepx_types::{Message, SessionMeta};

use crate::store;

/// 注入日志文件名（每 session 一个，append-only JSONL）。
pub const INJECTION_JOURNAL_FILE: &str = "injections.jsonl";

/// `injections.jsonl` 的单一事件行。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum InjectionJournalEntry {
    /// 注入命令已持久化入队（daemon ACK 前写入）。
    Enqueued {
        command_id: String,
        text: String,
    },
    /// 注入已提交到 messages.jsonl（worker 落盘后写入）。
    Committed {
        command_id: String,
    },
}

static INSTANCE: OnceLock<SessionManager> = OnceLock::new();

/// The LLM-facing view after a compact operation.  Raw messages remain in the
/// normal session archive; this is deliberately a separate, replaceable view.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompactContext {
    pub version: u32,
    pub checkpoint_id: String,
    pub parent_checkpoint_id: Option<String>,
    pub created_at: u64,
    pub archive_message_count: usize,
    pub messages: Vec<Message>,
}

fn read_messages_without_deduplication(path: &std::path::Path) -> Result<Vec<Message>, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    content
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str(line)
                .map_err(|error| format!("parse {} line {}: {error}", path.display(), index + 1))
        })
        .collect()
}

#[derive(Debug)]
pub struct SessionManager {
    sessions_dir: PathBuf,
    active_path: PathBuf,
    session_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl SessionManager {
    /// Initialize the global singleton. Must be called once at startup.
    /// Also triggers automatic migration from legacy TOML format if needed.
    pub fn init(data_dir: PathBuf) {
        let sessions_dir = data_dir.join("sessions");
        let _ = std::fs::create_dir_all(&sessions_dir);

        let mgr = Self {
            active_path: data_dir.join(".active_session"),
            session_locks: Mutex::new(HashMap::new()),
            sessions_dir,
        };
        // Migrate old TOML sessions on first startup of v0.4.0
        crate::migrate::run(&mgr.sessions_dir);
        // Workspace 注册表与 session 存储同根（组织语义，与运行环境 workspace 解耦）。
        crate::workspace::WorkspaceStore::init(data_dir);
        INSTANCE
            .set(mgr)
            .expect("SessionManager already initialized");
    }

    /// Access the global instance.
    pub fn global() -> &'static Self {
        INSTANCE
            .get()
            .expect("SessionManager not initialized — call init() first")
    }

    // ── Session listing ──

    /// List all sessions sorted by updated_at descending.
    pub fn list(&self) -> Vec<SessionMeta> {
        let mut metas = store::read_index(&self.sessions_dir);

        // Fallback: scan directories if index is empty
        if metas.is_empty() {
            if let Ok(entries) = std::fs::read_dir(&self.sessions_dir) {
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
        }

        metas.sort_by_key(|m| std::cmp::Reverse(m.updated_at));
        metas
    }

    /// Delete a session: removes the session directory and its index entry.
    pub fn delete(&self, seed: &str) -> Result<(), String> {
        let dir = self
            .session_dir(seed)
            .ok_or_else(|| format!("Session not found: {seed}"))?;

        std::fs::remove_dir_all(&dir).map_err(|e| format!("Failed to delete session: {e}"))?;

        store::remove_from_index(&self.sessions_dir, seed);
        // 同步清理 workspace 账户（会话删除后不留悬空引用）。
        crate::workspace::WorkspaceStore::global().remove_session(seed);

        log::info!("SessionManager: deleted session {seed}");
        Ok(())
    }

    // ── Load / Save ──

    /// Read the persisted JSONL files for a session.
    pub fn load(&self, seed: &str) -> Option<(SessionMeta, Vec<Message>)> {
        self.snapshot_from_files(seed).ok()
    }

    /// Load the immutable archive plus the latest compact context, if one
    /// exists.  Callers must use `active_messages` for the model loop and
    /// retain `archive_messages` for replay/pagination.
    pub fn load_for_resume(
        &self,
        seed: &str,
    ) -> Option<(SessionMeta, Vec<Message>, Option<CompactContext>)> {
        let (meta, archive_messages) = self.load(seed)?;
        let selected = self
            .read_compact_context(seed)
            .filter(|context| context.archive_message_count <= archive_messages.len());
        Some((meta, archive_messages, selected))
    }

    /// Persist a new checkpoint without rewriting the raw history archive.
    pub fn save_compact_context(&self, seed: &str, messages: &[Message]) {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        let archive_count = self
            .load_meta(seed)
            .map(|meta| meta.message_count)
            .unwrap_or_else(|| {
                store::count_message_lines(&self.session_path_dir(seed)).unwrap_or(0)
            });
        let parent_checkpoint_id = self
            .read_compact_context(seed)
            .map(|context| context.checkpoint_id);
        let now = Self::now_epoch();
        let context = CompactContext {
            version: 1,
            checkpoint_id: format!("compact-{now}-{archive_count}"),
            parent_checkpoint_id,
            created_at: now,
            archive_message_count: archive_count,
            messages: messages.to_vec(),
        };
        if let Err(error) = self.write_compact_context(seed, &context) {
            log::error!("SessionManager: write compact context failed for {seed}: {error}");
            return;
        }
    }

    /// Refresh the active view after later raw messages were appended.
    pub fn update_compact_context(&self, seed: &str, messages: &[Message]) {
        let Some(mut context) = self.read_compact_context(seed) else {
            return;
        };
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        context.archive_message_count = self
            .load_meta(seed)
            .map(|meta| meta.message_count)
            .unwrap_or_else(|| {
                store::count_message_lines(&self.session_path_dir(seed)).unwrap_or(0)
            });
        context.messages = messages.to_vec();
        if let Err(error) = self.write_compact_context(seed, &context) {
            log::error!("SessionManager: update compact context failed for {seed}: {error}");
            return;
        }
    }

    /// Check whether a session exists on disk.
    pub fn exists(&self, seed: &str) -> bool {
        if self.session_dir(seed).is_some() {
            return true;
        }
        false
    }

    /// Load only metadata (fast, no message parsing). JSON remains primary
    /// until the DB-primary readiness gate is explicitly promoted.
    pub fn load_meta(&self, seed: &str) -> Option<SessionMeta> {
        if let Some(dir) = self.session_dir(seed) {
            if let Some(meta) = store::read_meta(&dir) {
                return Some(meta);
            }
        }
        None
    }

    /// Persist agent mode to meta.json without rewriting messages.
    /// Called when the user switches PLAN/CODE mode so it survives agent restart.
    pub fn persist_mode(&self, seed: &str, mode: u8) {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        let dir = self.session_path_dir(seed);
        let mut meta = self.load_meta(seed).unwrap_or_default();
        meta.mode = mode;
        let _ = store::write_meta(&dir, &meta);
    }

    /// Persist tool mode (standard/minimal/custom) to meta.json without
    /// rewriting messages — survives agent restart (PLAN-TOOL-MODES.md 4.3).
    pub fn persist_tool_mode(&self, seed: &str, tool_mode: &str, custom_tools: &[String]) {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        let dir = self.session_path_dir(seed);
        let mut meta = self.load_meta(seed).unwrap_or_default();
        meta.tool_mode = tool_mode.to_string();
        meta.custom_tools = custom_tools.to_vec();
        let _ = store::write_meta(&dir, &meta);
    }

    pub fn persist_skills(&self, seed: &str, skills: deepx_types::SkillSessionStateV2) {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        let dir = self.session_path_dir(seed);
        let _ = std::fs::create_dir_all(&dir);
        let mut meta = self.load_meta(seed).unwrap_or_default();
        let now = Self::now_epoch();
        meta.seed = seed.to_string();
        if meta.created_at == 0 {
            meta.created_at = now;
        }
        meta.updated_at = now;
        meta.skills = skills;
        let _ = store::write_meta(&dir, &meta);
        store::upsert_index(&self.sessions_dir, &meta);
    }

    /// 设置会话归档标记（标签 × 归档 / 左侧列表恢复）。
    /// 仅改 meta.json（atomic replace-write），不触碰消息文件与 registry
    /// 实例——实例启停由调用方（daemon 拦截层）负责。
    pub fn set_archived(&self, seed: &str, archived: bool) {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        let dir = self.session_path_dir(seed);
        let mut meta = self.load_meta(seed).unwrap_or_default();
        if meta.seed.is_empty() {
            meta.seed = seed.to_string();
        }
        meta.archived = archived;
        meta.updated_at = Self::now_epoch();
        let _ = store::write_meta(&dir, &meta);
        store::upsert_index(&self.sessions_dir, &meta);
    }

    /// 设置会话运行环境工作目录（`workspace.set` / 子代理继承）。
    /// 统一数据源：`SessionMeta.cwd`——旧的 `sessions/{seed}/workspace.txt`
    /// 已退役（读取侧惰性迁移，见 `workspace::session_workspace_cwd`）。
    /// 仅改 meta.json（atomic replace-write）；`index` 控制是否同步会话索引
    /// （子代理 ephemeral，不进列表）。
    pub fn set_cwd(&self, seed: &str, cwd: &str, index: bool) {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        let dir = self.session_path_dir(seed);
        let _ = std::fs::create_dir_all(&dir);
        let mut meta = self.load_meta(seed).unwrap_or_default();
        if meta.seed.is_empty() {
            meta.seed = seed.to_string();
        }
        meta.cwd = Some(crate::workspace::canonical_cwd(std::path::Path::new(cwd)));
        // 非索引会话（子代理继承 workspace 等临时场景）= 临时会话：关闭时
        // 整个目录删除（用完即走）；正规会话（index=true）恒为 false。
        meta.ephemeral = !index;
        meta.updated_at = Self::now_epoch();
        let _ = store::write_meta(&dir, &meta);
        if index {
            store::upsert_index(&self.sessions_dir, &meta);
        }
    }

    /// 该 seed 是否为临时会话（子代理）：meta 存在且标记 ephemeral。
    /// 目录缺失（已清理）视为非临时，避免误触发删除路径。
    pub fn is_ephemeral(&self, seed: &str) -> bool {
        self.session_dir(seed).is_some()
            && self.load_meta(seed).map(|m| m.ephemeral).unwrap_or(false)
    }

    /// 上下文统计快照（可再生缓存）。写入 meta.json；只有正规会话
    /// （`created_at > 0`，即 persist_new_session 建立过）才同步索引——
    /// 子代理 worker 的 dashboard/compact 路径不会污染会话列表。
    pub fn set_context_stats(&self, seed: &str, stats: &serde_json::Value) {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        let dir = self.session_path_dir(seed);
        let _ = std::fs::create_dir_all(&dir);
        let mut meta = self.load_meta(seed).unwrap_or_default();
        if meta.seed.is_empty() {
            meta.seed = seed.to_string();
        }
        meta.context_stats = Some(stats.clone());
        meta.updated_at = Self::now_epoch();
        let _ = store::write_meta(&dir, &meta);
        if meta.created_at > 0 {
            store::upsert_index(&self.sessions_dir, &meta);
        }
    }

    /// Synchronously create a new session directory and initial meta.json
    /// on disk, so that the session exists before the agent process starts.
    /// This prevents the race where the frontend receives a seed from
    /// `session.new` but the session directory isn't created until the
    /// agent writes it asynchronously during boot.
    pub fn persist_new_session(&self, seed: &str) {
        self.persist_new_session_with_cwd(seed, None);
    }

    /// 同上，但记录创建时工作目录（workspace 归属基础）：
    /// canonicalize 成功存 canonical 路径，失败存原样字符串；
    /// cwd 命中某 workspace 路径时自动 attach（D1 双轨自动侧）。
    pub fn persist_new_session_with_cwd(&self, seed: &str, cwd: Option<&str>) {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        let dir = self.session_path_dir(seed);
        let _ = std::fs::create_dir_all(&dir);
        let mut meta = self.load_meta(seed).unwrap_or_default();
        let now = Self::now_epoch();
        meta.seed = seed.to_string();
        meta.created_at = now;
        meta.updated_at = now;
        meta.cwd = cwd.map(|c| crate::workspace::canonical_cwd(std::path::Path::new(c)));
        if !dir.join("messages.jsonl").exists() {
            let _ = store::append_messages(&dir, &[]);
        }
        let _ = store::write_meta(&dir, &meta);
        store::upsert_index(&self.sessions_dir, &meta);
        if let Some(cwd) = meta.cwd.as_deref() {
            crate::workspace::WorkspaceStore::global().attach_by_cwd(seed, cwd);
        }
    }

    pub fn persist_usage(
        &self,
        seed: &str,
        totals: deepx_types::UsageInfo,
        last_usage: Option<deepx_types::UsageInfo>,
        requests: u32,
        cache_reported_requests: u32,
    ) {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        let dir = self.session_path_dir(seed);
        let _ = std::fs::create_dir_all(&dir);
        let mut meta = self.load_meta(seed).unwrap_or_default();
        meta.seed = seed.to_string();
        meta.updated_at = Self::now_epoch();
        meta.usage_totals = totals;
        meta.last_usage = last_usage;
        meta.usage_requests = requests;
        meta.cache_reported_requests = cache_reported_requests;
        let _ = store::write_meta(&dir, &meta);
        store::upsert_index(&self.sessions_dir, &meta);
    }

    /// Append a single message to JSONL immediately (per-message persistence).
    pub fn save_one(&self, seed: &str, msg: &Message) {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        let dir = self.session_path_dir(seed);
        let _ = std::fs::create_dir_all(&dir);
        let mut meta = self.load_meta(seed).unwrap_or_default();
        let now = Self::now_epoch();
        meta.seed = seed.to_string();
        if meta.created_at == 0 {
            meta.created_at = now;
        }
        meta.updated_at = now;
        meta.message_count = meta.message_count.saturating_add(1);
        if let Err(e) = store::append_one(&dir, msg) {
            log::error!("SessionManager: save_one failed: {e}");
            return;
        }
        if let Err(e) = store::write_meta(&dir, &meta) {
            log::error!("SessionManager: save_one metadata write failed: {e}");
            return;
        }
        store::upsert_index(&self.sessions_dir, &meta);
    }

    /// Update session metadata and index after messages have been appended.
    pub fn update_meta(
        &self,
        seed: &str,
        model: &str,
        effort: Option<&str>,
        compact_skip: usize,
        turn_count: usize,
    ) {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        let now = Self::now_epoch();
        let dir = self.session_path_dir(seed);
        let mut meta = self.load_meta(seed).unwrap_or_default();
        meta.seed = seed.to_string();
        if meta.created_at == 0 {
            meta.created_at = now;
        }
        meta.updated_at = now;
        meta.model = model.to_string();
        meta.effort = effort.map(String::from);
        meta.turn_count = turn_count;
        meta.compact_skip = compact_skip;
        if let Err(e) = store::write_meta(&dir, &meta) {
            log::error!("SessionManager: write_meta failed: {e}");
            return;
        }
        store::upsert_index(&self.sessions_dir, &meta);
    }

    /// 更新会话标题（冻结语义：调用方负责只在首轮后调用一次；幂等覆盖）。
    /// 写 meta + index（daemon 的 `list()` 每次读盘，无需跨进程通知即可见）。
    pub fn update_title(&self, seed: &str, title: &str) {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        let dir = self.session_path_dir(seed);
        let mut meta = self.load_meta(seed).unwrap_or_default();
        meta.seed = seed.to_string();
        meta.title = Some(title.to_string());
        meta.updated_at = Self::now_epoch();
        if let Err(e) = store::write_meta(&dir, &meta) {
            log::error!("SessionManager: update_title write_meta failed: {e}");
            return;
        }
        store::upsert_index(&self.sessions_dir, &meta);
    }

    /// Save session: write meta + rewrite all messages.
    /// Used for initial save or after undo/compact.
    pub fn save_full(
        &self,
        seed: &str,
        messages: &[Message],
        model: &str,
        effort: Option<&str>,
        compact_skip: usize,
        turn_count: usize,
    ) {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        let now = Self::now_epoch();
        let dir = self.session_path_dir(seed);
        let _ = std::fs::create_dir_all(&dir);

        let created_at = self.load_meta(seed).map(|m| m.created_at).unwrap_or(now);

        let existing = self.load_meta(seed).unwrap_or_default();
        let last_summary = Self::extract_summary(messages);

        let meta = SessionMeta {
            seed: seed.to_string(),
            created_at,
            updated_at: now,
            model: model.to_string(),
            effort: effort.map(String::from),
            message_count: messages.len(),
            turn_count,
            last_summary,
            compact_skip,
            mode: existing.mode,
            skills: existing.skills,
            // 保留既有标题（save_messages 全量重写 meta；title 冻结语义
            // 不能在保存时被 Default 清空——早期缺陷，2026-08 修复）。
            title: existing.title.clone(),
            ..Default::default()
        };

        if let Err(e) = store::rewrite_messages(&dir, messages) {
            log::error!("SessionManager: rewrite_messages failed: {e}");
            return;
        }
        if let Err(e) = store::write_meta(&dir, &meta) {
            log::error!("SessionManager: write_meta failed: {e}");
            return;
        }
        store::upsert_index(&self.sessions_dir, &meta);
    }

    /// Append new messages (since last save) to the session JSONL.
    /// Updates meta and index.
    pub fn save_append(
        &self,
        seed: &str,
        new_messages: &[Message],
        model: &str,
        effort: Option<&str>,
        compact_skip: usize,
        turn_count: usize,
    ) {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        if new_messages.is_empty() {
            return;
        }

        let now = Self::now_epoch();
        let dir = self.session_path_dir(seed);
        let _ = std::fs::create_dir_all(&dir);

        let mut meta = self.load_meta(seed).unwrap_or_default();
        if meta.created_at == 0 {
            meta.created_at = now;
        }
        let last_summary = Self::extract_summary(new_messages);
        meta.seed = seed.to_string();
        meta.updated_at = now;
        meta.model = model.to_string();
        meta.effort = effort.map(String::from);
        meta.message_count = meta.message_count.saturating_add(new_messages.len());
        meta.turn_count = turn_count;
        meta.last_summary = last_summary;
        meta.compact_skip = compact_skip;

        // Append messages
        if let Err(e) = store::append_messages(&dir, new_messages) {
            log::error!("SessionManager: append_messages failed: {e}");
            return;
        }

        if let Err(e) = store::write_meta(&dir, &meta) {
            log::error!("SessionManager: write_meta failed: {e}");
            return;
        }
        store::upsert_index(&self.sessions_dir, &meta);
    }

    // ── Injection journal（持久化注入队列）──
    //
    // `injections.jsonl` 是外部注入命令（subagent 报告等 as_system 消息）的
    // 持久化队列，append-only，每行一个事件：
    //   {"event":"enqueued",  "command_id":"…", "text":"…"}
    //   {"event":"committed", "command_id":"…"}
    //
    // 写入方：daemon 在 ACK 前写 enqueued（accepted = 已持久化入队，而非仅已
    // 转发）；agent worker 在注入落盘到 messages.jsonl 后写 committed。进程
    // 崩溃后，worker 启动时重放无 committed 标记的 enqueued 行——配合
    // store 的文本幂等（push_trailing_system），实现至少一次 + 幂等 = 恰好一次。

    /// 持久化一条注入命令入队。失败时调用方必须拒绝该命令（不 ACK）。
    pub fn append_injection_enqueued(
        &self,
        seed: &str,
        command_id: &str,
        text: &str,
    ) -> Result<(), String> {
        let entry = InjectionJournalEntry::Enqueued {
            command_id: command_id.to_string(),
            text: text.to_string(),
        };
        self.append_injection_line(seed, &entry)
    }

    /// 标记一条注入命令已提交（已落盘到 messages.jsonl）。幂等：重复标记无害。
    pub fn append_injection_committed(&self, seed: &str, command_id: &str) -> Result<(), String> {
        let entry = InjectionJournalEntry::Committed {
            command_id: command_id.to_string(),
        };
        self.append_injection_line(seed, &entry)
    }

    /// 读取未提交（无 committed 标记）的注入，按入队顺序返回 (command_id, text)。
    /// 供 worker 启动时重放。文件不存在返回空；坏行跳过并告警。
    pub fn read_pending_injections(&self, seed: &str) -> Vec<(String, String)> {
        let path = self.session_path_dir(seed).join(INJECTION_JOURNAL_FILE);
        let Ok(content) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        let mut committed: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut enqueued: Vec<(String, String)> = Vec::new();
        for (index, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<InjectionJournalEntry>(line) {
                Ok(InjectionJournalEntry::Enqueued { command_id, text }) => {
                    enqueued.push((command_id, text));
                }
                Ok(InjectionJournalEntry::Committed { command_id }) => {
                    committed.insert(command_id);
                }
                Err(e) => log::warn!(
                    "[journal] skip bad line {} of {}: {e}",
                    index + 1,
                    path.display()
                ),
            }
        }
        enqueued
            .into_iter()
            .filter(|(command_id, _)| !committed.contains(command_id))
            .collect()
    }

    fn append_injection_line(&self, seed: &str, entry: &InjectionJournalEntry) -> Result<(), String> {
        use std::io::Write;
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap();
        let dir = self.session_path_dir(seed);
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("create session dir {}: {e}", dir.display()))?;
        let path = dir.join(INJECTION_JOURNAL_FILE);
        let line = serde_json::to_string(entry).map_err(|e| format!("serialize journal entry: {e}"))?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("open {}: {e}", path.display()))?;
        // 单次 write 追加整行：跨进程（daemon ↔ worker）并发追加在
        // Windows FILE_APPEND_DATA / POSIX O_APPEND 下是原子的，行不会交错。
        writeln!(file, "{line}").map_err(|e| format!("append {}: {e}", path.display()))?;
        file.flush()
            .map_err(|e| format!("flush {}: {e}", path.display()))
    }

    // ── Active session ──

    /// Read the currently active session seed.
    pub fn active_seed(&self) -> Option<String> {
        std::fs::read_to_string(&self.active_path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Set the active session seed (persisted to disk).
    pub fn set_active_seed(&self, seed: &str) {
        if let Some(parent) = self.active_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&self.active_path, seed).is_err() {
            log::error!("SessionManager: failed to write active session file");
        }
    }

    /// Clear the active session marker.
    pub fn clear_active(&self) {
        let _ = std::fs::remove_file(&self.active_path);
    }

    // ── Helpers ──

    /// Generate a new session seed (8 hex chars from hashed time + PID).
    pub fn generate_seed() -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .hash(&mut h);
        std::process::id().hash(&mut h);
        let v = h.finish();
        let mixed = (v as u32) ^ ((v >> 32) as u32);
        format!("{:08x}", mixed)
    }

    /// Current UNIX epoch.
    pub fn now_epoch() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    // ── Private ──

    fn session_lock(&self, seed: &str) -> Arc<Mutex<()>> {
        let mut locks = self.session_locks.lock().unwrap();
        locks
            .entry(seed.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn compact_context_path(&self, seed: &str) -> PathBuf {
        self.session_path_dir(seed).join("compact-context.json")
    }

    fn read_compact_context(&self, seed: &str) -> Option<CompactContext> {
        serde_json::from_str(&std::fs::read_to_string(self.compact_context_path(seed)).ok()?).ok()
    }

    fn write_compact_context(&self, seed: &str, context: &CompactContext) -> Result<(), String> {
        let path = self.compact_context_path(seed);
        std::fs::create_dir_all(self.session_path_dir(seed))
            .map_err(|error| format!("create compact context directory: {error}"))?;
        let temporary = path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(context)
            .map_err(|error| format!("serialize compact context: {error}"))?;
        std::fs::write(&temporary, data)
            .map_err(|error| format!("write compact context: {error}"))?;
        std::fs::rename(&temporary, &path)
            .map_err(|error| format!("activate compact context: {error}"))
    }

    fn snapshot_from_files(&self, seed: &str) -> Result<(SessionMeta, Vec<Message>), String> {
        let dir = self
            .session_dir(seed)
            .ok_or_else(|| format!("session directory is missing: {seed}"))?;
        let meta = store::read_meta(&dir)
            .ok_or_else(|| format!("meta.json is missing or unreadable: {seed}"))?;
        let messages = read_messages_without_deduplication(&dir.join("messages.jsonl"))?;
        Ok((meta, messages))
    }

    fn session_path_dir(&self, seed: &str) -> PathBuf {
        self.sessions_dir.join(seed)
    }

    fn session_dir(&self, seed: &str) -> Option<PathBuf> {
        let dir = self.session_path_dir(seed);
        if dir.exists() && dir.is_dir() {
            Some(dir)
        } else {
            None
        }
    }

    fn extract_summary(messages: &[Message]) -> String {
        messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant" && !m.content.is_empty())
            .and_then(|m| {
                m.content.iter().find_map(|b| {
                    if let deepx_types::ContentBlock::Text { text } = b {
                        Some(text.lines().next().unwrap_or(text))
                    } else {
                        None
                    }
                })
            })
            .map(|s| {
                if s.len() <= 80 {
                    return s.to_string();
                }
                let mut end = 80;
                while !s.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}..", &s[..end])
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod injection_journal_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    static TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    fn manager() -> (PathBuf, SessionManager) {
        let root = std::env::temp_dir().join(format!(
            "deepx-session-journal-{}-{}-{}",
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
        };
        (root, manager)
    }

    #[test]
    fn injection_journal_roundtrip_and_commit_marking() {
        let (root, manager) = manager();
        let seed = "journal-seed";

        // 初始：无待重放注入。
        assert!(manager.read_pending_injections(seed).is_empty());

        // daemon 侧：ACK 前持久化入队（按到达顺序）。
        manager
            .append_injection_enqueued(seed, "cmd-a", "[SUBAGENT 'a' COMPLETED]\n\nA")
            .expect("enqueue a");
        manager
            .append_injection_enqueued(seed, "cmd-b", "[SUBAGENT 'b' COMPLETED]\n\nB")
            .expect("enqueue b");
        let pending = manager.read_pending_injections(seed);
        assert_eq!(pending.len(), 2);
        assert_eq!(
            pending[0],
            ("cmd-a".to_string(), "[SUBAGENT 'a' COMPLETED]\n\nA".to_string())
        );
        assert_eq!(
            pending[1],
            ("cmd-b".to_string(), "[SUBAGENT 'b' COMPLETED]\n\nB".to_string())
        );

        // worker 侧：落盘后标记 committed；重复标记幂等。
        manager
            .append_injection_committed(seed, "cmd-a")
            .expect("commit a");
        manager
            .append_injection_committed(seed, "cmd-a")
            .expect("commit a again (idempotent)");
        let pending = manager.read_pending_injections(seed);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, "cmd-b");

        // 全部 committed → 无待重放。
        manager
            .append_injection_committed(seed, "cmd-b")
            .expect("commit b");
        assert!(manager.read_pending_injections(seed).is_empty());

        std::fs::remove_dir_all(&root).expect("remove test directory");
    }

    #[test]
    fn injection_journal_missing_file_is_empty() {
        let (root, manager) = manager();
        assert!(manager.read_pending_injections("no-such-seed").is_empty());
        std::fs::remove_dir_all(&root).expect("remove test directory");
    }
}

#[cfg(test)]
mod skill_persistence_tests {
    use super::*;
    use deepx_types::{SkillSessionEntry, SkillSessionEntryState, SkillSessionStateV2};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    static TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    fn manager() -> (PathBuf, SessionManager) {
        let root = std::env::temp_dir().join(format!(
            "deepx-session-skills-{}-{}-{}",
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
        };
        (root, manager)
    }

    fn state() -> SkillSessionStateV2 {
        SkillSessionStateV2 {
            version: 2,
            context_epoch: 7,
            operation_revision: 9,
            entries: vec![SkillSessionEntry {
                name: "alpha".into(),
                activation_order: 1,
                source: "model".into(),
                state: SkillSessionEntryState::Active,
            }],
        }
    }

    #[test]
    fn file_only_new_session_is_immediately_listable_and_loadable() {
        let (root, manager) = manager();
        manager.persist_new_session("file-only");

        let listed = manager.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].seed, "file-only");
        let (meta, messages) = manager.load("file-only").expect("file snapshot");
        assert_eq!(meta.seed, "file-only");
        assert!(messages.is_empty());

        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn metadata_rewrites_preserve_skill_session_state_v2() {
        let (root, manager) = manager();
        manager.persist_skills("seed", state());
        manager.update_meta("seed", "model", None, 0, 1);
        manager.save_full("seed", &[Message::user("hello")], "model", None, 0, 1);
        let meta = manager.load_meta("seed").expect("metadata");
        assert_eq!(meta.seed, "seed");
        assert_eq!(meta.skills, state());
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn compact_context_preserves_archive_and_restores_the_active_view() {
        let (root, manager) = manager();
        let archive = vec![
            Message::user("one"),
            Message::user("two"),
            Message::user("three"),
        ];
        manager.save_full("compact-seed", &archive, "model", None, 0, 2);
        let active = vec![
            Message::user("[Compacted 1 turns]\nsummary"),
            Message::user("three"),
        ];
        manager.save_compact_context("compact-seed", &active);

        let (_, restored_archive, context) =
            manager.load_for_resume("compact-seed").expect("resume");
        assert_eq!(
            restored_archive.len(),
            archive.len(),
            "raw archive must not be rewritten"
        );
        let context = context.expect("compact checkpoint");
        assert_eq!(context.messages.len(), active.len());
        assert_eq!(context.parent_checkpoint_id, None);
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn repeated_compact_links_checkpoints_without_losing_archive() {
        let (root, manager) = manager();
        let archive = vec![
            Message::user("one"),
            Message::user("two"),
            Message::user("three"),
        ];
        manager.save_full("multi-compact", &archive, "model", None, 0, 3);
        manager.save_compact_context("multi-compact", &[Message::user("[Compacted]\nfirst")]);
        let first = manager
            .read_compact_context("multi-compact")
            .expect("first checkpoint");
        manager.save_compact_context("multi-compact", &[Message::user("[Compacted]\nsecond")]);
        let second = manager
            .read_compact_context("multi-compact")
            .expect("second checkpoint");
        assert_eq!(
            second.parent_checkpoint_id.as_deref(),
            Some(first.checkpoint_id.as_str())
        );
        assert_eq!(
            manager.load("multi-compact").expect("archive").1.len(),
            archive.len()
        );
        std::fs::remove_dir_all(root).expect("remove test directory");
    }
}
