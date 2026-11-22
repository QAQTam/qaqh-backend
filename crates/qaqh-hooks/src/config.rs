//! Hook 配置面:TOML(config.toml `[[hooks.<Event>]]`)与 JSON(hooks 目录)
//! 双形态同构,发现层统一归一化成 [`HookEntry`]。
//!
//! 字段口径与三家调研对齐:matcher 正则、timeout 秒、env 注入(保留变量不可
//! 覆盖)、async 发后不管、on_failure 决定失败是否升级为 block(codex 口径;
//! 缺省 continue = fail-open)。

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::event::{HookEventName, DEFAULT_GATE_TIMEOUT_SECS, DEFAULT_OBSERVE_TIMEOUT_SECS};

/// 处理器类型词表。P0:command / http;mcp_tool/prompt/agent 留位,配置里
/// 出现即解析报错(明确失败,不做静默兼容层)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlerKind {
    Command,
    Http,
}

/// 失败策略(codex `HookFailurePolicy` 同口径):Continue = fail-open,
/// Block = 门事件失败时按 block 处理(观察事件仍只记录)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailurePolicy {
    #[default]
    Continue,
    Block,
}

/// 一个 handler(command 或 http)。TOML/JSON 反序列化共用;`type` 是判别键。
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub struct HookHandler {
    #[serde(rename = "type")]
    pub kind: String,
    /// command 类型:可执行路径(相对声明文件解析)或 shell 一行。
    #[serde(default)]
    pub command: Option<String>,
    /// http 类型:POST 目标。
    #[serde(default)]
    pub url: Option<String>,
    /// 超时(秒);缺省按事件权力面取值。
    #[serde(default)]
    pub timeout: Option<u64>,
    /// 发后不管:不等待完成,不参与决策。
    #[serde(default, rename = "async")]
    pub async_run: bool,
    /// 进程注入的用户 env;保留变量被丢弃并警告。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// 失败策略。
    #[serde(default, rename = "onFailure")]
    pub on_failure: FailurePolicy,
}

impl HookHandler {
    pub fn validate(&self) -> Result<HandlerKind, String> {
        match self.kind.as_str() {
            "command" => self
                .command
                .as_deref()
                .map(|c| {
                    if c.trim().is_empty() {
                        Err("command hook has empty command".to_string())
                    } else {
                        Ok(HandlerKind::Command)
                    }
                })
                .unwrap_or_else(|| Err("command hook missing `command`".to_string())),
            "http" => self
                .url
                .as_deref()
                .map(|u| {
                    if u.starts_with("http://") || u.starts_with("https://") {
                        Ok(HandlerKind::Http)
                    } else {
                        Err(format!("http hook url must be http(s): {u}"))
                    }
                })
                .unwrap_or_else(|| Err("http hook missing `url`".to_string())),
            other => Err(format!(
                "unsupported hook handler type `{other}` (supported: command, http)"
            )),
        }
    }

    /// 超时:门事件 60s,观察事件 10s;显式配置覆盖。
    pub fn effective_timeout(&self, event: HookEventName) -> u64 {
        self.timeout.unwrap_or(match event.traits().gate {
            crate::event::GateKind::Observe => DEFAULT_OBSERVE_TIMEOUT_SECS,
            _ => DEFAULT_GATE_TIMEOUT_SECS,
        })
    }
}

/// matcher 组:一个事件下的若干条 matcher + handler 列表。
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct MatcherGroup {
    /// 正则;空/缺省 = 匹配全部。
    #[serde(default)]
    pub matcher: Option<String>,
    #[serde(default)]
    pub hooks: Vec<HookHandler>,
}

/// TOML `[[hooks.<Event>]]` / JSON `{"hooks": {"<Event>": [...]}}` 的公共形状。
#[derive(Debug, Clone, Default, serde::Serialize, Deserialize)]
pub struct HookEventsConfig {
    #[serde(default, rename = "SessionStart")]
    pub session_start: Vec<MatcherGroup>,
    #[serde(default, rename = "SessionEnd")]
    pub session_end: Vec<MatcherGroup>,
    #[serde(default, rename = "UserPromptSubmit")]
    pub user_prompt_submit: Vec<MatcherGroup>,
    #[serde(default, rename = "PreToolUse")]
    pub pre_tool_use: Vec<MatcherGroup>,
    #[serde(default, rename = "PermissionRequest")]
    pub permission_request: Vec<MatcherGroup>,
    #[serde(default, rename = "PostToolUse")]
    pub post_tool_use: Vec<MatcherGroup>,
    #[serde(default, rename = "PostToolUseFailure")]
    pub post_tool_use_failure: Vec<MatcherGroup>,
    #[serde(default, rename = "Stop")]
    pub stop: Vec<MatcherGroup>,
    #[serde(default, rename = "StopFailure")]
    pub stop_failure: Vec<MatcherGroup>,
    #[serde(default, rename = "StopCancelled")]
    pub stop_cancelled: Vec<MatcherGroup>,
    #[serde(default, rename = "SubagentStart")]
    pub subagent_start: Vec<MatcherGroup>,
    #[serde(default, rename = "SubagentStop")]
    pub subagent_stop: Vec<MatcherGroup>,
    #[serde(default, rename = "PreCompact")]
    pub pre_compact: Vec<MatcherGroup>,
    #[serde(default, rename = "PostCompact")]
    pub post_compact: Vec<MatcherGroup>,
    #[serde(default, rename = "Notification")]
    pub notification: Vec<MatcherGroup>,
    #[serde(default, rename = "Interrupt")]
    pub interrupt: Vec<MatcherGroup>,
    #[serde(default, rename = "PermissionDenied")]
    pub permission_denied: Vec<MatcherGroup>,
    /// 未知事件键(grok 同律:跳过不致命;discovery 逐键警告,便于抓拼写错)。
    #[serde(flatten, default, skip_serializing)]
    pub unknown_events: BTreeMap<String, serde_json::Value>,
}

impl HookEventsConfig {
    /// 词表 → 组列表的稳定序;配置校验与 discovery 共用。
    pub fn groups_for(&self, event: HookEventName) -> &[MatcherGroup] {
        match event {
            HookEventName::SessionStart => &self.session_start,
            HookEventName::SessionEnd => &self.session_end,
            HookEventName::UserPromptSubmit => &self.user_prompt_submit,
            HookEventName::PreToolUse => &self.pre_tool_use,
            HookEventName::PermissionRequest => &self.permission_request,
            HookEventName::PostToolUse => &self.post_tool_use,
            HookEventName::PostToolUseFailure => &self.post_tool_use_failure,
            HookEventName::Stop => &self.stop,
            HookEventName::StopFailure => &self.stop_failure,
            HookEventName::StopCancelled => &self.stop_cancelled,
            HookEventName::SubagentStart => &self.subagent_start,
            HookEventName::SubagentStop => &self.subagent_stop,
            HookEventName::PreCompact => &self.pre_compact,
            HookEventName::PostCompact => &self.post_compact,
            HookEventName::Notification => &self.notification,
            HookEventName::Interrupt => &self.interrupt,
            HookEventName::PermissionDenied => &self.permission_denied,
        }
    }

    pub fn groups_for_mut(&mut self, event: HookEventName) -> &mut Vec<MatcherGroup> {
        match event {
            HookEventName::SessionStart => &mut self.session_start,
            HookEventName::SessionEnd => &mut self.session_end,
            HookEventName::UserPromptSubmit => &mut self.user_prompt_submit,
            HookEventName::PreToolUse => &mut self.pre_tool_use,
            HookEventName::PermissionRequest => &mut self.permission_request,
            HookEventName::PostToolUse => &mut self.post_tool_use,
            HookEventName::PostToolUseFailure => &mut self.post_tool_use_failure,
            HookEventName::Stop => &mut self.stop,
            HookEventName::StopFailure => &mut self.stop_failure,
            HookEventName::StopCancelled => &mut self.stop_cancelled,
            HookEventName::SubagentStart => &mut self.subagent_start,
            HookEventName::SubagentStop => &mut self.subagent_stop,
            HookEventName::PreCompact => &mut self.pre_compact,
            HookEventName::PostCompact => &mut self.post_compact,
            HookEventName::Notification => &mut self.notification,
            HookEventName::Interrupt => &mut self.interrupt,
            HookEventName::PermissionDenied => &mut self.permission_denied,
        }
    }

    pub fn is_empty(&self) -> bool {
        HookEventName::ALL
            .iter()
            .all(|event| self.groups_for(*event).is_empty())
    }

    pub fn handler_count(&self) -> usize {
        HookEventName::ALL
            .iter()
            .map(|event| self.groups_for(*event).iter().map(|g| g.hooks.len()).sum::<usize>())
            .sum()
    }
}
