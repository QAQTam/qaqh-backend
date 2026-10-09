//! Wire-neutral usage normalization — the single entry for every adapter.
//!
//! 各 wire 的字段名与缓存口径不同（`prompt_tokens` vs `input_tokens`、
//! `prompt_cache_hit_tokens` vs `prompt_tokens_details.cached_tokens` vs
//! `cache_read_input_tokens`），这部分不可合并，各适配器自己填 [`UsageSeed`]。
//! 可合并的是横切项：`total_tokens` 推导、`cache_usage_reported` 传递，以及
//! **未被建模的 provider 原生字段**（`credit`、`prompt_cache_write_tokens`、
//! `completion_thinking_tokens` 之流）。这些只在这里捕获一次——上游
//! `mutil_ai::Usage.raw` 持有的就是原生 usage 对象本身，此前被逐字段挑拣丢弃。

use std::collections::BTreeMap;

use serde_json::Value;

use qaqh_types::UsageInfo;

/// 已被 `UsageInfo` 建模的键：不得再进 `extras`，否则同一个数在两处出现。
///
/// 取全部 wire 的并集而非当前 wire 用到的那几个：`cached_tokens` 在
/// `CacheTokenField::UsageCachedTokens` 下是建模量，在别的端点下也是同一个语义量，
/// 只是没被采用。
const MODELED_KEYS: &[&str] = &[
    "prompt_tokens",
    "completion_tokens",
    "total_tokens",
    "input_tokens",
    "output_tokens",
    "prompt_cache_hit_tokens",
    "prompt_cache_miss_tokens",
    "cached_tokens",
    "prompt_tokens_details",
    "completion_tokens_details",
    "cache_read_input_tokens",
    "cache_creation_input_tokens",
    "reasoning_tokens",
];

/// `extras` 键数上限。这是 per-turn、进广播通道、还可能被聚合落盘的东西，
/// 不设上限等于把上游整个 usage 对象搬进流里。
const MAX_EXTRAS: usize = 16;

/// 各 wire 适配器解析出的中性输入（字段名差异留在适配器里）。
pub(crate) struct UsageSeed {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// 端点自报的 total；缺失时按 prompt + completion 推。
    pub total_tokens: Option<u32>,
    pub cache_hit_tokens: u32,
    pub cache_miss_tokens: u32,
    /// 端点是否真的回了缓存字段——真零命中 ≠ 没上报。
    pub cache_reported: bool,
    pub reasoning_tokens: u32,
}

/// 唯一出口。适配器只填 [`UsageSeed`]，口径与 `extras` 在这里定。
pub(crate) fn normalize(seed: UsageSeed, raw: Option<&Value>) -> UsageInfo {
    UsageInfo {
        prompt_tokens: seed.prompt_tokens,
        completion_tokens: seed.completion_tokens,
        total_tokens: seed
            .total_tokens
            .unwrap_or_else(|| seed.prompt_tokens.saturating_add(seed.completion_tokens)),
        prompt_cache_hit_tokens: seed.cache_hit_tokens,
        prompt_cache_miss_tokens: seed.cache_miss_tokens,
        reasoning_tokens: seed.reasoning_tokens,
        cache_usage_reported: Some(seed.cache_reported),
        extras: extras_from_raw(raw),
    }
}

/// 抓 provider 原生 usage 里**未被建模**的顶层整数字段。
///
/// 只取顶层标量：嵌套对象（`prompt_tokens_details` / `completion_tokens_details`）
/// 在各 wire 已有专门口径，再摊平一层只会把同一个数重复计数。非整数、非数值与
/// 超出上限的键一律丢弃——`extras` 是展示面，不是原样转储。
fn extras_from_raw(raw: Option<&Value>) -> BTreeMap<String, i64> {
    let mut extras = BTreeMap::new();
    let Some(object) = raw.and_then(Value::as_object) else {
        return extras;
    };
    for (key, value) in object {
        if extras.len() >= MAX_EXTRAS {
            break;
        }
        if MODELED_KEYS.contains(&key.as_str()) {
            continue;
        }
        let Some(number) = value.as_i64() else {
            continue;
        };
        extras.insert(key.clone(), number);
    }
    extras
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn seed() -> UsageSeed {
        UsageSeed {
            prompt_tokens: 4970,
            completion_tokens: 16,
            total_tokens: None,
            cache_hit_tokens: 4928,
            cache_miss_tokens: 42,
            cache_reported: true,
            reasoning_tokens: 32,
        }
    }

    #[test]
    fn derives_total_when_endpoint_omits_it() {
        let info = normalize(seed(), None);
        assert_eq!(info.total_tokens, 4986);
        assert!(info.extras.is_empty());
    }

    #[test]
    fn keeps_provider_native_fields_out_of_the_modeled_set() {
        // workbuddy 端点实测帧：credit / cache_write / thinking 都不在建模集里。
        let raw = json!({
            "prompt_tokens": 4970,
            "completion_tokens": 16,
            "total_tokens": 4986,
            "prompt_cache_hit_tokens": 4928,
            "prompt_cache_miss_tokens": 42,
            "cached_tokens": 0,
            "prompt_tokens_details": { "cached_tokens": 0 },
            "completion_tokens_details": { "reasoning_tokens": 32 },
            "reasoning_tokens": 32,
            "prompt_cache_write_tokens": 0,
            "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 0,
            "completion_thinking_tokens": 32,
            "credit": 7
        });
        let extras = normalize(seed(), Some(&raw)).extras;
        assert_eq!(extras.get("credit"), Some(&7));
        assert_eq!(extras.get("completion_thinking_tokens"), Some(&32));
        assert_eq!(extras.get("prompt_cache_write_tokens"), Some(&0));
        // 建模键一个都不许漏进 extras。
        for key in MODELED_KEYS {
            assert!(extras.get(*key).is_none(), "{key} 不该进 extras");
        }
    }

    #[test]
    fn drops_non_integer_and_caps_the_key_count() {
        let mut raw = serde_json::Map::new();
        raw.insert("note".into(), json!("不是数字"));
        raw.insert("nil".into(), Value::Null);
        raw.insert("fractional".into(), json!(1.5));
        for index in 0..MAX_EXTRAS + 8 {
            raw.insert(format!("field_{index:02}"), json!(index));
        }
        let extras = extras_from_raw(Some(&Value::Object(raw)));
        assert_eq!(extras.len(), MAX_EXTRAS);
        assert!(extras.get("note").is_none());
        assert!(extras.get("nil").is_none());
        assert!(extras.get("fractional").is_none());
    }
}
