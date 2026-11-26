//! 请求前缀缓存分叉诊断（只存摘要哈希与长度，零明文留存）。
//!
//! 开关：`QAQH_CACHE_DIFF_LOG`
//! - `0` / `false` / `off`：完全关闭，零追踪、零日志；
//! - `1` / `true` / `all` / `verbose`：第 2 轮起每轮打 `INFO` 诊断日志；
//! - 未设置（默认）：轻量追踪最近会话的前缀指纹，仅在第 2 轮起实测
//!   `cache_reported == true && prompt_cache_hit_tokens == 0` 时打 `WARN`，
//!   指出第一个发生分叉的段（`route` / `tools` / `system` / `message #N`），
//!   或确认所有前缀段逐字节一致（锁定为上游驱逐 / TTL 过期 / 路由副本漂移）。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use qaqh_types::{ContentBlock, Message, ToolDef, UsageInfo};

use crate::types::ProviderConfig;

/// 进程内最多保留的会话指纹条数（每条仅几 KB 哈希，上限 < 512 KB）。
const MAX_TRACKED_SESSIONS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiagMode {
    Off,
    AnomalyOnly,
    Verbose,
}

fn diag_mode() -> DiagMode {
    match std::env::var("QAQH_CACHE_DIFF_LOG") {
        Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "0" | "false" | "off" | "no" => DiagMode::Off,
            "1" | "true" | "all" | "verbose" | "yes" => DiagMode::Verbose,
            _ => DiagMode::AnomalyOnly,
        },
        Err(_) => DiagMode::AnomalyOnly,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Digest {
    hash: u64,
    bytes: usize,
}

impl Digest {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0100_0000_01b3;

    fn new() -> Self {
        Self {
            hash: Self::FNV_OFFSET,
            bytes: 0,
        }
    }

    fn update(&mut self, data: &[u8]) {
        for &byte in data {
            self.hash ^= u64::from(byte);
            self.hash = self.hash.wrapping_mul(Self::FNV_PRIME);
        }
        self.bytes = self.bytes.saturating_add(data.len());
    }

    fn update_tag(&mut self, tag: &str) {
        self.update(tag.as_bytes());
        self.update(&[0xff]);
    }

    fn of_str(text: &str) -> Self {
        let mut d = Self::new();
        d.update(text.as_bytes());
        d
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RouteFingerprint {
    kind: String,
    base_url: String,
    model: String,
    prompt_cache_key: Option<String>,
    effort: Option<String>,
    max_tokens: u32,
    supports_thinking: bool,
    thinking_mode: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolFingerprint {
    name: String,
    digest: Digest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SectionFingerprint {
    heading: String,
    digest: Digest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MessageFingerprint {
    role: String,
    digest: Digest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequestFingerprint {
    route: RouteFingerprint,
    tools: Vec<ToolFingerprint>,
    system_sections: Vec<SectionFingerprint>,
    messages: Vec<MessageFingerprint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PrefixDiff {
    Identical { prior_msgs: usize },
    Diverged(String),
}

struct SessionTrack {
    round: u32,
    seq: u64,
    last: RequestFingerprint,
}

struct Tracker {
    seq: u64,
    sessions: HashMap<String, SessionTrack>,
}

fn tracker() -> &'static Mutex<Tracker> {
    static TRACKER: OnceLock<Mutex<Tracker>> = OnceLock::new();
    TRACKER.get_or_init(|| {
        Mutex::new(Tracker {
            seq: 0,
            sessions: HashMap::new(),
        })
    })
}

pub(crate) struct PendingDiag {
    mode: DiagMode,
    session_key: String,
    fingerprint: RequestFingerprint,
}

/// 在发往 provider 之前采集当前请求的分段指纹。关闭开关或无会话标识时返回 `None`。
pub(crate) fn begin(
    provider: &ProviderConfig,
    messages: &[Message],
    tools: Option<&[ToolDef]>,
    max_tokens: u32,
    effort: Option<&str>,
    user_id: Option<&str>,
) -> Option<PendingDiag> {
    let mode = diag_mode();
    if mode == DiagMode::Off {
        return None;
    }
    let session_key = user_id.filter(|s| !s.is_empty())?.to_string();
    let fingerprint = fingerprint_request(provider, messages, tools, max_tokens, effort);
    Some(PendingDiag {
        mode,
        session_key,
        fingerprint,
    })
}

impl PendingDiag {
    /// 流结束拿到 usage 后对照上一轮指纹并更新基线（日志格式化与输出一律在锁外完成）。
    pub(crate) fn finish(self, usage: Option<&UsageInfo>) {
        let Some(usage) = usage else {
            return;
        };
        let reported = usage.cache_usage_reported == Some(true);
        let hit = usage.prompt_cache_hit_tokens;
        let miss = usage.prompt_cache_miss_tokens;
        let total = hit.saturating_add(miss);
        let log_is_warn = if reported && hit == 0 && total > 0 {
            Some(true)
        } else if self.mode == DiagMode::Verbose {
            Some(false)
        } else {
            None
        };

        let log_payload = {
            let mut guard = tracker()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.seq = guard.seq.wrapping_add(1);
            let seq = guard.seq;
            if let Some(track) = guard.sessions.get_mut(&self.session_key) {
                let round = track.round.saturating_add(1);
                let diff = diff_fingerprints(&track.last, &self.fingerprint);
                track.round = round;
                track.seq = seq;
                track.last = self.fingerprint;
                log_is_warn.map(|is_warn| {
                    (
                        is_warn,
                        round,
                        diff,
                        track.last.tools.len(),
                        track.last.system_sections.len(),
                        track.last.route.kind.clone(),
                        track.last.route.model.clone(),
                        track.last.route.prompt_cache_key.clone(),
                    )
                })
            } else {
                if guard.sessions.len() >= MAX_TRACKED_SESSIONS
                    && let Some(oldest_key) = guard
                        .sessions
                        .iter()
                        .min_by_key(|(_, track)| track.seq)
                        .map(|(k, _)| k.clone())
                {
                    guard.sessions.remove(&oldest_key);
                }
                guard.sessions.insert(
                    self.session_key.clone(),
                    SessionTrack {
                        round: 1,
                        seq,
                        last: self.fingerprint,
                    },
                );
                None
            }
        };

        if let Some((is_warn, round, diff, tools_len, sections_len, kind, model, cache_key)) =
            log_payload
        {
            let verdict = match &diff {
                PrefixDiff::Identical { prior_msgs } => format!(
                    "ALL prefix segments IDENTICAL (tools={tools_len}, system_sections={sections_len}, prior_msgs={prior_msgs}) \
                     -> likely upstream eviction / TTL expiry / backend shard miss",
                ),
                PrefixDiff::Diverged(detail) => format!("FIRST DIVERGENCE at {detail}"),
            };
            if is_warn {
                log::warn!(
                    "[CACHE-DIAG] {} round #{round} cache_hit=0.00% (miss={miss}, wire={kind}, model={model}, key={cache_key:?}): {verdict}",
                    self.session_key,
                );
            } else {
                let pct = if total > 0 {
                    (f64::from(hit) / f64::from(total)) * 100.0
                } else {
                    0.0
                };
                log::info!(
                    "[CACHE-DIAG] {} round #{round} cache_hit={pct:.2}% (hit={hit}, miss={miss}, wire={kind}, model={model}, key={cache_key:?}): {verdict}",
                    self.session_key,
                );
            }
        }
    }
}

pub(crate) fn fingerprint_request(
    provider: &ProviderConfig,
    messages: &[Message],
    tools: Option<&[ToolDef]>,
    max_tokens: u32,
    effort: Option<&str>,
) -> RequestFingerprint {
    let route = RouteFingerprint {
        kind: format!("{:?}", provider.kind),
        base_url: provider.base_url.clone(),
        model: provider.model.clone(),
        prompt_cache_key: provider.prompt_cache_key.clone(),
        effort: effort.map(str::to_string),
        max_tokens,
        supports_thinking: provider.supports_thinking,
        thinking_mode: format!("{:?}", provider.thinking_mode),
    };

    let tool_fps = tools
        .unwrap_or(&[])
        .iter()
        .map(|tool| {
            let mut d = Digest::new();
            d.update_tag(&tool.call_type);
            d.update_tag(&tool.function.name);
            d.update_tag(&tool.function.description);
            if let Ok(schema) = serde_json::to_string(&tool.function.parameters) {
                d.update(schema.as_bytes());
            }
            ToolFingerprint {
                name: tool.function.name.clone(),
                digest: d,
            }
        })
        .collect();

    let mut system_text = String::new();
    let mut msg_fps = Vec::new();
    let mut in_head_system = true;
    for msg in messages {
        if in_head_system && matches!(msg.role.as_str(), "system" | "developer") {
            for block in &msg.content {
                if let ContentBlock::Text { text } = block {
                    if !system_text.is_empty() {
                        system_text.push('\n');
                    }
                    system_text.push_str(text);
                }
            }
            continue;
        }
        in_head_system = false;
        msg_fps.push(fingerprint_message(msg));
    }

    RequestFingerprint {
        route,
        tools: tool_fps,
        system_sections: split_system_sections(&system_text),
        messages: msg_fps,
    }
}

fn split_system_sections(text: &str) -> Vec<SectionFingerprint> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut sections = Vec::new();
    let mut current_heading = "preamble".to_string();
    let mut current_start = 0usize;
    let bytes = text.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if let Some(rel) = text[cursor..].find("\n[") {
            let line_start = cursor + rel + 1;
            let line_end = text[line_start..]
                .find('\n')
                .map_or(bytes.len(), |n| line_start + n);
            let line = &text[line_start..line_end];
            if line.ends_with(']') && line.len() <= 64 {
                sections.push(SectionFingerprint {
                    heading: current_heading,
                    digest: Digest::of_str(&text[current_start..line_start]),
                });
                current_heading = line.to_string();
                current_start = line_start;
            }
            cursor = line_end;
        } else {
            break;
        }
    }
    sections.push(SectionFingerprint {
        heading: current_heading,
        digest: Digest::of_str(&text[current_start..]),
    });
    sections
}

fn fingerprint_message(msg: &Message) -> MessageFingerprint {
    let mut d = Digest::new();
    d.update_tag(&msg.role);
    if let Some(name) = &msg.name {
        d.update_tag(name);
    }
    for block in &msg.content {
        match block {
            ContentBlock::Text { text } => {
                d.update_tag("text");
                d.update(text.as_bytes());
            }
            ContentBlock::Reasoning { reasoning, kind } => {
                d.update_tag("reasoning");
                if let Some(k) = kind {
                    d.update_tag(&format!("{k:?}"));
                }
                d.update(reasoning.as_bytes());
            }
            ContentBlock::ToolUse { id, name, input } => {
                d.update_tag("tool_use");
                d.update_tag(id);
                d.update_tag(name);
                if let Ok(json) = serde_json::to_string(input) {
                    d.update(json.as_bytes());
                }
            }
            ContentBlock::ToolResult {
                tool_use_id,
                result,
            } => {
                d.update_tag("tool_result");
                d.update_tag(tool_use_id);
                d.update_tag(if result.is_success() { "ok" } else { "err" });
                d.update(result.model_text().as_bytes());
            }
            ContentBlock::Image { mime_type, data } => {
                d.update_tag("image");
                d.update_tag(mime_type);
                d.update(data.as_bytes());
            }
            ContentBlock::ImageRef {
                sha256,
                mime_type,
                bytes_len,
            } => {
                d.update_tag("image_ref");
                d.update_tag(sha256);
                d.update_tag(mime_type);
                d.update(&bytes_len.to_le_bytes());
            }
            ContentBlock::ResponseOutputItem { item } => {
                d.update_tag("response_output_item");
                if let Ok(json) = serde_json::to_string(item) {
                    d.update(json.as_bytes());
                }
            }
            ContentBlock::WebSearchCall { id, action } => {
                d.update_tag("web_search_call");
                d.update_tag(id);
                if let Ok(json) = serde_json::to_string(action) {
                    d.update(json.as_bytes());
                }
            }
        }
    }
    MessageFingerprint {
        role: msg.role.clone(),
        digest: d,
    }
}

pub(crate) fn diff_fingerprints(
    prev: &RequestFingerprint,
    curr: &RequestFingerprint,
) -> PrefixDiff {
    if prev.route != curr.route {
        let mut changed = Vec::new();
        if prev.route.kind != curr.route.kind {
            changed.push(format!("wire({}->{})", prev.route.kind, curr.route.kind));
        }
        if prev.route.base_url != curr.route.base_url {
            changed.push("base_url".to_string());
        }
        if prev.route.model != curr.route.model {
            changed.push(format!(
                "model({}->{})",
                prev.route.model, curr.route.model
            ));
        }
        if prev.route.prompt_cache_key != curr.route.prompt_cache_key {
            changed.push(format!(
                "prompt_cache_key({:?}->{:?})",
                prev.route.prompt_cache_key, curr.route.prompt_cache_key
            ));
        }
        if prev.route.effort != curr.route.effort {
            changed.push(format!(
                "effort({:?}->{:?})",
                prev.route.effort, curr.route.effort
            ));
        }
        if prev.route.max_tokens != curr.route.max_tokens {
            changed.push(format!(
                "max_tokens({}->{})",
                prev.route.max_tokens, curr.route.max_tokens
            ));
        }
        if prev.route.supports_thinking != curr.route.supports_thinking
            || prev.route.thinking_mode != curr.route.thinking_mode
        {
            changed.push("thinking_config".to_string());
        }
        return PrefixDiff::Diverged(format!("route [{}]", changed.join(", ")));
    }

    if prev.tools != curr.tools {
        let prev_names: Vec<&str> = prev.tools.iter().map(|t| t.name.as_str()).collect();
        let curr_names: Vec<&str> = curr.tools.iter().map(|t| t.name.as_str()).collect();
        let added: Vec<&str> = curr_names
            .iter()
            .copied()
            .filter(|n| !prev_names.contains(n))
            .collect();
        let removed: Vec<&str> = prev_names
            .iter()
            .copied()
            .filter(|n| !curr_names.contains(n))
            .collect();
        if !added.is_empty() || !removed.is_empty() {
            return PrefixDiff::Diverged(format!(
                "tools (count {} -> {}, added={added:?}, removed={removed:?})",
                prev.tools.len(),
                curr.tools.len()
            ));
        }
        for (idx, (a, b)) in prev.tools.iter().zip(curr.tools.iter()).enumerate() {
            if a.name != b.name {
                return PrefixDiff::Diverged(format!(
                    "tools reordered at #{idx} ({} -> {})",
                    a.name, b.name
                ));
            }
            if a.digest != b.digest {
                return PrefixDiff::Diverged(format!(
                    "tool schema/desc changed at #{idx} `{}` ({}B -> {}B)",
                    a.name, a.digest.bytes, b.digest.bytes
                ));
            }
        }
        return PrefixDiff::Diverged(format!(
            "tools changed (count {} -> {})",
            prev.tools.len(),
            curr.tools.len()
        ));
    }

    if prev.system_sections != curr.system_sections {
        for (idx, (a, b)) in prev
            .system_sections
            .iter()
            .zip(curr.system_sections.iter())
            .enumerate()
        {
            if a.heading != b.heading {
                return PrefixDiff::Diverged(format!(
                    "system section #{idx} heading ({:?} -> {:?})",
                    a.heading, b.heading
                ));
            }
            if a.digest != b.digest {
                return PrefixDiff::Diverged(format!(
                    "system section {:?} ({}B -> {}B)",
                    a.heading, a.digest.bytes, b.digest.bytes
                ));
            }
        }
        return PrefixDiff::Diverged(format!(
            "system section count ({} -> {})",
            prev.system_sections.len(),
            curr.system_sections.len()
        ));
    }

    if curr.messages.len() < prev.messages.len() {
        return PrefixDiff::Diverged(format!(
            "messages shrunk ({} -> {})",
            prev.messages.len(),
            curr.messages.len()
        ));
    }
    for (idx, (a, b)) in prev.messages.iter().zip(curr.messages.iter()).enumerate() {
        if a != b {
            return PrefixDiff::Diverged(format!(
                "message #{idx} (role {}->{}, {}B -> {}B)",
                a.role, b.role, a.digest.bytes, b.digest.bytes
            ));
        }
    }

    PrefixDiff::Identical {
        prior_msgs: prev.messages.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_types::ToolFunction;

    fn sample_provider() -> ProviderConfig {
        let mut p = ProviderConfig::responses("https://api.example.com", "k", "m1", None);
        p.prompt_cache_key = Some("ses-1".into());
        p
    }

    fn sample_tool(name: &str, desc: &str) -> ToolDef {
        ToolDef {
            call_type: "function".into(),
            function: ToolFunction {
                name: name.into(),
                description: desc.into(),
                parameters: serde_json::json!({"type": "object"}),
            },
        }
    }

    fn sys_msg(text: &str) -> Message {
        Message {
            msg_id: None,
            role: "system".into(),
            name: None,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    #[test]
    fn identical_prefix_reports_identical_even_when_new_messages_appended() {
        let p = sample_provider();
        let tools = vec![sample_tool("read", "read file")];
        let m1 = vec![
            sys_msg("intro\n[Environment]\nworkspace: /a\n[Skills]\ns1"),
            Message::user("hi"),
        ];
        let m2 = vec![
            sys_msg("intro\n[Environment]\nworkspace: /a\n[Skills]\ns1"),
            Message::user("hi"),
            Message {
                msg_id: None,
                role: "assistant".into(),
                name: None,
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
            },
            Message::user("next"),
        ];
        let fp1 = fingerprint_request(&p, &m1, Some(&tools), 1024, Some("high"));
        let fp2 = fingerprint_request(&p, &m2, Some(&tools), 1024, Some("high"));
        assert_eq!(
            diff_fingerprints(&fp1, &fp2),
            PrefixDiff::Identical { prior_msgs: 1 }
        );
    }

    #[test]
    fn detects_route_tool_system_and_message_divergences_precisely() {
        let p = sample_provider();
        let tools = vec![sample_tool("read", "read file")];
        let base_msgs = vec![
            sys_msg("intro\n[Environment]\nworkspace: /a\n[Project instructions]\nrule 1"),
            Message::user("turn 1"),
        ];
        let fp_base = fingerprint_request(&p, &base_msgs, Some(&tools), 1024, Some("high"));

        // 1. Route (prompt_cache_key / effort) changed
        let mut p_key = p.clone();
        p_key.prompt_cache_key = None;
        let fp_route = fingerprint_request(&p_key, &base_msgs, Some(&tools), 1024, Some("high"));
        match diff_fingerprints(&fp_base, &fp_route) {
            PrefixDiff::Diverged(detail) => assert!(detail.contains("prompt_cache_key"), "{detail}"),
            other => panic!("expected route divergence, got {other:?}"),
        }

        // 2. Tools added
        let tools_more = vec![
            sample_tool("read", "read file"),
            sample_tool("mcp__git__status", "status"),
        ];
        let fp_tools = fingerprint_request(&p, &base_msgs, Some(&tools_more), 1024, Some("high"));
        match diff_fingerprints(&fp_base, &fp_tools) {
            PrefixDiff::Diverged(detail) => {
                assert!(detail.contains("mcp__git__status"), "{detail}");
            }
            other => panic!("expected tools divergence, got {other:?}"),
        }

        // 3. System section mutated ([Project instructions])
        let msgs_sys = vec![
            sys_msg("intro\n[Environment]\nworkspace: /a\n[Project instructions]\nrule 1 updated"),
            Message::user("turn 1"),
        ];
        let fp_sys = fingerprint_request(&p, &msgs_sys, Some(&tools), 1024, Some("high"));
        match diff_fingerprints(&fp_base, &fp_sys) {
            PrefixDiff::Diverged(detail) => {
                assert!(detail.contains("[Project instructions]"), "{detail}");
            }
            other => panic!("expected system section divergence, got {other:?}"),
        }

        // 4. Historical message mutated (e.g. opaque item appeared on turn 2)
        let msgs_mut = vec![
            sys_msg("intro\n[Environment]\nworkspace: /a\n[Project instructions]\nrule 1"),
            Message::user("turn 1 mutated"),
        ];
        let fp_msg = fingerprint_request(&p, &msgs_mut, Some(&tools), 1024, Some("high"));
        match diff_fingerprints(&fp_base, &fp_msg) {
            PrefixDiff::Diverged(detail) => assert!(detail.contains("message #0"), "{detail}"),
            other => panic!("expected message divergence, got {other:?}"),
        }
    }
}
