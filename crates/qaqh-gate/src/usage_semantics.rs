//! Messages wire 的缓存用量口径判定（inclusive / exclusive）。
//!
//! ## 为什么需要这一层
//!
//! Anthropic Messages 的 `input_tokens` 是 **exclusive**：只算未命中部分，
//! `cache_read_input_tokens` / `cache_creation_input_tokens` 是并列的两个桶，
//! 总输入 = 三者之和。但很多反代把 OpenAI 口径直接套到 Anthropic wire——
//! `input_tokens` 就是整段 prompt（含命中），`cache_read_input_tokens` 只是它的
//! 子集。对后者按 Anthropic 求和等于**把缓存前缀算两遍**：
//!
//! ```text
//! 真值（inclusive 反代）:  input=6 835（含 6 144 命中）      命中率 89.9%
//! 按 exclusive 读:         总输入 = 6 835 + 6 144 = 12 979   命中率 47.3%
//! 长会话（真实命中 99%）:   显示恒 ≤ 50%，且命中率越高错得越狠
//! ```
//!
//! 这条偏差不止是显示：runtime 把 `prompt_tokens` 当上下文占用喂给模型
//! （`qaqh_types::context_usage`）与 token 校准（`observe_prepared_request`），
//! inclusive 端点上这两个读数会一起翻倍——模型被告知 2× 占用，compact 在真实
//! 窗口的一半就可能触发。
//!
//! ## 判定策略（阈值对齐 Claude Code，`occ/src/utils/usageSemantics.ts`）
//!
//! 1. 端点声明（[`CacheUsageSemantics`] 非 `Auto`）或环境变量
//!    `QAQH_CACHE_USAGE_SEMANTICS=inclusive|exclusive` 优先，直接锁定，不再观测；
//! 2. 否则按真实响应判定，且**两次不同样本给出一致信号**才锁定：
//!    `input >= 0.9 * cache_read` 判 inclusive，`input < 0.5 * cache_read` 判
//!    exclusive，中间区间视为无信号；
//! 3. 未锁定时按协议契约取 exclusive（Anthropic 原生语义）。
//!
//! 为什么必须两次：一轮大段粘贴会让暖缓存看起来像 inclusive
//! （`input=20K`、`cache_read=15K`），下一轮 `input` 掉回 ~200，两样本不一致就
//! 不该锁定。同一组数字被重复观测（UI 多个消费者读同一个响应）按签名去重，
//! 不算第二次。
//!
//! 作用域是**会话**（gate 收到的 `session_id`）：同一进程里不同 profile / 不同
//! 反代的会话互不污染；`chat_sync`（compact/title）没有会话身份时退化到
//! `(base_url, model)` 作键。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use qaqh_types::CacheUsageSemantics;

/// 判 inclusive 的下界系数：`input >= 0.9 * cache_read`。
const INCLUSIVE_RATIO: f64 = 0.9;
/// 判 exclusive 的上界系数：`input < 0.5 * cache_read`。
const EXCLUSIVE_RATIO: f64 = 0.5;
/// latch 表上限。纯观测缓存，超出直接清空——重新判定只需两轮请求。
const MAX_LATCHES: usize = 512;

/// 环境变量逃生口：对齐 Claude Code 的 `CLAUDE_CODE_CACHE_USAGE_SEMANTICS`。
const ENV_OVERRIDE: &str = "QAQH_CACHE_USAGE_SEMANTICS";

/// 判定后的口径（`Auto` 解析完只剩这两种）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resolved {
    Inclusive,
    Exclusive,
}

/// 一次请求的观测作用域：端点声明 + 归属键（会话 id）。
#[derive(Debug, Clone)]
pub(crate) struct Scope {
    /// 已确定的口气（声明 / 环境变量）。`None` = 需要按响应判定。
    fixed: Option<Resolved>,
    key: String,
}

impl Scope {
    /// `declared` 来自 [`qaqh_gate::ProviderConfig::cache_usage_semantics`]；
    /// `key` 一般是会话 id，`chat_sync` 用 `(base_url, model)`。
    pub(crate) fn new(declared: CacheUsageSemantics, key: impl Into<String>) -> Self {
        Self {
            fixed: declared_resolution(declared).or_else(env_resolution),
            key: key.into(),
        }
    }

    /// 本响应适用的口径。顺带把观测喂给 latch（幂等、可重复调用）。
    pub(crate) fn resolve(&self, input: u32, creation: u32, cache_read: u32) -> Resolved {
        if let Some(fixed) = self.fixed {
            return fixed;
        }
        observe(&self.key, input, creation, cache_read)
    }
}

/// 把一次响应的 usage 拆成 `(未命中, 命中)`；两者相加就是"总输入"。
///
/// - inclusive：`cache_read` / `cache_creation` 是 `input` 的子集，再加一遍
///   就是双计。命中按 `min(cache_read, input)` 夹住，越界读数不产生负数。
/// - exclusive：`input` 只是未命中部分，`cache_creation` 属于未命中侧。
pub(crate) fn split(semantics: Resolved, input: u32, creation: u32, cache_read: u32) -> (u32, u32) {
    match semantics {
        Resolved::Inclusive => {
            let hit = cache_read.min(input);
            (input.saturating_sub(hit), hit)
        }
        Resolved::Exclusive => (input.saturating_add(creation), cache_read),
    }
}

/// 单次观测的信号：`None` = 这次响应说明不了问题。
fn detect(input: u32, cache_read: u32) -> Option<Resolved> {
    if cache_read == 0 {
        // 冷启动 / 无缓存字段：inclusive 与 exclusive 在这里给出同样的数，
        // 拿它做判据只会在下一次真命中时被推翻。
        return None;
    }
    let input = f64::from(input);
    let cache_read = f64::from(cache_read);
    if input >= cache_read * INCLUSIVE_RATIO {
        Some(Resolved::Inclusive)
    } else if input < cache_read * EXCLUSIVE_RATIO {
        Some(Resolved::Exclusive)
    } else {
        None
    }
}

fn declared_resolution(declared: CacheUsageSemantics) -> Option<Resolved> {
    match declared {
        CacheUsageSemantics::Auto => None,
        CacheUsageSemantics::Inclusive => Some(Resolved::Inclusive),
        CacheUsageSemantics::Exclusive => Some(Resolved::Exclusive),
    }
}

fn env_resolution() -> Option<Resolved> {
    parse_override(std::env::var(ENV_OVERRIDE).ok().as_deref())
}

fn parse_override(raw: Option<&str>) -> Option<Resolved> {
    match raw?.trim().to_ascii_lowercase().as_str() {
        "inclusive" => Some(Resolved::Inclusive),
        "exclusive" => Some(Resolved::Exclusive),
        _ => None,
    }
}

/// 每个作用域的判定状态：锁定值 + 待确认信号 + 上次观测签名。
#[derive(Debug, Default)]
struct Latch {
    decided: Option<Resolved>,
    pending: Option<Resolved>,
    last_signature: Option<(u32, u32, u32)>,
}

fn latches() -> &'static Mutex<HashMap<String, Latch>> {
    static LATCHES: OnceLock<Mutex<HashMap<String, Latch>>> = OnceLock::new();
    LATCHES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn observe(key: &str, input: u32, creation: u32, cache_read: u32) -> Resolved {
    let Some(signal) = detect(input, cache_read) else {
        return Resolved::Exclusive;
    };
    let mut map = latches().lock().unwrap_or_else(|e| e.into_inner());
    if map.len() > MAX_LATCHES {
        map.clear();
    }
    let latch = map.entry(key.to_string()).or_default();
    if let Some(decided) = latch.decided {
        return decided;
    }
    let signature = (input, creation, cache_read);
    if latch.last_signature == Some(signature) {
        // 同一个响应被读了多次：不能自我确认。
        return Resolved::Exclusive;
    }
    latch.last_signature = Some(signature);
    if latch.pending == Some(signal) {
        latch.decided = Some(signal);
        latch.pending = None;
        log::info!(
            "[usage] cache semantics latched to {signal:?} for {key} (input={input} creation={creation} cache_read={cache_read})"
        );
        return signal;
    }
    latch.pending = Some(signal);
    Resolved::Exclusive
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(key: &str) -> Scope {
        Scope::new(CacheUsageSemantics::Auto, key)
    }

    #[test]
    fn inclusive_and_exclusive_signals_are_opposite() {
        // 真实反代样本：input 是总量，命中 6 144 → inclusive。
        assert_eq!(detect(6835, 6144), Some(Resolved::Inclusive));
        // 真 Anthropic：input 只是未命中，命中 15 000 → exclusive。
        assert_eq!(detect(200, 15_000), Some(Resolved::Exclusive));
        // 冷启动 / 无缓存字段：无信号。
        assert_eq!(detect(6835, 0), None);
        // 中间区间（大段粘贴的单轮假象）：无信号。
        assert_eq!(detect(8000, 10_000), None);
    }

    #[test]
    fn latch_needs_two_agreeing_samples() {
        let key = "latch-two-samples";
        let s = scope(key);
        assert_eq!(
            s.resolve(6835, 0, 6144),
            Resolved::Exclusive,
            "首个样本只登记待确认信号，先按协议契约的 exclusive 读数"
        );
        assert_eq!(
            s.resolve(7100, 0, 6600),
            Resolved::Inclusive,
            "第二个一致样本锁定 inclusive"
        );
        assert_eq!(
            s.resolve(200, 0, 15_000),
            Resolved::Inclusive,
            "锁定后不再翻转"
        );
    }

    #[test]
    fn one_big_paste_does_not_latch() {
        let key = "latch-big-paste";
        let s = scope(key);
        // 一轮大段粘贴：看起来像 inclusive。
        assert_eq!(s.resolve(20_000, 0, 15_000), Resolved::Exclusive);
        // 下一轮回到暖缓存常态：exclusive 信号，与待确认信号相反。
        assert_eq!(s.resolve(200, 0, 15_000), Resolved::Exclusive);
        assert_eq!(
            s.resolve(210, 0, 15_100),
            Resolved::Exclusive,
            "两次不一致 → 不锁定，继续按协议契约读"
        );
    }

    #[test]
    fn the_same_response_observed_twice_does_not_self_confirm() {
        let key = "latch-dedupe";
        let s = scope(key);
        let usage = (6835, 0, 6144);
        assert_eq!(s.resolve(usage.0, usage.1, usage.2), Resolved::Exclusive);
        assert_eq!(
            s.resolve(usage.0, usage.1, usage.2),
            Resolved::Exclusive,
            "同签名重复观测不算第二次样本"
        );
        assert_eq!(
            s.resolve(6836, 0, 6145),
            Resolved::Inclusive,
            "换一组数字才算第二样本"
        );
    }

    #[test]
    fn declared_semantics_wins_without_observation() {
        let key = "declared-inclusive";
        let s = Scope::new(CacheUsageSemantics::Inclusive, key);
        // 这组数字在自动判定里是 exclusive 信号，但声明优先。
        assert_eq!(s.resolve(200, 0, 15_000), Resolved::Inclusive);
        // 声明的作用域没有写 latch：换一个 Auto 作用域仍是未锁定状态。
        assert_eq!(
            scope(key).resolve(200, 0, 15_000),
            Resolved::Exclusive,
            "声明不写 latch，Auto 作用域仍从协议契约起步"
        );
    }

    #[test]
    fn env_spelling_is_case_insensitive_and_strict() {
        assert_eq!(
            parse_override(Some(" Inclusive ")),
            Some(Resolved::Inclusive)
        );
        assert_eq!(parse_override(Some("EXCLUSIVE")), Some(Resolved::Exclusive));
        assert_eq!(parse_override(Some("auto")), None);
        assert_eq!(parse_override(None), None);
    }

    /// 计数本身（命中 token）两种口径下都一致，只有分母不同。
    #[test]
    fn split_follows_the_two_semantics() {
        // 反代样本：总量 6 835、命中 6 144 → 未命中 691，命中率 89.9%。
        assert_eq!(split(Resolved::Inclusive, 6835, 0, 6144), (691, 6144));
        // 真 Anthropic 样本：未命中 481 + 写入 50、命中 413 000。
        assert_eq!(split(Resolved::Exclusive, 481, 50, 413_000), (531, 413_000));
        // inclusive 下越界读数（read > input）不产生负数。
        assert_eq!(split(Resolved::Inclusive, 100, 0, 500), (0, 100));
    }
}
