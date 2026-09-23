//! Canonical restore reconciliation for orphaned tool calls.
//!
//! `MessageStore::from_messages` repairs an assistant `tool_use` without a
//! persisted `tool_result` with a synthetic `[RESTORE]` placeholder. The
//! placeholder defaults to "not executed", which is only correct when no
//! durable execution intent exists.
//!
//! P3 makes `ToolIntent` the execution-start fact and `ToolFinished` the
//! terminal fact. This module reads the committed canonical ledger and refines
//! the placeholder when a call has started but its model-visible result was
//! lost. The legacy `tool_outbox.wal` is read only as a migration fallback for
//! sessions created before canonical wiring; new executions never write it.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use qaqh_session::canonical::{
    CANONICAL_IDENTITY_FILE, CanonicalSessionIdentity, CommittedFactReader, EVENTS_FILE,
};
use qaqh_session::session_fact_v2::{FactPayload, ToolCallId, ToolTerminalStatus};
use serde::Deserialize;

const LEGACY_OUTBOX_FILE_NAME: &str = "tool_outbox.wal";

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
struct LegacyOutboxRecord {
    call_id: String,
    name: String,
    status: String,
}

#[derive(Debug, Default)]
struct CanonicalToolState {
    started: bool,
    terminal: Option<ToolTerminalStatus>,
    name: Option<String>,
}

impl CanonicalToolState {
    fn started(&self) -> bool {
        self.started
            || self
                .terminal
                .is_some_and(|status| status != ToolTerminalStatus::Cancelled)
    }

    fn restore_note(&self, fallback_name: &str) -> String {
        let name = self.name.as_deref().unwrap_or(fallback_name);
        match self.terminal {
            Some(status) => format!(
                "[RESTORE] Tool \"{name}\" has canonical terminal {} before the session was \
                 saved, but its result was not persisted. Side effects may have occurred — \
                 verify the workspace state before retrying; do NOT blindly re-run.",
                terminal_label(status)
            ),
            None => format!(
                "[RESTORE] Tool \"{name}\" had a durable execution intent before the session was \
                 saved, but its terminal was not persisted (outcome unknown). Side effects may \
                 have occurred — verify the workspace state before retrying; do NOT blindly \
                 re-run."
            ),
        }
    }
}

fn terminal_label(status: ToolTerminalStatus) -> &'static str {
    match status {
        ToolTerminalStatus::Succeeded => "succeeded",
        ToolTerminalStatus::Failed => "failed",
        ToolTerminalStatus::Partial => "partial",
        ToolTerminalStatus::Cancelled => "cancelled",
        ToolTerminalStatus::TimedOut => "timed_out",
        ToolTerminalStatus::Backgrounded => "backgrounded",
        ToolTerminalStatus::Indeterminate => "indeterminate",
        ToolTerminalStatus::Denied => "denied",
    }
}

fn legacy_outbox_path(session_dir: &Path) -> PathBuf {
    session_dir.join(LEGACY_OUTBOX_FILE_NAME)
}

fn read_legacy_records(session_dir: &Path) -> Vec<LegacyOutboxRecord> {
    let path = legacy_outbox_path(session_dir);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            log::error!(
                "tool_recovery: read legacy outbox {} failed: {error}",
                path.display()
            );
            return Vec::new();
        }
    };
    let mut records = Vec::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<LegacyOutboxRecord>(trimmed) {
            Ok(record) => records.push(record),
            Err(error) => {
                log::error!(
                    "tool_recovery: corrupt legacy outbox tail in {} ({error})",
                    path.display()
                );
                break;
            }
        }
    }
    records
}

fn canonical_tool_states(
    session_dir: &Path,
) -> Result<HashMap<ToolCallId, CanonicalToolState>, String> {
    if !session_dir.join(CANONICAL_IDENTITY_FILE).exists()
        && !session_dir.join(EVENTS_FILE).exists()
    {
        return Ok(HashMap::new());
    }
    let identity =
        CanonicalSessionIdentity::open_or_create(session_dir).map_err(|error| error.to_string())?;
    let facts = CommittedFactReader::open(session_dir, identity.session_id, identity.log_id)
        .map_err(|error| error.to_string())?
        .read_all()
        .map_err(|error| error.to_string())?;

    let mut states: HashMap<ToolCallId, CanonicalToolState> = HashMap::new();
    for fact in facts {
        match fact.payload {
            FactPayload::ToolCallDeclared(payload) => {
                states.entry(payload.call_id).or_default().name = Some(payload.tool_name);
            }
            FactPayload::ToolIntent(payload) => {
                states.entry(payload.call_id).or_default().started = true;
            }
            FactPayload::ToolFinished(payload) => {
                let state = states.entry(payload.call_id).or_default();
                if payload.execution_id.is_some() {
                    state.started = true;
                }
                state.terminal = Some(payload.terminal_status);
            }
            _ => {}
        }
    }
    Ok(states)
}

fn retain_legacy_records(session_dir: &Path, keep: &[String]) {
    let path = legacy_outbox_path(session_dir);
    let records = read_legacy_records(session_dir);
    if records.is_empty() {
        return;
    }
    let kept: Vec<&LegacyOutboxRecord> = records
        .iter()
        .filter(|record| keep.iter().any(|call_id| call_id == &record.call_id))
        .collect();
    if kept.len() == records.len() {
        return;
    }
    if kept.is_empty() {
        if let Err(error) = fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::error!(
                "tool_recovery: remove retired legacy outbox {} failed: {error}",
                path.display()
            );
        }
        return;
    }

    let tmp = path.with_extension("wal.retire.tmp");
    let result = (|| -> std::io::Result<()> {
        let mut file = File::create(&tmp)?;
        for record in kept {
            let line = serde_json::to_string(record)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
            file.write_all(line.as_bytes())?;
            file.write_all(b"\n")?;
        }
        file.flush()?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &path)
    })();
    if let Err(error) = result {
        log::error!(
            "tool_recovery: legacy outbox retirement rewrite {} failed: {error}",
            path.display()
        );
    }
}

/// Refine synthetic `[RESTORE]` placeholders from the canonical ledger.
///
/// Canonical evidence is authoritative. The legacy outbox is consulted only
/// when no canonical state exists for that call, then retained until the
/// refined message is durably saved so an immediate crash cannot lose the
/// "execution started" fact.
pub fn reconcile_store(store: &mut qaqh_message::MessageStore, seed: &str) {
    reconcile_store_in(&qaqh_types::platform::sessions_dir().join(seed), store);
}

/// Directory-injected variant for tests and non-standard data roots.
pub fn reconcile_store_in(session_dir: &Path, store: &mut qaqh_message::MessageStore) {
    let canonical = match canonical_tool_states(session_dir) {
        Ok(states) => states,
        Err(error) => {
            log::error!(
                "tool_recovery: canonical ledger read for {} failed: {error}",
                session_dir.display()
            );
            HashMap::new()
        }
    };
    let legacy: HashMap<String, LegacyOutboxRecord> = read_legacy_records(session_dir)
        .into_iter()
        .map(|record| (record.call_id.clone(), record))
        .collect();

    let mut keep_legacy = Vec::new();
    let mut amended = 0usize;
    for (wire_call_id, declared_name) in store.synthetic_repair_entries() {
        let canonical_call_id = crate::agent::tool_runtime::canonical_call_id(&wire_call_id);
        let (note, from_legacy) = if let Some(state) = canonical.get(&canonical_call_id) {
            if !state.started() {
                continue;
            }
            (state.restore_note(&declared_name), false)
        } else if let Some(record) = legacy.get(&wire_call_id) {
            (
                format!(
                    "[RESTORE] Tool \"{}\" was executed before the session was saved, but its \
                     result was not persisted (legacy outcome: {}). Side effects may have \
                     occurred — verify the workspace state before retrying; do NOT blindly \
                     re-run.",
                    record.name, record.status
                ),
                true,
            )
        } else {
            continue;
        };
        if store.amend_synthetic_repair(&wire_call_id, &note) {
            amended += 1;
            if from_legacy {
                keep_legacy.push(wire_call_id);
            }
        }
    }

    if amended > 0 {
        log::info!("tool_recovery: refined {amended} orphan tool_use repair(s)");
    }
    retain_legacy_records(session_dir, &keep_legacy);
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_message::MessageStore;
    use qaqh_session::canonical::{ToolLedger, WriterId, generate_ulid, sha256_content_hash};
    use qaqh_session::session_fact_v2::{
        EventId, ExecutionId, PolicyDecisionRef, SideEffectClass, ToolFinished, ToolIntent,
        ToolIntentPolicyOutcome, ToolMetrics, ToolReplayCapability,
    };

    fn orphan_store(call_id: &str, name: &str) -> MessageStore {
        let mut assistant = qaqh_types::Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: Vec::new(),
        };
        assistant.content.push(qaqh_types::ContentBlock::ToolUse {
            id: call_id.to_string(),
            name: name.to_string(),
            input: serde_json::json!({"command": "echo hi"}),
        });
        let messages = vec![qaqh_types::Message::user("do it"), assistant];
        let (store, repairs) = MessageStore::from_messages("recovery-seed", &messages, 0);
        assert_eq!(repairs.len(), 1, "orphan tool_use must be repaired");
        store
    }

    fn open_ledger(
        session_dir: &Path,
        writer: &str,
    ) -> (
        ToolLedger,
        qaqh_session::session_fact_v2::ToolCallId,
        ExecutionId,
    ) {
        let identity =
            CanonicalSessionIdentity::open_or_create(session_dir).expect("canonical identity");
        let now = 1_700_000_000_000i64;
        let mut ledger = ToolLedger::open(
            session_dir,
            identity.session_id,
            identity.log_id,
            WriterId::new(writer),
            now,
            60_000,
        )
        .expect("open canonical ledger");
        let call_id = crate::agent::tool_runtime::canonical_call_id("wire-call");
        let execution_id = ExecutionId::new(format!("exec_{}", generate_ulid()));
        let intent = ToolIntent {
            call_id: call_id.clone(),
            execution_id: execution_id.clone(),
            idempotency_key: None,
            replay_capability: ToolReplayCapability::NoReplay,
            policy_decision: PolicyDecisionRef {
                outcome: ToolIntentPolicyOutcome::Allow,
                rule_id: "test".into(),
                decided_at_ms: now,
                reason_ref: None,
            },
            effective_args_ref: None,
            effective_args_hash: None,
            sandbox_spec_hash: sha256_content_hash(b"sandbox"),
            side_effect_class: SideEffectClass::WorkspaceWrite,
            intent_at_ms: now,
        };
        ledger
            .append_intent(EventId::new(generate_ulid()), None, intent, now)
            .expect("append intent");
        (ledger, call_id, execution_id)
    }

    fn repair_note(store: &MessageStore) -> String {
        store
            .turns()
            .iter()
            .flat_map(|turn| turn.steps.iter())
            .flat_map(|step| step.tool_results.iter())
            .find_map(|result| {
                result.content.iter().find_map(|block| match block {
                    qaqh_types::ContentBlock::ToolResult { result, .. } => {
                        Some(result.model_text().to_string())
                    }
                    _ => None,
                })
            })
            .expect("repair result exists")
    }

    #[test]
    fn canonical_open_intent_refines_restore_as_unknown() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (ledger, _, _) = open_ledger(dir.path(), "canonical-open-intent");
        drop(ledger);
        let mut store = orphan_store("wire-call", "edit");

        reconcile_store_in(dir.path(), &mut store);

        let note = repair_note(&store);
        assert!(note.contains("durable execution intent"), "{note}");
        assert!(note.contains("outcome unknown"), "{note}");
        assert!(
            !legacy_outbox_path(dir.path()).exists(),
            "canonical reconciliation must not create the retired WAL"
        );
    }

    #[test]
    fn canonical_terminal_refines_restore_with_terminal_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut ledger, call_id, execution_id) = open_ledger(dir.path(), "canonical-terminal");
        let now = 1_700_000_000_001i64;
        ledger
            .append_finished(
                EventId::new(generate_ulid()),
                None,
                ToolFinished {
                    call_id,
                    execution_id: Some(execution_id),
                    terminal_status: ToolTerminalStatus::Succeeded,
                    output_ref: None,
                    error: None,
                    metrics: ToolMetrics {
                        started_at_ms: now - 1,
                        finished_at_ms: now,
                        retry_count: 0,
                        output_bytes: 0,
                        progress_bytes_total: 0,
                    },
                    reconciled: false,
                    evidence_ref: None,
                    evidence_fact_seq: None,
                    evidence_event_id: None,
                    recovery_ref: None,
                    finished_at_ms: now,
                },
                now,
            )
            .expect("append finished");
        drop(ledger);
        let mut store = orphan_store("wire-call", "edit");

        reconcile_store_in(dir.path(), &mut store);

        let note = repair_note(&store);
        assert!(note.contains("canonical terminal succeeded"), "{note}");
        assert!(note.contains("do NOT blindly re-run"), "{note}");
    }

    #[test]
    fn executionless_cancel_keeps_the_not_executed_placeholder() {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity =
            CanonicalSessionIdentity::open_or_create(dir.path()).expect("canonical identity");
        let now = 1_700_000_000_002i64;
        let mut ledger = ToolLedger::open(
            dir.path(),
            identity.session_id,
            identity.log_id,
            WriterId::new("canonical-cancel"),
            now,
            60_000,
        )
        .expect("open canonical ledger");
        let call_id = crate::agent::tool_runtime::canonical_call_id("wire-call");
        ledger
            .append_finished(
                EventId::new(generate_ulid()),
                None,
                ToolFinished {
                    call_id,
                    execution_id: None,
                    terminal_status: ToolTerminalStatus::Cancelled,
                    output_ref: None,
                    error: None,
                    metrics: ToolMetrics {
                        started_at_ms: now,
                        finished_at_ms: now,
                        retry_count: 0,
                        output_bytes: 0,
                        progress_bytes_total: 0,
                    },
                    reconciled: false,
                    evidence_ref: None,
                    evidence_fact_seq: None,
                    evidence_event_id: None,
                    recovery_ref: None,
                    finished_at_ms: now,
                },
                now,
            )
            .expect("append executionless cancelled");
        drop(ledger);
        let mut store = orphan_store("wire-call", "edit");

        reconcile_store_in(dir.path(), &mut store);

        assert!(
            repair_note(&store).contains("not executed"),
            "executionless cancellation must keep the default not-executed note"
        );
    }

    #[test]
    fn legacy_outbox_is_read_only_migration_and_pruned_after_resolution() {
        let dir = tempfile::tempdir().expect("tempdir");
        let record = LegacyOutboxRecord {
            call_id: "legacy-call".into(),
            name: "bash".into(),
            status: "ok".into(),
        };
        fs::write(
            legacy_outbox_path(dir.path()),
            format!("{}\n", serde_json::to_string(&record).expect("serialize")),
        )
        .expect("write legacy outbox");
        let mut store = orphan_store("legacy-call", "bash");

        reconcile_store_in(dir.path(), &mut store);
        assert!(repair_note(&store).contains("legacy outcome: ok"));
        assert!(
            legacy_outbox_path(dir.path()).exists(),
            "legacy evidence must survive until the refined message is saved"
        );

        let resolved = vec![
            qaqh_types::Message::user("do it"),
            qaqh_types::Message::tool("legacy-call", "already persisted", true),
        ];
        let (mut resolved, repairs) = MessageStore::from_messages("recovery-seed", &resolved, 0);
        assert!(repairs.is_empty());
        reconcile_store_in(dir.path(), &mut resolved);
        assert!(
            !legacy_outbox_path(dir.path()).exists(),
            "resolved legacy record must be pruned"
        );
    }
}
