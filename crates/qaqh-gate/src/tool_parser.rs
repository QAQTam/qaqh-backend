//! 标准工具调用归一器。
//!
//! 2026-10-05:DSML/XML 文本态工具调用解析已整体移除——DeepSeek v4.1 起
//! 原生输出结构化 `tool_calls`,不再吐脏字符(文本内嵌标记)。本模块只剩
//! 一件事:把上游 tool_calls JSON(flat / nested 两种形态)归一为
//! [`ToolCall`];非标准格式不再有 fallback。

use qaqh_types::{FunctionCall, ToolCall};

/// Convert tool_calls JSON (flat or nested) to ToolCall vec.
///
/// - nested: `[{"id","function":{"name","arguments"}}]`(OpenAI Chat 形态)
/// - flat:   `[{"id","name","arguments"}]`(部分兼容网关的扁平形态)
///
/// `id` 或 `name` 缺失/为空的条目整条跳过——畸形条目不得污染整批。
pub fn parse_tool_calls(tcs: &serde_json::Value) -> Vec<ToolCall> {
    let arr = match tcs.as_array() {
        Some(a) => a,
        None => return vec![],
    };
    arr.iter()
        .filter_map(|tc| {
            let id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("");
            let (name, arguments) = if let Some(func) = tc.get("function") {
                let n = func.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let a = func
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}");
                (n, a)
            } else {
                let n = tc.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let a = tc.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}");
                (n, a)
            };
            if id.is_empty() || name.is_empty() {
                return None;
            }
            Some(ToolCall {
                id: id.to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: name.to_string(),
                    arguments: arguments.to_string(),
                },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_openai_shape_is_normalized() {
        let raw = serde_json::json!([
            {"id": "call_1", "type": "function",
             "function": {"name": "read", "arguments": "{\"path\":\"a.rs\"}"}},
        ]);
        let parsed = parse_tool_calls(&raw);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, "call_1");
        assert_eq!(parsed[0].function.name, "read");
        assert_eq!(parsed[0].function.arguments, "{\"path\":\"a.rs\"}");
        assert_eq!(parsed[0].call_type, "function");
    }

    #[test]
    fn flat_shape_is_normalized() {
        let raw = serde_json::json!([
            {"id": "call_2", "name": "exec", "arguments": "{\"command\":\"ls\"}"},
        ]);
        let parsed = parse_tool_calls(&raw);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].function.name, "exec");
    }

    #[test]
    fn malformed_entries_are_skipped_without_poisoning_the_batch() {
        let raw = serde_json::json!([
            {"id": "", "name": "no_id"},
            {"name": "no_id_field"},
            {"id": "ok", "function": {"name": "", "arguments": "{}"}},
            {"id": "ok2", "function": {"name": "read", "arguments": "{}"}},
        ]);
        let parsed = parse_tool_calls(&raw);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, "ok2");
    }

    #[test]
    fn non_array_input_yields_empty() {
        assert!(parse_tool_calls(&serde_json::json!(null)).is_empty());
        assert!(parse_tool_calls(&serde_json::json!({})).is_empty());
    }
}
