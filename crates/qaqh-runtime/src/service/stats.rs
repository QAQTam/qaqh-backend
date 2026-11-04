//! service::stats — dashboard/activity/config/context_stats 查询函数。

use serde_json::Value;

use std::io::BufRead;

use super::common::err;
use crate::ringing::service_methods::SessionToolActivity;

/// `tool_calls` 终态行的查询投影。
struct ToolActivityRow {
    tool_name: String,
    args_json: Option<String>,
    status: String,
    error_json: Option<String>,
    finished_at_ms: Option<i64>,
}

/// `session.dashboard`:任务卡来自 todo 状态投影,recent_edits 来自
/// code_stats.jsonl 的去重尾部(最近优先)。B08 权威 DTO,handler 直出
/// typed 结构,序列化即 wire 形状。
pub(crate) fn dashboard(
    session_id: &str,
) -> Result<crate::ringing::service_methods::SessionDashboard, String> {
    use crate::ringing::service_methods::{SessionDashboard, SessionDashboardTask};
    let dir = qaqh_types::platform::sessions_dir().join(session_id);
    let tasks: Vec<SessionDashboardTask> = qaqh_workspace::todo::todo_status_value(session_id)
        .ok()
        .and_then(|v| {
            v.get("items")?.as_array().map(|arr| {
                arr.iter()
                    .map(|item| SessionDashboardTask {
                        id: item["id"].as_str().unwrap_or_default().to_string(),
                        subject: item["title"].as_str().unwrap_or_default().to_string(),
                        description: item["description"].as_str().unwrap_or_default().to_string(),
                        status: item["status"].as_str().unwrap_or_default().to_string(),
                        evidence: item["evidence"].as_str().map(str::to_string),
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
    Ok(SessionDashboard {
        tasks,
        recent_edits: edits,
    })
}

pub(crate) fn activity(
    owners: &qaqh_session::owner::OwnerDirectory,
    session_id: &str,
) -> Result<Value, String> {
    // 终态工具调用直查业务表:最近 50 条,新→旧。
    let owned = session_id.to_string();
    let rows = owners
        .db()
        .read_blocking(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT tool_name, args_json, status, error_json, finished_at_ms
                 FROM tool_calls
                 WHERE session_id = ?1 AND finished_at_ms IS NOT NULL
                 ORDER BY finished_at_ms DESC LIMIT 50",
            )?;
            let rows = stmt.query_map([&owned], |row| {
                Ok(ToolActivityRow {
                    tool_name: row.get(0)?,
                    args_json: row.get(1)?,
                    status: row.get(2)?,
                    error_json: row.get(3)?,
                    finished_at_ms: row.get(4)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .map_err(|error| error.to_string())?;
    let result: Vec<SessionToolActivity> = rows
        .into_iter()
        .map(|row| {
            let status = match row.status.as_str() {
                "finished" => qaqh_types::ToolStatus::Ok,
                "failed" => qaqh_types::ToolStatus::Error,
                "cancelled" => qaqh_types::ToolStatus::Cancelled,
                _ => qaqh_types::ToolStatus::Backgrounded,
            };
            let summary = row
                .error_json
                .as_deref()
                .and_then(|json| serde_json::from_str::<Value>(json).ok())
                .and_then(|error| error.get("message")?.as_str().map(str::to_string))
                .unwrap_or_default();
            SessionToolActivity {
                tool_name: row.tool_name,
                summary,
                status,
                finished_at_ms: row.finished_at_ms.unwrap_or_default(),
                args: row.args_json.unwrap_or_default(),
            }
        })
        .collect();
    serde_json::to_value(result).map_err(err)
}

pub(crate) fn load_config() -> Result<Value, String> {
    let cfg = qaqh_config::Config::load().map_err(err)?;
    // P1-C2：读模型统一走 ConfigDto（camelCase wire + providers 目录内聚到
    // qaqh-config::dto），service 层不再手拼 json。
    serde_json::to_value(qaqh_config::dto::to_dto(&cfg)).map_err(err)
}
