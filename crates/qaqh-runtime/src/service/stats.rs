//! service::stats — dashboard/activity/config/context_stats 查询函数。

use serde_json::{Value, json};

use std::io::BufRead;

use super::common::err;
use crate::ringing::service_methods::SessionToolActivity;

pub(crate) fn dashboard(session_id: &str) -> Result<Value, String> {
    let dir = qaqh_types::platform::sessions_dir().join(session_id);
    let tasks: Vec<Value> = qaqh_workspace::todo::todo_status_value(session_id)
        .ok()
        .and_then(|v| {
            v.get("items")?.as_array().map(|arr| {
                arr.iter()
                    .map(|item| {
                        json!({
                            "id": item["id"],
                            "subject": item["title"],
                            "description": item["description"],
                            "status": item["status"],
                            "evidence": item["evidence"],
                        })
                    })
                    .collect()
            })
        })
        .unwrap_or_default();
    let mut edits = std::fs::File::open(dir.join("code_stats.jsonl"))
        .ok()
        .into_iter()
        .flat_map(|file| std::io::BufReader::new(file).lines().map_while(Result::ok))
        .filter_map(|line| {
            serde_json::from_str::<Value>(&line)
                .ok()?
                .get("file")?
                .as_str()
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    edits.reverse();
    edits.dedup();
    edits.truncate(10);
    Ok(json!({"tasks":tasks,"recent_edits":edits}))
}

pub(crate) fn activity(
    sessions: &qaqh_session::SessionManager,
    session_id: &str,
) -> Result<Value, String> {
    use qaqh_session::session_fact_v2::{FactPayload, ToolTerminalStatus};

    let session_dir = sessions.session_path_dir(session_id);
    if !qaqh_session::canonical::has_committed_facts(&session_dir) {
        return Err("session not found".to_string());
    }
    let identity = qaqh_session::canonical::open_session_identity(&session_dir)
        .map_err(|error| error.to_string())?;
    let commit = qaqh_session::session_commit::SessionCommit::open(
        &session_dir,
        identity.session_id,
        identity.log_id,
        crate::session_agent::assembly::default_context_budget(),
    )
    .map_err(|error| error.to_string())?;
    let facts = commit.committed_facts().map_err(|error| error.to_string())?;

    // Tool calls are declared with their name and an args blob; completions only
    // carry the call id, so the two passes have to meet here.
    const MAX_ARGS_BYTES: usize = 64 * 1024;
    let mut declared: std::collections::HashMap<String, (String, String)> = Default::default();
    for fact in &facts {
        if let FactPayload::ToolCallDeclared(call) = &fact.payload {
            let args = commit
                .get_blob(&call.args_ref, MAX_ARGS_BYTES)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .unwrap_or_default();
            declared.insert(call.call_id.as_str().to_string(), (call.tool_name.clone(), args));
        }
    }

    let mut result = Vec::new();
    for fact in &facts {
        let FactPayload::ToolFinished(finished) = &fact.payload else {
            continue;
        };
        let (tool_name, args) = declared
            .get(finished.call_id.as_str())
            .cloned()
            .unwrap_or_default();
        let status = match finished.terminal_status {
            ToolTerminalStatus::Succeeded => qaqh_types::ToolStatus::Ok,
            ToolTerminalStatus::Partial => qaqh_types::ToolStatus::Partial,
            ToolTerminalStatus::Backgrounded => qaqh_types::ToolStatus::Backgrounded,
            ToolTerminalStatus::Cancelled => qaqh_types::ToolStatus::Cancelled,
            _ => qaqh_types::ToolStatus::Error,
        };
        let summary = finished
            .error
            .as_ref()
            .map(|error| error.message.clone())
            .unwrap_or_default();
        result.push(SessionToolActivity {
            tool_name,
            summary,
            status,
            finished_at_ms: finished.finished_at_ms,
            args,
        });
    }
    result.reverse();
    result.truncate(50);
    serde_json::to_value(result).map_err(err)
}

pub(crate) fn load_config() -> Result<Value, String> {
    let cfg = qaqh_config::Config::load().map_err(err)?;
    // P1-C2：读模型统一走 ConfigDto（camelCase wire + providers 目录内聚到
    // qaqh-config::dto），service 层不再手拼 json。
    serde_json::to_value(qaqh_config::dto::to_dto(&cfg)).map_err(err)
}
