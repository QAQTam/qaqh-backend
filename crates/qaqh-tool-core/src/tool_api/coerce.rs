//! 模型侧参数的 schema 归一层（wire 容错，SDK v2 边界）。
//!
//! 工具参数由模型生成，标量与集合常被写成 JSON 字符串字面量——
//! `{"max_results":"3"}`、`{"dry":"true"}`、`{"glob":"[\"*.rs\"]"}`。typed 反
//! 序列化对这种输入是硬失败（`invalid type: string "3", expected u64`），
//! 而 typed 迁移前用的 `as_u64()/as_array()` 是静默吞掉的。本模块把两者
//! 的差补回来：在 `from_value` 之前按 input schema 做一次**只改值类型、
//! 不增删键**的归一。
//!
//! 设计约束：
//! - 只在「schema 声明的类型 ≠ 实收类型，且实收是字符串」时改写；无法
//!   无损归一的（`"abc"` 对 integer、非 JSON 的裸串对 array）保持原样，
//!   让反序列化错误带着 [`expected_shape_hint`] 上抛——静默猜测比失败更糟。
//! - schema 不是我们生成的（MCP/LSP 远端工具）也可能带 `["integer","null"]`
//!   联合形态，故 [`wanted_type`] 两种形态都吃。
//! - [`TypedToolAdapter`] 与 `dynamic` 适配器共用本函数；`#[serde(deny_unknown_fields)]`
//!   的拒绝面不受影响（我们不新增字段）。

use serde_json::{Map, Value};

/// 按 `schema` 归一 `value`，返回可直接喂给 `from_value` 的副本。
///
/// 无 schema（`Value::Null`）或类型已匹配时原样返回。
#[must_use]
pub fn coerce(value: &Value, schema: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let properties = schema.get("properties");
            let none = Value::Null;
            let mut out = Map::with_capacity(map.len());
            for (key, inner) in map {
                let sub = properties.and_then(|p| p.get(key)).unwrap_or(&none);
                out.insert(key.clone(), coerce(inner, sub));
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            // `items` 为 schema 对象时逐项归一；tuple 形态（items 是数组）不猜。
            let none = Value::Null;
            let item_schema = schema.get("items").unwrap_or(&none);
            if item_schema.is_object() {
                Value::Array(
                    items
                        .iter()
                        .map(|inner| coerce(inner, item_schema))
                        .collect(),
                )
            } else {
                value.clone()
            }
        }
        Value::String(raw) => normalize_scalar(raw, schema).unwrap_or_else(|| value.clone()),
        _ => value.clone(),
    }
}

/// 由 schema 的顶层 `properties` 拼出字段期望类型清单，作为反序列化失败时的 hint。
///
/// 只列顶层字段（嵌套结构的形状由 `properties` 递归承担，模型按字段名重试即可）。
/// 字段数上限 16，避免 hint 本身变成上下文税。
#[must_use]
pub fn expected_shape_hint(schema: &Value) -> String {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return "参数类型不匹配：数字/布尔/数组必须是 JSON 字面量，不是字符串（\
                用 3 而非 \"3\"，用 [\"a\"] 而非 \"[\\\"a\\\"]\"）。"
            .to_owned();
    };
    let mut fields = Vec::with_capacity(properties.len().min(16));
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for (key, sub) in properties.iter().take(16) {
        let name = if required.iter().any(|r| r.as_str() == Some(key.as_str())) {
            key.clone()
        } else {
            format!("{key}?")
        };
        fields.push(format!("{name}={}", describe_type(sub)));
    }
    format!(
        "字段类型：{}。数字/布尔/数组须为 JSON 字面量而非字符串；缺省的可选字段请直接省略，不要填 \"\" 或 null。",
        fields.join(", ")
    )
}

/// schema 节点的期望类型拼写（含 array 元素类型，如 `array<string>`）。
fn describe_type(schema: &Value) -> String {
    match wanted_type(schema) {
        Some(t) if t == "array" => match schema.get("items").and_then(wanted_type) {
            Some(item) => format!("array<{item}>"),
            None => "array".to_owned(),
        },
        Some(t) => t.to_owned(),
        None => schema
            .get("enum")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "any".to_owned()),
    }
}

/// 取 schema 声明的单一类型：接受 `"integer"` 与 `["integer","null"]` 两种形态。
fn wanted_type(schema: &Value) -> Option<&str> {
    match schema.get("type")? {
        Value::String(t) => Some(t),
        Value::Array(types) => types
            .iter()
            .filter_map(Value::as_str)
            .find(|t| *t != "null"),
        _ => None,
    }
}

/// 字符串 → schema 要求的 JSON 类型；不可能无损转换时返回 `None`。
fn normalize_scalar(raw: &str, schema: &Value) -> Option<Value> {
    match wanted_type(schema)? {
        "integer" => parse_integer(raw),
        "number" => parse_number(raw),
        "boolean" => parse_bool(raw),
        "array" => parse_json(raw, Value::is_array),
        "object" => parse_json(raw, Value::is_object),
        // string / null / 未声明类型：不动模型给的字面串。
        _ => None,
    }
}

fn parse_integer(raw: &str) -> Option<Value> {
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(v) = text.parse::<u64>() {
        return Some(Value::from(v));
    }
    if let Ok(v) = text.parse::<i64>() {
        return Some(Value::from(v));
    }
    // "3.0" 这类整值浮点仍是整数语义；"3.5" 拒绝（不得悄悄截断）。
    let v = text.parse::<f64>().ok()?;
    if v.fract() == 0.0 && v.abs() <= (u64::MAX as f64) {
        Some(Value::from(v as u64))
    } else {
        None
    }
}

/// 整数语义解析的**唯一事实源**：已收好的 `Value`（数字或字符串拼写）→ 整数
/// `Value::Number`，不成立时 `None`。
///
/// [`super::lenient`] 直接调它，两层因此共用同一套「什么算无损」的判定，
/// 不会出现 schema 面接受、类型面拒绝（或反之）的分叉。负数与小数在这里就
/// 被排除：`Value::as_u64()` 对它们给 `None`，调用方据此拒绝而非截断。
pub(crate) fn parse_integer_like(value: &Value) -> Option<Value> {
    match value {
        Value::String(raw) => parse_integer(raw),
        Value::Number(number) => number
            .as_u64()
            .map(Value::from)
            .or_else(|| number.as_f64().filter(|v| *v >= 0.0 && v.fract() == 0.0).map(|v| Value::from(v as u64))),
        _ => None,
    }
}

fn parse_number(raw: &str) -> Option<Value> {
    let v = raw.trim().parse::<f64>().ok()?;
    serde_json::Number::from_f64(v).map(Value::Number)
}

fn parse_bool(raw: &str) -> Option<Value> {
    match raw.trim() {
        "true" | "TRUE" | "True" | "1" | "yes" => Some(Value::Bool(true)),
        "false" | "FALSE" | "False" | "0" | "no" => Some(Value::Bool(false)),
        _ => None,
    }
}

fn parse_json(raw: &str, pred: fn(&Value) -> bool) -> Option<Value> {
    let parsed = serde_json::from_str::<Value>(raw).ok()?;
    if pred(&parsed) { Some(parsed) } else { None }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn args_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "start_line": { "type": "integer" },
                "max_results": { "type": ["integer", "null"] },
                "ratio": { "type": "number" },
                "case_sensitive": { "type": "boolean" },
                "glob": { "type": "array", "items": { "type": "string" } },
                "env": { "type": "object" },
                "requests": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" },
                            "end_line": { "type": "integer" }
                        }
                    }
                }
            },
            "required": ["path"]
        })
    }

    #[test]
    fn stringified_scalars_are_coerced() {
        let out = coerce(
            &json!({
                "path": "a.rs",
                "start_line": "3",
                "max_results": " 40 ",
                "ratio": "1.5",
                "case_sensitive": "true",
                "env": "{\"K\":\"V\"}"
            }),
            &args_schema(),
        );
        assert_eq!(out["start_line"], json!(3), "u64 字段收到 \"3\"");
        assert_eq!(out["max_results"], json!(40), "联合类型数组形态也要吃");
        assert_eq!(out["ratio"], json!(1.5));
        assert_eq!(out["case_sensitive"], json!(true));
        assert_eq!(out["env"], json!({"K": "V"}));
        assert_eq!(out["path"], json!("a.rs"), "string 字段原样");
    }

    #[test]
    fn stringified_arrays_and_nested_items_are_coerced() {
        let out = coerce(
            &json!({
                "path": "a.rs",
                "glob": "[\"*.rs\",\"*.md\"]",
                "requests": [{ "path": "b.rs", "end_line": "12" }]
            }),
            &args_schema(),
        );
        assert_eq!(out["glob"], json!(["*.rs", "*.md"]));
        assert_eq!(out["requests"][0]["end_line"], json!(12));
    }

    #[test]
    fn lossy_conversions_are_left_alone() {
        let src = json!({
            "path": "a.rs",
            "start_line": "abc",
            "max_results": "3.5",
            "glob": "*.rs",
            "case_sensitive": "maybe"
        });
        let out = coerce(&src, &args_schema());
        assert_eq!(out, src, "猜不出来的必须原样交给反序列化报错");
    }

    #[test]
    fn hint_lists_field_types_with_optional_marker() {
        let hint = expected_shape_hint(&args_schema());
        // required 字段裸名，可选字段带 `?`。
        assert!(hint.contains("path=string"), "{hint}");
        assert!(hint.contains("start_line?=integer"), "{hint}");
        assert!(hint.contains("max_results?=integer"), "{hint}");
        assert!(hint.contains("ratio?=number"), "{hint}");
        assert!(hint.contains("case_sensitive?=boolean"), "{hint}");
        assert!(hint.contains("glob?=array<string>"), "{hint}");
        assert!(hint.contains("env?=object"), "{hint}");
        assert!(hint.contains("requests?=array<object>"), "{hint}");
    }

    #[test]
    fn hint_degrades_without_object_schema() {
        let hint = expected_shape_hint(&json!({ "type": "string" }));
        assert!(hint.contains("JSON"), "{hint}");
    }

    #[test]
    fn coerce_without_schema_is_identity() {
        let src = json!({ "count": "3", "globs": "[\"a\"]" });
        assert_eq!(coerce(&src, &Value::Null), src);
    }
}
