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
//! 生命周期：仅根会话（子代理不进会话列表，命名无意义）且业务行尚无标题时
//! 声明；模型落笔成功即从请求面摘除，后续回合不再占用 token。客户端在
//! `session.create` 起过名或 `session.rename` 过的会话从头就不该看见它。

use qaqh_types::{ToolDef, ToolFunction};

/// 模型面工具名。客户端按这个名字识别「模型在自命名」。
pub const TOOL_NAME: &str = "session_title";
/// 标题字段。
pub const FIELD_TITLE: &str = "title";
/// 标题长度硬上限（字符数）。超限是模型的错，直接报回去重写。
pub const MAX_TITLE_CHARS: usize = 60;

fn tool_description() -> String {
    "给本会话命名（用于客户端会话列表展示）。在你明确任务之后、第一个回合内调用一次：\
     名词短语、不超过 20 个字、不加引号与末尾标点、写意图而非结论（如「daemon 启动崩溃\
     排查」而不是「已修复」）。标题写库后本工具自动下线，本回合的其余工作照常继续。"
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
                        "description": "会话标题：名词短语，≤20 字，无引号/末尾标点"
                    }
                },
                "required": [FIELD_TITLE]
            }),
        },
    }
}

/// 幂等把 `session_title` 追加进模型面工具清单。
///
/// 只在「根会话且业务行尚无标题」的闸口下调用；调用方负责闸口，这里只管去重。
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

/// 标题落定后把本工具从请求面摘除（回合内下一轮 provider 调用即生效）。
pub fn prune(tools: &mut Option<Vec<ToolDef>>) {
    if let Some(list) = tools {
        list.retain(|tool| tool.function.name != TOOL_NAME);
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
    format!("已为会话命名：{title}（title_source=auto，本工具已下线）")
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
    fn declare_is_idempotent_and_prune_removes() {
        let mut tools: Option<Vec<ToolDef>> = None;
        ensure_declared(&mut tools);
        ensure_declared(&mut tools);
        assert_eq!(tools.as_ref().unwrap().len(), 1);
        prune(&mut tools);
        assert!(tools.as_ref().unwrap().is_empty());
    }
}
