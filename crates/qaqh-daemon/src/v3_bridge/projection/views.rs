use qaqh_core_vocab::{ExecSessionId, ToolCallId};
use qaqh_ringing_contract::state as old_state;
use qaqh_ringing_runtime::RingingHub;
use qaqh_ringing_v3 as v3;
use qaqh_runtime::QaqhService;

use crate::v3_server::state::ServerState;

use super::content;

pub(crate) async fn interaction_view(
    _hub: &RingingHub,
    _state: &ServerState,
    _session_id: &str,
    interaction: &old_state::InteractionView,
) -> Result<v3::InteractionView, String> {
    let text = match &interaction.request {
        qaqh_ringing_contract::fact::ContentValue::Inline { text } => text.clone(),
        _ => return Err("interaction request must be inline JSON".into()),
    };
    let (kind, title, body_text, questions) = match interaction.kind {
        qaqh_ringing_contract::fact::InteractionKind::Permission => {
            let body: serde_json::Value =
                serde_json::from_str(&text).map_err(|error| format!("interaction body: {error}"))?;
            let title = body
                .get("title")
                .and_then(|value| value.as_str())
                .unwrap_or("Interaction")
                .to_string();
            (v3::InteractionKind::Permission, title, body.to_string(), Vec::new())
        }
        qaqh_ringing_contract::fact::InteractionKind::Ask => {
            let ask: qaqh_workspace::ask_user::NormalizedAsk =
                serde_json::from_str(&text).map_err(|error| format!("ask interaction: {error}"))?;
            let questions = ask
                .questions
                .into_iter()
                .map(|question| v3::AskQuestionView {
                    id: question.id,
                    prompt: question.question,
                    options: question.options,
                    allow_free_text: question.allow_custom,
                })
                .collect();
            (
                v3::InteractionKind::Ask,
                "Question".to_string(),
                text,
                questions,
            )
        }
    };
    Ok(v3::InteractionView {
        interaction_id: qaqh_core_vocab::InteractionId::new(interaction.interaction_id.as_str())
            .map_err(|error| error.to_string())?,
        kind,
        tool_call_id: interaction
            .call_id
            .as_ref()
            .map(|id| ToolCallId::new(id.as_str()).map_err(|error| error.to_string()))
            .transpose()?,
        title,
        body: body_text,
        options: interaction
            .choices
            .iter()
            .map(|choice| choice.label.clone())
            .collect(),
        questions,
        expires_at_ms: interaction.expires_at_ms.map(|value| value.max(0) as u64),
    })
}

pub(crate) async fn exec_view(
    hub: &RingingHub,
    state: &ServerState,
    exec: &old_state::ExecSessionView,
) -> Result<v3::ExecSessionSnapshot, String> {
    let output = match &exec.output {
        Some(output) => Some(
            content::content_ref(hub, state, exec.owner_session_id.as_str(), output.clone())
                .await?,
        ),
        None => None,
    };
    Ok(v3::ExecSessionSnapshot {
        exec_session_id: ExecSessionId::new(exec.id.as_str()).map_err(|error| error.to_string())?,
        command: exec.command.clone(),
        tty: exec.tty,
        finished: matches!(&exec.state, old_state::ExecSessionState::Finished { .. }),
        exit_code: match &exec.state {
            old_state::ExecSessionState::Finished {
                outcome: old_state::ExecSessionOutcome::Exited { exit_code },
            } => Some(*exit_code),
            _ => None,
        },
        next_offset: exec.next_offset,
        output,
    })
}

pub(super) async fn tool_name(
    service: &QaqhService,
    session_id: &str,
    call_id: &str,
) -> Result<String, String> {
    let (raw, call) = (session_id.to_string(), call_id.to_string());
    let db = service.owners().db().clone();
    tokio::task::spawn_blocking(move || {
        let rows = db
            .read_blocking(|conn| qaqh_session::db::store::recent_tool_calls(conn, &raw, 500))
            .map_err(|error| error.to_string())?;
        rows.into_iter()
            .find(|row| row.call_id == call)
            .map(|row| row.tool_name)
            .ok_or_else(|| format!("tool call missing: {call}"))
    })
    .await
    .map_err(|error| error.to_string())?
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use qaqh_ringing_contract::fact::{ContentValue, InteractionId, InteractionKind, TurnId};
    use qaqh_workspace::ask_user::{NormalizedAsk, NormalizedAskMode, NormalizedAskQuestion};

    use super::*;
    use crate::v3_server::state::ServerConfig;

    #[tokio::test]
    async fn ask_interaction_projects_normalized_question_and_allow_custom() {
        let temp = tempfile::tempdir().unwrap();
        let db = std::sync::Arc::new(qaqh_session::db::Db::open(temp.path().join("q.db")).unwrap());
        let hub = RingingHub::with_database("e", db);
        let state = ServerState::new(ServerConfig::default(), "user");
        let ask = NormalizedAsk {
            mode: NormalizedAskMode::Single,
            questions: vec![NormalizedAskQuestion {
                id: "q1".into(),
                question: "Pick one".into(),
                options: vec!["A".into(), "B".into()],
                allow_custom: false,
            }],
        };
        let raw = old_state::InteractionView {
            interaction_id: InteractionId::new("ia-1"),
            call_id: None,
            turn_id: Some(TurnId::new("t1")),
            kind: InteractionKind::Ask,
            request: ContentValue::Inline {
                text: serde_json::to_string(&ask).unwrap(),
            },
            choices: Vec::new(),
            expires_at_ms: None,
            resolution: None,
        };
        let projected = interaction_view(&hub, &state, "s1", &raw).await.unwrap();
        assert_eq!(projected.kind, v3::InteractionKind::Ask);
        assert_eq!(projected.questions.len(), 1);
        assert_eq!(projected.questions[0].prompt, "Pick one");
        assert!(!projected.questions[0].allow_free_text);
    }
}
