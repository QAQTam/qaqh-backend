//! SessionManager — unified singleton for session persistence and lifecycle.
//!
//! Stores each session as:
//!   {sessions_dir}/{seed}/
//!     meta.json       — SessionMeta (atomic replace-write)
//!     messages.jsonl  — one JSON line per Message (append-only)
//!     messages.wal    — L2 write-ahead log of un-drained persist ops
//!
//! A central `index.json` enables fast listing.

use qaqh_types::{Message, SessionMeta};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use qaqh_message::legacy_writer::LegacyWriterFacade;

use crate::store;

static INSTANCE: OnceLock<Arc<SessionManager>> = OnceLock::new();

/// 测试专用 seed 候选源（生产恒为 `None`）。见
/// [`SessionManager::allocate_seed`] 的碰撞回归说明。
type SeedSource = Option<fn() -> String>;
static SEED_SOURCE: OnceLock<Mutex<SeedSource>> = OnceLock::new();

/// 从一个可注入候选源里取第一个「未占用」的 seed。
///
/// 这是 `try_generate_unique_seed` 的可注入版本：候选序列由 `source` 决定，
/// 碰撞检查仍是同一份 `is_taken`——因此测试可以通过固定候选序列
/// （如 [已占用, 已占用, 空闲]）**确定性复现碰撞重试**。
fn generate_from_source(
    mut source: impl FnMut() -> String,
    mut is_taken: impl FnMut(&str) -> bool,
) -> String {
    for _ in 0..SessionManager::SEED_ALLOCATION_ATTEMPTS {
        let candidate = source();
        if !is_taken(&candidate) {
            return candidate;
        }
        log::warn!("[session] injected seed candidate {candidate} collided — retrying");
    }
    log::error!("[session] injected seed source produced only collisions — falling back to random");
    SessionManager::generate_seed()
}

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
    let total = content.lines().count();
    let messages = content
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str(line)
                .map_err(|error| {
                    // L-session：仅最后一行 torn tail（崩溃时写一半）容错截断；
                    // 中间行损坏仍拒绝——那意味着文件系统性损坏。
                    let is_last_line = index + 1 == total;
                    if is_last_line {
                        log::warn!(
                            "[session] tolerating torn tail at {} line {}",
                            path.display(),
                            index + 1
                        );
                        return format!("__torn_tail__{}", error);
                    }
                    format!("parse {} line {}: {error}", path.display(), index + 1)
                })
                .or_else(|error| {
                    if error.starts_with("__torn_tail__") {
                        Ok(None)
                    } else {
                        Err(error)
                    }
                })
        })
        // 过滤被容忍的 torn tail 行。
        .collect::<Result<Vec<Option<Message>>, String>>()?;
    // BUG-2026-09-12-07 读侧自愈（补落盘 2026-09-12 晚，此前 dry_run 未确认
    // 导致 0c89b51 缺失本补丁）：msg_id 会话内唯一是持久层契约，但历史双写
    // /畸形数据可能破坏它。读取时按 msg_id keep-first 去重（与 replay 的
    // msg_id 判据一致），让 resume/快照投影只看到干净视图；存量脏档在下次
    // SaveFull 重写时物理自愈。乱序但唯一的 id 不重排；无 id 消息不受影响。
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut deduped = Vec::with_capacity(messages.len());
    for message in messages.into_iter().flatten() {
        match message.msg_id {
            Some(id) if !seen.insert(id) => {
                log::warn!(
                    "[session] deduplicated repeated msg_id {id} at {} (keep-first)",
                    path.display()
                );
            }
            _ => deduped.push(message),
        }
    }
    Ok(deduped)
}

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
    claimed_seeds: Mutex<std::collections::HashSet<String>>,
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
            claimed_seeds: Mutex::new(std::collections::HashSet::new()),
            sessions_dir,
        };
        // Migrate old TOML sessions on first startup of v0.4.0
        crate::migrate::run(&mgr.sessions_dir);
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
            claimed_seeds: Mutex::new(std::collections::HashSet::new()),
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
    pub fn delete(&self, seed: &str) -> Result<(), String> {
        let dir = self.session_path_dir(seed);
        // 水位缓存按**目录路径**键控（不依赖目录是否存在），所以失效必须
        // 先于删除、且不因目录查询失败而跳过——否则同 seed 重建会读到旧
        // 水位，把新会话的低 msg_id 首批全部误判为已归档。
        store::invalidate_watermark(&dir);
        let dir = self
            .session_dir(seed)
            .ok_or_else(|| format!("Session not found: {seed}"))?;

        let _legacy_writer = LegacyWriterFacade::lock();
        std::fs::remove_dir_all(&dir).map_err(|e| format!("Failed to delete session: {e}"))?;

        store::remove_from_index(&self.sessions_dir, seed);
        // 同步清理 workspace 账户（会话删除后不留悬空引用）。
        crate::grouping::WorkspaceStore::global().remove_session(seed);
        // D-4：释放 per-seed 锁槽位与占用登记。两者都以 seed 为键、只在
        // 创建/首次取锁时插入，删除路径若不回收，长驻 daemon 每删一个会话就
        // 永久多留一条（无界增长；`release_seed_claim` 清的是另一个 map）。
        // 此处已释放全部其它锁，再取 `session_locks` 不引入反向获取顺序。
        self.session_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(seed);
        self.release_seed_claim(seed);

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
    ///
    /// Fail-closed compact semantics (BUG-007): if a compact checkpoint file
    /// exists but cannot be parsed or points past the archive, this returns
    /// `None`. Compacted history must never become reversible just because
    /// the checkpoint was damaged.
    ///
    /// Phase 2 note: this path reads the **full** archive (the model loop
    /// needs complete history). Projection-only consumers (timeline rebuild)
    /// should use [`Self::load_recent_for_projection`] instead — bounded tail
    /// read, no full-file cost.
    pub fn load_for_resume(
        &self,
        seed: &str,
    ) -> Option<(SessionMeta, Vec<Message>, Option<CompactContext>)> {
        // L2 recovery: fold any un-drained WAL ops into the archive BEFORE any
        // consumer projects from it (worker resume, conversation snapshot,
        // timeline rebuild all funnel through here). Idempotent — see
        // `replay_message_wal`.
        self.replay_message_wal(seed);
        let (meta, archive_messages) = self.load(seed)?;
        let selected = match self.read_compact_context_checked(seed) {
            Ok(None) => None,
            Ok(Some(context)) if context.archive_message_count <= archive_messages.len() => {
                Some(context)
            }
            Ok(Some(context)) => {
                log::error!(
                    "SessionManager: compact context for {seed} points past archive \
                     (archive_message_count={}, archive_len={}) — refusing full-history fallback",
                    context.archive_message_count,
                    archive_messages.len()
                );
                return None;
            }
            Err(error) => {
                log::error!(
                    "SessionManager: compact context for {seed} is unreadable \
                     ({error}) — refusing full-history fallback"
                );
                return None;
            }
        };
        Some((meta, archive_messages, selected))
    }

    /// Phase 2（有界恢复）：只读**最近 `recent` 条**消息做投影重建。
    ///
    /// 与 [`Self::load_for_resume`] 的语义边界：
    /// - 模型循环需要完整历史（compact context 优先）——那是 `load_for_resume`；
    /// - timeline 重建（BUG-006 降级路径）只需要前端 transcript 恢复窗口
    ///   （最近若干轮）。这里用反向扫描（`store::bounded_read`）只触碰
    ///   文件尾部，GB 级归档的重建从 O(文件) 降到 O(尾部)。
    ///
    /// 消息选择规则与 `load_for_resume` 同构：compact context 存在且完好
    /// 时优先（它是权威视图），否则用归档尾部。返回 `None` 表示磁盘上
    /// 无该会话（区别于“有会话但尾部为空”——那返回空 Vec）。
    ///
    /// WAL fold：与 `load_for_resume` 相同先折 WAL（幂等），保证尾部读到
    /// 已落盘的最新消息。
    pub fn load_recent_for_projection(&self, seed: &str, recent: usize) -> Option<Vec<Message>> {
        self.replay_message_wal(seed);
        self.session_dir(seed)?;
        // compact context 完好时优先（与 BUG-007 的 fail-closed 语义一致：
        // 损坏的 compact 在 load_for_resume 是整段拒绝；投影路径取归档尾部
        // ——投影是可重建派生物，不该因 compact 损坏而整体失败）。
        if let Ok(Some(context)) = self.read_compact_context_checked(seed) {
            return Some(context.messages);
        }
        let dir = self.session_path_dir(seed);
        Some(crate::store::bounded_read::read_messages_tail(
            &dir.join("messages.jsonl"),
            recent,
        ))
    }

    /// 归档尾部读取：**只看 append-only 的 `messages.jsonl`，无视 compact context**。
    ///
    /// 与 [`Self::load_recent_for_projection`] 的唯一差别就是去掉 compact 分支——
    /// 而这不是细节，是两个**不同读者**的分野（业界三家 harness 同款取舍：
    /// codex 的 `HistoryReplacement` 只换模型面、grok-build 把 `chat_history.jsonl`
    /// 与 append-only 的 `updates.jsonl` 分成两个文件、deepseek-harness 的
    /// `surface.ts` 直接写「the model-visible surface … is the wrong source for a
    /// human transcript」）：
    ///
    /// - **模型**读 compact 视图（摘要 + 保留段）——`load_recent_for_projection`；
    /// - **人类 transcript**（timeline）读归档——本函数。
    ///
    /// 混用会同时坏两件事：压缩摘要 `[Compacted N turns]` 会被 `from_messages`
    /// 当成一个真实回合显示给用户（`store.rs` 的 `push_user` 分支），而且
    /// `meta.turn_count`（真实持久化回合数）与投影出的回合数对不上，
    /// 全局回合序号就无从算起。
    ///
    /// 语义边界与 `load_recent_for_projection` 相同：`None` = 磁盘上无该会话
    /// （区别于「有会话但尾部为空」——那返回空 Vec）。WAL 同样先折（幂等）。
    pub fn load_archive_tail(&self, seed: &str, recent: usize) -> Option<Vec<Message>> {
        self.replay_message_wal(seed);
        self.session_dir(seed)?;
        let dir = self.session_path_dir(seed);
        Some(crate::store::bounded_read::read_messages_tail(
            &dir.join("messages.jsonl"),
            recent,
        ))
    }

    /// The single `PersistOp` → store mapping (PR-1-6 / Z5). The runtime's
    /// drain loop and the WAL recovery path both funnel through this method,
    /// so the mapping exists exactly once and replayed ops take byte-identical
    /// write paths to live ops. The shadow test in qaqh-message locks it.
    pub fn apply_persist_op(&self, op: &qaqh_message::PersistOp) {
        match op {
            qaqh_message::PersistOp::Append {
                seed,
                messages,
                model,
                effort,
                compact_skip,
                turn_count,
            } => {
                self.save_append(
                    seed,
                    messages,
                    model,
                    effort.as_deref(),
                    *compact_skip,
                    *turn_count,
                );
            }
            qaqh_message::PersistOp::UpdateMeta {
                seed,
                model,
                effort,
                compact_skip,
                turn_count,
            } => {
                self.update_meta(seed, model, effort.as_deref(), *compact_skip, *turn_count);
            }
            qaqh_message::PersistOp::UpdateCompactContext { seed, messages } => {
                self.update_compact_context(seed, messages);
            }
            qaqh_message::PersistOp::SaveCompactContext { seed, messages } => {
                self.save_compact_context(seed, messages);
            }
            qaqh_message::PersistOp::SaveFull {
                seed,
                messages,
                model,
                effort,
                compact_skip,
                turn_count,
            } => {
                self.save_full(
                    seed,
                    messages,
                    model,
                    effort.as_deref(),
                    *compact_skip,
                    *turn_count,
                );
            }
        }
    }

    /// L2 recovery: replay un-drained WAL ops into the archive.
    ///
    /// Idempotent by construction:
    /// - `Append` batches are filtered against the archive's max `msg_id`
    ///   (msg_ids are session-monotonic), so a crash between "op applied" and
    ///   "WAL checkpointed" converges instead of duplicating;
    /// - `UpdateMeta` is a pure metadata refresh;
    /// - `SaveFull` / compact-context ops never reach the WAL (generation
    ///   rewrites — see `qaqh_message::wal` module docs).
    ///
    /// Single-writer invariant: a non-empty WAL implies the previous worker
    /// died before draining, so no live writer exists for this seed while
    /// replay runs. Per-op application still takes the per-seed lock.
    fn replay_message_wal(&self, seed: &str) {
        let dir = self.session_path_dir(seed);
        let mut reader = match qaqh_message::wal::open_reader(&dir) {
            Ok(Some(reader)) => reader,
            Ok(None) => return,
            Err(error) => {
                // Fail-closed: an unreadable WAL is NOT an empty WAL. Applying
                // nothing (and, above all, not checkpointing) keeps the ops
                // replayable once the fault clears; a fresh WAL is never
                // installed over unread ops.
                log::error!(
                    "SessionManager: cannot open WAL for {seed} ({error}) — skipping replay, \
                     log kept for the next attempt"
                );
                return;
            }
        };
        let mut ops = Vec::new();
        loop {
            match reader.next_op() {
                Ok(Some(op)) => ops.push(op),
                Ok(None) => break,
                Err(error) => {
                    // Mid-stream IO error (disk EIO / sharing violation): the
                    // valid prefix is applied below, then the function returns
                    // *before* the checkpoint. Truncating here would destroy the
                    // ops the fault hid — this is the bug the issue reports.
                    log::error!(
                        "SessionManager: WAL read for {seed} failed after {} op(s) ({error}) — \
                         applying the valid prefix and keeping the log",
                        reader.prefix_len()
                    );
                    break;
                }
            }
        }
        if ops.is_empty() {
            return;
        }
        log::info!(
            "SessionManager: replaying {} WAL op(s) for {seed}",
            ops.len()
        );
        let mut applied_max_msg_id = self
            .load(seed)
            .map(|(_, messages)| {
                messages
                    .iter()
                    .filter_map(|message| message.msg_id)
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        for op in ops {
            match op {
                qaqh_message::PersistOp::Append {
                    seed: op_seed,
                    messages,
                    model,
                    effort,
                    compact_skip,
                    turn_count,
                } => {
                    let fresh: Vec<Message> = messages
                        .into_iter()
                        .filter(|message| message.msg_id.is_none_or(|id| id > applied_max_msg_id))
                        .collect();
                    if fresh.is_empty() {
                        continue;
                    }
                    applied_max_msg_id = applied_max_msg_id.max(
                        fresh
                            .iter()
                            .filter_map(|message| message.msg_id)
                            .max()
                            .unwrap_or(0),
                    );
                    self.apply_persist_op(&qaqh_message::PersistOp::Append {
                        seed: op_seed,
                        messages: fresh,
                        model,
                        effort,
                        compact_skip,
                        turn_count,
                    });
                }
                other => self.apply_persist_op(&other),
            }
        }
        if reader.has_failed() {
            // Evidence is already quarantined by the reader (`wal.unreadable-*`);
            // the unread ops stay in place for the next recovery attempt.
            return;
        }
        // Clean read: the ops were applied, so truncate the log. A failure here
        // only costs a redundant (idempotent) replay next time.
        if let Err(error) = qaqh_message::wal::checkpoint_file(&dir) {
            log::error!("SessionManager: WAL checkpoint for {seed} failed: {error}");
        }
    }

    /// Persist a new checkpoint without rewriting the raw history archive.
    pub fn save_compact_context(&self, seed: &str, messages: &[Message]) {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
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
        }
    }

    /// Refresh the active view after later raw messages were appended.
    pub fn update_compact_context(&self, seed: &str, messages: &[Message]) {
        let Some(mut context) = self.read_compact_context(seed) else {
            return;
        };
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        context.archive_message_count = self
            .load_meta(seed)
            .map(|meta| meta.message_count)
            .unwrap_or_else(|| {
                store::count_message_lines(&self.session_path_dir(seed)).unwrap_or(0)
            });
        context.messages = messages.to_vec();
        if let Err(error) = self.write_compact_context(seed, &context) {
            log::error!("SessionManager: update compact context failed for {seed}: {error}");
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
        if let Some(dir) = self.session_dir(seed)
            && let Some(meta) = store::read_meta(&dir)
        {
            return Some(meta);
        }
        None
    }

    /// 解析会话运行环境工作目录（PR-3-3 宿主注入的解析权威）：meta.cwd 优先，
    /// 旧 `workspace.txt` 惰性迁移（原子写 meta + 删 txt，两进程竞争幂等）。
    /// 宿主（agent loop / service）经注入句柄调用后把值注入 workspace。
    pub fn workspace_cwd(&self, seed: &str) -> Option<String> {
        let meta = self.load_meta(seed)?;
        if let Some(cwd) = meta.cwd.as_deref().filter(|c| !c.is_empty()) {
            // 存量修复：历史版本在非 Windows 上写入的 `\` 形态（事故
            // 692d1605 meta.json 反斜杠 cwd，2026-09-06 排查项）。
            return Some(crate::grouping::repair_legacy_backslash_cwd(cwd));
        }
        // 惰性迁移：旧 workspace.txt → meta.cwd
        let txt_path = qaqh_types::platform::sessions_dir()
            .join(seed)
            .join("workspace.txt");
        let legacy = std::fs::read_to_string(&txt_path).ok()?;
        let legacy = legacy.trim().to_string();
        if legacy.is_empty() {
            return None;
        }
        let canonical = crate::grouping::canonical_cwd(std::path::Path::new(&legacy));
        self.set_cwd(seed, &canonical, true);
        let _ = std::fs::remove_file(&txt_path);
        Some(canonical)
    }

    /// Persist agent mode to meta.json without rewriting messages.
    /// Called when the user switches PLAN/CODE mode so it survives agent restart.
    pub fn persist_mode(&self, seed: &str, mode: u8) {
        self.with_meta_locked(seed, false, |dir, meta| {
            meta.mode = mode;
            let _ = store::write_meta(dir, meta);
        });
    }

    /// Persist tool mode (standard/minimal/custom) to meta.json without
    /// rewriting messages — survives agent restart (PLAN-TOOL-MODES.md 4.3).
    ///
    /// 锁死检查点（CK-PERSIST）：写盘/索引失败不再静默吞掉，而是向上返回，
    /// 让 `session.set_tool_mode` action 返回 400 —— 前端据此回滚乐观值，
    /// 避免「UI 显示极简、meta.json 其实没写进去，重启后回到标准」。
    /// 空串统一规范化为 "standard"（旧会话零迁移语义显式落盘）。
    pub fn persist_tool_mode(
        &self,
        seed: &str,
        tool_mode: &str,
        custom_tools: &[String],
    ) -> Result<(), String> {
        self.with_meta_locked(seed, true, |dir, meta| {
            let normalized = if tool_mode.is_empty() {
                "standard"
            } else {
                tool_mode
            };
            meta.tool_mode = normalized.to_string();
            meta.custom_tools = custom_tools.to_vec();
            meta.updated_at = Self::now_epoch();
            store::write_meta(dir, meta)?;
            store::upsert_index(&self.sessions_dir, meta);
            log::info!(
                "[TOOL MODE] persisted {normalized} for {seed} ({} custom tools)",
                custom_tools.len()
            );
            Ok(())
        })
    }

    pub fn persist_skills(&self, seed: &str, skills: qaqh_types::SkillSessionStateV2) {
        self.with_meta_locked(seed, true, |dir, meta| {
            let now = Self::now_epoch();
            meta.seed = seed.to_string();
            if meta.created_at == 0 {
                meta.created_at = now;
            }
            meta.updated_at = now;
            meta.skills = skills;
            let _ = store::write_meta(dir, meta);
            store::upsert_index(&self.sessions_dir, meta);
        });
    }

    /// Persist the frozen [Environment] annotation (P0 cache fix). Written
    /// once per session by the agent loop, replayed into `AgentState` on
    /// resume so the first user message keeps its byte-identical prefix.
    pub fn persist_frozen_annotation(&self, seed: &str, annotation: &str) {
        self.with_meta_locked(seed, true, |dir, meta| {
            let now = Self::now_epoch();
            meta.seed = seed.to_string();
            if meta.created_at == 0 {
                meta.created_at = now;
            }
            meta.updated_at = now;
            meta.frozen_annotation = Some(annotation.to_string());
            let _ = store::write_meta(dir, meta);
            store::upsert_index(&self.sessions_dir, meta);
        });
    }

    /// 设置会话归档标记（标签 × 归档 / 左侧列表恢复）。
    /// 仅改 meta.json（atomic replace-write），不触碰消息文件与 registry
    /// 实例——实例启停由调用方（daemon 拦截层）负责。
    pub fn set_archived(&self, seed: &str, archived: bool) {
        self.with_meta_locked(seed, false, |dir, meta| {
            if meta.seed.is_empty() {
                meta.seed = seed.to_string();
            }
            meta.archived = archived;
            meta.updated_at = Self::now_epoch();
            let _ = store::write_meta(dir, meta);
            store::upsert_index(&self.sessions_dir, meta);
        });
    }

    /// 设置会话运行环境工作目录（`workspace.set` / 子代理继承）。
    /// 统一数据源：`SessionMeta.cwd`——旧的 `sessions/{seed}/workspace.txt`
    /// 已退役（读取侧惰性迁移，见 [`Self::workspace_cwd`]）。
    /// 仅改 meta.json（atomic replace-write）；`index` 控制是否同步会话索引
    /// （子代理 ephemeral，不进列表）。
    pub fn set_cwd(&self, seed: &str, cwd: &str, index: bool) {
        // 空 cwd 双保险：canonical_cwd("") = "" 会清掉已有工作区（前端重启后
        // 回空 cwd 的 bug 通道）；空串直接忽略，永不破坏现有归属。
        if cwd.trim().is_empty() {
            return;
        }
        self.with_meta_locked(seed, true, |dir, meta| {
            if meta.seed.is_empty() {
                meta.seed = seed.to_string();
            }
            meta.cwd = Some(crate::grouping::canonical_cwd(std::path::Path::new(cwd)));
            // 非索引会话（子代理继承 workspace 等临时场景）= 临时会话：关闭时
            // 整个目录删除（用完即走）；正规会话（index=true）恒为 false。
            meta.ephemeral = !index;
            meta.updated_at = Self::now_epoch();
            let _ = store::write_meta(dir, meta);
            if index {
                store::upsert_index(&self.sessions_dir, meta);
            }
            // 组织工作区自动归属（与 persist_new_session_with_cwd:404 同逻辑）：
            // 顶部 `workspace.set` 选目录后，左侧 `session.list.workspace_id` 需
            // 立即反映分组，否则左侧恒显未分组（两套工作区不通根因）。
            // `index=false` 为子代理临时会话，不进组织归属。
            if index && let Some(cwd) = meta.cwd.as_deref() {
                crate::grouping::WorkspaceStore::global().attach_by_cwd(seed, cwd);
            }
        });
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
        self.with_meta_locked(seed, true, |dir, meta| {
            if meta.seed.is_empty() {
                meta.seed = seed.to_string();
            }
            meta.context_stats = Some(stats.clone());
            meta.updated_at = Self::now_epoch();
            let _ = store::write_meta(dir, meta);
            if meta.created_at > 0 {
                store::upsert_index(&self.sessions_dir, meta);
            }
        });
    }

    /// Synchronously create a new session directory and initial meta.json
    /// on disk, so that the session exists before the agent process starts.
    /// This prevents the race where the frontend receives a seed from
    /// `session.new` but the session directory isn't created until the
    /// agent writes it asynchronously during boot.
    ///
    /// ⚠ BUG-2026-09-13-24：本方法**无条件覆盖**既有 meta（created_at/cwd
    /// 等）。调用方必须先确认 seed 未被占用（[`Self::is_seed_taken`]/
    /// [`Self::allocate_seed`]），否则会把新会话写进旧会话目录。
    pub fn persist_new_session(&self, seed: &str) {
        self.persist_new_session_with_cwd(seed, None);
    }

    /// 仅当 `seed` 未被占用时创建新会话目录 + 初始 meta。
    ///
    /// 与 [`Self::persist_new_session_with_cwd`] 的差异：占用即返回 `false`
    /// 并且**一个字节都不写**（不覆盖既有 meta、不追加 messages.jsonl）。
    /// 检查与落盘在 per-seed 锁内串行，并可选的 `claimed` 钩子在同一把锁
    /// 内二次校验（供调用方维护进程内占用集，见 `QaqhService`）。
    pub fn persist_new_session_if_absent(&self, seed: &str, cwd: Option<&str>) -> bool {
        self.persist_new_session_if_absent_with(seed, cwd, |_| true)
    }

    /// [`Self::persist_new_session_if_absent`] 的可注入版本：`claimed` 在
    /// 锁内、落盘前被调用，返回 `false` 视为「已被占用」，本次创建放弃。
    pub fn persist_new_session_if_absent_with(
        &self,
        seed: &str,
        cwd: Option<&str>,
        claimed: impl FnOnce(&str) -> bool,
    ) -> bool {
        if seed.is_empty() {
            log::error!("SessionManager: refusing to create a session with an empty seed");
            return false;
        }
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let _legacy_writer = LegacyWriterFacade::lock();
        if self.session_dir(seed).is_some() {
            log::warn!(
                "[session] create_session: seed {seed} already has a session directory — refusing to overwrite"
            );
            return false;
        }
        if !claimed(seed) {
            log::warn!(
                "[session] create_session: seed {seed} already claimed elsewhere — refusing to overwrite"
            );
            return false;
        }
        let dir = self.session_path_dir(seed);
        if let Err(error) = std::fs::create_dir_all(&dir) {
            log::error!("SessionManager: create session dir {seed} failed: {error}");
            return false;
        }
        let now = Self::now_epoch();
        let mut meta = store::read_meta(&dir).unwrap_or_default();
        meta.seed = seed.to_string();
        meta.created_at = now;
        meta.updated_at = now;
        meta.cwd = cwd.map(|c| crate::grouping::canonical_cwd(std::path::Path::new(c)));
        if !dir.join("messages.jsonl").exists()
            && let Err(error) = store::append_messages(&dir, &[])
        {
            log::error!("SessionManager: append_messages(initial) failed: {error}");
        }
        if let Err(error) = store::write_meta(&dir, &meta) {
            log::error!("SessionManager: write_meta(initial) failed: {error}");
            return false;
        }
        store::upsert_index(&self.sessions_dir, &meta);
        if let Some(cwd) = meta.cwd.as_deref() {
            crate::grouping::WorkspaceStore::global().attach_by_cwd(seed, cwd);
        }
        true
    }

    /// 同上，但记录创建时工作目录（workspace 归属基础）：
    /// canonicalize 成功存 canonical 路径，失败存原样字符串；
    /// cwd 命中某 workspace 路径时自动 attach（D1 双轨自动侧）。
    pub fn persist_new_session_with_cwd(&self, seed: &str, cwd: Option<&str>) {
        self.with_meta_locked(seed, true, |dir, meta| {
            let now = Self::now_epoch();
            meta.seed = seed.to_string();
            meta.created_at = now;
            meta.updated_at = now;
            meta.cwd = cwd.map(|c| crate::grouping::canonical_cwd(std::path::Path::new(c)));
            if !dir.join("messages.jsonl").exists() {
                let _ = store::append_messages(dir, &[]);
            }
            let _ = store::write_meta(dir, meta);
            store::upsert_index(&self.sessions_dir, meta);
            if let Some(cwd) = meta.cwd.as_deref() {
                crate::grouping::WorkspaceStore::global().attach_by_cwd(seed, cwd);
            }
        });
    }

    pub fn persist_usage(
        &self,
        seed: &str,
        totals: qaqh_types::UsageInfo,
        last_usage: Option<qaqh_types::UsageInfo>,
        requests: u32,
        cache_reported_requests: u32,
    ) {
        self.with_meta_locked(seed, true, |dir, meta| {
            meta.seed = seed.to_string();
            meta.updated_at = Self::now_epoch();
            meta.usage_totals = totals;
            meta.last_usage = last_usage;
            meta.usage_requests = requests;
            meta.cache_reported_requests = cache_reported_requests;
            let _ = store::write_meta(dir, meta);
            store::upsert_index(&self.sessions_dir, meta);
        });
    }

    /// Append a single message to JSONL immediately (per-message persistence).
    pub fn save_one(&self, seed: &str, msg: &Message) {
        self.with_meta_locked(seed, true, |dir, meta| {
            let now = Self::now_epoch();
            meta.seed = seed.to_string();
            if meta.created_at == 0 {
                meta.created_at = now;
            }
            meta.updated_at = now;
            meta.message_count = meta.message_count.saturating_add(1);
            if let Err(e) = store::append_one(dir, msg) {
                log::error!("SessionManager: save_one failed: {e}");
                return;
            }
            // save_one 绕过 save_append：不推进水位的话，其 msg_id 会被
            // 后续 append 的去重判据当成「未落盘」而重复写入。
            if let Some(id) = msg.msg_id {
                store::note_watermark(dir, id);
            }
            if let Err(e) = store::write_meta(dir, meta) {
                log::error!("SessionManager: save_one metadata write failed: {e}");
                return;
            }
            store::upsert_index(&self.sessions_dir, meta);
        });
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
        let now = Self::now_epoch();
        self.with_meta_locked(seed, false, |dir, meta| {
            meta.seed = seed.to_string();
            if meta.created_at == 0 {
                meta.created_at = now;
            }
            meta.updated_at = now;
            meta.model = model.to_string();
            meta.effort = effort.map(String::from);
            meta.turn_count = turn_count;
            meta.compact_skip = compact_skip;
            if let Err(e) = store::write_meta(dir, meta) {
                log::error!("SessionManager: write_meta failed: {e}");
                return;
            }
            store::upsert_index(&self.sessions_dir, meta);
        });
    }

    /// 更新会话标题（冻结语义：调用方负责只在首轮后调用一次；幂等覆盖）。
    /// 写 meta + index（daemon 的 `list()` 每次读盘，无需跨进程通知即可见）。
    pub fn update_title(&self, seed: &str, title: &str) {
        self.with_meta_locked(seed, false, |dir, meta| {
            meta.seed = seed.to_string();
            meta.title = Some(title.to_string());
            meta.updated_at = Self::now_epoch();
            if let Err(e) = store::write_meta(dir, meta) {
                log::error!("SessionManager: update_title write_meta failed: {e}");
                return;
            }
            store::upsert_index(&self.sessions_dir, meta);
        });
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
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let now = Self::now_epoch();
        let dir = self.session_path_dir(seed);
        let _ = std::fs::create_dir_all(&dir);

        let created_at = self.load_meta(seed).map(|m| m.created_at).unwrap_or(now);

        let existing = self.load_meta(seed).unwrap_or_default();
        let last_summary = Self::extract_summary(messages);

        // BUG-2026-09-13-05：整条继承既有 meta，再覆写本次调用真正拥有的字
        // 段。此前 `..Default::default()` 只保留 mode/skills/tool_mode/
        // custom_tools/title 五项，cwd/frozen_annotation/usage_*/archived/
        // ephemeral/context_stats 七项在每次 undo/compact 全量重写时被冲掉：
        // resume 后 cwd 退回 `"."`（load_session_workspace 读 meta.cwd）、
        // frozen_annotation 丢失导致首条 [Environment] 注解重生成（日期变
        // 化击穿 provider 前缀缓存）、归档/临时标记丢失。新增持久化字段
        // 默认自动继承，不再依赖维护者记得在这里补一行。
        let mut meta = existing.clone();
        meta.seed = seed.to_string();
        meta.created_at = created_at;
        meta.updated_at = now;
        meta.model = model.to_string();
        meta.effort = effort.map(String::from);
        meta.message_count = messages.len();
        meta.turn_count = turn_count;
        meta.last_summary = last_summary;
        meta.compact_skip = compact_skip;
        // 保留字段保持既有注释语义：tool_mode/custom_tools（工具模式不随
        // compact/undo 丢失）、title（冻结语义）。
        meta.mode = existing.mode;
        meta.skills = existing.skills.clone();
        meta.tool_mode = existing.tool_mode.clone();
        meta.custom_tools = existing.custom_tools.clone();
        meta.title = existing.title.clone();

        if let Err(e) = store::rewrite_messages(&dir, messages) {
            log::error!("SessionManager: rewrite_messages failed: {e}");
            return;
        }
        // 全量重写会把 msg_id 重基到新序列（undo/compact）：水位必须重算，
        // 否则旧高水位会把重写后的低 id 全部误判为「已归档」而丢弃。
        store::invalidate_watermark(&dir);
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
        let now = Self::now_epoch();
        self.with_meta_locked(seed, true, |dir, meta| {
            if new_messages.is_empty() {
                return;
            }
            // BUG-2026-09-12-07（首消息双写）：归档追加是盲写，无法区分
            // "live drain 送达的批次"与 "WAL replay 已写入的同一批消息"，
            // 两个入口交错时同一 msg_id 会落盘两行。msg_id 会话单调，
            // 在此按归档尾部实际最大 msg_id 过滤，使 append 对已落盘
            // 消息幂等（与 replay_message_wal 的去重判据一致）；无 id
            // 的消息保持原样写入（向后兼容旧调用方）。
            let archived_max = store::watermark_msg_id(dir);
            let fresh: Vec<Message> = new_messages
                .iter()
                .filter(|m| match m.msg_id {
                    Some(id) => id > archived_max,
                    None => true,
                })
                .cloned()
                .collect();
            if fresh.is_empty() {
                log::warn!(
                    "[session] save_append: all {} message(s) already archived (max msg_id {archived_max}) — deduped",
                    new_messages.len()
                );
                return;
            }
            if meta.created_at == 0 {
                meta.created_at = now;
            }
            let last_summary = Self::extract_summary(new_messages);
            meta.seed = seed.to_string();
            meta.updated_at = now;
            meta.model = model.to_string();
            meta.effort = effort.map(String::from);
            meta.message_count = meta.message_count.saturating_add(fresh.len());
            meta.turn_count = turn_count;
            meta.last_summary = last_summary;
            meta.compact_skip = compact_skip;

            // Append messages（仅写入过滤后的新消息）
            if let Err(e) = store::append_messages(dir, &fresh) {
                log::error!("SessionManager: append_messages failed: {e}");
                return;
            }
            // 落盘成功后才推进水位：失败批次不得让判据超前（否则丢消息）。
            store::note_watermark(dir, Self::batch_max_msg_id(&fresh).max(archived_max));

            if let Err(e) = store::write_meta(dir, meta) {
                log::error!("SessionManager: write_meta failed: {e}");
                return;
            }
            store::upsert_index(&self.sessions_dir, meta);
        });
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

    /// Maximum seed-allocation attempts before the counter fallback kicks in.
    /// 2^32 的 id 空间下连续 64 次随机命中同一批既有 seed 是病态事件，但
    /// 一旦发生必须收敛而不是无限重试。
    const SEED_ALLOCATION_ATTEMPTS: usize = 64;

    /// 顺序回退最多扫描的候选数（有界，避免 id 空间接近占满时退化为
    /// 2^32 次查询的病态阻塞）。
    const SEED_SEQUENTIAL_SCAN_LIMIT: usize = 4096;

    /// Generate a new session seed (8 hex chars from hashed time + PID).
    ///
    /// BUG-2026-09-13-24：这是**无碰撞检查**的原语（32 位截断哈希），
    /// 碰撞会命中既有会话目录，从而写穿旧会话的 meta/messages。新会话
    /// 分配一律走 [`Self::generate_unique_seed`] / [`Self::allocate_seed`]。
    pub fn generate_seed() -> String {
        let mut h = Self::seed_hasher(&Self::seed_nonce());
        Self::seed_from_hasher(&mut h)
    }

    /// 每次调用都不同的 64 位 nonce：时间戳纳秒数 + 进程 id + 进程内
    /// 单调计数器。
    ///
    /// 原实现只哈希 (nanos, pid)：Windows 的 `SystemTime` 时钟粒度可达
    /// 毫秒级，同一 tick 内连续两次调用会得到**逐位相同**的哈希——碰撞
    /// 不再是概率事件而是必然事件（同 tick 内的 `session.new` 会直接
    /// 复用上一个 seed）。计数器使同 tick 内也不可能重复。
    fn seed_nonce() -> u128 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed) as u128;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        // 计数器折叠纳秒低位：纳秒低位本就在同 tick 内原地踏步。
        (nanos & !0xffff) | (sequence & 0xffff)
    }

    fn seed_hasher(nonce: &u128) -> std::collections::hash_map::DefaultHasher {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        nonce.hash(&mut h);
        std::process::id().hash(&mut h);
        let _ = h.finish();
        h
    }

    /// 把 hasher 状态折叠成 8 位十六进制 seed（32 位 id 空间，形态不变）。
    fn seed_from_hasher(h: &mut std::collections::hash_map::DefaultHasher) -> String {
        use std::hash::Hasher;
        let h = std::mem::replace(h, Self::seed_hasher(&0));
        let v = h.finish();
        let mixed = (v as u32) ^ ((v >> 32) as u32);
        format!("{:08x}", mixed)
    }

    /// 在给定 id 空间内重试生成 seed，直到 `is_taken` 为 false。
    ///
    /// id 空间耗尽时（随机候选 64 次 + 顺序候选 4096 次全部被占）返回随机
    /// 兜底值。**新会话分配必须走 [`Self::try_generate_unique_seed`] /
    /// [`Self::allocate_unique_session_seed`]**（显式失败），本便捷入口仅供
    /// 不落盘的调用方（子代理 ephemeral seed）使用。
    pub fn generate_unique_seed(is_taken: impl FnMut(&str) -> bool) -> String {
        Self::try_generate_unique_seed(is_taken, Self::SEED_SEQUENTIAL_SCAN_LIMIT)
            .unwrap_or_else(Self::generate_seed)
    }

    /// 带显式失败的 seed 分配：先重试 64 次随机候选，再在 `scan_limit` 个
    /// 顺序候选内回退。
    ///
    /// 顺序回退必须**有界**：2^32 空间接近占满时，无界扫描会退化成数亿次
    /// 磁盘/索引查询，把"生成 seed"变成分钟级阻塞。返回 `None` 表示该上限
    /// 内无可用候选，调用方必须显式报错，**绝不能**把碰撞候选当成功返回。
    pub fn try_generate_unique_seed(
        mut is_taken: impl FnMut(&str) -> bool,
        scan_limit: usize,
    ) -> Option<String> {
        for _ in 0..Self::SEED_ALLOCATION_ATTEMPTS {
            let candidate = Self::generate_seed();
            if !is_taken(&candidate) {
                return Some(candidate);
            }
            log::warn!("[session] seed candidate {candidate} collided — retrying");
        }
        // 病态回退：随机空间连续不可用时逐号递增，但有界收敛。
        for offset in 0..scan_limit as u64 {
            let candidate = format!("{:08x}", offset as u32);
            if !is_taken(&candidate) {
                log::warn!(
                    "[session] seed allocation fell back to sequential candidate {candidate} \
                     after {} random collisions",
                    Self::SEED_ALLOCATION_ATTEMPTS
                );
                return Some(candidate);
            }
        }
        log::error!(
            "[session] seed id space exhausted: {} random candidates and {scan_limit} sequential \
             candidates are all taken",
            Self::SEED_ALLOCATION_ATTEMPTS
        );
        None
    }

    /// 失败即 `Err` 的分配入口：调用方（`session.new`）必须把 id 空间耗尽
    /// 变成显式错误，而不是悄悄复用别人的目录。
    pub fn allocate_unique_session_seed(&self) -> Result<String, String> {
        Self::try_generate_unique_seed(
            |candidate| self.is_seed_taken(candidate),
            Self::SEED_SEQUENTIAL_SCAN_LIMIT,
        )
        .ok_or_else(|| {
            "session seed space exhausted: no unused seed available — refusing to reuse an existing session"
                .to_string()
        })
    }

    /// 尝试把 `seed` 登记为本进程占用。返回 `false` 表示已被本进程占用。
    pub fn claim_seed(&self, seed: &str) -> bool {
        self.claimed_seeds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(seed.to_string())
    }

    /// 释放本进程占用登记（会话删除/分配失败回滚）。
    pub fn release_seed_claim(&self, seed: &str) {
        self.claimed_seeds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(seed);
    }

    /// 该 seed 是否已被占用：
    /// **本进程占用登记** ∪ **磁盘会话目录** ∪ **索引条目**。
    ///
    /// 目录检查覆盖「刚 persist 但 meta 尚未可读」的窗口；索引检查覆盖
    /// 「目录已删、索引尚未 compact」的幽灵 seed——复用幽灵 id 会让新会话
    /// 继承旧索引条目的身份。
    pub fn is_seed_taken(&self, seed: &str) -> bool {
        if seed.is_empty() {
            return true;
        }
        self.claimed_seeds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(seed)
            || self.session_dir(seed).is_some()
            || store::read_index(&self.sessions_dir)
                .iter()
                .any(|m| m.seed == seed)
    }

    /// Generate a session seed that does not collide with any existing
    /// session directory, index entry, or in-process claim
    /// (BUG-2026-09-13-24).
    pub fn generate_unique_session_seed(&self) -> String {
        Self::generate_unique_seed(|candidate| self.is_seed_taken(candidate))
    }

    /// **原子**分配一个新会话 seed、登记占用并落盘初始 meta。
    ///
    /// 碰撞检查、占用登记与落盘收在同一个 per-seed 锁内：并发的
    /// `session.new` 不可能拿到同一个 seed，也不可能在选中后才发现目录
    /// 已被别人写入（这正是旧 `persist_new_session` 的写穿路径——它无条件
    /// 覆盖既有 meta 的 created_at/cwd）。
    ///
    /// 返回 `(seed, 是否为全新会话)`。`created == false` 表示 seed 空间耗尽
    /// 或落盘失败，此时不会覆盖既有会话，调用方应显式报错而不是继续 spawn。
    ///
    /// **重试语义（返工修复）**：候选选择发生在 per-seed 锁**之外**，因此
    /// 两个并发分配器可能在都还没登记占用时选中**同一个候选**（Windows
    /// 毫秒级时钟粒度下同 tick 连续调用会得到逐位相同的候选）。此时
    /// `persist_new_session_if_absent_with` 会让其中一个落败。旧实现把落败
    /// 当终局返回 `(seed, false)` → `session.new` 直接报错，明明 id 空间还
    /// 空着却创建不出会话。现在落败即**换候选重试**，只有候选空间真的耗尽
    /// （或落盘 IO 连续失败）才返回失败。
    pub fn allocate_seed(&self, cwd: Option<&str>) -> (String, bool) {
        // 重试上限：候选源可注入（测试用），注入源与真实随机源共用同一套
        // 「生成 → 锁内校验 → 落盘」流程，因此碰撞回归可以在 CI 上
        // **确定性复现**，而不是靠 2^32 分之一的随机概率。
        for _ in 0..Self::SEED_ALLOCATION_ATTEMPTS {
            // 候选源可注入（测试用）：默认 = 真实随机 + 碰撞重试。
            let seed = match Self::seed_source() {
                Some(source) => {
                    generate_from_source(source, |candidate| self.is_seed_taken(candidate))
                }
                None => match self.allocate_unique_session_seed() {
                    Ok(seed) => seed,
                    Err(error) => {
                        log::error!("[session] allocate_seed: {error}");
                        return (String::new(), false);
                    }
                },
            };
            let created = self.persist_new_session_if_absent_with(&seed, cwd, |candidate| {
                self.claim_seed(candidate)
            });
            if created {
                return (seed, true);
            }
            // 落败：候选可能在「选中 → 落盘」之间被别人抢走（并发同候选），
            // 或者磁盘上本就有该目录（幽灵/残留）。释放占用登记后换候选重试，
            // 而不是把失败直接抛给调用方。
            self.release_seed_claim(&seed);
            log::warn!(
                "[session] allocate_seed: candidate {seed} lost the claim race — retrying with a fresh candidate"
            );
        }
        log::error!(
            "[session] allocate_seed: exhausted {} candidates — refusing to reuse an existing session",
            Self::SEED_ALLOCATION_ATTEMPTS
        );
        (String::new(), false)
    }

    /// 测试专用：把 seed 候选源固定为闭包（每次调用产出一个候选）。
    ///
    /// 生产代码从不设置，默认 `None` = 真实随机源。用 `#[doc(hidden)]`
    /// 暴露给集成测试（跨 crate 的 `#[cfg(test)]` 不可见），命名带
    /// `_for_test` 后缀以免被误用为生产 API。
    #[doc(hidden)]
    pub fn set_seed_source_for_test(source: Option<fn() -> String>) {
        *SEED_SOURCE
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = source;
    }

    fn seed_source() -> Option<fn() -> String> {
        SEED_SOURCE
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .copied()
    }

    /// Current UNIX epoch.
    pub fn now_epoch() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    /// Load-modify scaffolding for the meta.json critical section (Phase 3-3).
    ///
    /// Takes the per-seed lock, resolves the session dir (creating it when
    /// `create_dir`), loads (or defaults) the meta, then runs `f` with the
    /// lock **held**. Write-back (`store::write_meta`) and index sync
    /// (`store::upsert_index`) deliberately stay per call-site: error policy
    /// differs by path (CK-PERSIST `persist_tool_mode` returns `Result` while
    /// fire-and-forget paths swallow I/O errors, and index sync is
    /// conditional in `set_cwd`/`set_context_stats`).
    fn with_meta_locked<R>(
        &self,
        seed: &str,
        create_dir: bool,
        f: impl FnOnce(&PathBuf, &mut SessionMeta) -> R,
    ) -> R {
        let lock = self.session_lock(seed);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let _legacy_writer = LegacyWriterFacade::lock();
        let dir = self.session_path_dir(seed);
        if create_dir {
            let _ = std::fs::create_dir_all(&dir);
        }
        let mut meta = self.load_meta(seed).unwrap_or_default();
        f(&dir, &mut meta)
    }

    // ── Private ──

    fn session_lock(&self, seed: &str) -> Arc<Mutex<()>> {
        let mut locks = self.session_locks.lock().unwrap_or_else(|e| e.into_inner());
        locks
            .entry(seed.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn compact_context_path(&self, seed: &str) -> PathBuf {
        self.session_path_dir(seed).join("compact-context.json")
    }

    fn read_compact_context(&self, seed: &str) -> Option<CompactContext> {
        self.read_compact_context_checked(seed).ok().flatten()
    }

    /// Like [`Self::read_compact_context`], but distinguishes “no checkpoint
    /// file” from “checkpoint exists and is damaged”. `load_for_resume` uses
    /// this distinction to fail closed instead of falling back to the full
    /// pre-compact archive.
    fn read_compact_context_checked(&self, seed: &str) -> Result<Option<CompactContext>, String> {
        let path = self.compact_context_path(seed);
        let body = match std::fs::read_to_string(&path) {
            Ok(body) => body,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("read {}: {error}", path.display())),
        };
        serde_json::from_str(&body)
            .map(Some)
            .map_err(|error| format!("parse {}: {error}", path.display()))
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

    /// 会话目录路径（测试/诊断用）：`sessions/{seed}`，不要求目录存在。
    pub fn session_path_dir(&self, seed: &str) -> PathBuf {
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

    /// 一批消息的最大 `msg_id`（无 id 消息不参与）。
    fn batch_max_msg_id(messages: &[Message]) -> u64 {
        messages
            .iter()
            .filter_map(|message| message.msg_id)
            .max()
            .unwrap_or(0)
    }

    fn extract_summary(messages: &[Message]) -> String {
        messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant" && !m.content.is_empty())
            .and_then(|m| {
                m.content.iter().find_map(|b| {
                    if let qaqh_types::ContentBlock::Text { text } = b {
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
            claimed_seeds: Mutex::new(std::collections::HashSet::new()),
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
        assert!(manager.persist_new_session_if_absent_with("lock-gc-1", None, |_| true));
        assert_eq!(
            lock_len(&manager),
            baseline + 1,
            "创建会话应在 session_locks 里留下一条"
        );

        manager.delete("lock-gc-1").expect("delete session");
        assert_eq!(
            lock_len(&manager),
            baseline,
            "删除会话后 session_locks 必须回落到创建前水位"
        );
    }
}

#[cfg(test)]
mod skill_persistence_tests {
    use super::*;
    use qaqh_types::{SkillSessionEntry, SkillSessionEntryState, SkillSessionStateV2};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    static TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    fn manager() -> (PathBuf, SessionManager) {
        let root = std::env::temp_dir().join(format!(
            "qaqh-session-skills-{}-{}-{}",
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
            claimed_seeds: Mutex::new(std::collections::HashSet::new()),
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
    fn save_full_preserves_tool_mode() {
        let (root, manager) = manager();

        // 先建会话再写模式（CK-PERSIST：持久化失败必须可观测，这里 expect）。
        manager.persist_new_session("seed");
        // minimal：save_full（undo/compact 的 snapshot_full 路径）不得把
        // tool_mode 覆盖回 standard（PLAN-TOOL-MODES.md 4.3 回归）。
        manager
            .persist_tool_mode("seed", "minimal", &[])
            .expect("persist minimal");
        manager.save_full("seed", &[Message::user("hello")], "model", None, 0, 1);
        let meta = manager.load_meta("seed").expect("metadata");
        assert_eq!(meta.tool_mode, "minimal");

        // custom：custom_tools 同样必须保留。
        manager
            .persist_tool_mode("seed", "custom", &["bash".into(), "grep".into()])
            .expect("persist custom");
        manager.save_full("seed", &[Message::user("hello again")], "model", None, 0, 2);
        let meta = manager.load_meta("seed").expect("metadata");
        assert_eq!(meta.tool_mode, "custom");
        assert_eq!(meta.custom_tools, vec!["bash", "grep"]);

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

    #[test]
    fn corrupt_compact_context_refuses_full_archive_fallback() {
        let (root, manager) = manager();
        let archive = vec![Message::user("one"), Message::user("two")];
        manager.save_full("corrupt-compact", &archive, "model", None, 0, 2);
        std::fs::write(
            manager.compact_context_path("corrupt-compact"),
            b"{not-json",
        )
        .expect("write corrupt compact context");

        assert!(
            manager.load_for_resume("corrupt-compact").is_none(),
            "damaged compact context must never fall back to the full archive"
        );
        // The immutable archive itself is untouched; recovery tooling can still
        // inspect it deliberately.
        assert_eq!(
            manager.load("corrupt-compact").expect("archive").1.len(),
            archive.len()
        );
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn compact_context_past_archive_refuses_full_archive_fallback() {
        let (root, manager) = manager();
        let archive = vec![Message::user("one"), Message::user("two")];
        manager.save_full("past-archive", &archive, "model", None, 0, 2);
        let damaged = CompactContext {
            version: 1,
            checkpoint_id: "damaged-checkpoint".into(),
            parent_checkpoint_id: None,
            created_at: SessionManager::now_epoch(),
            archive_message_count: archive.len() + 1,
            messages: vec![Message::user("summary")],
        };
        std::fs::write(
            manager.compact_context_path("past-archive"),
            serde_json::to_vec_pretty(&damaged).expect("serialize damaged compact context"),
        )
        .expect("write damaged compact context");

        assert!(
            manager.load_for_resume("past-archive").is_none(),
            "archive_message_count past the archive must fail closed"
        );
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn recent_projection_reads_bounded_tail() {
        // Phase 2 契约：投影重建路径只读尾部窗口。
        // 1. 尾部窗口内含最近 N 条（正向序）；
        // 2. compact context 完好时优先于归档尾部（与 resume 同构）；
        // 3. 损坏的 compact 不阻断投影（归档尾部兑底，区别于 resume 的整段拒绝）。
        let (root, manager) = manager();
        let archive: Vec<Message> = (1..=50)
            .map(|index| Message::user(&format!("msg-{index}")))
            .collect();
        manager.save_full("bounded-tail", &archive, "model", None, 0, 25);

        let recent = manager
            .load_recent_for_projection("bounded-tail", 10)
            .expect("session exists");
        assert_eq!(recent.len(), 10, "tail window must be bounded");
        assert_eq!(
            recent.last().and_then(text_of),
            Some("msg-50".to_string()),
            "tail must keep the newest message"
        );
        assert_eq!(
            recent.first().and_then(text_of),
            Some("msg-41".to_string()),
            "tail window must be the newest contiguous slice"
        );

        // compact context 优先：投影应看到 active 视图而非归档尾部。
        manager.save_compact_context(
            "bounded-tail",
            &[
                Message::user("[Compacted]\nsummary"),
                Message::user("msg-50"),
            ],
        );
        let with_compact = manager
            .load_recent_for_projection("bounded-tail", 5)
            .expect("session exists");
        assert_eq!(
            with_compact.len(),
            2,
            "compact context wins over archive tail"
        );

        // 损坏的 compact：投影降级到归档尾部（fail-open），不整段拒绝。
        std::fs::write(manager.compact_context_path("bounded-tail"), b"{not-json")
            .expect("corrupt compact context");
        let degraded = manager
            .load_recent_for_projection("bounded-tail", 5)
            .expect("session exists");
        assert_eq!(
            degraded.last().and_then(text_of),
            Some("msg-50".to_string()),
            "corrupt compact must degrade to archive tail, not fail"
        );

        // 磁盘上无此会话 → None（区别于空尾部）。
        assert!(manager.load_recent_for_projection("ghost", 5).is_none());
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    fn text_of(message: &Message) -> Option<String> {
        message.content.iter().find_map(|block| match block {
            qaqh_types::ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
    }
}

#[cfg(test)]
mod wal_recovery_tests {
    use super::*;
    use qaqh_message::PersistOp;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    static TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    fn manager() -> (PathBuf, SessionManager) {
        let root = std::env::temp_dir().join(format!(
            "qaqh-session-wal-{}-{}-{}",
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
            claimed_seeds: Mutex::new(std::collections::HashSet::new()),
        };
        (root, manager)
    }

    fn user_msg(id: u64, text: &str) -> Message {
        Message {
            msg_id: Some(id),
            role: "user".into(),
            name: None,
            content: vec![qaqh_types::ContentBlock::text(text)],
        }
    }

    fn append_op(seed: &str, messages: Vec<Message>) -> PersistOp {
        PersistOp::Append {
            seed: seed.to_string(),
            messages,
            model: "m".into(),
            effort: None,
            compact_skip: 0,
            turn_count: 1,
        }
    }

    /// Simulate the crash window: flush_meta logged the ops into the WAL, the
    /// process died before the host-side drain applied them.
    fn write_wal(dir: &std::path::Path, ops: &[PersistOp]) {
        let mut writer = qaqh_message::WalWriter::open(dir).expect("open wal");
        for op in ops {
            writer.log_op(op).expect("log op");
        }
        writer.sync().expect("sync wal");
    }

    /// BUG-2026-09-13-06 的调用方回归：read_ops 把中途 IO 错误当 EOF 时，
    /// replay_message_wal 会把截断的 op 集写回归档，随后 checkpoint 把尚未读到的
    /// 有效 op 永久删除。修复后必须：应用已读到的有效前缀，并且 **不** checkpoint。
    #[test]
    fn io_fault_during_wal_replay_never_checkpoints_the_log() {
        let (root, manager) = manager();
        let seed = "wal-io-fault";
        let dir = root.join("sessions").join(seed);
        std::fs::create_dir_all(&dir).expect("mkdir");
        manager.save_full(seed, &[user_msg(1, "archived")], "m", None, 0, 1);
        // 前两条 op 可读，第三条处在 IO 故障之后。
        write_wal(
            &dir,
            &[
                append_op(seed, vec![user_msg(2, "readable-a")]),
                append_op(seed, vec![user_msg(3, "readable-b")]),
                append_op(seed, vec![user_msg(4, "behind-the-fault")]),
            ],
        );
        // 故障点设在第 2 条 op 之后：前两条可读，第三条不可读。
        let fault_at = qaqh_message::wal_fault::prefix_bytes(
            2,
            &append_op(seed, vec![user_msg(2, "readable-a")]),
        );
        let _armed = qaqh_message::wal_fault::arm(qaqh_message::wal_fault::FaultPlan {
            pass: 2,
            skip_bytes: fault_at,
            kind: std::io::ErrorKind::Other,
        });

        let (_, messages, _) = manager.load_for_resume(seed).expect("resume");

        // 1) 可读前缀已应用（fail-open 的“应用”部分保留）。
        assert_eq!(
            messages.len(),
            3,
            "the readable prefix must still be applied: {messages:#?}"
        );
        // 2) 未读到的 op 必须仍在磁盘上（没有被 checkpoint 掉）。
        let wal = std::fs::read_to_string(dir.join("messages.wal")).expect("wal readable");
        assert!(
            wal.contains("behind-the-fault"),
            "the op behind the fault must survive on disk: {wal}"
        );
        // 3) 证据被隔离保留（wal.unreadable-*），而不是删除。
        let quarantined = std::fs::read_dir(&dir)
            .expect("readdir")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("wal.unreadable-")
            })
            .count();
        assert_eq!(quarantined, 1, "the faulting log must be kept as evidence");
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn un_drained_wal_ops_are_replayed_into_the_archive() {
        let (root, manager) = manager();
        let seed = "wal-replay";
        let dir = root.join("sessions").join(seed);
        std::fs::create_dir_all(&dir).expect("mkdir");
        manager.save_full(seed, &[user_msg(1, "archived")], "m", None, 0, 1);
        write_wal(
            &dir,
            &[append_op(
                seed,
                vec![user_msg(2, "round-a"), user_msg(3, "round-b")],
            )],
        );

        let (_, messages, _) = manager.load_for_resume(seed).expect("resume");
        assert_eq!(messages.len(), 3, "WAL ops must fold into the archive");
        assert_eq!(messages[1].msg_id, Some(2));

        // The WAL is checkpointed after replay: a second load must not
        // duplicate, and the log file is back to a bare header.
        let (_, again, _) = manager.load_for_resume(seed).expect("resume again");
        assert_eq!(again.len(), 3);
        assert!(qaqh_message::wal::read_ops(&dir).is_empty());
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn crash_between_apply_and_checkpoint_converges() {
        let (root, manager) = manager();
        let seed = "wal-dedupe";
        let dir = root.join("sessions").join(seed);
        std::fs::create_dir_all(&dir).expect("mkdir");
        manager.save_full(seed, &[user_msg(1, "archived")], "m", None, 0, 1);
        // Op applied to the archive, WAL checkpoint never ran (crash window).
        manager.apply_persist_op(&append_op(
            seed,
            vec![user_msg(2, "applied-but-wal-not-cleared")],
        ));
        write_wal(
            &dir,
            &[append_op(
                seed,
                vec![user_msg(2, "applied-but-wal-not-cleared")],
            )],
        );

        let (_, messages, _) = manager.load_for_resume(seed).expect("resume");
        assert_eq!(
            messages.len(),
            2,
            "msg_id dedupe must converge instead of double-applying"
        );
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    /// BUG-2026-09-12-07（首消息双写）：live drain 与 WAL replay 交错时，
    /// replay 已把 msg_id=1/2 写入归档，live drain 的同一批消息再次到达
    /// save_append —— 必须被幂等过滤，不得追加重复行。
    #[test]
    fn save_append_is_idempotent_against_already_archived_msg_ids() {
        let (root, manager) = manager();
        let seed = "append-dedupe";
        let dir = root.join("sessions").join(seed);
        std::fs::create_dir_all(&dir).expect("mkdir");
        // 模拟 replay 先写入了 system(1) + user(2)。
        manager.apply_persist_op(&append_op(
            seed,
            vec![
                {
                    let mut m = user_msg(1, "system text");
                    m.role = "system".into();
                    m
                },
                user_msg(2, "first user"),
            ],
        ));
        // live drain 的同一批消息（相同 msg_id、相同字节）随后到达。
        manager.save_append(
            seed,
            &[
                {
                    let mut m = user_msg(1, "system text");
                    m.role = "system".into();
                    m
                },
                user_msg(2, "first user"),
            ],
            "m",
            None,
            0,
            1,
        );

        let (_, messages, _) = manager.load_for_resume(seed).expect("resume");
        assert_eq!(
            messages.len(),
            2,
            "double-delivered messages must not be appended twice"
        );
        // 混合批次：一条重复 + 一条全新 → 只追加全新的那条。
        manager.save_append(
            seed,
            &[user_msg(2, "first user"), user_msg(3, "second user")],
            "m",
            None,
            2,
            2,
        );
        let (_, messages, _) = manager.load_for_resume(seed).expect("resume");
        assert_eq!(
            messages.len(),
            3,
            "only the genuinely new message must be appended"
        );
        assert_eq!(messages[2].msg_id, Some(3));
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    /// BUG-2026-09-12-07 读侧自愈回归（补落盘）：归档里的重复 msg_id 行
    /// 在读取时被 keep-first 去重，resume 只见干净视图。
    #[test]
    fn read_side_dedupes_repeated_msg_ids_keep_first() {
        let (root, manager) = manager();
        let seed = "read-dedupe";
        let dir = root.join("sessions").join(seed);
        std::fs::create_dir_all(&dir).expect("mkdir");
        manager.apply_persist_op(&append_op(
            seed,
            vec![user_msg(1, "first user"), user_msg(2, "second user")],
        ));
        // 模拟历史双写：归档尾部追加重复行（同 msg_id 同字节）。
        let archived_path = dir.join("messages.jsonl");
        let archived = std::fs::read_to_string(&archived_path).expect("read archive");
        let first_two: Vec<&str> = archived.lines().take(2).collect();
        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&archived_path)
                .expect("open archive for duplicate append");
            for line in &first_two {
                writeln!(file, "{line}").expect("append duplicate line");
            }
        }

        let (_, messages, _) = manager.load_for_resume(seed).expect("resume");
        assert_eq!(
            messages.len(),
            2,
            "repeated msg_id lines must be dropped on read (keep-first)"
        );
        assert_eq!(messages[0].msg_id, Some(1));
        assert_eq!(messages[1].msg_id, Some(2));
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn torn_wal_tail_preserves_prefix() {
        let (root, manager) = manager();
        let seed = "wal-torn";
        let dir = root.join("sessions").join(seed);
        std::fs::create_dir_all(&dir).expect("mkdir");
        manager.save_full(seed, &[user_msg(1, "archived")], "m", None, 0, 1);
        write_wal(&dir, &[append_op(seed, vec![user_msg(2, "complete-line")])]);
        let wal_path = dir.join("messages.wal");
        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&wal_path)
                .expect("open wal for torn tail");
            file.write_all(b"{\"seq\":2,\"op\":{\"App")
                .expect("write torn tail");
        }

        let (_, messages, _) = manager.load_for_resume(seed).expect("resume");
        assert_eq!(
            messages.len(),
            2,
            "ops before the torn tail must survive the crash"
        );
        std::fs::remove_dir_all(root).expect("remove test directory");
    }
}

#[cfg(test)]
mod seed_collision_tests {
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
            claimed_seeds: Mutex::new(std::collections::HashSet::new()),
        };
        // `attach_by_cwd` 需要 WorkspaceStore（进程内单例：其它用例可能
        // 已初始化过 → 忽略重复初始化）。
        let _ = std::panic::catch_unwind(|| {
            crate::grouping::WorkspaceStore::init(root.clone());
        });
        (root, manager)
    }

    /// BUG-2026-09-13-24 核心回归：分配器在候选 seed 已被占用时必须换一个
    /// 候选，而不是把新会话写进旧会话目录（旧实现无条件覆盖既有 meta）。
    #[test]
    fn unique_seed_allocation_retries_on_collision() {
        let (root, manager) = manager();

        // 预置一个「已存在」的会话，模拟碰撞目标。
        manager.persist_new_session("deadbeef");
        let existing_dir = manager.session_path_dir("deadbeef");
        store::write_meta(
            &existing_dir,
            &SessionMeta {
                seed: "deadbeef".into(),
                created_at: 1234,
                cwd: Some("D:/old-project".into()),
                ..Default::default()
            },
        )
        .expect("write old meta");
        std::fs::write(
            existing_dir.join("messages.jsonl"),
            "{\"role\":\"user\",\"content\":[],\"msg_id\":1}\n",
        )
        .expect("write old messages");

        // 闭包对 "deadbeef" 恒判「已占用」：分配器必须重试到别的候选，
        // 并且绝不返回被占用的那个。
        let mut attempts = 0usize;
        let chosen = SessionManager::generate_unique_seed(|candidate| {
            attempts += 1;
            candidate == "deadbeef" || manager.is_seed_taken(candidate)
        });
        assert_ne!(
            chosen, "deadbeef",
            "allocator must skip colliding candidates"
        );
        assert!(!manager.is_seed_taken(&chosen));
        assert!(attempts >= 1, "collision probe must run at least once");

        // 顺序回退路径：只放行 8 位十六进制号 "00000003"，随机候选必然
        // 全部碰撞（32 位空间里随机命中该值的概率可忽略）→ 有界回退必须
        // 选到它，而不是无界扫描或放弃碰撞检查。
        let sequential =
            SessionManager::try_generate_unique_seed(|candidate| candidate != "00000003", 16);
        assert_eq!(
            sequential.as_deref(),
            Some("00000003"),
            "bounded sequential fallback must pick the next free id"
        );

        // 极端病态：整个候选空间都判「已占用」时必须**显式失败**（返回
        // None），而不是无限扫描或把碰撞候选当成功值返回。
        assert!(
            SessionManager::try_generate_unique_seed(|_| true, 16).is_none(),
            "an exhausted id space must fail explicitly, not loop forever"
        );

        // 旧会话目录与旧 meta 必须逐字节不变（未被写穿）。
        let old_meta = manager.load_meta("deadbeef").expect("old meta");
        assert_eq!(old_meta.created_at, 1234);
        assert_eq!(old_meta.cwd.as_deref(), Some("D:/old-project"));
        assert_eq!(
            std::fs::read_to_string(existing_dir.join("messages.jsonl")).expect("old messages"),
            "{\"role\":\"user\",\"content\":[],\"msg_id\":1}\n"
        );

        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    /// 占用即拒绝：`persist_new_session_if_absent` 对既有 seed 不写一个字节。
    #[test]
    fn create_session_refuses_to_overwrite_existing_seed() {
        let (root, manager) = manager();

        manager.persist_new_session("occupied");
        let dir = manager.session_path_dir("occupied");
        store::write_meta(
            &dir,
            &SessionMeta {
                seed: "occupied".into(),
                created_at: 777,
                cwd: Some("D:/kept".into()),
                ..Default::default()
            },
        )
        .expect("write existing meta");
        std::fs::write(dir.join("messages.jsonl"), "existing-line\n").expect("write messages");

        assert!(
            !manager.persist_new_session_if_absent("occupied", Some("D:/new-cwd")),
            "existing seed must be rejected"
        );
        let meta = manager.load_meta("occupied").expect("meta survives");
        assert_eq!(meta.created_at, 777, "created_at must not be rewritten");
        assert_eq!(meta.cwd.as_deref(), Some("D:/kept"));
        assert_eq!(
            std::fs::read_to_string(dir.join("messages.jsonl")).expect("messages survive"),
            "existing-line\n"
        );

        // 全新 seed 正常创建（带占用钩子时占用登记同样生效）。
        assert!(
            manager.persist_new_session_if_absent_with("fresh-seed", None, |seed| {
                manager.claim_seed(seed)
            })
        );
        assert!(manager.is_seed_taken("fresh-seed"));
        assert!(
            !manager.persist_new_session_if_absent_with("fresh-seed-2", None, |_| false),
            "caller-side claim rejection must abort creation"
        );
        assert!(
            !manager.is_seed_taken("fresh-seed-2"),
            "rejected seed must not be materialized nor claimed"
        );
        assert_eq!(
            manager.load_meta("fresh-seed").expect("fresh meta").seed,
            "fresh-seed"
        );

        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    /// **确定性**碰撞回归（BUG-2026-09-13-24）：通过注入候选源强制
    /// `allocate_seed` 的第一个候选就是已占用的 sentinel seed。
    ///
    /// 未修复的实现（`generate_seed` 无碰撞检查 + `persist_new_session`
    /// 无条件覆盖）在此输入下必然把新会话写进 sentinel 目录 —— 本测试
    /// 会在断言 `chosen != sentinel` / sentinel meta 未变处失败。
    #[test]
    fn allocate_seed_retries_injected_collision_and_preserves_sentinel() {
        let (root, manager) = manager();

        let sentinel = "c0ffee01";
        manager.persist_new_session(sentinel);
        let sentinel_dir = manager.session_path_dir(sentinel);
        store::write_meta(
            &sentinel_dir,
            &SessionMeta {
                seed: sentinel.into(),
                created_at: 4242,
                cwd: Some("D:/sentinel-project".into()),
                ..Default::default()
            },
        )
        .expect("sentinel meta");
        std::fs::write(sentinel_dir.join("messages.jsonl"), "sentinel\n").expect("sentinel msgs");
        let meta_before = std::fs::read_to_string(sentinel_dir.join("meta.json")).expect("meta");
        let msgs_before =
            std::fs::read_to_string(sentinel_dir.join("messages.jsonl")).expect("msgs");

        // 注入：候选源第一次返回 sentinel（必碰撞），之后返回真实随机 seed。
        static COLLIDE_ONCE: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(true);
        COLLIDE_ONCE.store(true, std::sync::atomic::Ordering::SeqCst);
        fn injected() -> String {
            if COLLIDE_ONCE.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return "c0ffee01".to_string();
            }
            SessionManager::generate_seed()
        }
        SessionManager::set_seed_source_for_test(Some(injected));

        let (chosen, created) = manager.allocate_seed(Some("D:/new-project"));
        SessionManager::set_seed_source_for_test(None);

        assert!(
            created,
            "allocation must succeed after retrying the collision"
        );
        assert_ne!(
            chosen, sentinel,
            "allocator must retry a colliding candidate instead of reusing it"
        );
        assert_eq!(
            manager.load_meta(&chosen).expect("new meta").cwd.as_deref(),
            Some("D:/new-project"),
            "the allocated session must own its own cwd"
        );

        // sentinel 会话逐字节未变。
        assert_eq!(
            std::fs::read_to_string(sentinel_dir.join("meta.json")).expect("meta"),
            meta_before,
            "a colliding candidate must never overwrite the existing session meta"
        );
        assert_eq!(
            std::fs::read_to_string(sentinel_dir.join("messages.jsonl")).expect("msgs"),
            msgs_before,
            "a colliding candidate must never touch the existing session messages"
        );

        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    /// 并发回归（BUG-2026-09-13-24 返工）：**真实生产路径**（无注入源）
    /// 上并发 `allocate_seed` 不得出现「两个线程都通过碰撞检查 → 一个落
    /// 盘失败 → `session.new` 直接报错」的窗口。
    ///
    /// 复现手法：把候选生成钉死为同一个值（等价于 Windows 毫秒时钟粒度
    /// 下同 tick 连续调用），并发调用 `allocate_seed`。修复前必现
    /// 「胜者 1 个 + 败者返回 `("", false)` 或空 seed 错误」；修复后
    /// 败者必须**重试到新候选**并成功创建自己的会话。
    #[test]
    fn concurrent_allocate_seed_never_fails_on_a_repeated_candidate() {
        let (root, manager) = manager();
        let manager = std::sync::Arc::new(manager);

        // RAII：无论成功/panic 都必须解除注入源——它是进程级单例，
        // 残留会让同进程其它用例吃到固定候选（与集成测试同一类串扰）。
        struct SeedSourceGuard;
        impl Drop for SeedSourceGuard {
            fn drop(&mut self) {
                SessionManager::set_seed_source_for_test(None);
            }
        }
        let _guard = SeedSourceGuard;

        // 与生产一致的候选重复来源：注入源每次返回同一个候选值，
        // 但 `generate_from_source` 的重试上限把它耗尽后回退随机值——
        // 因此这里并发调用的窗口正是「选中候选 → 落盘」之间。
        fn repeated() -> String {
            "baadf00d".to_string()
        }
        SessionManager::set_seed_source_for_test(Some(repeated));

        let threads = 4;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(threads));
        let mut handles = Vec::new();
        for _ in 0..threads {
            let manager = manager.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                manager.allocate_seed(Some("D:/concurrent"))
            }));
        }
        let results: Vec<(String, bool)> = handles
            .into_iter()
            .map(|handle| handle.join().expect("join allocation thread"))
            .collect();

        let created: Vec<&(String, bool)> =
            results.iter().filter(|(_, created)| *created).collect();
        assert!(
            !created.is_empty(),
            "at least one allocation must succeed: {results:?}"
        );
        for (seed, ok) in &results {
            if *ok {
                assert!(!seed.is_empty(), "created allocation must carry a seed");
                let meta = manager.load_meta(seed).expect("created seed has meta");
                assert_eq!(meta.seed, *seed, "created meta must be self-owned");
            }
        }
        // 关键断言：失败者只能是「显式放弃（seed 为空 + false）」，
        // 且必须**仍然存在可用候选**时不得整体失败——
        // 修复前的表现是败者拿到 seed 但 created=false（调用方报错）。
        for (seed, ok) in &results {
            if !*ok {
                assert!(
                    seed.is_empty(),
                    "a losing allocator must not report a seed it did not create: {seed}"
                );
            }
        }

        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    /// `allocate_seed` 端到端：分配结果一定未被占用，且落盘时确实创建了
    /// 新会话（而不是复用/写穿既有目录）。
    #[test]
    fn allocate_seed_never_returns_an_occupied_seed() {
        let (root, manager) = manager();

        // 先占用一批 seed。
        for seed in ["aaaaaaaa", "bbbbbbbb", "cccccccc"] {
            manager.persist_new_session(seed);
        }
        // 索引登记但磁盘无目录的幽灵 seed（目录已删、索引尚未 compact）——
        // 复用该 seed 会让新会话继承旧索引条目的身份，故也必须视为已占用。
        store::upsert_index(
            &manager.sessions_dir,
            &SessionMeta {
                seed: "dddddddd".into(),
                created_at: 5,
                ..Default::default()
            },
        );

        let mut allocated: Vec<String> = Vec::new();
        for _ in 0..32 {
            let (seed, created) = manager.allocate_seed(None);
            assert!(
                created,
                "allocation must succeed in a nearly empty id space"
            );
            assert!(
                !allocated.contains(&seed),
                "allocated seeds must be unique within the process"
            );
            assert!(
                manager.session_dir(&seed).is_some(),
                "allocated seed must be materialized on disk"
            );
            assert!(
                manager.is_seed_taken(&seed),
                "an allocated seed must be claimed (never re-handed out)"
            );
            allocated.push(seed);
        }
        // 幽灵 seed（索引有、目录无）同样不能再被分配。
        assert!(!allocated.iter().any(|seed| seed == "dddddddd"));

        std::fs::remove_dir_all(root).expect("remove test directory");
    }
}

#[cfg(test)]
mod save_full_meta_preservation_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    static TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    fn manager() -> (PathBuf, SessionManager) {
        let root = std::env::temp_dir().join(format!(
            "qaqh-session-savefull-{}-{}-{}",
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
            claimed_seeds: Mutex::new(std::collections::HashSet::new()),
        };
        (root, manager)
    }

    /// BUG-2026-09-13-05 回归：save_full（undo/compact 全量重写）必须保留
    /// 全部持久化字段——不只 mode/tool_mode/title，还包括 cwd、
    /// frozen_annotation、usage_*、archived、ephemeral、context_stats。
    #[test]
    fn save_full_preserves_all_persisted_meta_fields() {
        let (_root, manager) = manager();
        let seed = "savefull-meta-seed";
        let dir = manager.session_path_dir(seed);
        std::fs::create_dir_all(&dir).expect("mkdir");

        // 先落一份字段齐全的既有 meta（模拟真实会话的持久化状态）。
        let existing = SessionMeta {
            seed: seed.into(),
            created_at: 1000,
            updated_at: 2000,
            model: "test-model".into(),
            title: Some("kept-title".into()),
            mode: 1,
            tool_mode: "custom".into(),
            custom_tools: vec!["edit".into()],
            cwd: Some("D:/project/demo".into()),
            frozen_annotation: Some("<Environment>frozen</Environment>".into()),
            archived: true,
            ephemeral: true,
            context_stats: Some(serde_json::json!({"k": "v"})),
            usage_totals: qaqh_types::UsageInfo {
                prompt_cache_hit_tokens: 111,
                ..Default::default()
            },
            usage_requests: 3,
            cache_reported_requests: 2,
            last_usage: Some(qaqh_types::UsageInfo::default()),
            ..Default::default()
        };
        store::write_meta(&dir, &existing).expect("write existing meta");

        // 模拟 undo/compact 路径：save_full 全量重写。
        let messages = vec![qaqh_types::Message::user("hello")];
        manager.save_full(seed, &messages, "new-model", Some("high"), 0, 1);

        let saved = manager.load_meta(seed).expect("reload meta");
        // 覆写字段按本次调用更新。
        assert_eq!(saved.model, "new-model");
        assert_eq!(saved.effort.as_deref(), Some("high"));
        assert_eq!(saved.message_count, 1);
        assert_eq!(saved.turn_count, 1);
        // 保留字段一个都不能丢。
        assert_eq!(saved.cwd.as_deref(), Some("D:/project/demo"));
        assert_eq!(
            saved.frozen_annotation.as_deref(),
            Some("<Environment>frozen</Environment>")
        );
        assert!(saved.archived, "archived must survive save_full");
        assert!(saved.ephemeral, "ephemeral must survive save_full");
        assert_eq!(saved.context_stats.as_ref().unwrap()["k"], "v");
        assert_eq!(saved.usage_totals.prompt_cache_hit_tokens, 111);
        assert_eq!(saved.usage_requests, 3);
        assert_eq!(saved.cache_reported_requests, 2);
        assert!(saved.last_usage.is_some());
        assert_eq!(saved.mode, 1);
        assert_eq!(saved.tool_mode, "custom");
        assert_eq!(saved.title.as_deref(), Some("kept-title"));
    }
}
