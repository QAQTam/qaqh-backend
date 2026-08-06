//! Rust 侧会话列表投影 — XAML 侧栏（sidebar.rs）的唯一数据源。
//!
//! 镜像前端 `TaskSidebar` / `sessionRegistry` 的会话列表部分：
//!   - `title` = `last_summary.trim()` || `seed[..8]`（等价 `taskTitle()`，
//!     但 dashboardTitle 在 XAML 侧暂缺，先用 last_summary 兜底）；
//!   - `state` = activities[seed].state ?? (running ? Starting : Idle)。
//!
//! 与 `bridge.rs` 一致的风格：直接解析 `serde_json::Value`，不引入 deepx-proto
//! 依赖。纯函数，便于单测（feed daemon `session.list` / `session.activity`
//! 的真实 fixture）。

use serde_json::Value;

/// 会话活动状态（镜像 TS `SessionActivityState`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityState {
    Starting,
    Idle,
    Working,
    WaitingUser,
    Disconnected,
}

impl ActivityState {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "starting" => Some(Self::Starting),
            "idle" => Some(Self::Idle),
            "working" => Some(Self::Working),
            "waiting_user" => Some(Self::WaitingUser),
            "disconnected" => Some(Self::Disconnected),
            _ => None,
        }
    }

    /// 序列化形态（日志、无障碍标签、事件载荷）。
    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Idle => "idle",
            Self::Working => "working",
            Self::WaitingUser => "waiting_user",
            Self::Disconnected => "disconnected",
        }
    }
}

/// XAML 侧栏的一行会话。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionItem {
    pub seed: String,
    pub title: String,
    pub state: ActivityState,
    pub running: bool,
    pub updated_at: u64,
}

/// 从 daemon `session.list` 查询结果的一个元素投影一行。
///
/// 缺失/畸形字段按前端 `SessionMeta` 默认值兜底（`last_summary` 空 → 前缀 seed），
/// 返回 `None` 仅当连 seed 都没有（该元素不可用）。
pub fn project_session_meta(
    v: &Value,
    activity: Option<ActivityState>,
    running: bool,
) -> Option<SessionItem> {
    let seed = v.get("seed")?.as_str()?.to_string();
    let last_summary = v
        .get("last_summary")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let title = if last_summary.is_empty() {
        seed.chars().take(8).collect()
    } else {
        last_summary
    };
    let updated_at = v.get("updated_at").and_then(|u| u.as_u64()).unwrap_or(0);
    let state = activity.unwrap_or(if running {
        ActivityState::Starting
    } else {
        ActivityState::Idle
    });
    Some(SessionItem {
        seed,
        title,
        state,
        running,
        updated_at,
    })
}

/// 解析 daemon `session.activity` 查询结果（Value::Array）→ (seed, state) 列表。
///
/// 与前端 `parseSessionActivity` 等价：state 不在合法集合内的条目被丢弃。
pub fn parse_activities(v: &Value) -> Vec<(String, ActivityState)> {
    let Some(items) = v.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let seed = item.get("seed")?.as_str()?.to_string();
            let state = item
                .get("state")
                .and_then(|s| s.as_str())
                .and_then(ActivityState::parse)?;
            Some((seed, state))
        })
        .collect()
}

/// 从 control 频道 `session_activity_changed` 事件载荷提取 (seed, state)。
///
/// 事件形状（与前端 `ringingStores.ts` 的 `session_activity_changed` case 一致）：
/// `{ type: "session_activity_changed", seed, state, ... }`。
pub fn parse_activity_event(event: &Value) -> Option<(String, ActivityState)> {
    let seed = event.get("seed")?.as_str()?.to_string();
    let state = event.get("state")?.as_str().and_then(ActivityState::parse)?;
    Some((seed, state))
}

// ── XAML 技能页投影（skills_view.rs 的唯一数据源）──────────────────

/// 技能运行时条目（`skills_updated` 事件 runtime[] 元素 / bootstrap control.skills）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRuntimeItem {
    pub name: String,
    pub description: String,
    /// 生命周期状态：catalog | requested | active | unavailable。
    pub state: String,
    pub source: String,
    pub token_count: u64,
    pub error: Option<String>,
}

/// 技能目录条目（事件 available[] 元素）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCatalogItem {
    pub name: String,
    pub description: String,
    /// project | user。
    pub scope: String,
    pub source: String,
}

/// XAML 技能页数据投影——对齐 daemon `SkillsStatus`（snake_case JSON）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillsSnapshot {
    pub seed: String,
    pub available: Vec<SkillCatalogItem>,
    pub active: Vec<String>,
    pub runtime: Vec<SkillRuntimeItem>,
    pub catalog_revision: String,
    pub context_epoch: u64,
    pub operation_revision: u64,
    pub token_budget: u64,
    pub token_usage: u64,
    pub diagnostics: Vec<String>,
}

/// 解析任意 skills 状态 JSON（事件 payload 或 bootstrap 快照 control.skills）。
///
/// 两种来源同构（deepx-domain `SkillsStatus`，snake_case）：缺失字段按默认值
/// 兜底，不因字段缺失丢弃整份快照（事件可能省略部分可选字段）。
pub fn parse_skills_payload(v: &Value) -> SkillsSnapshot {
    let arr_str = |key: &str| -> Vec<String> {
        v.get(key)
            .and_then(|x| x.as_array())
            .map(|arr| arr.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    SkillsSnapshot {
        seed: String::new(), // 调用方填（batch.seed / active_seed）
        available: v
            .get("available")
            .and_then(|x| x.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| {
                        Some(SkillCatalogItem {
                            name: item.get("name")?.as_str()?.to_string(),
                            description: item
                                .get("description")
                                .and_then(|d| d.as_str())
                                .unwrap_or("")
                                .to_string(),
                            scope: item
                                .get("scope")
                                .and_then(|s| s.as_str())
                                .unwrap_or("project")
                                .to_string(),
                            source: item
                                .get("source")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        active: arr_str("active"),
        runtime: v
            .get("runtime")
            .and_then(|x| x.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| {
                        Some(SkillRuntimeItem {
                            name: item.get("name")?.as_str()?.to_string(),
                            description: item
                                .get("description")
                                .and_then(|d| d.as_str())
                                .unwrap_or("")
                                .to_string(),
                            state: item
                                .get("state")
                                .and_then(|s| s.as_str())
                                .unwrap_or("catalog")
                                .to_string(),
                            source: item
                                .get("source")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string(),
                            token_count: item.get("token_count").and_then(|t| t.as_u64()).unwrap_or(0),
                            error: item.get("error").and_then(|e| e.as_str()).map(str::to_string),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        catalog_revision: v
            .get("catalog_revision")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        context_epoch: v.get("context_epoch").and_then(|x| x.as_u64()).unwrap_or(0),
        operation_revision: v.get("operation_revision").and_then(|x| x.as_u64()).unwrap_or(0),
        token_budget: v.get("token_budget").and_then(|x| x.as_u64()).unwrap_or(0),
        token_usage: v.get("token_usage").and_then(|x| x.as_u64()).unwrap_or(0),
        diagnostics: arr_str("diagnostics"),
    }
}

/// 从 control 频道 `skills_updated` 事件提取完整快照。
///
/// 事件形状（deepx-domain `ControlEvent::SkillsUpdated`，`tag="type"` +
/// snake_case）：`{ type: "skills_updated", available, active,
/// catalog_revision?, operation_revision?, context_epoch, token_budget,
/// token_usage, runtime, diagnostics }`。`type` 不符返回 None。
pub fn parse_skills_event(event: &Value) -> Option<SkillsSnapshot> {
    if event.get("type")?.as_str()? != "skills_updated" {
        return None;
    }
    Some(parse_skills_payload(event))
}

// ── XAML 设置页投影（settings_view.rs 的唯一数据源）────────────────

/// 单个 provider 的 endpoint（config.load `providers[].endpoints[]`）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderEndpoint {
    pub id: String,
    pub display: String,
    pub base_url: String,
    pub default_model: String,
    pub models: Vec<String>,
}

/// provider 目录条目（config.load `providers[]`）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderInfo {
    pub id: String,
    pub display: String,
    pub endpoints: Vec<ProviderEndpoint>,
}

/// XAML 设置页数据投影——对齐 daemon `config.load` 返回（snake_case）
/// + `skills.list_tools` + `workspace.status` 合并。
///
/// 缺失字段全部兜底（config 可能省略未配置项），不因字段缺失丢整份快照。
/// `*_configured` 由 `api_key == "****"`（daemon 掩码约定）派生：掩码 =
/// 已配置且值不落回 UI（与 Web `SettingsView` 的 isMasked 判定一致）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SettingsSnapshot {
    // ── models / api / context ──
    pub api_key: String,
    pub api_key_configured: bool,
    pub model: String,
    pub base_url: String,
    pub provider_id: String,
    pub endpoint: String,
    pub max_tokens: u64,
    pub context_limit: u64,
    pub reasoning_effort: String,
    pub auto_compact_threshold: f64,
    pub compliance_enabled: bool,
    // ── subagent ──
    pub sub_model: String,
    pub sub_base_url: String,
    pub sub_api_key: String,
    pub sub_api_key_configured: bool,
    pub sub_max_tokens: u64,
    pub sub_timeout_secs: u64,
    pub sub_tools: Vec<String>,
    // ── multimodal ──
    pub mm_enabled: bool,
    pub mm_provider_type: String,
    pub mm_api_key: String,
    pub mm_api_key_configured: bool,
    pub mm_base_url: String,
    pub mm_model: String,
    pub mm_max_tokens: u64,
    // ── workspace 运行环境 ──
    pub workspace_mode: String,
    pub workspace_configured_mode: String,
    pub workspace_active_mode: String,
    pub workspace_endpoint: String,
    // ── 杂项 ──
    pub tokenizer_path: String,
    pub lang: String,
    pub permission_level: u64,
    /// config.load `providers[]`（provider/endpoint 联动选择）。
    pub providers: Vec<ProviderInfo>,
    /// `skills.list_tools` 返回（subagent 工具勾选列表）。
    pub tools: Vec<String>,
}

// ── Info 面板会话用量投影（info_panel.rs 的唯一数据源）──────────────

/// 单次请求/会话累计用量（对齐 daemon `UsageInfo` / Web 侧同名字段）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageInfo {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub prompt_cache_hit_tokens: u64,
    pub prompt_cache_miss_tokens: u64,
    pub cache_usage_reported: bool,
}

/// XAML Info 面板数据（bootstrap `conversation.state` 投影）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionDetail {
    pub model: String,
    pub context_limit: u64,
    /// 当前（最近一次）请求用量。
    pub usage: UsageInfo,
    /// 会话累计用量。
    pub totals: UsageInfo,
    pub usage_requests: u64,
    pub cache_reported_requests: u64,
}

/// 解析 UsageInfo 对象（字段缺失兜底 0/false——与 Web `addUsage` 语义一致）。
fn parse_usage(v: &Value) -> UsageInfo {
    let u64_of = |key: &str| v.get(key).and_then(|x| x.as_u64()).unwrap_or(0);
    UsageInfo {
        prompt_tokens: u64_of("prompt_tokens"),
        completion_tokens: u64_of("completion_tokens"),
        reasoning_tokens: u64_of("reasoning_tokens"),
        total_tokens: u64_of("total_tokens"),
        prompt_cache_hit_tokens: u64_of("prompt_cache_hit_tokens"),
        prompt_cache_miss_tokens: u64_of("prompt_cache_miss_tokens"),
        cache_usage_reported: v
            .get("cache_usage_reported")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
    }
}

/// 解析 bootstrap `conversation.state` → SessionDetail。
///
/// 形状（daemon conversation_snapshot.rs:29-39）：`{ usage, usage_totals,
/// usage_requests, cache_reported_requests, model, context_limit, ... }`。
/// 快照为 None（会话无持久状态）时由调用方保留旧缓存。
pub fn parse_conversation_state(v: &Value) -> SessionDetail {
    SessionDetail {
        model: v
            .get("model")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        context_limit: v
            .get("context_limit")
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
        usage: v.get("usage").map(parse_usage).unwrap_or_default(),
        totals: v.get("usage_totals").map(parse_usage).unwrap_or_default(),
        usage_requests: v
            .get("usage_requests")
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
        cache_reported_requests: v
            .get("cache_reported_requests")
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
    }
}

/// 掩码判定：daemon 对已配置的 secret 返回 `"****"`（与 Web isMasked 一致）。
fn is_masked(v: &str) -> bool {
    v == "****"
}

/// 解析 `config.load` 查询结果（Value::Object）→ SettingsSnapshot。
///
/// providers 形状：`[{id, display, endpoints: [{id, display, base_url,
/// default_model, models: [...]}]}]`；子对象 subagent/multimodal/workspace
/// 同理 snake_case。`lang`/`permission_level` 顶层返回（App.tsx L659 同源）。
pub fn parse_config_load(v: &Value) -> SettingsSnapshot {
    let str_of = |key: &str| -> String {
        v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
    };
    let u64_of = |key: &str| -> u64 {
        v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
    };
    let f64_of = |key: &str| -> f64 {
        v.get(key).and_then(|x| x.as_f64()).unwrap_or(0.0)
    };
    let bool_of = |key: &str| -> bool {
        v.get(key).and_then(|x| x.as_bool()).unwrap_or(false)
    };
    let sub = v.get("subagent");
    let sub_of = |key: &str| -> String {
        sub.and_then(|s| s.get(key))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()
    };
    let sub_u64 = |key: &str| -> u64 {
        sub.and_then(|s| s.get(key)).and_then(|x| x.as_u64()).unwrap_or(0)
    };
    let mm = v.get("multimodal");
    let mm_of = |key: &str| -> String {
        mm.and_then(|s| s.get(key))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()
    };
    let mm_u64 = |key: &str| -> u64 {
        mm.and_then(|s| s.get(key)).and_then(|x| x.as_u64()).unwrap_or(0)
    };
    let ws = v.get("workspace");

    let api_key = str_of("api_key");
    let sub_api_key = sub_of("api_key");
    let mm_api_key = mm_of("api_key");
    SettingsSnapshot {
        api_key: if is_masked(&api_key) { String::new() } else { api_key.clone() },
        api_key_configured: is_masked(&api_key),
        model: str_of("model"),
        base_url: str_of("base_url"),
        provider_id: str_of("provider_id"),
        endpoint: str_of("endpoint"),
        max_tokens: u64_of("max_tokens"),
        context_limit: u64_of("context_limit"),
        reasoning_effort: str_of("reasoning_effort"),
        auto_compact_threshold: f64_of("auto_compact_threshold"),
        compliance_enabled: bool_of("compliance_enabled"),
        sub_model: sub_of("model"),
        sub_base_url: sub_of("base_url"),
        sub_api_key: if is_masked(&sub_api_key) { String::new() } else { sub_api_key.clone() },
        sub_api_key_configured: is_masked(&sub_api_key),
        sub_max_tokens: sub_u64("max_tokens"),
        sub_timeout_secs: sub_u64("timeout_secs"),
        sub_tools: sub
            .and_then(|s| s.get("default_tools"))
            .and_then(|x| x.as_array())
            .map(|arr| arr.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default(),
        mm_enabled: mm.and_then(|s| s.get("enabled")).and_then(|x| x.as_bool()).unwrap_or(false),
        mm_provider_type: mm_of("provider_type"),
        mm_api_key: if is_masked(&mm_api_key) { String::new() } else { mm_api_key.clone() },
        mm_api_key_configured: is_masked(&mm_api_key),
        mm_base_url: mm_of("base_url"),
        mm_model: mm_of("model"),
        mm_max_tokens: mm_u64("max_tokens"),
        workspace_mode: ws
            .and_then(|s| s.get("mode"))
            .and_then(|x| x.as_str())
            .unwrap_or("local")
            .to_string(),
        workspace_configured_mode: String::new(),
        workspace_active_mode: String::new(),
        workspace_endpoint: String::new(),
        tokenizer_path: str_of("tokenizer_path"),
        lang: str_of("lang"),
        permission_level: u64_of("permission_level"),
        providers: v
            .get("providers")
            .and_then(|x| x.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| {
                        Some(ProviderInfo {
                            id: p.get("id")?.as_str()?.to_string(),
                            display: p.get("display")?.as_str()?.to_string(),
                            endpoints: p
                                .get("endpoints")
                                .and_then(|e| e.as_array())
                                .map(|eps| {
                                    eps.iter()
                                        .filter_map(|ep| {
                                            Some(ProviderEndpoint {
                                                id: ep.get("id")?.as_str()?.to_string(),
                                                display: ep.get("display")?.as_str()?.to_string(),
                                                base_url: ep
                                                    .get("base_url")
                                                    .and_then(|b| b.as_str())
                                                    .unwrap_or("")
                                                    .to_string(),
                                                default_model: ep
                                                    .get("default_model")
                                                    .and_then(|m| m.as_str())
                                                    .unwrap_or("")
                                                    .to_string(),
                                                models: ep
                                                    .get("models")
                                                    .and_then(|m| m.as_array())
                                                    .map(|ms| {
                                                        ms.iter()
                                                            .filter_map(|m| m.as_str().map(str::to_string))
                                                            .collect()
                                                    })
                                                    .unwrap_or_default(),
                                            })
                                        })
                                        .collect()
                                })
                                .unwrap_or_default(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        tools: Vec::new(),
    }
}

/// 解析 `skills.list_tools` 查询结果（Value::Array of string）。
pub fn parse_tools(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|arr| arr.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// 解析 `workspace.status` 查询结果 → (configured_mode, active_mode, endpoint)。
pub fn parse_workspace_status(v: &Value) -> (String, String, String) {
    (
        v.get("configured_mode").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        v.get("active_mode").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        v.get("endpoint").and_then(|x| x.as_str()).unwrap_or("").to_string(),
    )
}

/// 归一化 reasoning effort（对齐 Web `normalizeEffort`：off 值归到 low）。
pub fn normalize_effort(effort: &str) -> &str {
    match effort {
        "none" | "minimal" | "disable" | "disabled" | "off" | "" => "low",
        "low" | "medium" | "high" | "xhigh" | "max" => effort,
        _ => effort,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn projects_meta_with_last_summary() {
        let meta = json!({
            "seed": "abcd1234",
            "last_summary": "修复登录流程",
            "updated_at": 1720000000,
        });
        let item = project_session_meta(&meta, None, false).expect("project");
        assert_eq!(item.seed, "abcd1234");
        assert_eq!(item.title, "修复登录流程");
        assert_eq!(item.state, ActivityState::Idle);
        assert!(!item.running);
    }

    #[test]
    fn falls_back_to_seed_prefix_when_no_summary() {
        let meta = json!({
            "seed": "abcd1234",
            "last_summary": "",
            "updated_at": 0,
        });
        let item = project_session_meta(&meta, None, true).expect("project");
        assert_eq!(item.title, "abcd1234");
        assert_eq!(item.state, ActivityState::Starting);
        assert!(item.running);
    }

    #[test]
    fn trims_whitespace_summary() {
        let meta = json!({ "seed": "s1", "last_summary": "  \t " });
        let item = project_session_meta(&meta, None, false).expect("project");
        assert_eq!(item.title, "s1");
    }

    #[test]
    fn activity_overrides_default_state() {
        let meta = json!({ "seed": "s1", "last_summary": "t" });
        let item =
            project_session_meta(&meta, Some(ActivityState::Working), false).expect("project");
        assert_eq!(item.state, ActivityState::Working);
    }

    #[test]
    fn rejects_meta_without_seed() {
        assert!(project_session_meta(&json!({ "last_summary": "x" }), None, false).is_none());
    }

    #[test]
    fn parses_activity_array() {
        let v = json!([
            { "seed": "s1", "state": "working", "seq": 3, "updated_at": 1 },
            { "seed": "s2", "state": "idle", "seq": 1, "updated_at": 2 },
            { "seed": "s3", "state": "bogus", "seq": 1, "updated_at": 3 },
            { "seed": "s4" },
        ]);
        let parsed = parse_activities(&v);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0], ("s1".to_string(), ActivityState::Working));
        assert_eq!(parsed[1], ("s2".to_string(), ActivityState::Idle));
    }

    #[test]
    fn parses_activity_event_payload() {
        let event = json!({
            "type": "session_activity_changed",
            "channel": "control",
            "seed": "s1",
            "state": "waiting_user",
            "turn_id": "t7",
            "seq": 4,
            "updated_at": 5,
        });
        let (seed, state) = parse_activity_event(&event).expect("event");
        assert_eq!(seed, "s1");
        assert_eq!(state, ActivityState::WaitingUser);
    }

    #[test]
    fn activity_state_roundtrip() {
        for s in ["starting", "idle", "working", "waiting_user", "disconnected"] {
            let state = ActivityState::parse(s).unwrap_or_else(|| panic!("{s}"));
            assert_eq!(state.as_str(), s);
        }
        assert!(ActivityState::parse("unknown").is_none());
    }

    #[test]
    fn skills_event_full_payload() {
        // 对齐 daemon ControlEvent::SkillsUpdated 序列化（snake_case）。
        let event = json!({
            "type": "skills_updated",
            "available": [
                { "name": "find-docs", "description": "查文档", "scope": "project", "source": "skills/find-docs" },
                { "name": "frontend-design", "description": "视觉设计", "scope": "user", "source": "user/skills/frontend-design" },
            ],
            "active": ["find-docs"],
            "catalog_revision": "abc123def456",
            "operation_revision": 7,
            "context_epoch": 3,
            "token_budget": 100000,
            "token_usage": 12345,
            "runtime": [
                { "name": "find-docs", "description": "查文档", "state": "active", "source": "skills/find-docs", "token_count": 512 },
                { "name": "todo", "description": "任务", "state": "catalog", "source": "skills/todo", "token_count": 0 },
            ],
            "diagnostics": ["skills/broken: parse error"],
        });
        let snap = parse_skills_event(&event).expect("event");
        assert_eq!(snap.available.len(), 2);
        assert_eq!(snap.available[0].name, "find-docs");
        assert_eq!(snap.available[0].scope, "project");
        assert_eq!(snap.available[1].scope, "user");
        assert_eq!(snap.active, vec!["find-docs"]);
        assert_eq!(snap.catalog_revision, "abc123def456");
        assert_eq!(snap.operation_revision, 7);
        assert_eq!(snap.context_epoch, 3);
        assert_eq!(snap.token_budget, 100000);
        assert_eq!(snap.token_usage, 12345);
        assert_eq!(snap.runtime.len(), 2);
        assert_eq!(snap.runtime[0].state, "active");
        assert_eq!(snap.runtime[0].token_count, 512);
        assert_eq!(snap.diagnostics, vec!["skills/broken: parse error"]);
    }

    #[test]
    fn skills_event_wrong_type_is_none() {
        assert!(parse_skills_event(&json!({ "type": "session_activity_changed" })).is_none());
        assert!(parse_skills_event(&json!({})).is_none());
    }

    #[test]
    fn skills_payload_tolerates_missing_fields() {
        // bootstrap 快照 control.skills 可能省略可选字段——全部兜底。
        let snap = parse_skills_payload(&json!({
            "available": [{ "name": "a", "description": "d" }],
        }));
        assert_eq!(snap.available.len(), 1);
        assert_eq!(snap.available[0].scope, "project");
        assert_eq!(snap.available[0].source, "");
        assert!(snap.active.is_empty());
        assert!(snap.runtime.is_empty());
        assert_eq!(snap.catalog_revision, "");
        assert_eq!(snap.context_epoch, 0);
        assert_eq!(snap.operation_revision, 0);
        assert_eq!(snap.token_budget, 0);
        assert_eq!(snap.token_usage, 0);
        assert!(snap.diagnostics.is_empty());
    }

    #[test]
    fn skills_payload_from_bootstrap_snapshot() {
        // bootstrap 快照的 control.skills 没有 type 字段——直接解析 payload。
        let control = json!({
            "skills": {
                "available": [{ "name": "solidjs-v2", "description": "Solid 2", "scope": "project", "source": "skills/solidjs-v2" }],
                "active": [],
                "runtime": [{ "name": "solidjs-v2", "description": "Solid 2", "state": "catalog", "source": "skills/solidjs-v2", "token_count": 42 }],
                "catalog_revision": "rev-1",
                "context_epoch": 1,
                "operation_revision": 2,
                "token_budget": 50000,
                "token_usage": 10,
                "diagnostics": [],
            }
        });
        let snap = parse_skills_payload(control.get("skills").expect("skills"));
        assert_eq!(snap.available[0].name, "solidjs-v2");
        assert_eq!(snap.runtime[0].token_count, 42);
        assert_eq!(snap.operation_revision, 2);
    }

    #[test]
    fn parses_config_load_full() {
        // 对齐 daemon config.load 序列化（snake_case + 掩码约定）。
        let v = json!({
            "api_key": "****",
            "model": "deepseek-chat",
            "base_url": "https://api.deepseek.com/v1",
            "provider_id": "deepseek",
            "endpoint": "openai",
            "max_tokens": 16384,
            "context_limit": 1000000,
            "reasoning_effort": "high",
            "auto_compact_threshold": 0.75,
            "compliance_enabled": true,
            "lang": "zh",
            "permission_level": 3,
            "tokenizer_path": "C:/tok.json",
            "subagent": {
                "model": "deepseek-reasoner",
                "base_url": "https://api.deepseek.com/v1",
                "api_key": "****",
                "max_tokens": 4096,
                "timeout_secs": 120,
                "default_tools": ["read_file", "grep"],
            },
            "multimodal": {
                "enabled": true,
                "provider_type": "mimo",
                "api_key": "real-key",
                "base_url": "https://mm.example.com",
                "model": "mimo-v2.5",
                "max_tokens": 4096,
            },
            "workspace": { "mode": "wsl" },
            "providers": [
                {
                    "id": "deepseek",
                    "display": "DeepSeek",
                    "endpoints": [{
                        "id": "openai",
                        "display": "OpenAI 兼容",
                        "base_url": "https://api.deepseek.com/v1",
                        "default_model": "deepseek-chat",
                        "models": ["deepseek-chat", "deepseek-reasoner"],
                    }],
                }
            ],
        });
        let snap = parse_config_load(&v);
        assert!(snap.api_key_configured);
        assert_eq!(snap.api_key, "");
        assert_eq!(snap.model, "deepseek-chat");
        assert_eq!(snap.provider_id, "deepseek");
        assert_eq!(snap.max_tokens, 16384);
        assert!((snap.auto_compact_threshold - 0.75).abs() < 1e-9);
        assert!(snap.compliance_enabled);
        assert_eq!(snap.lang, "zh");
        assert_eq!(snap.permission_level, 3);
        assert_eq!(snap.tokenizer_path, "C:/tok.json");
        assert!(snap.sub_api_key_configured);
        assert_eq!(snap.sub_api_key, "");
        assert_eq!(snap.sub_model, "deepseek-reasoner");
        assert_eq!(snap.sub_tools, vec!["read_file", "grep"]);
        assert!(snap.mm_enabled);
        assert_eq!(snap.mm_api_key, "real-key"); // 未掩码 → 原样（可回显）
        assert!(!snap.mm_api_key_configured);
        assert_eq!(snap.mm_model, "mimo-v2.5");
        assert_eq!(snap.workspace_mode, "wsl");
        assert_eq!(snap.providers.len(), 1);
        assert_eq!(snap.providers[0].id, "deepseek");
        assert_eq!(snap.providers[0].endpoints[0].models, vec!["deepseek-chat", "deepseek-reasoner"]);
    }

    #[test]
    fn config_load_tolerates_missing_fields() {
        let snap = parse_config_load(&json!({ "providers": [] }));
        assert!(!snap.api_key_configured);
        assert_eq!(snap.model, "");
        assert_eq!(snap.workspace_mode, "local"); // workspace 缺省 local
        assert_eq!(snap.permission_level, 0);
        assert!(snap.providers.is_empty());
        assert!(snap.sub_tools.is_empty());
        assert!(!snap.mm_enabled);
    }

    #[test]
    fn parses_tools_and_workspace_status() {
        let tools = parse_tools(&json!(["read_file", "grep", "exec"]));
        assert_eq!(tools, vec!["read_file", "grep", "exec"]);
        assert!(parse_tools(&json!({})).is_empty());
        let (cfg, active, ep) =
            parse_workspace_status(&json!({ "configured_mode": "local", "active_mode": "local", "endpoint": "wsl://ubuntu" }));
        assert_eq!(cfg, "local");
        assert_eq!(active, "local");
        assert_eq!(ep, "wsl://ubuntu");
    }

    #[test]
    fn normalizes_effort_off_values() {
        for off in ["none", "minimal", "disable", "disabled", "off", ""] {
            assert_eq!(normalize_effort(off), "low");
        }
        assert_eq!(normalize_effort("high"), "high");
        assert_eq!(normalize_effort("max"), "max");
    }

    #[test]
    fn parses_conversation_state_full() {
        // 对齐 daemon conversation_snapshot.rs:29-39 序列化（snake_case）。
        let v = json!({
            "turns": [],
            "total_turns": 3,
            "has_more": false,
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "reasoning_tokens": 20,
                "total_tokens": 170,
                "prompt_cache_hit_tokens": 30,
                "prompt_cache_miss_tokens": 70,
                "cache_usage_reported": true,
            },
            "usage_totals": {
                "prompt_tokens": 1000,
                "completion_tokens": 500,
                "reasoning_tokens": 200,
                "total_tokens": 1700,
                "prompt_cache_hit_tokens": 300,
                "prompt_cache_miss_tokens": 700,
                "cache_usage_reported": true,
            },
            "usage_requests": 3,
            "cache_reported_requests": 2,
            "model": "deepseek-chat",
            "context_limit": 1000000,
        });
        let d = parse_conversation_state(&v);
        assert_eq!(d.model, "deepseek-chat");
        assert_eq!(d.context_limit, 1000000);
        assert_eq!(d.usage.prompt_tokens, 100);
        assert_eq!(d.usage.total_tokens, 170);
        assert!(d.usage.cache_usage_reported);
        assert_eq!(d.totals.completion_tokens, 500);
        assert_eq!(d.totals.prompt_cache_hit_tokens, 300);
        assert_eq!(d.usage_requests, 3);
        assert_eq!(d.cache_reported_requests, 2);
    }

    #[test]
    fn parses_conversation_state_missing_fields() {
        // 旧 daemon / 未持久化字段全部兜底。
        let d = parse_conversation_state(&json!({}));
        assert_eq!(d.model, "");
        assert_eq!(d.context_limit, 0);
        assert_eq!(d.usage, UsageInfo::default());
        assert_eq!(d.totals, UsageInfo::default());
        assert_eq!(d.usage_requests, 0);
        assert_eq!(d.cache_reported_requests, 0);
    }
}
