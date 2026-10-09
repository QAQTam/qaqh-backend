//! service::plan — plan 读写自由函数。

use serde::{Deserialize, Serialize};

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
    use serde_json::json;

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
