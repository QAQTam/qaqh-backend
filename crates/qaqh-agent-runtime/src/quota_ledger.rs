//! Root-session quota ledger.
//!
//! v4.4 P1-3(MUST 铁闸 2):状态权威是 `qaqh.db` 的 `quota_reservations`
//! 表。旧的「`quota/<root>/ledger.jsonl` 全量 replay + 无超时 `file.lock()`
//! + 逐条 `sync_all()`」(L-02)整体删除:预算判定与预留行写入落在同一个
//! SQLite 写事务里(串行写执行器 = 互斥),durable-before-ack 语义由 WAL +
//! synchronous=FULL 的提交保证。

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use qaqh_session::db::{Db, DbError, quota};
use serde::{Deserialize, Serialize};

static NEXT_RESERVATION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaKind {
    Spawn,
    Message,
    Content,
    Tool,
}

impl QuotaKind {
    fn gated_at_soft_limit(self) -> bool {
        matches!(self, Self::Spawn | Self::Tool)
    }
}

impl QuotaKind {
    /// DB 词表(与 sidecar 吸入、`quota_reservations.kind` 列逐字对齐)。
    fn as_db_str(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Message => "message",
            Self::Content => "content",
            Self::Tool => "tool",
        }
    }
}

impl ReleaseReason {
    fn as_db_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::NoCanonicalEdge => "no_canonical_edge",
            Self::Reconciliation => "reconciliation",
        }
    }
}

/// 闭包内的业务错误经 `DbError::Workspace` 穿墙,原样回到调用方(信封
/// 文案是对外契约,不得套一层数据库错误话术)。
fn business_error(error: DbError) -> String {
    match error {
        DbError::Workspace(message) => message,
        other => other.to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseReason {
    Completed,
    Cancelled,
    NoCanonicalEdge,
    Reconciliation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaLimits {
    pub soft: u64,
    pub hard: u64,
}

impl QuotaLimits {
    pub fn new(soft: u64, hard: u64) -> Result<Self, String> {
        if soft > hard {
            return Err(format!(
                "quota soft limit {soft} must not exceed hard limit {hard}"
            ));
        }
        Ok(Self { soft, hard })
    }

    pub fn unlimited() -> Self {
        Self {
            soft: u64::MAX,
            hard: u64::MAX,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaReservation {
    pub reservation_id: String,
    pub kind: QuotaKind,
    pub amount: u64,
    pub canonical_key: String,
    pub child_session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaSnapshot {
    pub held: u64,
    pub committed: u64,
    pub reservations: usize,
    pub soft_limit: u64,
    pub hard_limit: u64,
}

/// One root session's durable quota ledger.
pub struct QuotaLedger {
    root_session_id: String,
    limits: QuotaLimits,
    db: Arc<Db>,
}

// `Db` 本身不暴露 Debug(执行器句柄无诊断价值),ledger 只印可得字段。
impl std::fmt::Debug for QuotaLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuotaLedger")
            .field("root_session_id", &self.root_session_id)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl QuotaLedger {
    /// Bind one root session's ledger to the business database.
    pub fn open(root_session_id: &str, limits: QuotaLimits, db: Arc<Db>) -> Result<Self, String> {
        if root_session_id.is_empty() {
            return Err("quota root_session_id must not be empty".into());
        }
        Ok(Self {
            root_session_id: root_session_id.to_string(),
            limits,
            db,
        })
    }

    /// 预留行落库所在数据库(诊断/测试用)。
    pub fn db(&self) -> &Arc<Db> {
        &self.db
    }

    pub fn root_session_id(&self) -> &str {
        &self.root_session_id
    }

    pub fn limits(&self) -> QuotaLimits {
        self.limits
    }
    /// Durably reserve capacity. The returned reservation is the ack the
    /// caller may use before performing the protected side effect; the
    /// budget check and the INSERT share one SQLite write transaction.
    pub fn reserve(
        &mut self,
        kind: QuotaKind,
        amount: u64,
        canonical_key: impl Into<String>,
        child_session_id: Option<String>,
    ) -> Result<QuotaReservation, String> {
        let canonical_key: String = canonical_key.into();
        let reservation_id = format!(
            "q-{}-{:x}",
            NEXT_RESERVATION.fetch_add(1, Ordering::Relaxed),
            now_ms()
        );
        let root = self.root_session_id.clone();
        let limits = self.limits;
        let child = child_session_id.clone();
        let key_for_db = canonical_key.clone();
        let id_for_db = reservation_id.clone();
        self.db
            .write_blocking(move |conn| -> Result<(), DbError> {
                let tx = conn.transaction()?;
                let totals = quota::used_totals(&tx, &root)?;
                let used = totals.held.saturating_add(totals.committed);
                let total = used.saturating_add(amount);
                if total > limits.hard {
                    return Err(DbError::Workspace(format!(
                        "quota hard limit exceeded: {used} + {amount} > {}",
                        limits.hard
                    )));
                }
                if total > limits.soft && kind.gated_at_soft_limit() {
                    return Err(DbError::Workspace(format!(
                        "quota soft limit exceeded for {kind:?}: {used} + {amount} > {}",
                        limits.soft
                    )));
                }
                quota::insert_reserved(
                    &tx,
                    &quota::NewReservation {
                        reservation_id: &id_for_db,
                        root_session_id: &root,
                        child_session_id: child.as_deref(),
                        kind: kind.as_db_str(),
                        amount,
                        canonical_key: &key_for_db,
                        reserved_at_ms: now_ms(),
                    },
                )?;
                tx.commit()?;
                Ok(())
            })
            .map_err(business_error)?;
        Ok(QuotaReservation {
            reservation_id,
            kind,
            amount,
            canonical_key,
            child_session_id,
        })
    }

    /// Commit a held reservation. Duplicate commit is idempotent.
    pub fn commit(&mut self, reservation_id: &str) -> Result<(), String> {
        let reservation_id = reservation_id.to_string();
        self.db
            .write_blocking(move |conn| -> Result<(), DbError> {
                let tx = conn.transaction()?;
                match quota::get_reservation(&tx, &reservation_id)? {
                    // 已是终态(committed/released) = 重复 commit 幂等。
                    Some(stored) if stored.state != "held" => {}
                    // amount 由行自身携带,held→committed 只改 state,SUM 即口径。
                    Some(_) => {
                        quota::settle(&tx, &reservation_id, "committed", None, now_ms())?;
                    }
                    None => {
                        return Err(DbError::Workspace(format!(
                            "quota reservation {reservation_id} is not held"
                        )));
                    }
                }
                tx.commit()?;
                Ok(())
            })
            .map_err(business_error)
    }

    /// Release a held reservation. Duplicate release is idempotent.
    pub fn release(&mut self, reservation_id: &str, reason: ReleaseReason) -> Result<(), String> {
        let reservation_id = reservation_id.to_string();
        self.db
            .write_blocking(move |conn| -> Result<(), DbError> {
                let tx = conn.transaction()?;
                match quota::get_reservation(&tx, &reservation_id)? {
                    Some(stored) if stored.state != "held" => {}
                    Some(_) => {
                        quota::settle(
                            &tx,
                            &reservation_id,
                            "released",
                            Some(reason.as_db_str()),
                            now_ms(),
                        )?;
                    }
                    None => {
                        return Err(DbError::Workspace(format!(
                            "quota reservation {reservation_id} is not held"
                        )));
                    }
                }
                tx.commit()?;
                Ok(())
            })
            .map_err(business_error)
    }

    /// Release spawn reservations whose child has no canonical edge.
    pub fn reconcile_spawns(
        &mut self,
        canonical_children: &HashSet<String>,
    ) -> Result<usize, String> {
        let canonical_children = canonical_children.clone();
        let root = self.root_session_id.clone();
        self.db
            .write_blocking(move |conn| -> Result<usize, DbError> {
                let tx = conn.transaction()?;
                let stale = quota::stale_spawn_reservations(&tx, &root, &canonical_children)?;
                for (reservation_id, child) in &stale {
                    quota::settle(
                        &tx,
                        reservation_id,
                        "released",
                        Some(ReleaseReason::NoCanonicalEdge.as_db_str()),
                        now_ms(),
                    )?;
                    log::warn!(
                        "[quota] released unbacked spawn reservation {reservation_id} for child {child}"
                    );
                }
                tx.commit()?;
                Ok(stale.len())
            })
            .map_err(business_error)
    }

    pub fn snapshot(&mut self) -> Result<QuotaSnapshot, String> {
        let root = self.root_session_id.clone();
        let limits = self.limits;
        let totals = self
            .db
            .read_blocking(move |conn| quota::used_totals(conn, &root))
            .map_err(|error| error.to_string())?;
        Ok(QuotaSnapshot {
            held: totals.held,
            committed: totals.committed,
            reservations: totals.held_count as usize,
            soft_limit: limits.soft,
            hard_limit: limits.hard,
        })
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_ledger(dir: &tempfile::TempDir, soft: u64, hard: u64) -> QuotaLedger {
        QuotaLedger::open(
            "root-session",
            QuotaLimits::new(soft, hard).expect("limits"),
            open_db(dir),
        )
        .expect("ledger")
    }

    fn open_db(dir: &tempfile::TempDir) -> Arc<Db> {
        Arc::new(Db::open(dir.path().join("qaqh.db")).expect("open db"))
    }

    #[test]
    fn ledger_never_touches_files_any_more() {
        // 歼灭自证(铁闸 2 运行期零残留):整条写路径跑完后,数据根下不得
        // 再出现 quota/ 目录或 ledger.jsonl——状态只在 qaqh.db 里。
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = open_ledger(&dir, 100, 200);
        ledger
            .reserve(QuotaKind::Spawn, 1, "spawn:x", Some("child".into()))
            .expect("reserve");
        ledger.snapshot().expect("snapshot");
        assert!(!dir.path().join("quota").exists(), "quota 文件目录必须不再被创建");
        assert_eq!(ledger.snapshot().expect("snapshot").held, 1);
    }

    #[test]
    fn durability_survives_a_new_database_handle() {
        // 旧 reopen 语义的 DB 等价物:第二个 Db 句柄(独立连接)看到同样的账。
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = open_ledger(&dir, 100, 200);
        ledger
            .reserve(QuotaKind::Message, 7, "message:1", None)
            .expect("reserve");
        drop(ledger);
        let mut reopened =
            QuotaLedger::open("root-session", QuotaLimits::new(100, 200).unwrap(), open_db(&dir))
                .expect("ledger");
        let snapshot = reopened.snapshot().expect("snapshot");
        assert_eq!(snapshot.held, 7);
        assert_eq!(snapshot.reservations, 1);
    }

    #[test]
    fn reservation_is_durable_before_ack() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = open_ledger(&dir, 100, 200);
        let reservation = ledger
            .reserve(QuotaKind::Message, 7, "message:1", None)
            .expect("reserve");
        drop(ledger);

        let mut reopened = open_ledger(&dir, 100, 200);
        let snapshot = reopened.snapshot().expect("snapshot");
        assert_eq!(snapshot.held, 7);
        assert_eq!(snapshot.reservations, 1);
        assert_eq!(reservation.amount, 7);
    }

    #[test]
    fn commit_and_release_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = open_ledger(&dir, 100, 200);
        let reservation = ledger
            .reserve(QuotaKind::Content, 11, "content:1", None)
            .expect("reserve");
        ledger.commit(&reservation.reservation_id).expect("commit");
        ledger
            .commit(&reservation.reservation_id)
            .expect("commit replay");
        let snapshot = ledger.snapshot().expect("snapshot");
        assert_eq!(snapshot.held, 0);
        assert_eq!(snapshot.committed, 11);
    }

    #[test]
    fn hard_limit_rejects_without_appending() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = open_ledger(&dir, 5, 10);
        ledger
            .reserve(QuotaKind::Message, 10, "message:1", None)
            .expect("first reservation");
        assert!(
            ledger
                .reserve(QuotaKind::Message, 1, "message:2", None)
                .is_err()
        );
        assert_eq!(ledger.snapshot().expect("snapshot").held, 10);
    }

    #[test]
    fn committed_usage_still_counts_toward_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = open_ledger(&dir, 10, 10);
        let reservation = ledger
            .reserve(QuotaKind::Message, 10, "message:1", None)
            .expect("first reservation");
        ledger.commit(&reservation.reservation_id).expect("commit");
        assert!(
            ledger
                .reserve(QuotaKind::Message, 1, "message:2", None)
                .is_err(),
            "committed usage must remain part of the quota watermark"
        );
    }

    #[test]
    fn soft_limit_blocks_spawn_but_allows_message() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = open_ledger(&dir, 10, 20);
        ledger
            .reserve(QuotaKind::Message, 10, "message:1", None)
            .expect("message at soft limit");
        assert!(
            ledger
                .reserve(QuotaKind::Spawn, 1, "spawn:1", Some("child".into()))
                .is_err()
        );
        ledger
            .reserve(QuotaKind::Message, 5, "message:2", None)
            .expect("message below hard limit");
    }

    #[test]
    fn reconciliation_releases_unbacked_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = open_ledger(&dir, 100, 200);
        ledger
            .reserve(QuotaKind::Spawn, 3, "spawn:child", Some("child".into()))
            .expect("reserve spawn");
        assert_eq!(
            ledger.reconcile_spawns(&HashSet::new()).expect("reconcile"),
            1
        );
        let snapshot = ledger.snapshot().expect("snapshot");
        assert_eq!(snapshot.held, 0);
    }
}
