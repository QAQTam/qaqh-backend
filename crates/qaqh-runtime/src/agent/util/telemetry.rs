//! Token-usage telemetry (split from the former util monolith).
//!
//! The only filesystem side effect left in the loop's utility belt; kept behind
//! this module so a future `TelemetryPort` can replace it wholesale.

use super::datetime::chrono_local_date;

/// Append per-turn token usage to `token_stats.jsonl` for dashboard aggregation.
pub(crate) fn record_token_usage(usage: &qaqh_types::UsageInfo, model: &str) {
    use std::io::Write;
    let dir = qaqh_types::platform::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("token_stats.jsonl");
    let today = chrono_local_date();
    let line = serde_json::json!({
        "date": today,
        "prompt_tokens": usage.prompt_tokens,
        "completion_tokens": usage.completion_tokens,
        "cache_hit": usage.prompt_cache_hit_tokens,
        "cache_miss": usage.prompt_cache_miss_tokens,
        "model": model,
    });
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{}", serde_json::to_string(&line).unwrap_or_default());
    }
}
