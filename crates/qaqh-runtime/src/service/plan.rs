//! service::plan — token 统计 + plan 读写自由函数。

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use std::io::BufRead;

use super::common::err;

use super::fs_git::qaqh_dir;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum PlanItemStatus {
    #[serde(rename = "")]
    Pending,
    #[serde(rename = "✓")]
    Approved,
    #[serde(rename = "-")]
    Rejected,
    #[serde(rename = "?")]
    Question,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlanItemView {
    pub id: String,
    pub title: String,
    pub status: PlanItemStatus,
    pub comment: String,
    pub actions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct PlanListOutput(pub Vec<PlanItemView>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlanActionOutput {
    pub item_id: String,
    pub action: String,
    pub item: Option<PlanItemView>,
}

/// `stats.token_usage.days` 是 IPC 直传参数，同时决定条目数与循环数：
/// 未封顶时 `days = u32::MAX` 会产出约 43 亿条目（daemon 线程 OOM + 挂死）。
/// 366 天覆盖一年窗口，超出即钳制（与 config 侧默认窗口同量级）。
pub(crate) const MAX_TOKEN_STATS_DAYS: u32 = 366;

pub(crate) fn token_stats(days: u32) -> Result<Value, String> {
    use std::collections::BTreeMap;
    let days = days.clamp(1, MAX_TOKEN_STATS_DAYS);
    let cutoff = days_before_today(days);
    let mut daily: BTreeMap<String, Value> = BTreeMap::new();
    if let Ok(file) =
        std::fs::File::open(qaqh_types::platform::data_dir().join("token_stats.jsonl"))
    {
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(entry) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let date = entry["date"].as_str().unwrap_or_default().to_string();
            if date < cutoff {
                continue;
            }
            let day=daily.entry(date).or_insert_with(||json!({"prompt_tokens":0,"completion_tokens":0,"cache_hit":0,"cache_miss":0,"calls":0}));
            for key in [
                "prompt_tokens",
                "completion_tokens",
                "cache_hit",
                "cache_miss",
            ] {
                day[key] = json!(day[key].as_u64().unwrap_or(0) + entry[key].as_u64().unwrap_or(0));
            }
            day["calls"] = json!(day["calls"].as_u64().unwrap_or(0) + 1);
        }
    }
    let mut values = Vec::new();
    let mut prompt = 0;
    let mut completion = 0;
    let mut hit = 0;
    let mut miss = 0;
    let mut calls = 0;
    for offset in (0..days).rev() {
        let date = days_before_today(offset);
        let entry=daily.get(&date).cloned().unwrap_or_else(||json!({"prompt_tokens":0,"completion_tokens":0,"cache_hit":0,"cache_miss":0,"calls":0}));
        prompt += entry["prompt_tokens"].as_u64().unwrap_or(0);
        completion += entry["completion_tokens"].as_u64().unwrap_or(0);
        hit += entry["cache_hit"].as_u64().unwrap_or(0);
        miss += entry["cache_miss"].as_u64().unwrap_or(0);
        calls += entry["calls"].as_u64().unwrap_or(0);
        values.push(json!({"date":date,"prompt_tokens":entry["prompt_tokens"],"completion_tokens":entry["completion_tokens"],"cache_hit":entry["cache_hit"],"cache_miss":entry["cache_miss"],"calls":entry["calls"]}));
    }
    let pct = if hit + miss > 0 {
        (hit as f64 / (hit + miss) as f64 * 1000.0).round() / 10.0
    } else {
        0.0
    };
    Ok(
        json!({"daily":values,"totals":{"prompt_tokens":prompt,"completion_tokens":completion,"calls":calls,"cache_hit_pct":pct}}),
    )
}
pub(crate) fn days_before_today(days: u32) -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_sub(days as u64 * 86400);
    let (y, m, d) = qaqh_types::platform::civil_from_days((seconds / 86400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

pub(crate) fn read_plan(
    sessions: &qaqh_session::SessionManager,
    session_id: &str,
) -> Result<PlanListOutput, String> {
    let content = match std::fs::read_to_string(qaqh_dir(sessions, session_id).join("PLAN.md")) {
        Ok(value) => value,
        Err(_) => return Ok(PlanListOutput(Vec::new())),
    };
    Ok(parse_plan(&content))
}

pub(crate) fn plan_action(
    sessions: &qaqh_session::SessionManager,
    session_id: &str,
    item_id: &str,
    action: &str,
    comment: &str,
) -> Result<PlanActionOutput, String> {
    let path = qaqh_dir(sessions, session_id).join("PLAN.md");
    let content = std::fs::read_to_string(&path).map_err(err)?;
    let mut found = false;
    let output = content
        .lines()
        .filter_map(|line| {
            if !found && line.trim().starts_with("- [") && line.contains(&format!(" {item_id}: ")) {
                found = true;
                if action == "delete" {
                    return None;
                }
                let end = line.find(']')?;
                // ']' 为单字节 ASCII，end+1 必为 char boundary。
                let rest = line.split_at(end + 1).1;
                let base = format!("- [ ]{rest}");
                return Some(match action {
                    "approve" => base.replacen("- [ ]", "- [✓]", 1),
                    "reject" => {
                        let value = base.replacen("- [ ]", "- [-]", 1);
                        if comment.is_empty() {
                            value
                        } else {
                            format!("{value} | {comment}")
                        }
                    }
                    "ask" => base.replacen("- [ ]", "- [?]", 1),
                    _ => line.to_string(),
                });
            }
            Some(line.to_string())
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !found {
        return Err(format!("plan item {item_id} not found"));
    }
    // `lines()` 丢弃了末尾换行；回写时补回，避免每次裁决都静默改写文件尾
    //（计划文件由模型/前端共同读写，尾部形态必须稳定）。
    let output = if content.ends_with('\n') && !output.ends_with('\n') {
        format!("{output}\n")
    } else {
        output
    };
    let item = parse_plan(&output)
        .0
        .into_iter()
        .find(|item| item.id == item_id);
    std::fs::write(path, output).map_err(err)?;
    Ok(PlanActionOutput {
        item_id: item_id.to_string(),
        action: action.to_string(),
        item,
    })
}

fn parse_plan(content: &str) -> PlanListOutput {
    PlanListOutput(content.lines().filter_map(parse_plan_line).collect())
}

fn parse_plan_line(line: &str) -> Option<PlanItemView> {
    let line = line.trim();
    let rest = line.strip_prefix("- [")?;
    let end = rest.find(']')?;
    let status = match rest.get(..end)? {
        " " => PlanItemStatus::Pending,
        "✓" => PlanItemStatus::Approved,
        "-" => PlanItemStatus::Rejected,
        "?" => PlanItemStatus::Question,
        _ => return None,
    };
    let rest = rest.get(end + 1..)?.trim();
    let (id, title) = rest.split_once(": ")?;
    let (title, comment) = title
        .split_once(" | ")
        .map_or((title, String::new()), |(title, comment)| {
            (title, comment.to_string())
        });
    Some(PlanItemView {
        id: id.to_string(),
        title: title.to_string(),
        status,
        comment,
        actions: Vec::new(),
    })
}

#[cfg(test)]
mod typed_plan_tests {
    use super::*;

    #[test]
    fn plan_projection_keeps_wire_shape_and_separates_comments() {
        let output = parse_plan(
            "- [ ] item-1: first step\n\
             - [✓] item-2: second step | approved in review\n\
             - [-] item-3: rejected step | not needed\n",
        );
        let value = serde_json::to_value(output).expect("typed plan output");
        assert_eq!(
            value,
            json!([
                {
                    "id": "item-1",
                    "title": "first step",
                    "status": "",
                    "comment": "",
                    "actions": []
                },
                {
                    "id": "item-2",
                    "title": "second step",
                    "status": "✓",
                    "comment": "approved in review",
                    "actions": []
                },
                {
                    "id": "item-3",
                    "title": "rejected step",
                    "status": "-",
                    "comment": "not needed",
                    "actions": []
                }
            ])
        );
    }
}
