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
}
