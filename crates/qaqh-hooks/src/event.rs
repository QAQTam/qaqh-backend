//! Hook 事件词表与进程信封。
//!
//! 设计对齐三家调研（docs/hooks-lifecycle-research-2026-11-22.md）:
//! - 事件自带权力元数据([`GateKind`]):哪个事件能返回什么决策,词表说了算,
//!   dispatcher 不做第二次判断(grok `EventTraits` 同律)。
//! - 信封([`HookEventEnvelope`])是唯一 stdin/POST 载荷:公共头部 + 按
//!   事件 flatten 的 payload;camelCase 为权威键,snake_case 追加别名供
//!   Claude 系脚本直读(grok `SNAKE_CASE_ALIASES` 同律)。
//! - 载荷上限 [`MAX_PAYLOAD_SIZE`]:超限截断,防止把 30MB 工具输出灌给脚本。

use serde::Serialize;

/// 信封 JSON 上限;工具输出在入载荷前截断。
pub const MAX_PAYLOAD_SIZE: usize = 128 * 1024;

/// hook stdout 解析上限;超出按截断处理(ZCode 32KB 口径放宽一倍)。
pub const MAX_OUTPUT_SIZE: usize = 64 * 1024;

/// 门事件 exit code 2 = 无条件 deny/block(Claude 系兼容口径)。
pub const GATE_EXIT_CODE: i32 = 2;

/// 门事件的缺省超时(秒);观察事件用 [`DEFAULT_OBSERVE_TIMEOUT_SECS`]。
pub const DEFAULT_GATE_TIMEOUT_SECS: u64 = 60;
/// 观察事件缺省超时(秒)。
pub const DEFAULT_OBSERVE_TIMEOUT_SECS: u64 = 10;

/// 事件权力元数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventTraits {
    /// 该事件允许的决策面。
    pub gate: GateKind,
    /// matcher 是否参与筛选(只对工具事件/notification 生效)。
    pub matcher: MatcherPolicy,
}

/// 事件的决策面。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateKind {
    /// 只观察:输出记录进 HookFinished,不影响控制流。
    Observe,
    /// PreToolUse:allow / ask(升级权限交互) / deny / updatedInput 改写。
    Tool,
    /// PermissionRequest:allow / deny(预答权限交互)。
    Permission,
    /// PostToolUse:additionalContext / block reason / updatedToolOutput 替换。
    PostTool,
    /// UserPromptSubmit:block(prompt 不入库,理由只给人看)。
    Prompt,
    /// Stop:block(回合续跑,reason 回灌模型)。
    Stop,
}

/// matcher 对该事件是否生效。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatcherPolicy {
    /// matcher 被忽略(配置里写了就警告)。
    Ignored,
    /// matcher 参与筛选。
    Tested,
}

/// 17 事件一次到位;新增事件 = 加变体 + traits,不再动 dispatcher 形状。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEventName {
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    PreToolUse,
    PermissionRequest,
    PostToolUse,
    PostToolUseFailure,
    Stop,
    StopFailure,
    StopCancelled,
    SubagentStart,
    SubagentStop,
    PreCompact,
    PostCompact,
    Notification,
    Interrupt,
    PermissionDenied,
}

macro_rules! hook_events {
    ($($variant:ident {
        gate: $gate:ident,
        matcher: $matcher:ident $(,)?
    }),* $(,)?) => {
        impl HookEventName {
            /// 词表全集,discovery 校验与诊断输出用。
            pub const ALL: &'static [HookEventName] = &[$(HookEventName::$variant),*];

            /// wire/配置键(PascalCase,Claude 系同形)。
            pub fn key(self) -> &'static str {
                match self {
                    $(HookEventName::$variant => stringify!($variant),)*
                }
            }

            pub fn traits(self) -> EventTraits {
                use GateKind as G;
                use MatcherPolicy as M;
                match self {
                    $(HookEventName::$variant => EventTraits {
                        gate: G::$gate,
                        matcher: M::$matcher,
                    },)*
                }
            }
        }

        impl std::fmt::Display for HookEventName {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.key())
            }
        }
    };
}

hook_events! {
    SessionStart        { gate: Observe,  matcher: Ignored },
    SessionEnd          { gate: Observe,  matcher: Ignored },
    UserPromptSubmit    { gate: Prompt,   matcher: Ignored },
    PreToolUse          { gate: Tool,     matcher: Tested },
    PermissionRequest   { gate: Permission, matcher: Tested },
    PostToolUse         { gate: PostTool, matcher: Tested },
    PostToolUseFailure  { gate: Observe,  matcher: Tested },
    Stop                { gate: Stop,     matcher: Ignored },
    StopFailure         { gate: Observe,  matcher: Ignored },
    StopCancelled       { gate: Observe,  matcher: Ignored },
    SubagentStart       { gate: Observe,  matcher: Tested },
    SubagentStop        { gate: Observe,  matcher: Tested },
    PreCompact          { gate: Observe,  matcher: Ignored },
    PostCompact         { gate: Observe,  matcher: Ignored },
    Notification        { gate: Observe,  matcher: Tested },
    Interrupt           { gate: Observe,  matcher: Ignored },
    PermissionDenied    { gate: Observe,  matcher: Tested },
}

impl std::str::FromStr for HookEventName {
    type Err = String;
    /// 接受 PascalCase(`PreToolUse`)与 snake_case(`pre_tool_use`);
    /// 未知事件名在 discovery 处跳过并警告,不致命(grok 同律)。
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = s.to_ascii_lowercase();
        Self::ALL
            .iter()
            .find(|event| event.key().to_ascii_lowercase() == normalized)
            .copied()
            .ok_or_else(|| format!("unknown hook event name: {s}"))
    }
}

/// 事件信封:公共头 + flatten 载荷。command hook 走 stdin,http hook 走 POST 体。
#[derive(Debug, Clone, Serialize)]
pub struct HookEventEnvelope {
    pub hook_event_name: HookEventName,
    pub session_id: String,
    pub cwd: String,
    pub workspace_root: String,
    /// RFC3339 UTC。
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_hook_active: Option<bool>,
    /// Stop 门的最后一条助手消息(32KB 截断)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_assistant_message: Option<String>,
    #[serde(flatten)]
    pub payload: HookPayload,
}

/// 按事件分支的载荷;只承载该事件真正有意义的字段。
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum HookPayload {
    /// SessionStart / 通用观察事件的稀疏载荷。
    Sparse {
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        trigger: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_type: Option<String>,
    },
    UserPrompt {
        prompt: String,
    },
    ToolUse {
        tool_name: String,
        tool_input: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_output: Option<String>,
    },
    Notification {
        notification_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

impl HookEventName {
    /// matcher 测试值:工具事件测工具名,Notification 测通知类型,其余 None。
    pub fn match_value<'a>(self, payload: &'a HookPayload) -> Option<&'a str> {
        match (self, payload) {
            (
                Self::PreToolUse
                | Self::PermissionRequest
                | Self::PostToolUse
                | Self::PostToolUseFailure
                | Self::PermissionDenied,
                HookPayload::ToolUse { tool_name, .. },
            ) => Some(tool_name),
            (Self::Notification, HookPayload::Notification { notification_type, .. }) => {
                Some(notification_type)
            }
            _ => None,
        }
    }
}

/// snake_case 别名(Claude 系脚本直读);camelCase 权威键始终在场。
const SNAKE_CASE_ALIASES: &[(&str, &str)] = &[
    ("hookEventName", "hook_event_name"),
    ("sessionId", "session_id"),
    ("workspaceRoot", "workspace_root"),
    ("permissionMode", "permission_mode"),
    ("stopHookActive", "stop_hook_active"),
    ("lastAssistantMessage", "last_assistant_message"),
    ("toolName", "tool_name"),
    ("toolInput", "tool_input"),
    ("toolOutput", "tool_output"),
    ("callId", "call_id"),
    ("turnId", "turn_id"),
    ("notificationType", "notification_type"),
];

impl HookEventEnvelope {
    pub fn to_hook_json(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        if let serde_json::Value::Object(map) = &mut value {
            for (camel, snake) in SNAKE_CASE_ALIASES {
                if let Some(aliased) = map.get(*camel).cloned() {
                    map.entry((*snake).to_string()).or_insert(aliased);
                }
            }
            // hook_event_name 权威值是 PascalCase;serde 序列化出来是 snake_case,
            // 补一份 PascalCase 供 Claude 系脚本 switch。
            map.insert(
                "hook_event_name".to_string(),
                self.hook_event_name.key().into(),
            );
        }
        value
    }
}

/// RFC3339 UTC 当前时刻;无 chrono 依赖,civil_from_days 为 Hinnant 算法。
pub fn rfc3339_utc_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format_rfc3339(now.as_secs())
}

pub fn format_rfc3339(secs: u64) -> String {
    let days = secs / 86400;
    let rem = secs % 86400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 载荷内文本上限:128KB 信封口径下对超长文本统一截断。
pub fn clip_payload_text(text: &str) -> String {
    if text.len() <= MAX_PAYLOAD_SIZE {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(MAX_PAYLOAD_SIZE).collect();
    clipped.push_str("\n[qaqh-hooks: payload truncated]");
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_known_epochs() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        // 2026-11-22 00:00:00 UTC = 1795305600
        assert_eq!(format_rfc3339(1_795_305_600), "2026-11-22T00:00:00Z");
    }

    #[test]
    fn clip_marks_truncation() {
        let text = "x".repeat(MAX_PAYLOAD_SIZE + 1);
        let clipped = clip_payload_text(&text);
        assert!(clipped.len() < text.len() + 64);
        assert!(clipped.ends_with("[qaqh-hooks: payload truncated]"));
    }
}
