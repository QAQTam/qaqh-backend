//! 服务端一次性审批 challenge（移动端 M0；spec-daemon-auth-devices §9）。
//!
//! 从 `webui/src-tauri/src/challenge.rs` 移植：TTL/上限/去重/一次性/scope 校验语义
//! 原样保留。**canonical id 不出 daemon**——device 身份只拿不透明 `challenge_id`
//! 与有界展示 details；应答时 daemon 侧 `consume` → `command_for` 映射回 canonical
//! 命令，再进 `execute_command`。
//!
//! admin 身份不走本模块（桌面宿主已在壳侧做映射，维持现状透传 canonical id）。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use qaqh_domain::{AskAnswer, ControlCommand, ToolCommand};
use qaqh_ringing::RingingCommand;
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
    /// canonical daemon id（`call_<ULID>` / `int_<ULID>`）——绝不出 daemon。
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

/// daemon 侧 challenge 存储：签发（去重/上限/惰性清理）与一次性消费。
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

    fn cleanup(map: &mut HashMap<String, ApprovalChallenge>) {
        map.retain(|_, challenge| !challenge.is_expired());
    }

    /// 按 (kind, session_id, source_id) 去重签发；达到上限报 `approval_limit`。
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

    /// 消费一个 challenge（一次性）。应答命令失败或被拒都不得复用同一 challenge。
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
    /// `pending_permission` 的 details 只放行展示字段（`details_unavailable` 不出
    /// daemon）；`pending_interaction` 的 details 原样（daemon 侧已是投影形状）。
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

/// 决策 + 载荷 → canonical Ringing 命令（决策集是固定集合）。
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

/// daemon 投影 id 校验：非空、≤256、无控制字符。
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

    #[test]
    fn tool_decision_maps_to_canonical_command() {
        let command = command_for(
            &challenge(ApprovalKind::ToolPermission),
            "trust",
            &json!({}),
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
            &json!({ "answers": [{ "question_id": "q1", "answer": "yes" }] }),
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
            command_for(&challenge(ApprovalKind::Ask), "dismiss", &json!({})).unwrap(),
            RingingCommand::Control(ControlCommand::InteractionAskDismiss { interaction_id })
                if interaction_id == "canonical-id"
        ));
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
        // scope 失败同样消费掉（一次性语义，防跨 seed 重放枚举）。
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
    fn permission_details_are_whitelisted() {
        let store = ChallengeStore::default();
        let pending = json!({
            "pending_permission": {
                "tool_call_id": "call_01ABC",
                "tool_name": "write_file",
                "details_unavailable": false,
            }
        });
        let views = store.issue_views("seed1", &pending).unwrap();
        let details = &views[0]["details"];
        assert!(details.get("tool_name").is_some());
        assert!(details.get("details_unavailable").is_none());
        assert!(
            views[0]["challenge_id"]
                .as_str()
                .is_some_and(|id| id.len() == 64)
        );
    }

    #[test]
    fn pending_limit_is_enforced() {
        let store = Arc::new(ChallengeStore::default());
        for index in 0..MAX_PENDING_APPROVALS {
            let pending = json!({
                "pending_interaction": { "id": format!("int_{index:028}"), "kind": "ask", "details": {} }
            });
            assert!(store.issue_views("seed1", &pending).is_ok());
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
