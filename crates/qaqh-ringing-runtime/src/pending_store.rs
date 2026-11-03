//! Pending command store — Ringing V1 command idempotency + 4096 cap.
//!
//! Extracted from the legacy daemon HTTP layer (now
//! `qaqh-daemon/src/axum_server`) for sharing between the old TCP and axum paths.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use qaqh_ringing::{
    RingingCommandState, RingingCommandStatus, RingingV2CommandResult, RingingV2CommandStatus,
    RingingV2ExistingResult,
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

/// Explicit-path receipt loading never silently treats unreadable data as empty.
#[derive(Debug)]
pub enum ReceiptLoadError {
    Read(std::io::Error),
    Decode(serde_json::Error),
}

impl std::fmt::Display for ReceiptLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(error) => write!(f, "cannot read command receipt store: {error}"),
            Self::Decode(error) => write!(f, "cannot decode command receipt store: {error}"),
        }
    }
}

impl std::error::Error for ReceiptLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            Self::Decode(error) => Some(error),
        }
    }
}

/// 已 accepted 命令的幂等表（有界 TTL；accepted 后断线重试不得重复执行）。
#[derive(Debug, Default)]
pub struct PendingCommandStore {
    accepted: HashMap<String, CommandReceipt>,
    max_entries: usize,
    persistence_path: Option<PathBuf>,
}

/// 冻结告警条目：Accepted/Running 超时无终态的 receipt 快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleRunningReceipt {
    pub command_id: String,
    pub state: RingingCommandState,
    pub age_secs: u64,
    pub client_session_id: Option<String>,
}

#[derive(Debug, Clone)]
struct CommandReceipt {
    fingerprint: String,
    client_session_id: Option<String>,
    accepted_at: Instant,
    state: RingingCommandState,
    terminal_event_id: Option<String>,
    error_code: Option<String>,
    /// 终态的 typed payload（ask/plan 等）。ACK 丢失后重放 command_id 或
    /// 轮询 command_status 时返回，客户端不必再靠 code/message 猜结果。
    result: Option<RingingV2CommandResult>,
    /// 上次冻结告警时刻（epoch ms，仅内存不持久化）：同一 receipt 的重复
    /// 告警按间隔限频，避免周期巡检刷屏。
    last_stale_warn_ms: Option<u64>,
}

/// v2 command 的已有 receipt 视图（含指纹，供重放时做 payload 校验）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingCommandReceipt {
    pub state: RingingCommandState,
    pub payload_fingerprint: String,
    pub terminal_event_id: Option<String>,
    pub error_code: Option<String>,
    pub result: Option<RingingV2CommandResult>,
}

impl ExistingCommandReceipt {
    pub fn into_existing(self) -> RingingV2ExistingResult {
        RingingV2ExistingResult::CommandReceipt {
            state: self.state,
            terminal_event_id: self.terminal_event_id,
            error_code: self.error_code,
            result: self.result,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PersistedCommandReceipt {
    fingerprint: String,
    #[serde(default)]
    client_session_id: Option<String>,
    accepted_at_ms: u64,
    state: RingingCommandState,
    #[serde(default)]
    terminal_event_id: Option<String>,
    #[serde(default)]
    error_code: Option<String>,
    #[serde(default)]
    result: Option<RingingV2CommandResult>,
}

impl PendingCommandStore {
    pub fn new() -> Self {
        Self {
            accepted: HashMap::new(),
            max_entries: RECEIPT_POLICY.max_entries,
            persistence_path: None,
        }
    }

    /// Receipt 存在 daemon 数据目录的独立 Ringing V1 namespace；只保存哈希，
    /// 不把命令正文、用户文本或附件元数据写入磁盘。
    pub fn new_persistent() -> Self {
        let data_dir = qaqh_types::platform::data_dir();
        let path = data_dir.join("ringing-command-receipts.json");
        let mut store = Self {
            persistence_path: Some(path.clone()),
            ..Self::new()
        };
        if let Err(error) = store.load(&path) {
            log::warn!("[ringing] {error}; starting empty");
        }
        store
    }

    /// Load the existing receipt cache without consulting process environment.
    ///
    /// A missing file starts empty; other read/format errors are reported. Writes
    /// retain the existing best-effort semantics: successfully recording a
    /// receipt does not promise durable execution or crash-safe exactly-once.
    pub fn open_persistent_at(path: impl Into<PathBuf>) -> Result<Self, ReceiptLoadError> {
        let path = path.into();
        let mut store = Self {
            persistence_path: Some(path.clone()),
            ..Self::new()
        };
        store.load(&path)?;
        Ok(store)
    }

    fn load(&mut self, path: &Path) -> Result<(), ReceiptLoadError> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(ReceiptLoadError::Read(error)),
        };
        let saved = serde_json::from_slice::<HashMap<String, PersistedCommandReceipt>>(&bytes)
            .map_err(ReceiptLoadError::Decode)?;
        let now_ms = unix_millis();
        for (command_id, receipt) in saved {
            let Some(age) = now_ms.checked_sub(receipt.accepted_at_ms) else {
                continue;
            };
            if age >= RECEIPT_TTL.as_millis() as u64 {
                continue;
            }
            self.accepted.insert(
                command_id,
                CommandReceipt {
                    fingerprint: receipt.fingerprint,
                    client_session_id: receipt.client_session_id,
                    accepted_at: Instant::now() - Duration::from_millis(age),
                    state: receipt.state,
                    terminal_event_id: receipt.terminal_event_id,
                    error_code: receipt.error_code,
                    result: receipt.result,
                    last_stale_warn_ms: None,
                },
            );
        }
        Ok(())
    }

    fn persist(&self) {
        let Some(path) = &self.persistence_path else {
            return;
        };
        let saved: HashMap<_, _> = self
            .accepted
            .iter()
            .filter_map(|(command_id, receipt)| {
                let age = receipt.accepted_at.elapsed();
                (age < RECEIPT_TTL).then(|| {
                    (
                        command_id.clone(),
                        PersistedCommandReceipt {
                            fingerprint: receipt.fingerprint.clone(),
                            client_session_id: receipt.client_session_id.clone(),
                            accepted_at_ms: unix_millis().saturating_sub(age.as_millis() as u64),
                            state: receipt.state,
                            terminal_event_id: receipt.terminal_event_id.clone(),
                            error_code: receipt.error_code.clone(),
                            result: receipt.result.clone(),
                        },
                    )
                })
            })
            .collect();
        let Ok(bytes) = serde_json::to_vec(&saved) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
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
        let now = Instant::now();
        if let Some(receipt) = self.accepted.get(command_id)
            && receipt.accepted_at + RECEIPT_TTL > now
        {
            if receipt.fingerprint != fingerprint
                || receipt.client_session_id.as_deref() != client_session_id
            {
                return Err(());
            }
            return Ok(false); // 重复：已接受且在 TTL 内
        }
        self.accepted.insert(
            command_id.to_string(),
            CommandReceipt {
                fingerprint: fingerprint.to_string(),
                client_session_id: client_session_id.map(str::to_string),
                accepted_at: now,
                state: RingingCommandState::Accepted,
                terminal_event_id: None,
                error_code: None,
                result: None,
                last_stale_warn_ms: None,
            },
        );
        while self.accepted.len() > self.max_entries {
            let victim = self
                .accepted
                .iter()
                .min_by_key(|(_, receipt)| receipt.accepted_at)
                .map(|(id, _)| id.clone())
                .expect("non-empty");
            self.accepted.remove(&victim);
        }
        self.persist();
        Ok(true)
    }

    pub fn is_known(&self, command_id: &str) -> bool {
        self.accepted
            .get(command_id)
            .is_some_and(|receipt| receipt.accepted_at + RECEIPT_TTL > Instant::now())
    }

    /// 转发失败回滚预留。
    pub fn rollback(&mut self, command_id: &str) {
        self.accepted.remove(command_id);
        self.persist();
    }

    pub fn mark_running(&mut self, command_id: &str) {
        if let Some(receipt) = self.accepted.get_mut(command_id) {
            if receipt.state == RingingCommandState::Accepted {
                receipt.state = RingingCommandState::Running;
            }
            self.persist();
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
        if let Some(receipt) = self.accepted.get_mut(command_id) {
            receipt.state = state;
            receipt.terminal_event_id = event_id;
            receipt.error_code = error_code;
            receipt.result = result;
            self.persist();
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
        let now = Instant::now();
        let now_ms = unix_millis();
        let repeat_ms = repeat_interval.as_millis() as u64;
        let expired: Vec<String> = self
            .accepted
            .iter()
            .filter(|(_, receipt)| receipt.accepted_at + RECEIPT_TTL <= now)
            .map(|(command_id, _)| command_id.clone())
            .collect();
        for command_id in expired {
            self.accepted.remove(&command_id);
        }
        let mut stale = Vec::new();
        for (command_id, receipt) in self.accepted.iter_mut() {
            if !matches!(
                receipt.state,
                RingingCommandState::Accepted | RingingCommandState::Running
            ) {
                continue;
            }
            let age = now.saturating_duration_since(receipt.accepted_at);
            if age < warn_after {
                continue;
            }
            let warned_recently = receipt
                .last_stale_warn_ms
                .is_some_and(|last| now_ms.saturating_sub(last) < repeat_ms);
            if warned_recently {
                continue;
            }
            receipt.last_stale_warn_ms = Some(now_ms);
            stale.push(StaleRunningReceipt {
                command_id: command_id.clone(),
                state: receipt.state,
                age_secs: age.as_secs(),
                client_session_id: receipt.client_session_id.clone(),
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
        self.accepted.get(command_id).and_then(|receipt| {
            (receipt.accepted_at + RECEIPT_TTL > Instant::now()
                && receipt.client_session_id.as_deref() == Some(client_session_id))
            .then(|| RingingCommandStatus {
                command_id: command_id.to_string(),
                state: receipt.state,
                payload_fingerprint: receipt.fingerprint.clone(),
                terminal_event_id: receipt.terminal_event_id.clone(),
                error_code: receipt.error_code.clone(),
            })
        })
    }

    /// v2 status view: same receipt plus the typed terminal payload.
    pub fn v2_status_for_session(
        &self,
        command_id: &str,
        client_session_id: &str,
    ) -> Option<RingingV2CommandStatus> {
        self.accepted.get(command_id).and_then(|receipt| {
            (receipt.accepted_at + RECEIPT_TTL > Instant::now()
                && receipt.client_session_id.as_deref() == Some(client_session_id))
            .then(|| RingingV2CommandStatus {
                command_id: command_id.to_string(),
                state: receipt.state,
                payload_fingerprint: receipt.fingerprint.clone(),
                terminal_event_id: receipt.terminal_event_id.clone(),
                error_code: receipt.error_code.clone(),
                result: receipt.result.clone(),
            })
        })
    }

    /// Existing receipt for a replayed `command_id`, scoped to its owning lease.
    pub fn existing_receipt_for_session(
        &self,
        command_id: &str,
        client_session_id: &str,
    ) -> Option<ExistingCommandReceipt> {
        self.accepted.get(command_id).and_then(|receipt| {
            (receipt.accepted_at + RECEIPT_TTL > Instant::now()
                && receipt.client_session_id.as_deref() == Some(client_session_id))
            .then(|| ExistingCommandReceipt {
                state: receipt.state,
                payload_fingerprint: receipt.fingerprint.clone(),
                terminal_event_id: receipt.terminal_event_id.clone(),
                error_code: receipt.error_code.clone(),
                result: receipt.result.clone(),
            })
        })
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

    /// 过 TTL 的条目不再参与幂等，也不该继续占着日志（2026-10-05 实测 2487 条
    /// 同源 `stuck in Running` 告警全部来自早已过期的僵尸条目）。
    #[test]
    fn expired_receipts_are_pruned_instead_of_warning_forever() {
        let mut store = PendingCommandStore::new();
        let uuid_command = "92455601-b53a-4f25-8df5-94124676055b";
        assert!(
            store
                .record_fingerprint_for_session(uuid_command, "fp", "session-a")
                .expect("first accept")
        );
        store
            .accepted
            .get_mut(uuid_command)
            .expect("receipt")
            .accepted_at = Instant::now() - RECEIPT_TTL - Duration::from_secs(1);

        assert!(
            store
                .warn_stale_running(Duration::ZERO, Duration::ZERO)
                .is_empty(),
            "an expired receipt must not keep emitting warnings"
        );
        assert!(
            !store.accepted.contains_key(uuid_command),
            "expired receipts are dropped from the index"
        );
    }

    #[test]
    fn pending_command_idempotency() {
        let mut store = PendingCommandStore::new();
        assert!(store.record("cmd-1"), "first accept");
        assert!(!store.record("cmd-1"), "duplicate within TTL rejected");
        assert!(store.is_known("cmd-1"));
        assert!(store.record("cmd-2"), "distinct id accepted");
        store.rollback("cmd-2");
        assert!(store.record("cmd-2"), "retry after rollback accepted");
    }

    #[test]
    fn command_receipts_are_scoped_to_the_owning_client_session() {
        let mut store = PendingCommandStore::new();
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
        let mut store = PendingCommandStore::new();
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
