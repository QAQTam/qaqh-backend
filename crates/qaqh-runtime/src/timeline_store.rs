//! Durable Ringing V1 timeline state. One atomically replaced record per session
//! holds the materialized recovery snapshot.
//!
//! 2026-09-10：移除 `timeline-journal/{seed}.jsonl` append-only 权威日志。
//!
//! 移除理由（实测）：`BlockCheckpoint` 携带该块的**全量文本**，写入 append-only
//! 日志后形成 O(n²) 膨胀——单个会话落盘 829 MB（1,170,412 行），其中 99.2% 的
//! checkpoint 文本是重复旧内容；加载时 `ensure_timeline_loaded` 会在持锁期间
//! 把整个文件反序列化成 `Vec<TimelineJournalOp>`，常驻约 1.2 GB（1.48× 文件），
//! 且因 `lazy_load` 串行锁，多 session 场景下相互阻塞、前端渲染卡顿。
//!
//! 该日志原本承担的三项职责改由更合适的载体承担：
//!   - transcript 读侧：`ringing-timeline/{seed}.json`（本模块持久化的快照）
//!   - 断线重连回放尾：`TimelineAppender` 内存 journal（进程内有界）
//!   - 崩溃恢复：`timeline_rebuild`（从 messages.jsonl 归档重建）
//!
//! 代价（已确认可接受）：daemon 崩溃后，客户端重连时无法回放**上一次进程**的
//! 中间帧，只能从快照 watermark 重新基线化。客户端 `recover_gap` 会自动完成
//! 这一步（`qaqh-client`），表现为一次额外的快照拉取。

use std::collections::{HashMap, HashSet};
use std::io::{BufRead as _, BufReader, Seek as _, SeekFrom, Write as _};
use std::path::PathBuf;

use qaqh_domain::{TimelineEntry, TimelineEvent, TimelineSnapshot};
use qaqh_message::legacy_writer::LegacyWriterFacade;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedTimeline {
    pub session_id: String,
    pub snapshot: TimelineSnapshot,
    /// 活跃 turn 的重连回放尾。已 seal turn 的条目在 `snapshot` 内物化，不会出现
    /// 在此。保留该字段以兼容既有缓存文件（旧格式含尾部长度的条目）。
    #[serde(default)]
    pub journal: Vec<TimelineEntry>,
}

/// Timeline 持久化：`ringing-timeline/{seed}.json`（原子替换的物化快照）+
/// `timeline-audit/{seed}.jsonl`（轻量审计）。
#[derive(Debug)]
pub struct TimelineStore {
    root: PathBuf,
    audit_root: PathBuf,
    /// 每个 seed 已审计到的最大 seq（懒计算，防重复追加）。
    audit_watermarks: HashMap<String, u64>,
    /// seed → (turn_id → sidecar byte offset)。首次读取时扫描一次；后续
    /// 读取直接 seek 到最新一行，避免每次恢复都全文件扫描。
    offload_offsets: HashMap<String, HashMap<String, u64>>,
    /// 已建立 sidecar offset 索引的 seed（空文件也要记录，防止 append
    /// 创建部分索引后误以为历史行已扫描）。
    offload_indexed: HashSet<String>,
    /// 已落盘快照 watermark。异步 checkpoint 与同步终态写并发时，旧快照
    /// 可能在拿到 store 锁后晚于新快照到达；该水位拒绝这种回退写。
    persisted_watermarks: HashMap<String, u64>,
}

/// 审计文件体积上限。超过时保留尾部一半（滚动），使磁盘占用恒定。
///
/// 审计仅用于**近期事故定位**（冻结断层），不需要全量历史；单行约 60 B，
/// 2 MiB ≈ 3.4 万条 ≈ 数十分钟重负载流式，足以覆盖一次事故窗口。
pub(crate) const AUDIT_ROTATE_BYTES: u64 = 2 * 1024 * 1024;

impl TimelineStore {
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let parent = root.into();
        let root = parent.join("ringing-timeline");
        std::fs::create_dir_all(&root)?;
        let audit_root = parent.join("timeline-audit");
        std::fs::create_dir_all(&audit_root)?;
        Ok(Self {
            root,
            audit_root,
            audit_watermarks: HashMap::new(),
            offload_offsets: HashMap::new(),
            offload_indexed: HashSet::new(),
            persisted_watermarks: HashMap::new(),
        })
    }

    /// offload 侧车路径：`ringing-offload/{seed}.jsonl`，append-only（每行一个
    /// 已 seal turn 的完整 TimelineTurn JSON）。append 语义 O(文本) 无放大；
    /// 同 turn 重 seal（reopen）时后行胜（读侧取该 turn_id 最后一条）。
    fn offload_path_for(&self, session_id: &str) -> PathBuf {
        self.root
            .parent()
            .unwrap_or(&self.root)
            .join("ringing-offload")
            .join(format!("{}.jsonl", sanitize_session(session_id)))
    }

    /// turn seal 卸载：把完整 turn 文本追加进侧车。
    pub fn append_offloaded_turn(
        &mut self,
        session_id: &str,
        turn: &qaqh_domain::TimelineTurn,
    ) -> std::io::Result<()> {
        let _legacy_writer = LegacyWriterFacade::lock();
        self.ensure_offload_index(session_id);
        let path = self.offload_path_for(session_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let line = serde_json::to_string(turn).map_err(io_error)?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let offset = file.metadata()?.len();
        writeln!(file, "{line}")?;
        file.flush()?;
        self.offload_offsets
            .entry(session_id.to_string())
            .or_default()
            .insert(turn.turn_id.clone(), offset);
        Ok(())
    }

    /// 读取某 turn 的最新完整文本（侧车同 turn_id 后行胜）。
    pub fn load_offloaded_turn(
        &mut self,
        session_id: &str,
        turn_id: &str,
    ) -> Option<qaqh_domain::TimelineTurn> {
        self.ensure_offload_index(session_id);
        let offset = *self.offload_offsets.get(session_id)?.get(turn_id)?;
        let path = self.offload_path_for(session_id);
        let mut file = std::fs::File::open(path).ok()?;
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut line = String::new();
        BufReader::new(file).read_line(&mut line).ok()?;
        serde_json::from_str(&line).ok()
    }

    /// 首次读取 sidecar 时建立 `turn_id → 最新行 offset` 索引。损坏行跳过；
    /// 同 turn 的后续行覆盖前值，保持后行胜语义。
    fn ensure_offload_index(&mut self, session_id: &str) {
        if self.offload_indexed.contains(session_id) {
            return;
        }
        let mut offsets = HashMap::new();
        let path = self.offload_path_for(session_id);
        if let Ok(mut file) = std::fs::File::open(path) {
            let mut offset = 0u64;
            let mut reader = BufReader::new(&mut file);
            let mut line = Vec::new();
            loop {
                line.clear();
                let Ok(read) = reader.read_until(b'\n', &mut line) else {
                    break;
                };
                if read == 0 {
                    break;
                }
                let line_offset = offset;
                offset = offset.saturating_add(read as u64);
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                if let Ok(turn) = serde_json::from_slice::<qaqh_domain::TimelineTurn>(&line) {
                    offsets.insert(turn.turn_id, line_offset);
                }
            }
        }
        self.offload_offsets.insert(session_id.to_string(), offsets);
        self.offload_indexed.insert(session_id.to_string());
    }

    pub fn persist(
        &mut self,
        session_id: &str,
        snapshot: &TimelineSnapshot,
        journal: Vec<TimelineEntry>,
    ) -> std::io::Result<()> {
        let _legacy_writer = LegacyWriterFacade::lock();
        if let Some(&watermark) = self.persisted_watermarks.get(session_id) {
            if watermark > snapshot.watermark {
                return Ok(());
            }
        } else if let Some(existing) = self.load_session(session_id)
            && existing.snapshot.watermark > snapshot.watermark
        {
            return Ok(());
        }
        let path = self.path_for(session_id);
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_vec(&PersistedTimeline {
            session_id: session_id.to_string(),
            snapshot: snapshot.clone(),
            journal,
        })
        .map_err(io_error)?;
        std::fs::write(&tmp, body)?;
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        std::fs::rename(tmp, path)?;
        self.persisted_watermarks
            .insert(session_id.to_string(), snapshot.watermark);
        Ok(())
    }

    /// 全量装载（仅测试用；生产走 `list_sessions` + `load_session` 懒加载）。
    #[cfg(test)]
    pub fn load(&self) -> std::io::Result<std::collections::HashMap<String, PersistedTimeline>> {
        let mut timelines = std::collections::HashMap::new();
        for entry in std::fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read(&path)
                .ok()
                .and_then(|body| serde_json::from_slice::<PersistedTimeline>(&body).ok())
            {
                Some(timeline) => {
                    if timeline.session_id.is_empty() {
                        log::warn!("[timeline] skip record without seed {}", path.display());
                    } else {
                        timelines.insert(timeline.session_id.clone(), timeline);
                    }
                }
                None => log::warn!(
                    "[timeline] skip corrupt persistent record {}",
                    path.display()
                ),
            }
        }
        Ok(timelines)
    }

    /// 磁盘上的 timeline seed 清单（懒加载索引；不读取文件内容）。
    pub fn list_sessions(&self) -> std::io::Result<Vec<String>> {
        let mut sessions = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if let Some(session_id) = path.file_stem().and_then(|s| s.to_str())
                && !session_id.is_empty()
            {
                sessions.push(session_id.to_string());
            }
        }
        Ok(sessions)
    }

    /// 装载单个 seed 的持久化 timeline（懒加载按需恢复用）。
    pub fn load_session(&self, session_id: &str) -> Option<PersistedTimeline> {
        let path = self.path_for(session_id);
        std::fs::read(&path)
            .ok()
            .and_then(|body| serde_json::from_slice::<PersistedTimeline>(&body).ok())
    }

    fn path_for(&self, session_id: &str) -> PathBuf {
        self.root
            .join(format!("{}.json", sanitize_session(session_id)))
    }

    fn audit_path_for(&self, session_id: &str) -> PathBuf {
        self.audit_root
            .join(format!("{}.jsonl", sanitize_session(session_id)))
    }

    /// 追加轻量审计行（`seq` + `ts` + 事件类型，**不含正文**）。
    ///
    /// 这是 timeline-journal 移除后保留的唯一审计能力：每行约 60 B，用
    /// “事件何时产生/落盘”定位冻结断层，而不携带内容（内容在快照与
    /// messages.jsonl 中已有唯一权威）。
    ///
    /// - 懒计算 watermark：只追加 seq 大于上次审计值的条目（与旧 journal
    ///   `journal_watermark` 同语义），避免重复行。
    /// - 超过 [`AUDIT_ROTATE_BYTES`] 时保留尾部一半后重写，使磁盘占用恒定。
    /// - 任何 I/O 失败仅记录日志，**绝不**影响事件路径（审计是旁路）。
    pub fn append_audit(&mut self, session_id: &str, entries: &[TimelineEntry]) {
        let _legacy_writer = LegacyWriterFacade::lock();
        if entries.is_empty() {
            return;
        }
        let watermark = self.audit_watermark(session_id);
        let new: Vec<&TimelineEntry> = {
            let mut fresh: Vec<&TimelineEntry> = entries
                .iter()
                .filter(|entry| entry.timeline_seq > watermark)
                .collect();
            fresh.sort_by_key(|entry| entry.timeline_seq);
            fresh
        };
        if new.is_empty() {
            return;
        }
        let path = self.audit_path_for(session_id);
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            log::warn!("[timeline] audit dir create failed for {session_id}: {error}");
            return;
        }
        let mut file = match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) => {
                log::warn!("[timeline] audit open failed for {session_id}: {error}");
                return;
            }
        };
        for entry in &new {
            let line = serde_json::json!({
                "seq": entry.timeline_seq,
                "ts": epoch_millis(),
                "type": event_kind(&entry.event),
                "turn": entry.turn_id,
            });
            if let Err(error) = writeln!(file, "{line}") {
                log::warn!("[timeline] audit write failed for {session_id}: {error}");
                return;
            }
        }
        if let Err(error) = file.flush() {
            log::warn!("[timeline] audit flush failed for {session_id}: {error}");
            return;
        }
        let max = new
            .iter()
            .map(|entry| entry.timeline_seq)
            .max()
            .unwrap_or(watermark);
        self.audit_watermarks.insert(session_id.to_string(), max);
        drop(file);
        self.rotate_audit_if_needed(session_id, &path);
    }

    /// 该 seed 已审计到的最大 seq（无文件则为 0）。懒计算并缓存。
    fn audit_watermark(&mut self, session_id: &str) -> u64 {
        if let Some(&watermark) = self.audit_watermarks.get(session_id) {
            return watermark;
        }
        let watermark = self.audit_max_seq(session_id);
        self.audit_watermarks
            .insert(session_id.to_string(), watermark);
        watermark
    }

    /// 扫描审计文件取最大 seq。只解析 `seq` 字段；损坏行跳过。
    fn audit_max_seq(&self, session_id: &str) -> u64 {
        let path = self.audit_path_for(session_id);
        let Ok(file) = std::fs::File::open(&path) else {
            return 0;
        };
        let mut max = 0u64;
        for line in std::io::BufRead::lines(std::io::BufReader::new(file)) {
            let Ok(line) = line else { break };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                continue;
            };
            if let Some(seq) = value.get("seq").and_then(serde_json::Value::as_u64) {
                max = max.max(seq);
            }
        }
        max
    }

    /// 审计文件超过上限时保留尾部一半（原子替换），使磁盘占用恒定。
    fn rotate_audit_if_needed(&self, session_id: &str, path: &std::path::Path) {
        let size = match std::fs::metadata(path) {
            Ok(meta) => meta.len(),
            Err(_) => return,
        };
        if size <= AUDIT_ROTATE_BYTES {
            return;
        }
        let Ok(body) = std::fs::read(path) else {
            return;
        };
        // 从字节中点起找下一条完整行，保证切的永远是行边界。
        let mid = body.len() / 2;
        let keep_from = body[mid..]
            .iter()
            .position(|&byte| byte == b'\n')
            .map(|offset| mid + offset + 1)
            .unwrap_or(body.len());
        let tmp = path.with_extension("jsonl.tmp");
        if std::fs::write(&tmp, &body[keep_from..]).is_err() {
            return;
        }
        if std::fs::rename(&tmp, path).is_ok() {
            log::info!(
                "[timeline] audit rotated for {session_id}: {size} bytes -> {} bytes",
                body.len() - keep_from
            );
        }
    }
}

fn sanitize_session(session_id: &str) -> String {
    session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn io_error(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
}

fn epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 事件类型名（审计用；稳定字符串，不随枚举重构漂移）。
fn event_kind(event: &TimelineEvent) -> &'static str {
    match event {
        TimelineEvent::TurnOpened { .. } => "turn_opened",
        TimelineEvent::TurnSealed { .. } => "turn_sealed",
        TimelineEvent::BlockOpened { .. } => "block_opened",
        TimelineEvent::BlockSealed { .. } => "block_sealed",
        TimelineEvent::TextDelta { .. } => "text_delta",
        TimelineEvent::BlockCheckpoint { .. } => "block_checkpoint",
        TimelineEvent::ToolUpdated { .. } => "tool_updated",
        TimelineEvent::ToolProgress { .. } => "tool_progress",
        TimelineEvent::ToolEstimated { .. } => "tool_estimated",
        TimelineEvent::RoundSealed { .. } => "round_sealed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::{
        TimelineBlock, TimelineBlockKind, TimelineBlockState, TimelineTool, TimelineToolState,
        TimelineTurn, TimelineTurnState,
    };

    fn test_turn(turn_id: &str, progress: &str) -> TimelineTurn {
        TimelineTurn {
            turn_id: turn_id.into(),
            // 夹具走实时路径语义：不带全局序号。
            turn_index: None,
            created_seq: 1,
            user_text: "question".into(),
            sealed: true,
            offloaded: false,
            state: TimelineTurnState::Completed,
            failure: None,
            rounds: vec![qaqh_domain::TimelineRound {
                round_num: 0,
                sealed: true,
                is_final: true,
                blocks: vec![TimelineBlock {
                    block_id: "tool".into(),
                    block_order: 0,
                    kind: TimelineBlockKind::Tool,
                    state: TimelineBlockState::Sealed,
                    text: String::new(),
                    tool: Some(TimelineTool {
                        exit_code: None,
                        completed_at_ms: None,
                        tool_call_id: "call-1".into(),
                        name: "exec".into(),
                        state: TimelineToolState::Succeeded,
                        summary: None,
                        args_json: None,
                        output: Some("done".into()),
                        diff: None,
                        progress: progress.into(),
                        progress_truncated: false,
                        progress_stream: None,
                        progress_bytes_total: 0,
                        display: None,
                        failure: None,
                        permission: None,
                    }),
                }],
            }],
        }
    }

    #[test]
    fn load_returns_every_persisted_session() {
        let root = std::env::temp_dir().join(format!("qaqh-timeline-store-{}", std::process::id()));
        let mut store = TimelineStore::new(&root).unwrap();
        store
            .persist(
                "seed",
                &TimelineSnapshot {
                    watermark: 0,
                    turns: vec![],
                },
                vec![],
            )
            .unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded["seed"].snapshot.watermark, 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn offload_sidecar_uses_latest_row_and_survives_reopen() {
        let root = std::env::temp_dir().join(format!(
            "qaqh-timeline-offload-latest-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        {
            let mut store = TimelineStore::new(&root).unwrap();
            store
                .append_offloaded_turn("s", &test_turn("t", "old"))
                .unwrap();
            store
                .append_offloaded_turn("s", &test_turn("t", "new"))
                .unwrap();
        }

        let mut reopened = TimelineStore::new(&root).unwrap();
        let loaded = reopened.load_offloaded_turn("s", "t").unwrap();
        let progress = &loaded.rounds[0].blocks[0].tool.as_ref().unwrap().progress;
        assert_eq!(progress, "new");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn offload_sidecar_reports_write_failures() {
        let root = std::env::temp_dir().join(format!(
            "qaqh-timeline-offload-error-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut store = TimelineStore::new(&root).unwrap();
        let offload_dir = root.join("ringing-offload");
        std::fs::create_dir_all(&offload_dir).unwrap();
        std::fs::create_dir(offload_dir.join("s.jsonl")).unwrap();

        assert!(
            store
                .append_offloaded_turn("s", &test_turn("t", "progress"))
                .is_err()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn persisted_watermark_rejects_a_stale_snapshot() {
        let root =
            std::env::temp_dir().join(format!("qaqh-timeline-watermark-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut store = TimelineStore::new(&root).unwrap();
        store
            .persist(
                "s",
                &TimelineSnapshot {
                    watermark: 10,
                    turns: vec![test_turn("t", "new")],
                },
                vec![],
            )
            .unwrap();
        store
            .persist(
                "s",
                &TimelineSnapshot {
                    watermark: 5,
                    turns: vec![test_turn("t", "old")],
                },
                vec![],
            )
            .unwrap();

        let persisted = store.load_session("s").unwrap();
        assert_eq!(persisted.snapshot.watermark, 10);
        let progress = &persisted.snapshot.turns[0].rounds[0].blocks[0]
            .tool
            .as_ref()
            .unwrap()
            .progress;
        assert_eq!(progress, "new");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn removed_journal_leaves_no_file_on_disk() {
        // 回归守卫：持久化只产生 `ringing-timeline/{seed}.json`，
        // 不得再生成 `timeline-journal/` 目录或其下的 jsonl。
        let root = std::env::temp_dir().join(format!(
            "qaqh-timeline-store-no-journal-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut store = TimelineStore::new(&root).unwrap();
        store
            .persist(
                "s",
                &TimelineSnapshot {
                    watermark: 3,
                    turns: vec![],
                },
                vec![],
            )
            .unwrap();
        assert!(!root.join("timeline-journal").exists());
        assert!(root.join("ringing-timeline").join("s.json").is_file());
        let _ = std::fs::remove_dir_all(root);
    }

    fn audit_entries() -> Vec<TimelineEntry> {
        use qaqh_domain::{TimelineBlock, TimelineBlockKind, TimelineBlockState, TimelineEvent};
        vec![
            TimelineEntry {
                timeline_seq: 1,
                turn_id: "t1".into(),
                round_num: None,
                event: TimelineEvent::TurnOpened {
                    user_text: "secret content must not be persisted here".into(),
                },
            },
            TimelineEntry {
                timeline_seq: 2,
                turn_id: "t1".into(),
                round_num: Some(0),
                event: TimelineEvent::BlockOpened {
                    block: TimelineBlock {
                        block_id: "b".into(),
                        block_order: 0,
                        kind: TimelineBlockKind::Text,
                        state: TimelineBlockState::Open,
                        text: String::new(),
                        tool: None,
                    },
                },
            },
        ]
    }

    #[test]
    fn audit_records_seq_ts_and_kind_without_message_content() {
        // 审计契约（timeline-journal 移除后的唯一替代）：
        //   1. 每条记 seq + ts + type + turn；
        //   2. **不写正文**（内容在快照 / messages.jsonl 已有唯一权威）；
        //   3. 水位去重：重复调用不得产生重复行。
        let root = std::env::temp_dir().join(format!("qaqh-timeline-audit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut store = TimelineStore::new(&root).unwrap();
        let entries = audit_entries();
        store.append_audit("s", &entries);
        // 重复追加（同一批）——水位去重，不产生新行。
        store.append_audit("s", &entries);

        let path = root.join("timeline-audit").join("s.jsonl");
        assert!(path.is_file(), "audit file expected");
        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 2, "watermark must dedupe repeated appends");

        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["seq"], 1);
        assert_eq!(first["type"], "turn_opened");
        assert_eq!(first["turn"], "t1");
        assert!(first["ts"].as_u64().unwrap_or(0) > 0, "ts must be present");

        // 正文绝不落入审计文件。
        assert!(
            !body.contains("secret content"),
            "audit must never carry message content"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn audit_rotation_keeps_recent_rows_within_bound() {
        // 滚动上界：超过 AUDIT_ROTATE_BYTES 后只保留尾部，磁盘占用恒定。
        use qaqh_domain::TimelineEvent;
        let root =
            std::env::temp_dir().join(format!("qaqh-timeline-audit-rotate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut store = TimelineStore::new(&root).unwrap();
        // 每行 ~60 B；写入足够多使其越过 2 MiB 上限。
        let total = AUDIT_ROTATE_BYTES / 60 + 2000;
        for seq in 1..=total {
            store.append_audit(
                "s",
                &[TimelineEntry {
                    timeline_seq: seq,
                    turn_id: "t".into(),
                    round_num: None,
                    event: TimelineEvent::TurnSealed {
                        state: qaqh_domain::TimelineTurnState::Completed,
                        failure: None,
                    },
                }],
            );
        }
        let path = root.join("timeline-audit").join("s.jsonl");
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(
            size <= AUDIT_ROTATE_BYTES,
            "audit must stay bounded, got {size} bytes"
        );
        // 尾部仍是合法行，且包含最新 seq。
        let body = std::fs::read_to_string(&path).unwrap();
        let last = body.lines().rev().find(|l| !l.trim().is_empty()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(last).unwrap();
        assert_eq!(parsed["seq"], total, "newest row must survive rotation");
        let _ = std::fs::remove_dir_all(root);
    }
}
