//! `session_title`：模型给自己的会话命名，控制面工具。
//!
//! 这不是旧 `engine_title` 旁路的复活：没有任何额外 LLM 调用，后端不代笔。
//! 当前回合的主模型上下文最全，它自己起的标题比一次性旁路更准；调用即落库，
//! 会话不终止、回合继续推进（与 `handoff` 的终局语义相反）。
//!
//! 契约面：零新增。声明走 `tool_declared`、结果走 `tool_finished`（都是
//! RingingV2 已有字段）；业务列写入经 `owner::update_metadata` 唯一写口，事件
//! 走已有的 `session_meta_changed{title}` 帧；`title_source="auto"` 是业务列，
//! 不进 v3.0 冻结契约。
//!
//! 生命周期：仅根会话（子代理不进会话列表，命名无意义）且业务行无标题或标题
//! 来源为 `auto` 时声明；落笔成功后工具留在请求面，任务需求变化时模型可重新
//! 生成。客户端在 `session.create` 起过名或 `session.rename` 过的会话从头就不该
//! 看见它；会话中途被用户改名时，写口（`run_session_title`）再挡一次。

use qaqh_types::{ToolDef, ToolFunction};

/// 模型面工具名。客户端按这个名字识别「模型在自命名」。
pub const TOOL_NAME: &str = "session_title";
/// 标题字段。
pub const FIELD_TITLE: &str = "title";
/// 标题长度硬上限（字符数）。超限是模型的错，直接报回去重写。
pub const MAX_TITLE_CHARS: usize = 60;

fn tool_description() -> String {
    "Name this session for the client's session list. At the start of the session, \
     generate a fitting title from the user's request; you may regenerate it when the \
     task requirements change. A noun phrase of at most 20 characters, no quotes or \
     trailing punctuation, describing intent rather than outcome (e.g. \"Daemon startup \
     crash investigation\", not \"Fixed\"). The title is persisted immediately and the \
     turn continues; sessions already named by the user never expose this tool."
        .to_string()
}

/// 交给模型的声明。
#[must_use]
pub fn tool_def() -> ToolDef {
    ToolDef {
        call_type: "function".to_string(),
        function: ToolFunction {
            name: TOOL_NAME.to_string(),
            description: tool_description(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    FIELD_TITLE: {
                        "type": "string",
                        "description": "Session title: noun phrase, at most 20 characters, no quotes or trailing punctuation"
                    }
                },
                "required": [FIELD_TITLE]
            }),
        },
    }
}

/// 声明闸口（根会话判定由调用方负责）：无标题、或标题出自模型（`auto`）时
/// 声明——后者允许任务需求变化时重新生成；用户命名（`user`）的会话永不声明。
pub fn should_declare(title: Option<&str>, source: Option<&str>) -> bool {
    title.is_none() || source == Some("auto")
}

/// 幂等把 `session_title` 追加进模型面工具清单。
///
/// 只在 `should_declare` 闸口下调用；调用方负责闸口，这里只管去重。
pub fn ensure_declared(tools: &mut Option<Vec<ToolDef>>) {
    match tools {
        Some(list) => {
            if !list.iter().any(|tool| tool.function.name == TOOL_NAME) {
                list.push(tool_def());
            }
        }
        None => *tools = Some(vec![tool_def()]),
    }
}

/// 解析模型提交的标题。
///
/// # Errors
/// 字段缺失、类型不对、trim 后为空或超长时报错——让模型重写，后端不替它编。
pub fn parse_title(input: &serde_json::Value) -> Result<String, String> {
    let raw = match input.get(FIELD_TITLE) {
        None => return Err(format!("session_title 缺少 `{FIELD_TITLE}` 字段")),
        Some(serde_json::Value::String(text)) => text,
        Some(_) => return Err(format!("session_title 的 `{FIELD_TITLE}` 必须是字符串")),
    };
    let title = raw.trim();
    if title.is_empty() {
        return Err("session_title 的标题不能为空".to_string());
    }
    let count = title.chars().count();
    if count > MAX_TITLE_CHARS {
        return Err(format!(
            "session_title 的标题过长（{count} 字符，上限 {MAX_TITLE_CHARS}）"
        ));
    }
    Ok(title.to_string())
}

/// 落库成功的回执文案。
#[must_use]
pub fn success_output(title: &str) -> String {
    format!("已为会话命名：{title}（title_source=auto）")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_trims_and_rejects_junk() {
        assert_eq!(
            parse_title(&serde_json::json!({"title": " daemon 崩溃排查 "})).unwrap(),
            "daemon 崩溃排查"
        );
        assert!(parse_title(&serde_json::json!({})).is_err());
        assert!(parse_title(&serde_json::json!({"title": "   "})).is_err());
        assert!(parse_title(&serde_json::json!({"title": 42})).is_err());
        let long = "长".repeat(MAX_TITLE_CHARS + 1);
        assert!(parse_title(&serde_json::json!({"title": long})).is_err());
    }

    #[test]
    fn declare_is_idempotent() {
        let mut tools: Option<Vec<ToolDef>> = None;
        ensure_declared(&mut tools);
        ensure_declared(&mut tools);
        assert_eq!(tools.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn declare_gate_allows_revision_of_auto_titles_only() {
        assert!(should_declare(None, None));
        assert!(should_declare(None, Some("user")));
        assert!(should_declare(Some("模型起的题"), Some("auto")));
        assert!(!should_declare(Some("用户起的题"), Some("user")));
        assert!(!should_declare(Some("来历不明的题"), None));
    }
}
