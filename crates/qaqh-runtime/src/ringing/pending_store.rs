//! Pending command store — Ringing V1 command idempotency + 4096 cap.
//!
//! Extracted from the legacy daemon HTTP layer (now
//! `qaqh-daemon/src/axum_server`) for sharing between the old TCP and axum paths.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use qaqh_ringing::{
    RingingCommandState, RingingCommandStatus, RingingV2CommandResult, RingingV2CommandStatus,
    RingingV2ExistingResult,
};
use qaqh_session::canonical::causation_for_command;
use qaqh_session::session_fact_v2::{
    ControlDelta, ConversationDelta, ToolTerminalStatus, TurnTerminal,
};
use qaqh_session::session_fact_v2::{ProjectionEvent, ProjectionPayload};

/// 已 accepted 命令的幂等表（有界 TTL；accepted 后断线重试不得重复执行）。
#[derive(Debug, Default)]
pub struct PendingCommandStore {
    accepted: HashMap<String, CommandReceipt>,
    /// Canonical causation id → the client's `command_id`, for the ids that
    /// `causation_for_command` has to encode (mobile clients submit UUIDs).
    /// Facts carry the encoded form, so folding needs this way back.
    by_causation: HashMap<String, String>,
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
            by_causation: HashMap::new(),
            max_entries: 4096,
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
        store.load(&path);
        store
    }

    fn load(&mut self, path: &std::path::Path) {
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let Ok(saved) = serde_json::from_slice::<HashMap<String, PersistedCommandReceipt>>(&bytes)
        else {
            log::warn!("[ringing] command receipt store is unreadable; starting empty");
            return;
        };
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
        let ids: Vec<String> = self.accepted.keys().cloned().collect();
        for command_id in ids {
            self.index_causation(&command_id);
        }
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
        self.index_causation(command_id);
        while self.accepted.len() > self.max_entries {
            let victim = self
                .accepted
                .iter()
                .min_by_key(|(_, receipt)| receipt.accepted_at)
                .map(|(id, _)| id.clone())
                .expect("non-empty");
            self.unindex_causation(&victim);
            self.accepted.remove(&victim);
        }
        self.persist();
        Ok(true)
    }

    /// Index one receipt under the causation id its facts will carry, when that
    /// differs from the client's raw `command_id`.
    fn index_causation(&mut self, command_id: &str) {
        if let Some(causation) = causation_for_command(command_id)
            && causation.as_str() != command_id
        {
            self.by_causation
                .insert(causation.as_str().to_string(), command_id.to_string());
        }
    }

    fn unindex_causation(&mut self, command_id: &str) {
        if let Some(causation) = causation_for_command(command_id) {
            self.by_causation.remove(causation.as_str());
        }
    }

    /// The `command_id` a fact's `causation_id` belongs to.
    fn command_for_causation(&self, causation: &str) -> Option<String> {
        if self.accepted.contains_key(causation) {
            return Some(causation.to_string());
        }
        self.by_causation.get(causation).cloned()
    }

    pub fn is_known(&self, command_id: &str) -> bool {
        self.accepted
            .get(command_id)
            .is_some_and(|receipt| receipt.accepted_at + RECEIPT_TTL > Instant::now())
    }

    /// 转发失败回滚预留。
    pub fn rollback(&mut self, command_id: &str) {
        self.unindex_causation(command_id);
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

    /// Canonical fact 投影链上的回执折叠（hub-fact-bus spec 阶段 2.3）。
    ///
    /// ACK 只代表命令进入了 worker；业务终态由这条折叠链负责。事件侧的
    /// `causation_id` 是 worker 在本次 dispatch 内写下的（`FactCausation`），
    /// 可能已被 [`causation_for_command`] 编码过，故先还原成 client 的
    /// `command_id` 再落终态。
    /// 降级说明：v1 的 `SkillsUpdated` / `OperationCompleted` / `OperationFailed` /
    /// `SessionStateChanged` 在 fact 侧无一比一对应物（spec §6 冻结 canonical log
    /// 磁盘格式，暂不补 fact），相关回执靠 TTL 过期而非事件折叠。纯对话回合的
    /// `TurnFinished` 同理——生产侧尚未写 turn fact，只有回合内的
    /// tool/interaction 终态能折叠到回执。
    pub fn observe_projection_events(&mut self, events: &[ProjectionEvent]) {
        for event in events {
            let Some(causation) = event.causation_id.as_ref() else {
                continue;
            };
            let terminal = match &event.payload {
                ProjectionPayload::ConversationDelta(ConversationDelta::TurnFinished {
                    terminal,
                    error,
                    ..
                }) => match terminal {
                    TurnTerminal::Failed => {
                        Some((RingingCommandState::Failed, error_code_of(error.as_ref())))
                    }
                    TurnTerminal::Completed | TurnTerminal::Cancelled => {
                        Some((RingingCommandState::Succeeded, None))
                    }
                },
                ProjectionPayload::ConversationDelta(ConversationDelta::CompactionApplied {
                    ..
                }) => Some((RingingCommandState::Succeeded, None)),
                ProjectionPayload::ControlDelta(ControlDelta::ToolFinished {
                    terminal_status,
                    error,
                    ..
                }) => match terminal_status {
                    ToolTerminalStatus::Failed
                    | ToolTerminalStatus::TimedOut
                    | ToolTerminalStatus::Denied => {
                        Some((RingingCommandState::Failed, error_code_of(error.as_ref())))
                    }
                    _ => Some((RingingCommandState::Succeeded, None)),
                },
                ProjectionPayload::ControlDelta(
                    ControlDelta::InteractionResolved { .. }
                    | ControlDelta::InteractionExpired { .. },
                ) => Some((RingingCommandState::Succeeded, None)),
                ProjectionPayload::ControlDelta(ControlDelta::DriverChanged { .. }) => {
                    Some((RingingCommandState::Succeeded, None))
                }
                _ => None,
            };
            if let Some((state, error_code)) = terminal {
                let Some(command_id) = self.command_for_causation(causation.as_str()) else {
                    continue;
                };
                self.mark_terminal_with_result(
                    &command_id,
                    state,
                    Some(event.event_id.0.clone()),
                    error_code,
                    None,
                );
            }
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
            self.unindex_causation(&command_id);
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

/// Derive the typed payload a client can reconcile from, when the terminal
/// event carries one. Permission resolution is not yet a canonical fact, so it
/// is intentionally absent until the permission registry lands.
/// Turn/Tool 失败时的错误码提取（fact 侧折叠用）。
fn error_code_of<E>(error: Option<&E>) -> Option<String>
where
    E: HasErrorCode,
{
    error.map(|e| e.code().to_string())
}

/// `TurnError` / `ToolError` 共有的 code 字段的最小访问接口。
trait HasErrorCode {
    fn code(&self) -> &str;
}

impl HasErrorCode for qaqh_session::session_fact_v2::TurnError {
    fn code(&self) -> &str {
        &self.code
    }
}

impl HasErrorCode for qaqh_session::session_fact_v2::ToolError {
    fn code(&self) -> &str {
        &self.code
    }
}

const RECEIPT_TTL: Duration = Duration::from_secs(300);

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use qaqh_session::canonical::{generate_ulid, ulid_from_text};
    use qaqh_session::session_fact_v2::{ActorKind, ActorRef, Delivery, EventId, InteractionId};

    use super::*;

    fn control_event(causation: Option<EventId>) -> ProjectionEvent {
        ProjectionEvent {
            event_id: EventId::new(generate_ulid()),
            source_fact_seq: 1,
            source_event_id: EventId::new(generate_ulid()),
            causation_id: causation,
            ts_ms: Some(1_789_830_000_000),
            stream_key: qaqh_session::session_fact_v2::StreamKey::Channel(
                qaqh_domain::RingingChannel::Control,
            ),
            delivery: Delivery::Ephemeral,
            projection_slot: None,
            projection_index: None,
            payload: ProjectionPayload::ControlDelta(ControlDelta::InteractionResolved {
                revision: 1,
                interaction_id: InteractionId::new("int_test"),
                decision: qaqh_session::session_fact_v2::ContentValue::Inline {
                    text: "{\"decision\":\"approved\"}".into(),
                },
                verdict: None,
                resolved_by: ActorRef {
                    kind: ActorKind::User,
                    id: "user".into(),
                    display_name: None,
                },
                resolution_seq: 1,
            }),
        }
    }

    /// 2026-10-05 回归（`docs/bug-ringing-v2-commands-stuck-in-running.md`）：
    /// fact 链折叠是 v2 命令唯一可达的终态来源，而 fact 上的 `causation_id` 是
    /// 客户端命令 id 的 canonical 派生（移动端提交 UUID，落盘为 `ulid_from_text`
    /// 编码）。折叠必须按同一映射找回命令 id，否则回执永远停在 Running。
    #[test]
    fn receipts_fold_through_the_canonical_causation_lane() {
        let mut store = PendingCommandStore::new();
        let uuid_command = "92455601-b53a-4f25-8df5-94124676055b".to_string();
        let ulid_command = generate_ulid();
        for command_id in [&uuid_command, &ulid_command] {
            assert!(
                store
                    .record_fingerprint_for_session(command_id, "fp", "session-a")
                    .expect("first accept")
            );
            store.mark_running(command_id);
        }

        store.observe_projection_events(&[
            control_event(Some(EventId::new(ulid_from_text(&uuid_command)))),
            control_event(Some(EventId::new(ulid_command.as_str()))),
        ]);

        for command_id in [&uuid_command, &ulid_command] {
            assert_eq!(
                store
                    .v2_status_for_session(command_id, "session-a")
                    .expect("receipt")
                    .state,
                RingingCommandState::Succeeded,
                "{command_id} must fold from its canonical causation"
            );
        }
    }

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
            !store.accepted.contains_key(uuid_command) && store.by_causation.is_empty(),
            "expired receipts are dropped from both indexes"
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
