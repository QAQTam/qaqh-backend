//! Server-issued, one-shot approval challenges（宿主侧审批防御纵深）。
//!
//! 从 `qaqh-webui-gateway/src/approval.rs`（+ `session.rs` 的 challenge 存取、
//! `lib.rs` 的投影白名单）移植。webview 永远拿不到 daemon 的 canonical
//! interaction / tool-call id,只有不透明 challenge id + 有界展示 details;
//! 宿主保存 canonical id 与 seed 绑定,并在消费时校验 scope。
//!
//! 与 gateway 的差异:TTL/上限/去重语义不变;存储从 per-browser-session 变为
//! 宿主单例（webview 是可信壳,不存在多会话并存的消费方）;lease 换新时不再
//! 强制清空（canonical id 跨 lease 续期仍有效）,仅在 active seed 切换时清空。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use qaqh_client::{AskAnswer, CommandOptions, ControlCommand, RingingCommand, ToolCommand};
use serde_json::{Value, json};

pub const APPROVAL_TTL: Duration = Duration::from_secs(5 * 60);
pub const MAX_PENDING_APPROVALS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalKind {
    ToolPermission,
    Ask,
    Plan,
}

impl ApprovalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ToolPermission => "tool_permission",
            Self::Ask => "ask",
            Self::Plan => "plan",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ApprovalChallenge {
    pub id: String,
    pub kind: ApprovalKind,
    /// canonical daemon id（`call_<ULID>` / `int_<ULID>`）——绝不出宿主。
    pub source_id: String,
    pub session_id: String,
    /// 有界展示 payload（白名单字段）。
    pub details: Value,
    pub issued_at: Instant,
}

impl ApprovalChallenge {
    pub fn is_expired(&self) -> bool {
        self.issued_at.elapsed() >= APPROVAL_TTL
    }

    pub fn expires_in_secs(&self) -> u64 {
        APPROVAL_TTL
            .saturating_sub(self.issued_at.elapsed())
            .as_secs()
    }

    pub fn view(&self) -> Value {
        json!({
            "challenge_id": self.id,
            "kind": self.kind.as_str(),
            "expires_in": self.expires_in_secs(),
            "details": self.details,
        })
    }
}

/// 宿主侧 challenge 存储:签发（去重/上限/惰性清理）与一次性消费。
#[derive(Default)]
pub struct ChallengeStore {
    challenges: Mutex<HashMap<String, ApprovalChallenge>>,
}

impl ChallengeStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, ApprovalChallenge>> {
        self.challenges
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// active seed 切换（attach 到别的会话）时清空:旧 seed 的 challenge 一律失效。
    pub fn clear(&self) {
        self.lock().clear();
    }

    fn cleanup(map: &mut HashMap<String, ApprovalChallenge>) {
        map.retain(|_, challenge| !challenge.is_expired());
    }

    /// 按 (kind, session_id, source_id) 去重签发;达到上限报 `approval_limit`。
    fn issue(
        &self,
        session_id: &str,
        kind: ApprovalKind,
        source_id: String,
        details: Value,
    ) -> Result<ApprovalChallenge, &'static str> {
        let mut map = self.lock();
        Self::cleanup(&mut map);
        if let Some(existing) = map.values().find(|challenge| {
            challenge.kind == kind
                && challenge.session_id == session_id
                && challenge.source_id == source_id
        }) {
            return Ok(existing.clone());
        }
        if map.len() >= MAX_PENDING_APPROVALS {
            return Err("approval_limit");
        }
        let id = loop {
            let candidate = random_token();
            if !map.contains_key(&candidate) {
                break candidate;
            }
        };
        let challenge = ApprovalChallenge {
            id: id.clone(),
            kind,
            source_id,
            session_id: session_id.to_string(),
            details,
            issued_at: Instant::now(),
        };
        map.insert(id, challenge.clone());
        Ok(challenge)
    }

    /// Consume a challenge before any daemon call. A failed or rejected
    /// command therefore cannot be retried with the same challenge.
    pub fn consume(&self, id: &str, active_seed: &str) -> Result<ApprovalChallenge, &'static str> {
        let mut map = self.lock();
        Self::cleanup(&mut map);
        let challenge = map.remove(id).ok_or("approval_not_found")?;
        if challenge.session_id != active_seed {
            return Err("approval_scope_violation");
        }
        Ok(challenge)
    }

    /// daemon `/approvals` 投影 → 不透明 challenge 视图列表。
    ///
    /// `pending_permission` 的 details 只放行展示字段（`tool_call_id`/
    /// `details_unavailable` 不出宿主）;`pending_interaction` 的 details 原样
    /// （daemon 侧已是投影形状）。
    pub fn issue_views(
        &self,
        session_id: &str,
        pending: &Value,
    ) -> Result<Vec<Value>, &'static str> {
        let mut views = Vec::new();

        if let Some(tool) = pending
            .get("pending_permission")
            .filter(|value| !value.is_null())
        {
            let source_id = tool
                .get("tool_call_id")
                .and_then(Value::as_str)
                .filter(|value| valid_daemon_id(value))
                .ok_or("invalid_pending_permission")?;
            let details = json!({
                "tool_name": tool.get("tool_name").cloned().unwrap_or(Value::Null),
                "action_summary": tool.get("action_summary").cloned().unwrap_or(Value::Null),
                "reason": tool.get("reason").cloned().unwrap_or(Value::Null),
                "paths": tool.get("paths").cloned().unwrap_or_else(|| json!([])),
                "category": tool.get("category").cloned().unwrap_or(Value::Null),
                "level": tool.get("level").cloned().unwrap_or(Value::Null),
                "risk": tool.get("risk").cloned().unwrap_or(Value::Null),
                "consequence": tool.get("consequence").cloned().unwrap_or(Value::Null),
            });
            let challenge = self
                .issue(
                    session_id,
                    ApprovalKind::ToolPermission,
                    source_id.to_string(),
                    details,
                )
                .map_err(|_| "approval_limit")?;
            views.push(challenge.view());
        }

        if let Some(interaction) = pending
            .get("pending_interaction")
            .filter(|value| !value.is_null())
        {
            let source_id = interaction
                .get("id")
                .and_then(Value::as_str)
                .filter(|value| valid_daemon_id(value))
                .ok_or("invalid_pending_interaction")?;
            let kind = match interaction.get("kind").and_then(Value::as_str) {
                Some("ask") => ApprovalKind::Ask,
                Some("plan") => ApprovalKind::Plan,
                _ => return Err("invalid_pending_interaction"),
            };
            let details = interaction
                .get("details")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let challenge = self
                .issue(session_id, kind, source_id.to_string(), details)
                .map_err(|_| "approval_limit")?;
            views.push(challenge.view());
        }

        Ok(views)
    }
}

/// 决策 + 载荷 → canonical Ringing 命令（决策集即 A-1 契约缺口下的固定集合）。
pub fn command_for(
    challenge: &ApprovalChallenge,
    decision: &str,
    payload: &Value,
) -> Result<RingingCommand, &'static str> {
    match challenge.kind {
        ApprovalKind::ToolPermission => {
            let (approved, trust_folder) = match decision {
                "approve" => (true, false),
                "reject" => (false, false),
                "trust" => (true, true),
                _ => return Err("invalid_decision"),
            };
            Ok(RingingCommand::Tool(ToolCommand::ToolPermissionRespond {
                tool_call_id: challenge.source_id.clone(),
                approved,
                trust_folder,
            }))
        }
        ApprovalKind::Ask => match decision {
            "submit" => {
                let answers = payload.get("answers").cloned().ok_or("missing_answers")?;
                let answers: Vec<AskAnswer> =
                    serde_json::from_value(answers).map_err(|_| "invalid_answers")?;
                Ok(RingingCommand::Control(
                    ControlCommand::InteractionAskRespond {
                        interaction_id: challenge.source_id.clone(),
                        answers,
                    },
                ))
            }
            "dismiss" => Ok(RingingCommand::Control(
                ControlCommand::InteractionAskDismiss {
                    interaction_id: challenge.source_id.clone(),
                },
            )),
            _ => Err("invalid_decision"),
        },
        ApprovalKind::Plan => {
            let approved = match decision {
                "approve" => true,
                "reject" => false,
                _ => return Err("invalid_decision"),
            };
            let message = payload
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let autonomous = payload
                .get("autonomous")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok(RingingCommand::Control(ControlCommand::PlanReviewRespond {
                interaction_id: challenge.source_id.clone(),
                approved,
                message,
                autonomous,
            }))
        }
    }
}

/// 审批提交不携带驱动权参数（interaction 应答显式不受 driver 门控）。
pub fn command_options() -> CommandOptions {
    CommandOptions::default()
}

/// daemon 投影 id 校验:非空、≤256、无控制字符。
fn valid_daemon_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

fn random_token() -> String {
    let bytes: [u8; 32] = rand::random();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn challenge(kind: ApprovalKind) -> ApprovalChallenge {
        ApprovalChallenge {
            id: "challenge".into(),
            kind,
            source_id: "canonical-id".into(),
            session_id: "0123abcd".into(),
            details: json!({}),
            issued_at: Instant::now(),
        }
    }

    fn payload(value: Value) -> Value {
        value
    }

    #[test]
    fn tool_decision_maps_to_canonical_command() {
        let command = command_for(
            &challenge(ApprovalKind::ToolPermission),
            "trust",
            &payload(json!({})),
        )
        .unwrap();
        assert!(matches!(
            command,
            RingingCommand::Tool(ToolCommand::ToolPermissionRespond {
                tool_call_id,
                approved: true,
                trust_folder: true,
            }) if tool_call_id == "canonical-id"
        ));
    }

    #[test]
    fn ask_payload_is_typed_and_dismiss_is_explicit() {
        let command = command_for(
            &challenge(ApprovalKind::Ask),
            "submit",
            &payload(json!({ "answers": [{ "question_id": "q1", "answer": "yes" }] })),
        )
        .unwrap();
        assert!(matches!(
            command,
            RingingCommand::Control(ControlCommand::InteractionAskRespond {
                interaction_id,
                answers,
            }) if interaction_id == "canonical-id"
                && answers == vec![AskAnswer {
                    question_id: "q1".into(),
                    answer: "yes".into(),
                }]
        ));

        assert!(matches!(
            command_for(&challenge(ApprovalKind::Ask), "dismiss", &payload(json!({}))).unwrap(),
            RingingCommand::Control(ControlCommand::InteractionAskDismiss { interaction_id })
                if interaction_id == "canonical-id"
        ));
    }

    #[test]
    fn plan_payload_is_limited_to_known_fields() {
        let command = command_for(
            &challenge(ApprovalKind::Plan),
            "approve",
            &payload(json!({ "message": "ok", "autonomous": true, "ignored": "x" })),
        )
        .unwrap();
        assert!(matches!(
            command,
            RingingCommand::Control(ControlCommand::PlanReviewRespond {
                interaction_id,
                approved: true,
                message: Some(message),
                autonomous: true,
            }) if interaction_id == "canonical-id" && message == "ok"
        ));
    }

    #[test]
    fn unknown_decisions_are_rejected() {
        for kind in [
            ApprovalKind::ToolPermission,
            ApprovalKind::Ask,
            ApprovalKind::Plan,
        ] {
            assert_eq!(
                command_for(&challenge(kind), "auto_approve", &payload(json!({}))).unwrap_err(),
                "invalid_decision"
            );
        }
    }

    #[test]
    fn permission_details_are_whitelisted() {
        let store = ChallengeStore::default();
        let pending = json!({
            "pending_permission": {
                "tool_call_id": "call_01ABC",
                "tool_name": "write_file",
                "action_summary": "写入",
                "reason": "需要写文件",
                "paths": ["/tmp/a"],
                "category": "fs",
                "level": 2,
                "risk": "medium",
                "consequence": "修改磁盘",
                "details_unavailable": false,
            }
        });
        let views = store.issue_views("seed1", &pending).unwrap();
        assert_eq!(views.len(), 1);
        let details = &views[0]["details"];
        assert!(details.get("tool_name").is_some());
        assert!(
            details.get("details_unavailable").is_none(),
            "宿主内部细节不得出壳"
        );
        assert!(
            views[0]["challenge_id"]
                .as_str()
                .is_some_and(|id| id.len() == 64)
        );
    }

    #[test]
    fn issue_is_deduped_per_source_id() {
        let store = ChallengeStore::default();
        let pending = json!({
            "pending_interaction": { "id": "int_01ABC", "kind": "ask", "details": {"q": 1} }
        });
        let first = store.issue_views("seed1", &pending).unwrap();
        let second = store.issue_views("seed1", &pending).unwrap();
        assert_eq!(first[0]["challenge_id"], second[0]["challenge_id"]);
    }

    #[test]
    fn consume_is_one_shot_and_scope_checked() {
        let store = ChallengeStore::default();
        let pending = json!({
            "pending_interaction": { "id": "int_01ABC", "kind": "plan", "details": {} }
        });
        let views = store.issue_views("seed1", &pending).unwrap();
        let id = views[0]["challenge_id"].as_str().unwrap().to_string();

        assert_eq!(
            store.consume(&id, "other-seed").unwrap_err(),
            "approval_scope_violation"
        );
        // scope 失败同样消费掉(一次性语义,防跨 seed 重放枚举)。
        assert_eq!(
            store.consume(&id, "other-seed").unwrap_err(),
            "approval_not_found"
        );

        let views = store.issue_views("seed1", &pending).unwrap();
        let id = views[0]["challenge_id"].as_str().unwrap().to_string();
        let consumed = store.consume(&id, "seed1").unwrap();
        assert_eq!(consumed.source_id, "int_01ABC");
        assert_eq!(
            store.consume(&id, "seed1").unwrap_err(),
            "approval_not_found"
        );
    }

    #[test]
    fn clear_drops_all_challenges() {
        let store = Arc::new(ChallengeStore::default());
        let pending = json!({
            "pending_interaction": { "id": "int_01ABC", "kind": "ask", "details": {} }
        });
        store.issue_views("seed1", &pending).unwrap();
        store.clear();
        assert!(store.issue_views("seed1", &json!(null)).is_ok());
    }

    #[test]
    fn invalid_daemon_ids_are_rejected() {
        let store = ChallengeStore::default();
        let bad = json!({ "pending_interaction": { "id": "", "kind": "ask", "details": {} } });
        assert_eq!(
            store.issue_views("seed1", &bad).unwrap_err(),
            "invalid_pending_interaction"
        );
        let bad_kind = json!({ "pending_interaction": { "id": "int_x", "kind": "permission", "details": {} } });
        assert_eq!(
            store.issue_views("seed1", &bad_kind).unwrap_err(),
            "invalid_pending_interaction"
        );
        let bad_tool = json!({ "pending_permission": { "tool_name": "x" } });
        assert_eq!(
            store.issue_views("seed1", &bad_tool).unwrap_err(),
            "invalid_pending_permission"
        );
    }

    #[test]
    fn pending_limit_is_enforced() {
        let store = ChallengeStore::default();
        for index in 0..MAX_PENDING_APPROVALS {
            let pending = json!({
                "pending_interaction": { "id": format!("int_{index:028}"), "kind": "ask", "details": {} }
            });
            assert!(
                store.issue_views("seed1", &pending).is_ok(),
                "第 {index} 个应可签发"
            );
        }
        let overflow = json!({
            "pending_interaction": { "id": "int_overflow", "kind": "ask", "details": {} }
        });
        assert_eq!(
            store.issue_views("seed1", &overflow).unwrap_err(),
            "approval_limit"
        );
    }
}
