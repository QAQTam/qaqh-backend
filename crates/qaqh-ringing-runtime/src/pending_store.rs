//! Pending command store — Ringing V1 command idempotency + 4096 cap.
//!
//! v4.4 P1-3（MUST 铁闸 2）：权威是 `qaqh.db` 的 `command_receipts` 表。
//! 旧的 `ringing-command-receipts.json` 全量覆写（L-06）与
//! `Instant + persistence_path` 的内存索引整体删除；TTL/容量裁剪都是 SQL。
//! 调用点已在阻塞池（P1-0a），方法保持 sync——`write_blocking` 与串行写
//! 执行器同步返回，预留成功即已 COMMIT，durable-before-ack 由 WAL +
//! synchronous=FULL 保证（不再是 best-effort 文件写）。
//! DB 故障在幂等闸门上按 **fail-closed** 处理：record 返回 `Err(())`
//! （调用方视作 Mismatch，不派发执行）、status 返回 `None`（unknown，
//! SDK 不得自动重试副作用）——两者都绝不伪装成「可安全执行」。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use qaqh_session::db::{Db, receipts};

use qaqh_ringing::{
    RingingCommandState, RingingCommandStatus, RingingV2CommandResult, RingingV2CommandStatus,
    RingingV2ExistingResult, RingingV2SyncResult,
};

/// Retention metadata for `PendingCommandStore::new()` and persistent stores.
///
/// This is a bounded reconciliation cache, not an execution ledger. Missing
/// receipts mean an unknown outcome, not proof that a command did not execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct ReceiptPolicy {
    pub ttl_secs: u64,
    pub max_entries: usize,
    pub owner_scope: ReceiptOwnerScope,
    pub id_scope: ReceiptIdScope,
    pub persistence: ReceiptPersistence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum ReceiptOwnerScope {
    /// §5（2026-10-23 审查）：回执 owner 是**稳定身份键**（device 身份为
    /// `device_id`，admin 为 open 时自报的 instance id），不是临时
    /// `client_session_id`。换连接（重开 lease）后同一 command_id 仍可对账，
    /// 重复提交返回原回执；不同身份冒用同一 command_id 仍被指纹守卫拒绝。
    IdentityInstanceKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum ReceiptIdScope {
    Store,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum ReceiptPersistence {
    BestEffort,
}

/// Uses the same values as the receipt retention and insertion paths.
pub const RECEIPT_POLICY: ReceiptPolicy = ReceiptPolicy {
    ttl_secs: 300,
    max_entries: 4096,
    owner_scope: ReceiptOwnerScope::IdentityInstanceKey,
    id_scope: ReceiptIdScope::Store,
    persistence: ReceiptPersistence::BestEffort,
};

pub struct PendingCommandStore {
    db: Arc<Db>,
    max_entries: usize,
    /// 冻结告警限频状态（command_id → 上次告警 ms）。刻意不落库：
    /// 与旧实现「纯内存限频、重启后重新计」同律，重启后重复告警可接受。
    warn_state: Mutex<HashMap<String, u64>>,
}

// `Db` 不暴露 Debug(执行器句柄无诊断价值)。
impl std::fmt::Debug for PendingCommandStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingCommandStore")
            .field("max_entries", &self.max_entries)
            .finish_non_exhaustive()
    }
}

/// 冻结告警条目：Accepted/Running 超时无终态的 receipt 快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleRunningReceipt {
    pub command_id: String,
    pub state: RingingCommandState,
    pub age_secs: u64,
    pub client_session_id: Option<String>,
}

/// v2 command 的已有 receipt 视图（含指纹，供重放时做 payload 校验）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingCommandReceipt {
    pub state: RingingCommandState,
    pub payload_fingerprint: String,
    pub terminal_event_id: Option<String>,
    pub error_code: Option<String>,
    pub result: Option<RingingV2CommandResult>,
    pub sync_result: Option<RingingV2SyncResult>,
}

impl ExistingCommandReceipt {
    pub fn into_existing(self) -> RingingV2ExistingResult {
        RingingV2ExistingResult::CommandReceipt {
            state: self.state,
            terminal_event_id: self.terminal_event_id,
            error_code: self.error_code,
            result: self.result,
            sync_result: self.sync_result,
        }
    }
}

impl PendingCommandStore {
    /// Receipt 权威在 `command_receipts`；只保存哈希，不把命令正文、
    /// 用户文本或附件元数据写入库表。
    pub fn new(db: Arc<Db>) -> Self {
        Self {
            db,
            max_entries: RECEIPT_POLICY.max_entries,
            warn_state: Mutex::new(HashMap::new()),
        }
    }

    fn ttl_cutoff_ms(now_ms: u64) -> i64 {
        now_ms.saturating_sub(RECEIPT_TTL.as_millis() as u64) as i64
    }

    fn state_to_db(state: RingingCommandState) -> &'static str {
        match state {
            RingingCommandState::Accepted => "accepted",
            RingingCommandState::Running => "running",
            RingingCommandState::Succeeded => "succeeded",
            RingingCommandState::Failed => "failed",
            RingingCommandState::Rejected => "rejected",
        }
    }

    fn state_from_db(raw: &str) -> Option<RingingCommandState> {
        match raw {
            "accepted" => Some(RingingCommandState::Accepted),
            "running" => Some(RingingCommandState::Running),
            "succeeded" => Some(RingingCommandState::Succeeded),
            "failed" => Some(RingingCommandState::Failed),
            "rejected" => Some(RingingCommandState::Rejected),
            _ => None,
        }
    }

    /// 行 → 视图。state/payload 词表解不动即 `None`（数据损坏 fail-closed
    /// 当 unknown，并留 error 痕——不伪装回执，也不回退文件）。
    fn row_to_existing(row: &receipts::ReceiptRow) -> Option<ExistingCommandReceipt> {
        let state = Self::state_from_db(&row.state)?;
        let result = match row.result_json.as_deref() {
            None => None,
            Some(raw) => match serde_json::from_str::<RingingV2CommandResult>(raw) {
                Ok(result) => Some(result),
                Err(error) => {
                    log::error!("[receipt] corrupt result_json for {}: {error}", row.command_id);
                    return None;
                }
            },
        };
        let sync_result = match row.sync_result_json.as_deref() {
            None => None,
            Some(raw) => match serde_json::from_str::<RingingV2SyncResult>(raw) {
                Ok(result) => Some(result),
                Err(error) => {
                    log::error!(
                        "[receipt] corrupt sync_result_json for {}: {error}",
                        row.command_id
                    );
                    return None;
                }
            },
        };
        Some(ExistingCommandReceipt {
            state,
            payload_fingerprint: row.fingerprint.clone(),
            terminal_event_id: row.terminal_event_id.clone(),
            error_code: row.error_code.clone(),
            result,
            sync_result,
        })
    }

    /// 记录 accepted。返回 false 表示重复（已 accepted 且未过期）。
    pub fn record(&mut self, command_id: &str) -> bool {
        self.record_fingerprint(command_id, command_id)
            .unwrap_or(false)
    }

    /// 预留 receipt；相同 ID 不同 payload 是协议错误。
    #[allow(clippy::result_unit_err)] // 空错误类型为既有信号量语义
    pub fn record_fingerprint(&mut self, command_id: &str, fingerprint: &str) -> Result<bool, ()> {
        self.record_fingerprint_owned(command_id, fingerprint, None)
    }

    /// `owner` 是稳定身份键（`RECEIPT_POLICY.owner_scope`）；参数名保留
    /// `client_session_id` 以兼容持久化字段，语义已按 §5 收紧为身份键。
    #[allow(clippy::result_unit_err)] // 空错误类型为既有信号量语义
    pub fn record_fingerprint_for_session(
        &mut self,
        command_id: &str,
        fingerprint: &str,
        client_session_id: &str,
    ) -> Result<bool, ()> {
        self.record_fingerprint_owned(command_id, fingerprint, Some(client_session_id))
    }

    fn record_fingerprint_owned(
        &mut self,
        command_id: &str,
        fingerprint: &str,
        client_session_id: Option<&str>,
    ) -> Result<bool, ()> {
        let command_id = command_id.to_string();
        let fingerprint = fingerprint.to_string();
        let owner = client_session_id.map(str::to_string);
        let max_entries = self.max_entries;
        #[derive(Debug)]
        enum Admit {
            New,
            Duplicate,
            Mismatch,
        }
        let outcome = self.db.write_blocking(move |conn| -> Result<Admit, qaqh_session::db::DbError> {
            let tx = conn.transaction()?;
            let now_ms = unix_millis();
            let existing = receipts::get(&tx, &command_id)?;
            if let Some(row) = existing.as_ref() {
                let fresh = row.accepted_at_ms >= Self::ttl_cutoff_ms(now_ms)
                    // 未来时间戳同样 unknown（不可信时钟不得续命幂等表）。
                    && row.accepted_at_ms <= now_ms as i64;
                if fresh {
                    // 词表损坏的行不能证明「原回执存在」——既不 Duplicate
                    // 放行,也不覆盖现场;按 Mismatch fail-closed 并留痕。
                    if Self::state_from_db(&row.state).is_none() {
                        log::error!(
                            "[receipt] stored row {} has unparseable state vocabulary",
                            row.command_id
                        );
                        return Ok(Admit::Mismatch);
                    }
                    if row.fingerprint != fingerprint
                        || row.client_session_id != owner
                    {
                        return Ok(Admit::Mismatch);
                    }
                    return Ok(Admit::Duplicate); // 重复：已接受且在 TTL 内
                }
            }
            tx.execute("DELETE FROM command_receipts WHERE command_id = ?1", [&command_id])?;
            let row = receipts::ReceiptRow {
                command_id: command_id.clone(),
                fingerprint,
                client_session_id: owner,
                state: Self::state_to_db(RingingCommandState::Accepted).to_string(),
                terminal_event_id: None,
                error_code: None,
                result_json: None,
                sync_result_json: None,
                accepted_at_ms: now_ms as i64,
            };
            receipts::upsert(&tx, &row)?;
            receipts::delete_expired(&tx, Self::ttl_cutoff_ms(now_ms))?;
            receipts::prune_oldest_over(&tx, max_entries)?;
            tx.commit()?;
            Ok(Admit::New)
        });
        match outcome {
            Ok(Admit::New) => Ok(true),
            Ok(Admit::Duplicate) => Ok(false),
            // 指纹/owner 冲突与 DB 故障都是 Err——闸门 fail-closed，不派发执行。
            Ok(Admit::Mismatch) => Err(()),
            Err(error) => {
                log::error!("[receipt] reservation gate storage failure, failing closed: {error}");
                Err(())
            }
        }
    }

    pub fn is_known(&self, command_id: &str) -> bool {
        let command_id = command_id.to_string();
        self.db
            .read_blocking(move |conn| receipts::get(conn, &command_id))
            .map_err(|error| {
                log::error!("[receipt] is_known storage failure, treating as unknown: {error}");
                error
            })
            .ok()
            .flatten()
            .is_some_and(|row| Self::row_is_fresh(&row))
    }

    fn row_is_fresh(row: &receipts::ReceiptRow) -> bool {
        let now_ms = unix_millis();
        row.accepted_at_ms >= Self::ttl_cutoff_ms(now_ms) && row.accepted_at_ms <= now_ms as i64
    }

    /// 转发失败回滚预留。
    pub fn rollback(&mut self, command_id: &str) {
        let command_id = command_id.to_string();
        if let Err(error) = self
            .db
            .write_blocking(move |conn| receipts::delete(conn, &command_id))
        {
            log::error!("[receipt] rollback storage failure: {error}");
        }
    }

    pub fn mark_running(&mut self, command_id: &str) {
        let command_id = command_id.to_string();
        if let Err(error) = self.db.write_blocking(move |conn| {
            conn.execute(
                "UPDATE command_receipts SET state = 'running' \
                 WHERE command_id = ?1 AND state = 'accepted'",
                [&command_id],
            )
        }) {
            log::error!("[receipt] mark_running storage failure: {error}");
        }
    }

    pub fn mark_terminal(
        &mut self,
        command_id: &str,
        state: RingingCommandState,
        event_id: Option<String>,
        error_code: Option<String>,
    ) {
        self.mark_terminal_with_result(command_id, state, event_id, error_code, None);
    }

    /// 终态落库，同时记录可被 v2 ACK/status 回放的 typed payload。
    pub fn mark_terminal_with_result(
        &mut self,
        command_id: &str,
        state: RingingCommandState,
        event_id: Option<String>,
        error_code: Option<String>,
        result: Option<RingingV2CommandResult>,
    ) {
        let command_id = command_id.to_string();
        let state = Self::state_to_db(state).to_string();
        let result_json = result.as_ref().and_then(|result| serde_json::to_string(result).ok());
        let write = self.db.write_blocking(move |conn| {
            receipts::update_state(
                conn,
                &command_id,
                &state,
                event_id.as_deref(),
                error_code.as_deref(),
                result_json.as_deref(),
                None,
            )
        });
        if let Err(error) = write {
            log::error!("[receipt] mark_terminal storage failure: {error}");
        }
    }

    /// 同步命令终态（B05）：完整 result/operation/revision 落 receipt，
    /// ACK 丢失后重放原 command_id 或轮询 status 都能恢复原结果。
    /// `succeeded` 且不调用此方法 = 引擎类命令（无同步结果，不伪造）。
    pub fn mark_terminal_sync(&mut self, command_id: &str, sync_result: RingingV2SyncResult) {
        let command_id = command_id.to_string();
        let sync_json = match serde_json::to_string(&sync_result) {
            Ok(json) => json,
            Err(error) => {
                log::error!("[receipt] sync_result serialization failed, skip persisting: {error}");
                return;
            }
        };
        let state = Self::state_to_db(RingingCommandState::Succeeded).to_string();
        let write = self.db.write_blocking(move |conn| {
            receipts::update_state(
                conn,
                &command_id,
                &state,
                None,
                None,
                None,
                Some(&sync_json),
            )
        });
        if let Err(error) = write {
            log::error!("[receipt] mark_terminal_sync storage failure: {error}");
        }
    }

    /// 巡检 Accepted/Running 超时无终态的 receipt 并告警（返回告警清单）。
    ///
    /// 冻结事故（2026-09-02，session 692d1605 t7）："ConversationCancel accepted
    /// 但永无终态"的僵尸只能靠人肉轮询发现。此方法由 daemon 周期任务调用：
    /// 超过 `warn_after` 未达终态即 WARN，同一 receipt 按 `repeat_interval`
    /// 限频重复告警，直至终态折叠。纯内存限频状态，不触发 persist。
    ///
    /// 折叠窗口只有 `RECEIPT_TTL`：过了 TTL 的条目幂等表已不再认它，继续告警
    /// 只会把日志刷满（2026-10-05 实测 2487 条同源告警）。这里直接把过期条目
    /// 摘掉，告警只覆盖 `warn_after..TTL` 这段真正还有救的窗口。
    pub fn warn_stale_running(
        &mut self,
        warn_after: Duration,
        repeat_interval: Duration,
    ) -> Vec<StaleRunningReceipt> {
        let now_ms = unix_millis();
        let repeat_ms = repeat_interval.as_millis() as u64;
        let cutoff = Self::ttl_cutoff_ms(now_ms);
        // 巡检顺带物理摘除过期行（原实现如此：过 TTL 的僵尸不再占告警面）。
        let candidates = self
            .db
            .write_blocking(move |conn| -> Result<Vec<receipts::ReceiptRow>, qaqh_session::db::DbError> {
                let tx = conn.transaction()?;
                receipts::delete_expired(&tx, cutoff)?;
                let rows = receipts::list_unsettled(&tx, cutoff)?;
                tx.commit()?;
                Ok(rows)
            })
            .unwrap_or_else(|error| {
                log::error!("[receipt] stale scan storage failure, skipping this sweep: {error}");
                Vec::new()
            });
        let live_ids: std::collections::HashSet<String> = candidates
            .iter()
            .map(|row| row.command_id.clone())
            .collect();
        let mut warn_state = self.warn_state.lock().unwrap_or_else(|e| e.into_inner());
        warn_state.retain(|id, _| live_ids.contains(id));
        let mut stale = Vec::new();
        for row in &candidates {
            let Some(state) = Self::state_from_db(&row.state) else {
                continue;
            };
            let age_ms = now_ms.saturating_sub(row.accepted_at_ms.max(0) as u64);
            if age_ms < warn_after.as_millis() as u64 {
                continue;
            }
            let warned_recently = warn_state
                .get(&row.command_id)
                .is_some_and(|last| now_ms.saturating_sub(*last) < repeat_ms);
            if warned_recently {
                continue;
            }
            warn_state.insert(row.command_id.clone(), now_ms);
            stale.push(StaleRunningReceipt {
                command_id: row.command_id.clone(),
                state,
                age_secs: age_ms / 1000,
                client_session_id: row.client_session_id.clone(),
            });
        }
        for entry in &stale {
            log::warn!(
                "[ringing] command receipt stuck in {:?} for {}s (no terminal event): {} client_session={:?} — possible frozen worker/seal path",
                entry.state,
                entry.age_secs,
                entry.command_id,
                entry.client_session_id
            );
        }
        stale
    }

    pub fn status_for_session(
        &self,
        command_id: &str,
        client_session_id: &str,
    ) -> Option<RingingCommandStatus> {
        let existing = self.existing_row(command_id, client_session_id)?;
        Some(RingingCommandStatus {
            command_id: command_id.to_string(),
            state: existing.state,
            payload_fingerprint: existing.payload_fingerprint,
            terminal_event_id: existing.terminal_event_id,
            error_code: existing.error_code,
        })
    }

    /// v2 status view: same receipt plus the typed terminal payload.
    pub fn v2_status_for_session(
        &self,
        command_id: &str,
        client_session_id: &str,
    ) -> Option<RingingV2CommandStatus> {
        let existing = self.existing_row(command_id, client_session_id)?;
        Some(RingingV2CommandStatus {
            command_id: command_id.to_string(),
            state: existing.state,
            payload_fingerprint: existing.payload_fingerprint,
            terminal_event_id: existing.terminal_event_id,
            error_code: existing.error_code,
            result: existing.result,
            sync_result: existing.sync_result,
        })
    }

    /// Existing receipt for a replayed `command_id`, scoped to its owning lease.
    pub fn existing_receipt_for_session(
        &self,
        command_id: &str,
        client_session_id: &str,
    ) -> Option<ExistingCommandReceipt> {
        self.existing_row(command_id, client_session_id)
    }

    /// 三条读路径的共同闸门：行存在、在 TTL 内、owner 匹配；
    /// DB 故障与损坏数据都是 `None`（unknown，SDK 不得自动重试副作用）。
    fn existing_row(&self, command_id: &str, client_session_id: &str) -> Option<ExistingCommandReceipt> {
        let command_id = command_id.to_string();
        let owner = client_session_id.to_string();
        let row = self
            .db
            .read_blocking(move |conn| receipts::get(conn, &command_id))
            .map_err(|error| {
                log::error!("[receipt] status storage failure, treating as unknown: {error}");
                error
            })
            .ok()
            .flatten()?;
        if !Self::row_is_fresh(&row) || row.client_session_id.as_deref() != Some(owner.as_str()) {
            return None;
        }
        Self::row_to_existing(&row)
    }
}

const RECEIPT_TTL: Duration = Duration::from_secs(RECEIPT_POLICY.ttl_secs);

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> (tempfile::TempDir, PendingCommandStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(Db::open(dir.path().join("qaqh.db")).expect("open db"));
        (dir, PendingCommandStore::new(db))
    }

    /// 过 TTL 的条目不再参与幂等，也不该继续占着日志（2026-10-05 实测 2487 条
    /// 同源 `stuck in Running` 告警全部来自早已过期的僵尸条目）。
    #[test]
    fn expired_receipts_are_pruned_instead_of_warning_forever() {
        let (_dir, mut store) = test_store();
        let uuid_command = "92455601-b53a-4f25-8df5-94124676055b";
        assert!(
            store
                .record_fingerprint_for_session(uuid_command, "fp", "session-a")
                .expect("first accept")
        );
        // 用 SQL 把行龄推过期——不靠私有字段,过期判据走的是同一条 TTL 口径。
        let aged_row = receipts::ReceiptRow {
            command_id: uuid_command.to_string(),
            fingerprint: "fp".to_string(),
            client_session_id: Some("session-a".to_string()),
            state: "accepted".to_string(),
            terminal_event_id: None,
            error_code: None,
            result_json: None,
            sync_result_json: None,
            accepted_at_ms: unix_millis() as i64 - RECEIPT_TTL.as_millis() as i64 - 1000,
        };
        store
            .db
            .write_blocking(move |conn| receipts::upsert(conn, &aged_row))
            .expect("age the row");

        assert!(
            store
                .warn_stale_running(Duration::ZERO, Duration::ZERO)
                .is_empty(),
            "an expired receipt must not keep emitting warnings"
        );
        assert!(
            !store.is_known(uuid_command),
            "expired receipts are dropped from the table"
        );
        let alive: i64 = store
            .db
            .read_blocking(|conn| {
                conn.query_row("SELECT COUNT(*) FROM command_receipts", [], |row| row.get(0))
            })
            .expect("count");
        assert_eq!(alive, 0, "过期行必须被巡检物理摘除");
    }

    #[test]
    fn pending_command_idempotency() {
        let (_dir, mut store) = test_store();
        assert!(store.record("cmd-1"), "first accept");
        assert!(!store.record("cmd-1"), "duplicate within TTL rejected");
        assert!(store.is_known("cmd-1"));
        assert!(store.record("cmd-2"), "distinct id accepted");
        store.rollback("cmd-2");
        assert!(store.record("cmd-2"), "retry after rollback accepted");
    }

    #[test]
    fn command_receipts_are_scoped_to_the_owning_client_session() {
        let (_dir, mut store) = test_store();
        assert!(
            store
                .record_fingerprint_for_session("cmd-owner", "fp", "session-a")
                .expect("first accept")
        );
        assert!(store.status_for_session("cmd-owner", "session-a").is_some());
        assert!(store.status_for_session("cmd-owner", "session-b").is_none());
        assert!(
            store
                .record_fingerprint_for_session("cmd-owner", "fp", "session-b")
                .is_err()
        );
    }

    #[test]
    fn stale_running_receipts_are_warned_and_rate_limited() {
        let (_dir, mut store) = test_store();
        assert!(store.record("cmd-stuck"));
        // 0s 阈值：任何 Accepted/Running 立即命中。
        let first = store.warn_stale_running(Duration::ZERO, Duration::from_secs(60));
        assert_eq!(first.len(), 1, "stale receipt must be reported once");
        assert_eq!(first[0].command_id, "cmd-stuck");
        assert_eq!(first[0].state, RingingCommandState::Accepted);
        // 限频：重复间隔内再次巡检不再报告。
        let second = store.warn_stale_running(Duration::ZERO, Duration::from_secs(60));
        assert!(
            second.is_empty(),
            "repeat within interval must be suppressed"
        );
        // 限频归零（间隔 0）：再次报告。
        let third = store.warn_stale_running(Duration::ZERO, Duration::ZERO);
        assert_eq!(third.len(), 1, "repeat_interval=0 re-warns");
        // 终态折叠后不再告警。
        store.mark_terminal("cmd-stuck", RingingCommandState::Succeeded, None, None);
        let fourth = store.warn_stale_running(Duration::ZERO, Duration::ZERO);
        assert!(fourth.is_empty(), "terminal receipts are never stale");
    }
}

#[cfg(test)]
#[path = "pending_store/boundary_tests.rs"]
mod boundary_tests;
