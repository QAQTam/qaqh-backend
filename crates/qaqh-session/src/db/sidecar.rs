//! 铁闸 2：野生 JSON/JSONL 状态账本的**启动期一次性单向吸入**（v4.4–v4.6 存续）。
//!
//! 存在理由（`docs/v4.3-4.8/MUST.md` §铁闸 2）：`todo.json`、
//! `ringing-devices.json`、`ringing-command-receipts.json`、
//! `quota/*/ledger.jsonl` 这四类文件此前各自持一把进程锁、各自全量覆写、
//! 各自带一套「损坏即空启动 / 无超时文件锁」的降级语义。v4.4 把它们并入
//! `qaqh.db` 单权威，运行期只留 SQLite 一条读写路径。
//!
//! 协议（逐步照 MUST 落地，不得放宽）：
//! 1. 吸入跑在 `schema::initialize` 的**同一个 `EXCLUSIVE` 事务**里；
//! 2. 每类文件在事务内核对「解析记录数 == 实际落库行数」，任何一条不等
//!    就地 `Err` ⇒ 事务回滚 ⇒ 数据库版本位不前进 ⇒ **不改名** ⇒ 下次启动重试；
//! 3. 改名（`*.migrated-v4.4` 墓碑）只允许在 `COMMIT` 之后由
//!    [`IngestionReport::retire`] 执行，由 `Db::open` 调用；
//! 4. **严禁运行期 fallback**：本模块之外不得再有这些文件的读写点（物理歼灭
//!    清单见 PR 四联单②，`tests/sidecar_ingestion.rs` 用 grep 自证）。
//!
//! 损坏的旧文件在这里是**硬失败**，不沿用各文件 owner 原先「读不懂就当空」
//! 的静默降级——那正是 L-20 的病灶本身（宪法 #5：不能工作视为代码错误）。
//! 幂等性由版本位保证：吸入只跑在 `SCHEMA_VERSION` 抬到 12 的那一个事务里；
//! `COMMIT` 与改名之间崩溃 ⇒ 下次启动版本位已是 12 ⇒ 不再重复吸入。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{Transaction, params};
use serde::Deserialize;

use super::DbError;

/// 墓碑后缀：吸入成功后原地封存旧文件（防重复吸入，同时留现场备查）。
pub const MIGRATED_SUFFIX: &str = "migrated-v4.4";

/// 一次吸入的结果：已提交落库的旧文件清单。
#[derive(Debug, Default)]
pub struct IngestionReport {
    tombstones: Vec<PathBuf>,
}

impl IngestionReport {
    /// 本次吸入是否动过任何旧文件。
    pub fn is_empty(&self) -> bool {
        self.tombstones.is_empty()
    }

    /// 已封存（改名）的旧文件路径，供测试断言。
    pub fn tombstones(&self) -> &[PathBuf] {
        &self.tombstones
    }

    /// `COMMIT` 之后调用：把吸入过的旧文件改名为 `*.migrated-v4.4`。
    ///
    /// 改名失败**不致命**：数据已在库里，运行期也不再读这些文件；这里只
    /// WARN 留痕，避免把「一次成功的迁移」降级成「启动失败」。
    pub fn retire(&self) {
        for path in &self.tombstones {
            let target = tombstone_path(path);
            match std::fs::rename(path, &target) {
                Ok(()) => log::info!(
                    "[sidecar] ingested {} -> {}",
                    path.display(),
                    target.display()
                ),
                Err(error) => log::warn!(
                    "[sidecar] cannot tombstone {} after commit: {error}",
                    path.display()
                ),
            }
        }
    }
}

fn tombstone_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(format!(".{MIGRATED_SUFFIX}"));
    path.with_file_name(name)
}

/// 事务内核对：解析出多少条记录，就必须落库多少行。
fn reconcile(source: &str, parsed: usize, stored: usize) -> Result<(), DbError> {
    if parsed != stored {
        return Err(DbError::Corrupt(format!(
            "sidecar ingest {source}: {parsed} record(s) parsed but {stored} row(s) written"
        )));
    }
    Ok(())
}

/// 吸入四类旧账本。`data_dir` 是 `qaqh.db` 的父目录。
///
/// 只在 `SCHEMA_VERSION` 抬到本版本的升级事务（以及空库首建）里调用。
pub fn ingest_legacy_sidecar_files(
    tx: &Transaction<'_>,
    data_dir: &Path,
) -> Result<IngestionReport, DbError> {
    let mut report = IngestionReport::default();
    ingest_todos(tx, data_dir, &mut report)?;
    ingest_devices(tx, data_dir, &mut report)?;
    ingest_receipts(tx, data_dir, &mut report)?;
    ingest_quota(tx, data_dir, &mut report)?;
    Ok(report)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, DbError> {
    let bytes = std::fs::read(path)
        .map_err(|error| DbError::Corrupt(format!("sidecar read {}: {error}", path.display())))?;
    serde_json::from_slice::<T>(&bytes)
        .map_err(|error| DbError::Corrupt(format!("sidecar parse {}: {error}", path.display())))
}

// ═══════════════════════════════════════════════════════════════════════════
// sessions/<seed>/todo.json -> session_todos + session_todo_state
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Deserialize)]
struct LegacyTodoItem {
    #[serde(default)]
    id: String,
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    status: LegacyTodoStatus,
    #[serde(default)]
    evidence: Option<String>,
}

/// 与 `qaqh_workspace::todo::TodoStatus` 的 snake_case 词表逐字对齐。
#[derive(Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum LegacyTodoStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl LegacyTodoStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Deserialize)]
struct LegacyTodoFile {
    #[serde(default)]
    items: Vec<LegacyTodoItem>,
    #[serde(default)]
    mode: LegacyTodoMode,
    #[serde(default)]
    current_id: Option<String>,
    #[serde(default)]
    auto_turns: u32,
    #[serde(default = "default_max_auto_turns")]
    max_auto_turns: u32,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    cleared_notified: bool,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum LegacyTodoMode {
    #[default]
    Manual,
    Goal,
}

impl LegacyTodoMode {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Goal => "goal",
        }
    }
}

fn default_max_auto_turns() -> u32 {
    24
}

fn ingest_todos(
    tx: &Transaction<'_>,
    data_dir: &Path,
    report: &mut IngestionReport,
) -> Result<(), DbError> {
    let Ok(entries) = std::fs::read_dir(data_dir.join("sessions")) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let todo_path = path.join("todo.json");
        if !todo_path.is_file() {
            continue;
        }
        let session_id = entry.file_name().to_string_lossy().into_owned();
        let file = read_json::<LegacyTodoFile>(&todo_path)?;
        let mut written = 0usize;
        for (ord, item) in file.items.iter().enumerate() {
            written += tx.execute(
                "INSERT INTO session_todos \
                 (session_id, ord, todo_id, title, description, status, evidence) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    session_id,
                    ord as i64,
                    if item.id.is_empty() { item.title.clone() } else { item.id.clone() },
                    item.title,
                    item.description,
                    item.status.as_str(),
                    item.evidence,
                ],
            )?;
        }
        reconcile(&format!("todo.json items for {session_id}"), file.items.len(), written)?;
        // 清单头恒落一行：items 为空也要保住 mode/archived 等状态位。
        let state_written = tx.execute(
            "INSERT INTO session_todo_state \
             (session_id, mode, current_id, auto_turns, max_auto_turns, archived, cleared_notified) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session_id,
                file.mode.as_str(),
                file.current_id,
                file.auto_turns as i64,
                file.max_auto_turns as i64,
                file.archived,
                file.cleared_notified,
            ],
        )?;
        reconcile(&format!("todo.json state for {session_id}"), 1, state_written)?;
        report.tombstones.push(todo_path);
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
// ringing-devices.json -> paired_devices
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Deserialize)]
struct LegacyDevice {
    device_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    platform: String,
    #[serde(default)]
    scope: LegacyScope,
    token_digest: String,
    #[serde(default)]
    created_at_ms: u64,
    #[serde(default)]
    last_seen_ms: u64,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum LegacyScope {
    #[default]
    View,
    Interact,
    Admin,
}

impl LegacyScope {
    fn as_str(&self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Interact => "interact",
            Self::Admin => "admin",
        }
    }
}

fn ingest_devices(
    tx: &Transaction<'_>,
    data_dir: &Path,
    report: &mut IngestionReport,
) -> Result<(), DbError> {
    let path = data_dir.join("ringing-devices.json");
    if !path.is_file() {
        return Ok(());
    }
    let devices = read_json::<Vec<LegacyDevice>>(&path)?;
    let mut written = 0usize;
    for device in &devices {
        written += tx.execute(
            "INSERT INTO paired_devices \
             (device_id, name, platform, scope, token_digest, created_at_ms, last_seen_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                device.device_id,
                device.name,
                device.platform,
                device.scope.as_str(),
                device.token_digest,
                device.created_at_ms as i64,
                device.last_seen_ms as i64,
            ],
        )?;
    }
    reconcile("ringing-devices.json", devices.len(), written)?;
    report.tombstones.push(path);
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
// ringing-command-receipts.json -> command_receipts
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Deserialize)]
struct LegacyReceipt {
    fingerprint: String,
    #[serde(default)]
    client_session_id: Option<String>,
    accepted_at_ms: u64,
    #[serde(default)]
    state: LegacyReceiptState,
    #[serde(default)]
    terminal_event_id: Option<String>,
    #[serde(default)]
    error_code: Option<String>,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    sync_result: Option<serde_json::Value>,
}

/// 与 `qaqh_ringing::RingingCommandState` 的 snake_case 词表逐字对齐。
#[derive(Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum LegacyReceiptState {
    #[default]
    Accepted,
    Running,
    Succeeded,
    Failed,
    Rejected,
}

impl LegacyReceiptState {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Rejected => "rejected",
        }
    }
}

fn ingest_receipts(
    tx: &Transaction<'_>,
    data_dir: &Path,
    report: &mut IngestionReport,
) -> Result<(), DbError> {
    let path = data_dir.join("ringing-command-receipts.json");
    if !path.is_file() {
        return Ok(());
    }
    let receipts = read_json::<HashMap<String, LegacyReceipt>>(&path)?;
    let mut written = 0usize;
    for (command_id, receipt) in &receipts {
        written += tx.execute(
            "INSERT INTO command_receipts \
             (command_id, fingerprint, client_session_id, state, terminal_event_id, \
              error_code, result_json, sync_result_json, accepted_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                command_id,
                receipt.fingerprint,
                receipt.client_session_id,
                receipt.state.as_str(),
                receipt.terminal_event_id,
                receipt.error_code,
                receipt.result.as_ref().map(|value| value.to_string()),
                receipt.sync_result.as_ref().map(|value| value.to_string()),
                receipt.accepted_at_ms as i64,
            ],
        )?;
    }
    reconcile("ringing-command-receipts.json", receipts.len(), written)?;
    report.tombstones.push(path);
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
// quota/<root>/ledger.jsonl -> quota_reservations（事件折叠成终态行）
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum LegacyQuotaRecord {
    Reserved {
        reservation_id: String,
        root_session_id: String,
        #[serde(default)]
        child_session_id: Option<String>,
        #[serde(default)]
        kind: LegacyQuotaKind,
        #[serde(default)]
        amount: u64,
        #[serde(default)]
        canonical_key: String,
        #[serde(default)]
        at_ms: i64,
    },
    Committed {
        reservation_id: String,
        #[serde(default)]
        amount: u64,
        #[serde(default)]
        at_ms: i64,
    },
    Released {
        reservation_id: String,
        #[serde(default)]
        reason: LegacyReleaseReason,
        #[serde(default)]
        at_ms: i64,
    },
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum LegacyQuotaKind {
    #[default]
    Spawn,
    Message,
    Content,
    Tool,
}

impl LegacyQuotaKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Message => "message",
            Self::Content => "content",
            Self::Tool => "tool",
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum LegacyReleaseReason {
    #[default]
    Completed,
    Cancelled,
    NoCanonicalEdge,
    Reconciliation,
}

impl LegacyReleaseReason {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::NoCanonicalEdge => "no_canonical_edge",
            Self::Reconciliation => "reconciliation",
        }
    }
}

/// 折叠后的单条预留终态，字段与 `quota_reservations` 列一一对应。
struct FoldedQuota {
    reservation_id: String,
    child_session_id: Option<String>,
    kind: String,
    amount: i64,
    canonical_key: String,
    state: &'static str,
    release_reason: Option<String>,
    reserved_at_ms: i64,
    settled_at_ms: Option<i64>,
}

fn ingest_quota(
    tx: &Transaction<'_>,
    data_dir: &Path,
    report: &mut IngestionReport,
) -> Result<(), DbError> {
    let Ok(entries) = std::fs::read_dir(data_dir.join("quota")) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let ledger = dir.join("ledger.jsonl");
        if !ledger.is_file() {
            continue;
        }
        let root = entry.file_name().to_string_lossy().into_owned();
        let records = read_ledger_lines(&ledger)?;
        let folded = fold_quota_ledger(&root, &ledger, &records)?;
        let mut written = 0usize;
        for row in &folded {
            written += tx.execute(
                "INSERT INTO quota_reservations \
                 (reservation_id, root_session_id, child_session_id, kind, amount, \
                  canonical_key, state, release_reason, reserved_at_ms, settled_at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    row.reservation_id,
                    root,
                    row.child_session_id,
                    row.kind,
                    row.amount,
                    row.canonical_key,
                    row.state,
                    row.release_reason,
                    row.reserved_at_ms,
                    row.settled_at_ms,
                ],
            )?;
        }
        reconcile(
            &format!("quota/{root}/ledger.jsonl"),
            folded.len(),
            written,
        )?;
        report.tombstones.push(ledger);
    }
    Ok(())
}

fn read_ledger_lines(path: &Path) -> Result<Vec<LegacyQuotaRecord>, DbError> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| DbError::Corrupt(format!("sidecar read {}: {error}", path.display())))?;
    let mut records = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        records.push(serde_json::from_str::<LegacyQuotaRecord>(line).map_err(|error| {
            DbError::Corrupt(format!(
                "sidecar parse {}:{}: {error}",
                path.display(),
                index + 1
            ))
        })?);
    }
    Ok(records)
}

/// 事件流 -> 终态行。重放语义与 `quota_ledger::refresh` 逐条对齐：
/// 重复 reserved、commit/release 未知预留都判为损坏。
fn fold_quota_ledger(
    root: &str,
    ledger: &Path,
    records: &[LegacyQuotaRecord],
) -> Result<Vec<FoldedQuota>, DbError> {
    let mut order: Vec<String> = Vec::new();
    let mut rows: HashMap<String, FoldedQuota> = HashMap::new();
    for record in records {
        match record {
            LegacyQuotaRecord::Reserved {
                reservation_id,
                root_session_id,
                child_session_id,
                kind,
                amount,
                canonical_key,
                at_ms,
            } => {
                if root_session_id != root {
                    return Err(DbError::Corrupt(format!(
                        "sidecar {}: reservation root {root_session_id} != ledger dir {root}",
                        ledger.display()
                    )));
                }
                if rows.contains_key(reservation_id) {
                    return Err(DbError::Corrupt(format!(
                        "sidecar {}: duplicate reservation {reservation_id}",
                        ledger.display()
                    )));
                }
                order.push(reservation_id.clone());
                rows.insert(
                    reservation_id.clone(),
                    FoldedQuota {
                        reservation_id: reservation_id.clone(),
                        child_session_id: child_session_id.clone(),
                        kind: kind.as_str().to_string(),
                        amount: *amount as i64,
                        canonical_key: canonical_key.clone(),
                        state: "held",
                        release_reason: None,
                        reserved_at_ms: *at_ms,
                        settled_at_ms: None,
                    },
                );
            }
            LegacyQuotaRecord::Committed {
                reservation_id,
                amount,
                at_ms,
            } => {
                let Some(row) = rows.get_mut(reservation_id) else {
                    return Err(DbError::Corrupt(format!(
                        "sidecar {}: commits unknown reservation {reservation_id}",
                        ledger.display()
                    )));
                };
                if row.state == "held" {
                    row.amount = *amount as i64;
                    row.state = "committed";
                    row.settled_at_ms = Some(*at_ms);
                }
            }
            LegacyQuotaRecord::Released {
                reservation_id,
                reason,
                at_ms,
            } => {
                let Some(row) = rows.get_mut(reservation_id) else {
                    return Err(DbError::Corrupt(format!(
                        "sidecar {}: releases unknown reservation {reservation_id}",
                        ledger.display()
                    )));
                };
                if row.state == "held" {
                    row.state = "released";
                    row.release_reason = Some(reason.as_str().to_string());
                    row.settled_at_ms = Some(*at_ms);
                }
            }
        }
    }
    Ok(order
        .into_iter()
        .filter_map(|id| rows.remove(&id))
        .collect())
}
