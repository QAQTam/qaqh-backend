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
}
