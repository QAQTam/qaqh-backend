//! 工具 schema 单一事实源（SDK v2）。
//!
//! input/output schema 一律由 `Args`/`Output` 类型生成，禁止手写 JSON schema：
//! 类型与 wire 描述的漂移从机制上消失，聚合形态误加 `oneOf` 的空间也不复存在。
//!
//! 生成设置：
//! - `meta_schema = None`：不发 `$schema`（provider 不消费，纯 wire 税）；
//! - `inline_subschemas = true`：不产 `$defs`/`$ref`，工具面 schema 自包含
//!   （OpenAI structured outputs 对外部 `$ref` 支持不一）；
//! - 字段描述来自 doc comment（schemars derive 约定），`#[schemars(description)]`
//!   可显式覆盖；`#[serde(deny_unknown_fields)]` 生成 `additionalProperties: false`；
//! - [`collapse_null_types`] 把 schemars 对可选字段产出的
//!   `"type": ["T","null"]` 折叠回单类型（见该函数注释——不发联合类型是
//!   provider 面的硬要求，不是风格问题）。

use schemars::JsonSchema;
use schemars::generate::{SchemaGenerator, SchemaSettings};
use serde_json::Value;

fn generator() -> SchemaGenerator {
    let mut settings = SchemaSettings::draft2020_12();
    settings.meta_schema = None;
    settings.inline_subschemas = true;
    SchemaGenerator::new(settings)
}

/// 从类型生成工具参数/输出 schema（object 形态）。
pub fn schema_of<T: JsonSchema>() -> Value {
    let schema = serde_json::to_value(generator().into_root_schema_for::<T>())
        .expect("JsonSchema 生成结果必可序列化为 JSON");
    strip_provider_unsafe_format(collapse_null_types(schema))
}

/// 丢掉 `format` 注解（递归）。
///
/// schemars 给 `u64` 发 `"format": "uint64"`，而各家 JSON-Schema 子集对
/// `format` 的白名单里没有这个拼写（OpenAI 只认 `int32`/`int64`/`float`/
/// `double`），gate 的注释也把 `format` 列为「endpoints reject or silently
/// misbehave」的成员——但那条塑形（`sanitize_openai_schema`）只覆盖
/// Responses 一条路径，chat/anthropic/gemini 是裸发的。在生成端一次性剥掉，
/// 四条路径就都拿到同一份诚实 schema；`minimum`/`maxItems` 这类真约束保留。
#[must_use]
pub fn strip_provider_unsafe_format(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(key, _)| key != "format")
                .map(|(key, inner)| (key, strip_provider_unsafe_format(inner)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(strip_provider_unsafe_format)
                .collect(),
        ),
        scalar => scalar,
    }
}

/// 把 `"type": ["T", "null"]` 折叠成 `"type": "T"`（递归，含 `$defs`/items/properties）。
///
/// schemars 对 `Option<T>` 字段（draft2020_12、无 `AddNullable` transform 时）
/// 会把 `type` 改写成数组形态。工具 schema 直发 provider 时这是撒谎：部分
/// OpenAI 兼容网关与 Gemini 不支持数组型 `type`，会拒绝或降级成 `string`，
/// 模型于是照着降级后的 schema 把数字/数组写成字符串（`"3"`、`"[\"*.rs\"]"`），
/// typed 反序列化再硬失败成 `invalid type: string "3", expected u64`。
///
/// 可选性本来就由「字段不在 `required` 里」表达，`null` 是纯噪声；Rust 侧
/// `Option<T>` 仍接受 `null`，所以折叠只收紧模型面，不放宽反序列化面。
#[must_use]
pub fn collapse_null_types(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, inner)| {
                    if key == "type" {
                        (key, collapse_type_array(inner))
                    } else {
                        (key, collapse_null_types(inner))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(collapse_null_types).collect()),
        scalar => scalar,
    }
}

/// 仅当 `type` 的值确实是「JSON 类型名数组且含 null」时才折叠，避免误伤
/// 恰好叫 `type` 的业务字段（其 schema 形态是 object，不是字符串数组）。
fn collapse_type_array(value: Value) -> Value {
    const TYPES: [&str; 7] = [
        "string", "number", "integer", "object", "array", "boolean", "null",
    ];
    let Value::Array(types) = value else {
        return value;
    };
    let names: Vec<String> = types
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    if names.len() != types.len() || !names.iter().all(|n| TYPES.contains(&n.as_str())) {
        return Value::Array(types);
    }
    let kept: Vec<String> = names.into_iter().filter(|n| n != "null").collect();
    match kept.len() {
        0 => Value::String("null".to_owned()),
        1 => Value::String(kept[0].clone()),
        _ => Value::Array(kept.into_iter().map(Value::String).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    /// 读取文件。
    #[allow(dead_code)] // 仅用于 schema 生成断言，字段从不读取。
    struct SampleArgs {
        /// 文件路径。
        path: String,
        /// 起始行（1-based）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start_line: Option<u64>,
    }

    #[test]
    fn generated_schema_is_compact_and_object_shaped() {
        let schema = schema_of::<SampleArgs>();
        assert_eq!(schema["type"], "object");
        assert!(schema.get("$schema").is_none(), "不发 $schema");
        assert!(schema.get("$defs").is_none(), "子模式内联");
        assert_eq!(schema["additionalProperties"], false);
        // doc comment → description；不得混入 title。
        assert_eq!(schema["properties"]["path"]["description"], "文件路径。");
        assert!(schema["properties"]["path"].get("title").is_none());
        assert!(schema["properties"]["start_line"].get("title").is_none());
    }

    #[test]
    fn generated_schema_has_no_composition_keys() {
        // oneOf/anyOf/allOf 是手写聚合 schema 的残留形态；类型生成路径不允许出现。
        let schema = schema_of::<SampleArgs>().to_string();
        assert!(!schema.contains("oneOf"));
        assert!(!schema.contains("anyOf"));
        assert!(!schema.contains("allOf"));
    }

    #[test]
    fn optional_fields_never_emit_union_type_arrays() {
        // Option<u64> 经 schemars 会产 `["integer","null"]`；provider 面必须看到单类型。
        let schema = schema_of::<SampleArgs>();
        assert_eq!(
            schema["properties"]["start_line"]["type"], "integer",
            "可选字段的 type 不得是数组：{schema}"
        );
        assert_eq!(schema["properties"]["path"]["type"], "string");
        // 折叠只作用于 `type` 键，不得顺手清掉其它数组（required 等）。
        assert_eq!(schema["required"], serde_json::json!(["path"]));
        // `format: "uint64"` 不在 provider 白名单里 → 剥掉；真约束 `minimum` 保留。
        let start_line = &schema["properties"]["start_line"];
        assert!(start_line.get("format").is_none(), "{start_line}");
        assert_eq!(start_line["minimum"], serde_json::json!(0));
    }

    #[test]
    fn collapse_is_structural_not_textual() {
        // 恰好叫 type 的业务字段（值是 object 形态）与非类型名数组都不得被动。
        let collapsed = collapse_null_types(serde_json::json!({
            "properties": {
                "kind": { "type": ["array", "null"], "items": { "type": ["string", "null"] } },
                "type": { "type": "string" },
                "enum": { "enum": ["a", null] }
            },
            "$defs": { "Deep": { "type": ["object", "null"] } },
            "required": ["kind"]
        }));
        assert_eq!(
            collapsed,
            serde_json::json!({
                "properties": {
                    "kind": { "type": "array", "items": { "type": "string" } },
                    "type": { "type": "string" },
                    "enum": { "enum": ["a", null] }
                },
                "$defs": { "Deep": { "type": "object" } },
                "required": ["kind"]
            })
        );
        // 多类型非 null 联合保留数组形态（我们不猜哪个对）。
        assert_eq!(
            collapse_null_types(serde_json::json!({ "type": ["string", "integer"] })),
            serde_json::json!({ "type": ["string", "integer"] })
        );
        // 纯 null 折叠回单类型字符串。
        assert_eq!(
            collapse_null_types(serde_json::json!({ "type": ["null"] })),
            serde_json::json!({ "type": "null" })
        );
    }
}
