//! 类型级 wire 容错（与 [`super::coerce`] 互补的第二层）。
//!
//! [`super::coerce`] 是 **schema 驱动**的：它按 input schema 里声明的字段类型
//! 改写值形态。于是有一类字段它结构上看不见——`#[schemars(skip)]` 藏出模型面
//! 的字段（`read` 的单请求糖字段 `start_line`/`end_line` 就是这一类），以及
//! 宿主侧把 schema 裁剪/降级后「据以容错的类型」与「实收形态」不同源的字段。
//! 这类字段上的 `invalid arguments: invalid type: string "220", expected u64`
//! 只能由**反序列化器自己**兜住：类型自己声明「我接受数字，也接受数字的字符串
//! 拼写」，不依赖任何外部 schema 是否提到我。
//!
//! 两层分工（语义一致、可叠加）：
//! - coerce：在 typed 边界前按 schema 归一 stringified 标量/集合，覆盖全字段面；
//! - 本模块：对「类型即事实源」的标量字段兜最后一道，schema 缺席也照样成立。
//!
//! 无损性约束与 coerce 同源（复用 [`super::coerce::parse_integer_like`]）：
//! `"abc"`、`"3.5"`、`"-1"`、`""` 一律拒绝并上抛带类型的错误，绝不静默猜 0
//! 或截断——静默猜测比失败更糟。

use serde::Deserialize;
use serde::de::Error as _;
use serde_json::Value;

/// `Option<u64>`：接受 JSON 数字、整值浮点，以及它们的字符串拼写（`"220"`、
/// `" 40 "`、`"3.0"`）；`null` 视作缺省。
///
/// 用法：`#[serde(default, deserialize_with = "crate::tool_api::lenient::opt_u64")]`。
///
/// 经 `Value` 中转，故要求反序列化器支持 `deserialize_any`——本 SDK 的两条
/// 派发路径（[`super::typed`] 与 [`super::dynamic`]）递给 handler 的都是
/// `serde_json::Value`，恒成立。
pub fn opt_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    if value.is_null() {
        return Ok(None);
    }
    super::coerce::parse_integer_like(&value)
        .and_then(|parsed| parsed.as_u64())
        .map(Some)
        .ok_or_else(|| {
            D::Error::custom(format!(
                "invalid type: {}, expected u64（数字或数字的字符串拼写，如 3 / \\\"3\\\"；\
                 小数、负数、非数字文本一律拒绝）",
                describe(&value)
            ))
        })
}

/// 错误文案里的实收形态，与 serde 原生 `invalid type: string "x"` 拼写对齐，
/// 让模型沿用上一次的纠错直觉。
fn describe(value: &Value) -> String {
    match value {
        Value::String(raw) => format!("string {raw:?}"),
        Value::Number(n) => format!("number {n}"),
        Value::Bool(b) => format!("boolean {b}"),
        Value::Null => "null".to_owned(),
        Value::Array(_) => "array".to_owned(),
        Value::Object(_) => "object".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use serde_json::json;

    use super::*;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Args {
        #[serde(default, deserialize_with = "opt_u64")]
        end_line: Option<u64>,
    }

    fn parse(raw: Value) -> Result<Args, serde_json::Error> {
        serde_json::from_value(raw)
    }

    #[test]
    fn numbers_and_stringified_numbers_are_both_accepted() {
        assert_eq!(
            parse(json!({"end_line": "220"})).unwrap(),
            parse(json!({"end_line": 220})).unwrap(),
            "字符串拼写与数字必须解析成同一个值"
        );
        assert_eq!(parse(json!({"end_line": " 40 "})).unwrap().end_line, Some(40));
        assert_eq!(parse(json!({"end_line": "3.0"})).unwrap().end_line, Some(3));
        assert_eq!(parse(json!({"end_line": null})).unwrap().end_line, None);
        assert_eq!(parse(json!({})).unwrap().end_line, None);
    }

    #[test]
    fn lossy_or_bogus_input_is_rejected_not_guessed() {
        for raw in [
            json!({"end_line": "abc"}),
            json!({"end_line": "3.5"}),
            json!({"end_line": ""}),
            json!({"end_line": "-1"}),
            json!({"end_line": true}),
            json!({"end_line": [1]}),
            json!({"end_line": {"a": 1}}),
        ] {
            let error = parse(raw.clone()).expect_err("不得静默猜测");
            let text = error.to_string();
            assert!(text.contains("expected u64"), "{raw} → {text}");
        }
    }

    #[test]
    fn error_text_names_the_received_shape() {
        let error = parse(json!({"end_line": "220x"})).expect_err("非数字文本");
        assert!(
            error.to_string().contains("string \"220x\""),
            "{}",
            error
        );
    }
}
