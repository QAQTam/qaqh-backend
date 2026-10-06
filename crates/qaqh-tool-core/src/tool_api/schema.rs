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
//!   可显式覆盖；`#[serde(deny_unknown_fields)]` 生成 `additionalProperties: false`。

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
    serde_json::to_value(generator().into_root_schema_for::<T>())
        .expect("JsonSchema 生成结果必可序列化为 JSON")
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
}
