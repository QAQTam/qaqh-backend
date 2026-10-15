//! 标准工具调用归一器。
//!
//! 2026-10-05:DSML/XML 文本态工具调用解析已整体移除——DeepSeek v4.1 起
//! 原生输出结构化 `tool_calls`,不再吐脏字符(文本内嵌标记)。本模块只剩
//! 一件事:把上游 tool_calls JSON(flat / nested 两种形态)归一为
//! [`ToolCall`];非标准格式不再有 fallback。

use qaqh_types::{FunctionCall, ToolCall};

/// `arguments` 归一：**只接受字符串或 object 两种形态，其余保持原样不掺假**。
///
/// - string：标准 OpenAI Chat 形态，原样透传；
/// - object：部分兼容网关直接内嵌 JSON 对象——旧代码 `as_str()` 失败后
///   一律折成 `"{}"`，参数被静默吞掉，typed 工具随即报
///   「missing field `x`」假错。这里重新序列化保真；
/// - 缺失：模型确实没发参数（全可选参数工具合法），收敛为 `"{}"`；
/// - 其它（null / 数组 / 标量）：**原样透传**为字符串——Null 是 gate 收口层
///   「上游参数不可还原」的真值标记，runtime 执行界面据此如实报错；
///   折成 `"{}"` 反而制造 missing-field 假错。
fn normalize_arguments(v: Option<&serde_json::Value>) -> String {
    match v {
        None => "{}".to_string(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

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
                (n, normalize_arguments(func.get("arguments")))
            } else {
                let n = tc.get("name").and_then(|v| v.as_str()).unwrap_or("");
                (n, normalize_arguments(tc.get("arguments")))
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

    /// BUG-2026-10-15 根因锁：object 形态 arguments（部分兼容网关）必须保真
    /// 重序列化——旧代码 `as_str()` 失败后折成 `"{}"`，参数被静默吞掉，
    /// typed 工具报「missing field `x`」假错。
    #[test]
    fn object_form_arguments_are_preserved_verbatim() {
        let raw = serde_json::json!([
            {"id": "c1", "name": "apply_patch",
             "arguments": {"patch": "*** Begin Patch\n*** Update File: a.rs"}}
        ]);
        let parsed = parse_tool_calls(&raw);
        assert_eq!(parsed.len(), 1);
        let v: serde_json::Value =
            serde_json::from_str(&parsed[0].function.arguments).expect("仍是合法 JSON");
        assert_eq!(v["patch"], "*** Begin Patch\n*** Update File: a.rs");
    }

    /// null / 数组形态原样透传为字符串——Null 是收口层「参数不可还原」的
    /// 真值标记，折成 `"{}"` 反而制造 missing-field 假错。
    #[test]
    fn non_object_non_string_arguments_pass_through_without_fabrication() {
        let raw = serde_json::json!([
            {"id": "c1", "name": "apply_patch", "arguments": null}
        ]);
        let parsed = parse_tool_calls(&raw);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].function.arguments, "null");
    }
}
