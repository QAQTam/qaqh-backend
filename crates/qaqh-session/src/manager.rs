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

use crate::canonical::{CANONICAL_IDENTITY_FILE, CanonicalSessionIdentity};
use crate::store;

static INSTANCE: OnceLock<Arc<SessionManager>> = OnceLock::new();

/// Derive the model-visible view from the immutable archive.
///
/// A compacted session keeps every original message in `messages.jsonl`; the
/// latest `[Compacted N turns]` message is the replacement summary and
/// `compact_covered_through_msg_id` marks the hidden prefix. The active view is
/// therefore `leading system messages + latest summary + every non-summary
/// message with msg_id > watermark`, regardless of the summary's physical
/// append position.
fn derive_active_messages(meta: &SessionMeta, archive: &[Message]) -> Result<Vec<Message>, String> {
    let Some(covered) = meta.compact_covered_through_msg_id else {
        return Ok(archive.to_vec());
    };
    let max_id = archive.iter().filter_map(|m| m.msg_id).max().unwrap_or(0);
    if covered > max_id {
        return Err(format!(
            "compact watermark {covered} points past archive max msg_id {max_id}"
        ));
    }
    let summary = archive
        .iter()
        .filter(|message| qaqh_message::is_compaction_summary(message))
        .filter(|message| message.msg_id.is_some_and(|id| id > covered))
        .max_by_key(|message| message.msg_id.unwrap_or(0))
        .ok_or_else(|| {
            format!("compact watermark {covered} has no newer [Compacted] summary in archive")
        })?;

    let mut active = Vec::new();
    let mut prefix_len = 0;
    while prefix_len < archive.len() && archive[prefix_len].role == "system" {
        active.push(archive[prefix_len].clone());
        prefix_len += 1;
    }
    active.push(summary.clone());
    for message in &archive[prefix_len..] {
        if qaqh_message::is_compaction_summary(message) {
            continue;
        }
        if message.msg_id.is_none_or(|id| id > covered) {
            active.push(message.clone());
        }
    }
    Ok(active)
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
    claimed_sessions: Mutex<std::collections::HashSet<String>>,
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
        let dir = self.session_path_dir(session_id);
        // 水位缓存按**目录路径**键控（不依赖目录是否存在），所以失效必须
        // 先于删除、且不因目录查询失败而跳过——否则同 seed 重建会读到旧
        // 水位，把新会话的低 msg_id 首批全部误判为已归档。
        store::invalidate_watermark(&dir);
        let dir = self
            .session_dir(session_id)
            .ok_or_else(|| format!("Session not found: {session_id}"))?;

        let _legacy_writer = LegacyWriterFacade::lock();
        std::fs::remove_dir_all(&dir).map_err(|e| format!("Failed to delete session: {e}"))?;

        store::remove_from_index(&self.sessions_dir, session_id);
        // 同步清理 workspace 账户（会话删除后不留悬空引用）。
        crate::grouping::WorkspaceStore::global().remove_session(session_id);
        // D-4：释放 per-seed 锁槽位与占用登记。两者都以 seed 为键、只在
        // 创建/首次取锁时插入，删除路径若不回收，长驻 daemon 每删一个会话就
        // 永久多留一条（无界增长；`release_seed_claim` 清的是另一个 map）。
        // 此处已释放全部其它锁，再取 `session_locks` 不引入反向获取顺序。
        self.session_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
        self.release_session_claim(session_id);

        log::info!("SessionManager: deleted session {session_id}");
        Ok(())
    }

    // ── Load / Save ──

    /// Read the persisted JSONL files for a session.
    pub fn load(&self, session_id: &str) -> Option<(SessionMeta, Vec<Message>)> {
        self.snapshot_from_files(session_id).ok()
    }

    /// Load the immutable archive and the active model view derived from it.
    ///
    /// The third element is always the model-visible view: either the full
    /// archive (no compaction) or `summary + messages after the covered
    /// watermark`. The second element remains the raw archive for
    /// replay/pagination and human-facing projection.
    ///
    /// Fail-closed compact semantics: a watermark past the archive or without a
    /// matching newer summary returns `None`. Compacted history must never
    /// become reversible because the marker is damaged.
    ///
    /// Phase 2 note: this path reads the **full** archive (the model loop
    /// needs complete history). Projection-only consumers (timeline rebuild)
    /// should use [`Self::load_archive_tail`] instead — bounded tail read, no
    /// full-file cost.
    pub fn load_for_resume(
        &self,
        session_id: &str,
    ) -> Option<(SessionMeta, Vec<Message>, Vec<Message>)> {
        // L2 recovery: fold any un-drained WAL ops into the archive BEFORE any
        // consumer projects from it (worker resume, conversation snapshot,
        // timeline rebuild all funnel through here). Idempotent — see
        // `replay_message_wal`.
        self.replay_message_wal(session_id);
        let (meta, archive_messages) = self.load(session_id)?;
        let active_messages = match derive_active_messages(&meta, &archive_messages) {
            Ok(messages) => messages,
            Err(error) => {
                log::error!(
                    "SessionManager: compact watermark for {session_id} is invalid ({error}) \
                     — refusing full-history fallback"
                );
                return None;
            }
        };
        Some((meta, archive_messages, active_messages))
    }

    /// Phase 2（有界恢复）：只读**最近 `recent` 条**消息做投影重建。
    ///
    /// - 无 compact marker：反向扫描（`store::bounded_read`）只触碰文件尾部，
    ///   GB 级归档的重建从 O(文件) 降到 O(尾部)。
    /// - 有 compact marker：活跃视图由归档水位推导；为保证摘要与保留段
    ///   完整，这里复用 `load_for_resume` 的派生结果。
    ///
    /// 返回 `None` 表示磁盘上无该会话（区别于“有会话但尾部为空”——那返回
    /// 空 Vec）。WAL fold 与 `load_for_resume` 相同，先折幂等。
    pub fn load_recent_for_projection(
        &self,
        session_id: &str,
        recent: usize,
    ) -> Option<Vec<Message>> {
        self.replay_message_wal(session_id);
        let meta = self.load_meta(session_id)?;
        if meta.compact_covered_through_msg_id.is_some() {
            let (_, _, active_messages) = self.load_for_resume(session_id)?;
            return Some(active_messages);
        }
        let dir = self.session_path_dir(session_id);
        let messages =
            crate::store::bounded_read::read_messages_tail(&dir.join("messages.jsonl"), recent);
        Some(
            messages
                .into_iter()
                .filter(|message| !qaqh_message::is_compaction_summary(message))
                .collect(),
        )
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
    /// 全局回合序号就无从算起。摘要现在是归档里的真实行，因此本函数显式
    /// 过滤它，只把真实回合交给人类 transcript。
    ///
    /// 语义边界与 `load_recent_for_projection` 相同：`None` = 磁盘上无该会话
    /// （区别于「有会话但尾部为空」——那返回空 Vec）。WAL 同样先折（幂等）。
    pub fn load_archive_tail(&self, session_id: &str, recent: usize) -> Option<Vec<Message>> {
        self.replay_message_wal(session_id);
        self.session_dir(session_id)?;
        let dir = self.session_path_dir(session_id);
        Some(
            crate::store::bounded_read::read_messages_tail(&dir.join("messages.jsonl"), recent)
                .into_iter()
                .filter(|message| !qaqh_message::is_compaction_summary(message))
                .collect(),
        )
    }

    /// The single `PersistOp` → store mapping (PR-1-6 / Z5). The runtime's
    /// drain loop and the WAL recovery path both funnel through this method,
    /// so the mapping exists exactly once and replayed ops take byte-identical
    /// write paths to live ops. The shadow test in qaqh-message locks it.
    pub fn apply_persist_op(&self, op: &qaqh_message::PersistOp) {
        match op {
            qaqh_message::PersistOp::Append {
                session_id,
                messages,
                model,
                effort,
                compact_skip,
                compact_covered_through_msg_id,
                turn_count,
            } => {
                self.save_append_with_watermark(
                    session_id,
                    messages,
                    model,
                    effort.as_deref(),
                    *compact_skip,
                    *compact_covered_through_msg_id,
                    *turn_count,
                );
            }
            qaqh_message::PersistOp::UpdateMeta {
                session_id,
                model,
                effort,
                compact_skip,
                turn_count,
            } => {
                self.update_meta(
                    session_id,
                    model,
                    effort.as_deref(),
                    *compact_skip,
                    *turn_count,
                );
            }
            qaqh_message::PersistOp::SaveFull {
                session_id,
                messages,
                model,
                effort,
                compact_skip,
                compact_covered_through_msg_id,
                turn_count,
            } => {
                self.save_full_with_watermark(
                    session_id,
                    messages,
                    model,
                    effort.as_deref(),
                    *compact_skip,
                    *compact_covered_through_msg_id,
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
    /// - `SaveFull` never reaches the WAL (generation rewrite — see
    ///   `qaqh_message::wal` module docs).
    ///
    /// Single-writer invariant: a non-empty WAL implies the previous worker
    /// died before draining, so no live writer exists for this seed while
    /// replay runs. Per-op application still takes the per-seed lock.
    fn replay_message_wal(&self, session_id: &str) {
        let dir = self.session_path_dir(session_id);
        let mut reader = match qaqh_message::wal::open_reader(&dir) {
            Ok(Some(reader)) => reader,
            Ok(None) => return,
            Err(error) => {
                // Fail-closed: an unreadable WAL is NOT an empty WAL. Applying
                // nothing (and, above all, not checkpointing) keeps the ops
                // replayable once the fault clears; a fresh WAL is never
                // installed over unread ops.
                log::error!(
                    "SessionManager: cannot open WAL for {session_id} ({error}) — skipping replay, \
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
                        "SessionManager: WAL read for {session_id} failed after {} op(s) ({error}) — \
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
            "SessionManager: replaying {} WAL op(s) for {session_id}",
            ops.len()
        );
        let mut applied_max_msg_id = self
            .load(session_id)
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
                    session_id: op_session,
                    messages,
                    model,
                    effort,
                    compact_skip,
                    compact_covered_through_msg_id,
                    turn_count,
                } => {
                    let fresh: Vec<Message> = messages
                        .into_iter()
                        .filter(|message| message.msg_id.is_none_or(|id| id > applied_max_msg_id))
                        .collect();
                    if fresh.is_empty() && compact_covered_through_msg_id.is_none() {
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
                        session_id: op_session,
                        messages: fresh,
                        model,
                        effort,
                        compact_skip,
                        compact_covered_through_msg_id,
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
            log::error!("SessionManager: WAL checkpoint for {session_id} failed: {error}");
        }
    }

    /// Check whether a session exists on disk.
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
        let meta = self.load_meta(session_id)?;
        if let Some(cwd) = meta.cwd.as_deref().filter(|c| !c.is_empty()) {
            // 存量修复：历史版本在非 Windows 上写入的 `\` 形态（事故
            // 692d1605 meta.json 反斜杠 cwd，2026-09-06 排查项）。
            return Some(crate::grouping::repair_legacy_backslash_cwd(cwd));
        }
        // 惰性迁移：旧 workspace.txt → meta.cwd
        let txt_path = qaqh_types::platform::sessions_dir()
            .join(session_id)
            .join("workspace.txt");
        let legacy = std::fs::read_to_string(&txt_path).ok()?;
        let legacy = legacy.trim().to_string();
        if legacy.is_empty() {
            return None;
        }
        let canonical = crate::grouping::canonical_cwd(std::path::Path::new(&legacy));
        self.set_cwd(session_id, &canonical, true);
        let _ = std::fs::remove_file(&txt_path);
        Some(canonical)
    }

    /// Persist agent mode to meta.json without rewriting messages.
    /// Called when the user switches PLAN/CODE mode so it survives agent restart.
    pub fn persist_mode(&self, session_id: &str, mode: u8) {
        self.with_meta_locked(session_id, false, |dir, meta| {
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
        session_id: &str,
        tool_mode: &str,
        custom_tools: &[String],
    ) -> Result<(), String> {
        self.with_meta_locked(session_id, true, |dir, meta| {
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
                "[TOOL MODE] persisted {normalized} for {session_id} ({} custom tools)",
                custom_tools.len()
            );
            Ok(())
        })
    }

    pub fn persist_skills(&self, session_id: &str, skills: qaqh_types::SkillSessionStateV2) {
        self.with_meta_locked(session_id, true, |dir, meta| {
            let now = Self::now_epoch();
            meta.session_id = session_id.to_string();
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
    pub fn persist_frozen_annotation(&self, session_id: &str, annotation: &str) {
        self.with_meta_locked(session_id, true, |dir, meta| {
            let now = Self::now_epoch();
            meta.session_id = session_id.to_string();
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
    pub fn set_archived(&self, session_id: &str, archived: bool) {
        self.with_meta_locked(session_id, false, |dir, meta| {
            if meta.session_id.is_empty() {
                meta.session_id = session_id.to_string();
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
    pub fn set_cwd(&self, session_id: &str, cwd: &str, index: bool) {
        // 空 cwd 双保险：canonical_cwd("") = "" 会清掉已有工作区（前端重启后
        // 回空 cwd 的 bug 通道）；空串直接忽略，永不破坏现有归属。
        if cwd.trim().is_empty() {
            return;
        }
        self.with_meta_locked(session_id, true, |dir, meta| {
            if meta.session_id.is_empty() {
                meta.session_id = session_id.to_string();
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
                crate::grouping::WorkspaceStore::global().attach_by_cwd(session_id, cwd);
            }
        });
    }

    /// 该 seed 是否为临时会话（子代理）：meta 存在且标记 ephemeral。
    /// 目录缺失（已清理）视为非临时，避免误触发删除路径。
    pub fn is_ephemeral(&self, session_id: &str) -> bool {
        self.session_dir(session_id).is_some()
            && self
                .load_meta(session_id)
                .map(|m| m.ephemeral)
                .unwrap_or(false)
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

    /// 上下文统计快照（可再生缓存）。写入 meta.json；只有正规会话
    /// （`created_at > 0`，即 persist_new_session 建立过）才同步索引——
    /// 子代理 worker 的 dashboard/compact 路径不会污染会话列表。
    pub fn set_context_stats(&self, session_id: &str, stats: &serde_json::Value) {
        self.with_meta_locked(session_id, true, |dir, meta| {
            if meta.session_id.is_empty() {
                meta.session_id = session_id.to_string();
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
        let _legacy_writer = LegacyWriterFacade::lock();
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

        let now = Self::now_epoch();
        let mut meta = store::read_meta(&dir).unwrap_or_default();
        meta.session_id = session_id.to_string();
        meta.created_at = now;
        meta.updated_at = now;
        meta.ephemeral = !index_session;
        meta.cwd = cwd.map(|c| crate::grouping::canonical_cwd(std::path::Path::new(c)));
        if !dir.join("messages.jsonl").exists()
            && let Err(error) = store::append_messages(&dir, &[])
        {
            log::error!("SessionManager: append_messages(initial) failed: {error}");
        }
        if let Err(error) = store::write_meta(&dir, &meta) {
            return cleanup_identity_dir(format!("write_meta(initial) failed: {error}"));
        }
        if index_session {
            store::upsert_index(&self.sessions_dir, &meta);
            if let Some(cwd) = meta.cwd.as_deref() {
                crate::grouping::WorkspaceStore::global().attach_by_cwd(session_id, cwd);
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
                    log::warn!(
                        "[session] allocate_session: candidate {session_id} failed: {error}; retrying"
                    );
                }
            }
        }
        Err(format!(
            "allocate_session: exhausted {} canonical identity attempts",
            Self::SESSION_ALLOCATION_ATTEMPTS
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
            if !dir.join("messages.jsonl").exists() {
                let _ = store::append_messages(dir, &[]);
            }
            let _ = store::write_meta(dir, meta);
            store::upsert_index(&self.sessions_dir, meta);
            if let Some(cwd) = meta.cwd.as_deref() {
                crate::grouping::WorkspaceStore::global().attach_by_cwd(session_id, cwd);
            }
        });
    }

    pub fn persist_usage(
        &self,
        session_id: &str,
        totals: qaqh_types::UsageInfo,
        last_usage: Option<qaqh_types::UsageInfo>,
        requests: u32,
        cache_reported_requests: u32,
    ) {
        self.with_meta_locked(session_id, true, |dir, meta| {
            meta.session_id = session_id.to_string();
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
    pub fn save_one(&self, session_id: &str, msg: &Message) {
        self.with_meta_locked(session_id, true, |dir, meta| {
            let now = Self::now_epoch();
            meta.session_id = session_id.to_string();
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
        session_id: &str,
        model: &str,
        effort: Option<&str>,
        compact_skip: usize,
        turn_count: usize,
    ) {
        let now = Self::now_epoch();
        self.with_meta_locked(session_id, false, |dir, meta| {
            meta.session_id = session_id.to_string();
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
    pub fn update_title(&self, session_id: &str, title: &str) {
        self.with_meta_locked(session_id, false, |dir, meta| {
            meta.session_id = session_id.to_string();
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
    /// Used for initial save or after undo/image repair.
    pub fn save_full(
        &self,
        session_id: &str,
        messages: &[Message],
        model: &str,
        effort: Option<&str>,
        compact_skip: usize,
        turn_count: usize,
    ) {
        self.save_full_with_watermark(
            session_id,
            messages,
            model,
            effort,
            compact_skip,
            None,
            turn_count,
        );
    }

    /// Full rewrite that optionally preserves an archive-derived compact
    /// watermark. `None` clears the marker; `Some(id)` keeps the rewritten
    /// active view filtered after restart.
    #[allow(clippy::too_many_arguments)]
    pub fn save_full_with_watermark(
        &self,
        session_id: &str,
        messages: &[Message],
        model: &str,
        effort: Option<&str>,
        compact_skip: usize,
        compact_covered_through_msg_id: Option<u64>,
        turn_count: usize,
    ) {
        let lock = self.session_lock(session_id);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let now = Self::now_epoch();
        let dir = self.session_path_dir(session_id);
        let _ = std::fs::create_dir_all(&dir);

        let created_at = self
            .load_meta(session_id)
            .map(|m| m.created_at)
            .unwrap_or(now);

        let existing = self.load_meta(session_id).unwrap_or_default();
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
        meta.session_id = session_id.to_string();
        meta.created_at = created_at;
        meta.updated_at = now;
        meta.model = model.to_string();
        meta.effort = effort.map(String::from);
        meta.message_count = messages.len();
        meta.turn_count = turn_count;
        meta.last_summary = last_summary;
        meta.compact_skip = compact_skip;
        meta.compact_covered_through_msg_id = compact_covered_through_msg_id;
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
        session_id: &str,
        new_messages: &[Message],
        model: &str,
        effort: Option<&str>,
        compact_skip: usize,
        turn_count: usize,
    ) {
        self.save_append_with_watermark(
            session_id,
            new_messages,
            model,
            effort,
            compact_skip,
            None,
            turn_count,
        );
    }

    /// Append new messages and optionally advance the compact watermark in the
    /// same meta critical section. `None` preserves the existing watermark.
    #[allow(clippy::too_many_arguments)]
    pub fn save_append_with_watermark(
        &self,
        session_id: &str,
        new_messages: &[Message],
        model: &str,
        effort: Option<&str>,
        compact_skip: usize,
        compact_covered_through_msg_id: Option<u64>,
        turn_count: usize,
    ) {
        let now = Self::now_epoch();
        self.with_meta_locked(session_id, true, |dir, meta| {
            // A WAL replay can arrive after the summary bytes were appended but
            // before meta.json was updated. In that case the message batch is
            // already archived, but the watermark still must be applied.
            let mut persist_watermark_only = || {
                let Some(covered) = compact_covered_through_msg_id else {
                    return;
                };
                meta.session_id = session_id.to_string();
                meta.updated_at = now;
                meta.compact_covered_through_msg_id = Some(covered);
                if let Err(e) = store::write_meta(dir, meta) {
                    log::error!("SessionManager: watermark-only meta write failed: {e}");
                    return;
                }
                store::upsert_index(&self.sessions_dir, meta);
            };

            if new_messages.is_empty() {
                persist_watermark_only();
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
                persist_watermark_only();
                return;
            }
            if meta.created_at == 0 {
                meta.created_at = now;
            }
            let last_summary = Self::extract_summary(new_messages);
            meta.session_id = session_id.to_string();
            meta.updated_at = now;
            meta.model = model.to_string();
            meta.effort = effort.map(String::from);
            meta.message_count = meta.message_count.saturating_add(fresh.len());
            meta.turn_count = turn_count;
            meta.last_summary = last_summary;
            meta.compact_skip = compact_skip;
            if let Some(covered) = compact_covered_through_msg_id {
                meta.compact_covered_through_msg_id = Some(covered);
            }

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

    // ── Session identity migration ──

    /// Resolve a legacy directory seed to its canonical identity.
    ///
    /// This is the read-only compatibility boundary: it never creates an
    /// identity and never enters new canonical facts.
    pub fn canonical_identity_for_session(
        &self,
        session_id: &str,
    ) -> Result<Option<CanonicalSessionIdentity>, String> {
        let dir = self.session_path_dir(session_id);
        if dir.is_dir() && dir.join(CANONICAL_IDENTITY_FILE).exists() {
            return CanonicalSessionIdentity::open(&dir)
                .map(Some)
                .map_err(|error| format!("read canonical identity at {}: {error}", dir.display()));
        }
        Ok(None)
    }

    // ── Helpers ──

    /// Maximum seed-allocation attempts before the counter fallback kicks in.
    /// 2^32 的 id 空间下连续 64 次随机命中同一批既有 seed 是病态事件，但
    /// 一旦发生必须收敛而不是无限重试。
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
        session_id: &str,
        create_dir: bool,
        f: impl FnOnce(&PathBuf, &mut SessionMeta) -> R,
    ) -> R {
        let lock = self.session_lock(session_id);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let _legacy_writer = LegacyWriterFacade::lock();
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

    fn snapshot_from_files(&self, session_id: &str) -> Result<(SessionMeta, Vec<Message>), String> {
        let dir = self
            .session_dir(session_id)
            .ok_or_else(|| format!("session directory is missing: {session_id}"))?;
        let meta = store::read_meta(&dir)
            .ok_or_else(|| format!("meta.json is missing or unreadable: {session_id}"))?;
        let messages = read_messages_without_deduplication(&dir.join("messages.jsonl"))?;
        Ok((meta, messages))
    }

    /// 会话目录路径（测试/诊断用）：`sessions/{seed}`，不要求目录存在。
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

    /// Resolve a canonical `SessionId` to its seed-keyed session directory.
    ///
    /// BETA-01 will eventually make the directory name equal the canonical id.
    /// Until then, the identity sidecar is the only durable mapping and this
    /// resolver scans for it without creating missing identities.
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
            claimed_sessions: Mutex::new(std::collections::HashSet::new()),
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
        assert_eq!(listed[0].session_id, "file-only");
        let (meta, messages) = manager.load("file-only").expect("file snapshot");
        assert_eq!(meta.session_id, "file-only");
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
        assert_eq!(meta.session_id, "seed");
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

    fn stamped(id: u64, role: &str, text: &str) -> Message {
        Message {
            msg_id: Some(id),
            role: role.into(),
            name: None,
            content: vec![qaqh_types::ContentBlock::text(text)],
        }
    }

    #[test]
    fn compact_marker_preserves_archive_and_derives_active_view() {
        let (root, manager) = manager();
        let archive = vec![
            stamped(1, "system", "base"),
            stamped(2, "user", "one"),
            stamped(3, "assistant", "reply one"),
            stamped(4, "user", "two"),
            stamped(5, "assistant", "reply two"),
        ];
        manager.save_full("compact-seed", &archive, "model", None, 0, 2);
        manager.save_append_with_watermark(
            "compact-seed",
            &[stamped(6, "user", "[Compacted 1 turns]\nsummary")],
            "model",
            None,
            0,
            Some(3),
            2,
        );

        let (_, restored_archive, active) =
            manager.load_for_resume("compact-seed").expect("resume");
        assert_eq!(restored_archive.len(), 6, "archive must remain append-only");
        assert_eq!(active.len(), 4, "system + summary + two kept messages");
        assert_eq!(active[0].role, "system");
        assert!(qaqh_message::is_compaction_summary(&active[1]));
        assert_eq!(active[2].msg_id, Some(4));
        assert_eq!(active[3].msg_id, Some(5));
        assert!(
            !manager
                .session_path_dir("compact-seed")
                .join("compact-context.json")
                .exists(),
            "route 1 must not create a second compact truth source"
        );

        let tail = manager
            .load_archive_tail("compact-seed", 10)
            .expect("archive tail");
        assert!(
            tail.iter()
                .all(|message| !qaqh_message::is_compaction_summary(message)),
            "human transcript must not show the synthetic summary"
        );
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn repeated_compaction_uses_latest_summary_and_watermark() {
        let (root, manager) = manager();
        let archive = vec![
            stamped(1, "system", "base"),
            stamped(2, "user", "one"),
            stamped(3, "assistant", "reply one"),
            stamped(4, "user", "two"),
            stamped(5, "assistant", "reply two"),
        ];
        manager.save_full("multi-compact", &archive, "model", None, 0, 2);
        manager.save_append_with_watermark(
            "multi-compact",
            &[stamped(6, "user", "[Compacted 1 turns]\nfirst")],
            "model",
            None,
            0,
            Some(3),
            2,
        );
        manager.save_append_with_watermark(
            "multi-compact",
            &[stamped(7, "user", "[Compacted 2 turns]\nsecond")],
            "model",
            None,
            0,
            Some(5),
            2,
        );

        let (_, archive, active) = manager.load_for_resume("multi-compact").expect("resume");
        assert_eq!(archive.len(), 7, "both summaries remain in the archive");
        assert_eq!(
            active.len(),
            2,
            "only latest summary + post-watermark messages"
        );
        assert!(qaqh_message::is_compaction_summary(&active[1]));
        assert!(text_of(&active[1]).unwrap_or_default().contains("second"));
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn invalid_compact_watermark_fails_closed() {
        let (root, manager) = manager();
        manager.save_full(
            "past-watermark",
            &[stamped(1, "user", "one")],
            "model",
            None,
            0,
            1,
        );
        manager.save_append_with_watermark(
            "past-watermark",
            &[stamped(2, "user", "[Compacted 1 turns]\nsummary")],
            "model",
            None,
            0,
            Some(99),
            1,
        );
        assert!(
            manager.load_for_resume("past-watermark").is_none(),
            "watermark past the archive must fail closed"
        );

        manager.save_full(
            "missing-summary",
            &[stamped(1, "user", "one")],
            "model",
            None,
            0,
            1,
        );
        manager.save_append_with_watermark(
            "missing-summary",
            &[stamped(2, "user", "ordinary")],
            "model",
            None,
            0,
            Some(1),
            1,
        );
        assert!(
            manager.load_for_resume("missing-summary").is_none(),
            "watermark without a newer summary must fail closed"
        );
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn recent_projection_reads_bounded_tail_without_compaction() {
        let (root, manager) = manager();
        let archive: Vec<Message> = (1..=50)
            .map(|index| stamped(index, "user", &format!("msg-{index}")))
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
            claimed_sessions: Mutex::new(std::collections::HashSet::new()),
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

    fn append_op(session_id: &str, messages: Vec<Message>) -> PersistOp {
        PersistOp::Append {
            session_id: session_id.to_string(),
            messages,
            model: "m".into(),
            effort: None,
            compact_skip: 0,
            compact_covered_through_msg_id: None,
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
        let session_id = "wal-io-fault";
        let dir = root.join("sessions").join(session_id);
        std::fs::create_dir_all(&dir).expect("mkdir");
        manager.save_full(session_id, &[user_msg(1, "archived")], "m", None, 0, 1);
        // 前两条 op 可读，第三条处在 IO 故障之后。
        write_wal(
            &dir,
            &[
                append_op(session_id, vec![user_msg(2, "readable-a")]),
                append_op(session_id, vec![user_msg(3, "readable-b")]),
                append_op(session_id, vec![user_msg(4, "behind-the-fault")]),
            ],
        );
        // 故障点设在第 2 条 op 之后：前两条可读，第三条不可读。
        let fault_at = qaqh_message::wal_fault::prefix_bytes(
            2,
            &append_op(session_id, vec![user_msg(2, "readable-a")]),
        );
        let _armed = qaqh_message::wal_fault::arm(qaqh_message::wal_fault::FaultPlan {
            pass: 2,
            skip_bytes: fault_at,
            kind: std::io::ErrorKind::Other,
        });

        let (_, messages, _) = manager.load_for_resume(session_id).expect("resume");

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
    fn wal_replay_applies_watermark_when_summary_is_already_archived() {
        let (root, manager) = manager();
        let session_id = "wal-compact-watermark";
        let dir = root.join("sessions").join(session_id);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let mut system = user_msg(1, "system");
        system.role = "system".into();
        let archive = vec![
            system,
            user_msg(2, "one"),
            user_msg(3, "reply one"),
            user_msg(4, "two"),
            user_msg(5, "reply two"),
        ];
        manager.save_full(session_id, &archive, "m", None, 0, 2);
        let summary = user_msg(6, "[Compacted 1 turns]\nsummary");
        manager.save_append(session_id, std::slice::from_ref(&summary), "m", None, 0, 2);

        // Simulate the crash window: summary bytes reached the archive, but
        // meta.json still lacks the watermark and the op remains in WAL.
        write_wal(
            &dir,
            &[PersistOp::Append {
                session_id: session_id.to_string(),
                messages: vec![summary],
                model: "m".into(),
                effort: None,
                compact_skip: 0,
                compact_covered_through_msg_id: Some(3),
                turn_count: 2,
            }],
        );

        let (meta, _, active) = manager.load_for_resume(session_id).expect("resume");
        assert_eq!(meta.compact_covered_through_msg_id, Some(3));
        assert_eq!(active.len(), 4, "system + summary + two kept messages");
        assert!(qaqh_message::is_compaction_summary(&active[1]));
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn un_drained_wal_ops_are_replayed_into_the_archive() {
        let (root, manager) = manager();
        let session_id = "wal-replay";
        let dir = root.join("sessions").join(session_id);
        std::fs::create_dir_all(&dir).expect("mkdir");
        manager.save_full(session_id, &[user_msg(1, "archived")], "m", None, 0, 1);
        write_wal(
            &dir,
            &[append_op(
                session_id,
                vec![user_msg(2, "round-a"), user_msg(3, "round-b")],
            )],
        );

        let (_, messages, _) = manager.load_for_resume(session_id).expect("resume");
        assert_eq!(messages.len(), 3, "WAL ops must fold into the archive");
        assert_eq!(messages[1].msg_id, Some(2));

        // The WAL is checkpointed after replay: a second load must not
        // duplicate, and the log file is back to a bare header.
        let (_, again, _) = manager.load_for_resume(session_id).expect("resume again");
        assert_eq!(again.len(), 3);
        assert!(qaqh_message::wal::read_ops(&dir).is_empty());
        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn crash_between_apply_and_checkpoint_converges() {
        let (root, manager) = manager();
        let session_id = "wal-dedupe";
        let dir = root.join("sessions").join(session_id);
        std::fs::create_dir_all(&dir).expect("mkdir");
        manager.save_full(session_id, &[user_msg(1, "archived")], "m", None, 0, 1);
        // Op applied to the archive, WAL checkpoint never ran (crash window).
        manager.apply_persist_op(&append_op(
            session_id,
            vec![user_msg(2, "applied-but-wal-not-cleared")],
        ));
        write_wal(
            &dir,
            &[append_op(
                session_id,
                vec![user_msg(2, "applied-but-wal-not-cleared")],
            )],
        );

        let (_, messages, _) = manager.load_for_resume(session_id).expect("resume");
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
        let session_id = "append-dedupe";
        let dir = root.join("sessions").join(session_id);
        std::fs::create_dir_all(&dir).expect("mkdir");
        // 模拟 replay 先写入了 system(1) + user(2)。
        manager.apply_persist_op(&append_op(
            session_id,
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
            session_id,
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

        let (_, messages, _) = manager.load_for_resume(session_id).expect("resume");
        assert_eq!(
            messages.len(),
            2,
            "double-delivered messages must not be appended twice"
        );
        // 混合批次：一条重复 + 一条全新 → 只追加全新的那条。
        manager.save_append(
            session_id,
            &[user_msg(2, "first user"), user_msg(3, "second user")],
            "m",
            None,
            2,
            2,
        );
        let (_, messages, _) = manager.load_for_resume(session_id).expect("resume");
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
        let session_id = "read-dedupe";
        let dir = root.join("sessions").join(session_id);
        std::fs::create_dir_all(&dir).expect("mkdir");
        manager.apply_persist_op(&append_op(
            session_id,
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

        let (_, messages, _) = manager.load_for_resume(session_id).expect("resume");
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
        let session_id = "wal-torn";
        let dir = root.join("sessions").join(session_id);
        std::fs::create_dir_all(&dir).expect("mkdir");
        manager.save_full(session_id, &[user_msg(1, "archived")], "m", None, 0, 1);
        write_wal(
            &dir,
            &[append_op(session_id, vec![user_msg(2, "complete-line")])],
        );
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

        let (_, messages, _) = manager.load_for_resume(session_id).expect("resume");
        assert_eq!(
            messages.len(),
            2,
            "ops before the torn tail must survive the crash"
        );
        std::fs::remove_dir_all(root).expect("remove test directory");
    }
}

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

    /// BUG-2026-09-13-24 核心回归：canonical 分配器不得复用已被占用的会话
    /// 目录，也不得把新会话写进旧会话目录（旧实现无条件覆盖既有 meta）。
    #[test]
    fn allocation_never_writes_through_an_occupied_session() {
        let (root, manager) = manager();

        // 预置一个「已存在」的会话，模拟碰撞目标。
        manager.persist_new_session("deadbeef");
        let existing_dir = manager.session_path_dir("deadbeef");
        store::write_meta(
            &existing_dir,
            &SessionMeta {
                session_id: "deadbeef".into(),
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

        // 已占用的 "deadbeef" 目录不得被复用：canonical 分配必须给出一个
        // 全新的 UUIDv7 会话，且既有目录/索引视为已占用。
        let identity = manager.allocate_session(None).expect("allocate session");
        let chosen = identity.session_id.as_str().to_string();
        assert_ne!(
            chosen, "deadbeef",
            "allocator must not reuse an occupied session id"
        );
        assert!(
            manager.is_session_taken(&chosen),
            "an allocated session must be claimed"
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
    fn create_session_refuses_to_overwrite_existing_session() {
        let (root, manager) = manager();

        manager.persist_new_session("occupied");
        let dir = manager.session_path_dir("occupied");
        store::write_meta(
            &dir,
            &SessionMeta {
                session_id: "occupied".into(),
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
            manager.persist_new_session_if_absent_with("fresh-seed", None, |session_id| {
                manager.claim_session(session_id)
            })
        );
        assert!(manager.is_session_taken("fresh-seed"));
        assert!(
            !manager.persist_new_session_if_absent_with("fresh-seed-2", None, |_| false),
            "caller-side claim rejection must abort creation"
        );
        assert!(
            !manager.is_session_taken("fresh-seed-2"),
            "rejected seed must not be materialized nor claimed"
        );
        assert_eq!(
            manager
                .load_meta("fresh-seed")
                .expect("fresh meta")
                .session_id,
            "fresh-seed"
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
        let meta = manager.load_meta(session_id).expect("load meta");
        assert_eq!(meta.session_id, session_id);
        assert!(!meta.ephemeral, "normal sessions are indexed and durable");
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
        let meta = manager.load_meta(session_id).expect("load child meta");

        assert_eq!(meta.session_id, session_id);
        assert!(meta.ephemeral, "unindexed child starts ephemeral");
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

    /// 回归（BUG-2026-09-13-24）：分配新会话时**已占用的会话目录逐字节不变**
    /// —— 旧实现（无碰撞检查 + 无条件覆盖）会把新会话写进既有目录。
    #[test]
    fn allocation_preserves_an_occupied_sentinel_session() {
        let (root, manager) = manager();

        let sentinel = "c0ffee01";
        manager.persist_new_session(sentinel);
        let sentinel_dir = manager.session_path_dir(sentinel);
        store::write_meta(
            &sentinel_dir,
            &SessionMeta {
                session_id: sentinel.into(),
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

        // canonical 分配必须给出一个与 sentinel 不同的全新会话，且不得
        // 触碰既有目录。
        let identity = manager
            .allocate_session(Some("D:/new-project"))
            .expect("allocate session");
        let chosen = identity.session_id.as_str().to_string();

        assert_ne!(
            chosen, sentinel,
            "allocator must not reuse an occupied session id"
        );
        // Windows 上 cwd 经 PathBuf 规范化后是反斜杠（D:\new-project）；
        // 语义相同即通过，分隔符差异不属于本测试的关注点。
        assert_eq!(
            manager
                .load_meta(&chosen)
                .expect("new meta")
                .cwd
                .as_deref()
                .map(std::path::Path::new)
                .map(|p| p == std::path::Path::new("D:/new-project")),
            Some(true),
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
            let meta = manager
                .load_meta(session_id)
                .expect("created session has meta");
            assert_eq!(
                meta.session_id, *session_id,
                "created meta must be self-owned"
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

        // 先占用一批 seed。
        for session_id in ["aaaaaaaa", "bbbbbbbb", "cccccccc"] {
            manager.persist_new_session(session_id);
        }
        // 索引登记但磁盘无目录的幽灵 seed（目录已删、索引尚未 compact）——
        // 复用该 seed 会让新会话继承旧索引条目的身份，故也必须视为已占用。
        store::upsert_index(
            &manager.sessions_dir,
            &SessionMeta {
                session_id: "dddddddd".into(),
                created_at: 5,
                ..Default::default()
            },
        );

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
        // 幽灵 seed（索引有、目录无）同样不能再被分配。
        assert!(!allocated.iter().any(|session_id| session_id == "dddddddd"));

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
            claimed_sessions: Mutex::new(std::collections::HashSet::new()),
        };
        (root, manager)
    }

    /// BUG-2026-09-13-05 回归：save_full（undo/compact 全量重写）必须保留
    /// 全部持久化字段——不只 mode/tool_mode/title，还包括 cwd、
    /// frozen_annotation、usage_*、archived、ephemeral、context_stats。
    #[test]
    fn save_full_preserves_all_persisted_meta_fields() {
        let (_root, manager) = manager();
        let session_id = "savefull-meta-seed";
        let dir = manager.session_path_dir(session_id);
        std::fs::create_dir_all(&dir).expect("mkdir");

        // 先落一份字段齐全的既有 meta（模拟真实会话的持久化状态）。
        let existing = SessionMeta {
            session_id: session_id.into(),
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
        manager.save_full(session_id, &messages, "new-model", Some("high"), 0, 1);

        let saved = manager.load_meta(session_id).expect("reload meta");
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
