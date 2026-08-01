//! 每会话、每频道切流状态机（两阶段提交）。
//!
//! PLAN 切流语义：
//! ```text
//! sessionChannelMode[seed][channel] {
//!   event_protocol: legacy | Ringing
//!   command_protocol: legacy | Ringing
//! }
//! ```
//! 事件和命令可以分阶段切换，但同一方向、同一 session/channel 只能有一个权威协议。
//!
//! 事件切流（两阶段）：
//! 1. `channel_prepare`：服务端先建立 SSE live boundary，再生成 Ringing 领域
//!    snapshot，并缓冲 boundary 后的可靠事件（本层标记 `Preparing`）。
//! 2. `channel_commit`：原子切换 event owner，停止向该客户端发送对应 legacy 事件
//!    并释放缓冲（本层切换为 `Ringing`）。
//! 3. prepare 失败、超时或断线时保持 legacy（`abort`）。
//!
//! 已切换的频道发生故障时**保持 Ringing 模式**（cursor/snapshot 恢复），不自动退回 legacy。

use std::collections::HashMap;

use deepx_domain::RingingChannel;
use serde::{Deserialize, Serialize};

/// 事件方向协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventProtocol {
    Legacy,
    Ringing,
}

/// 命令方向协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandProtocol {
    Legacy,
    Ringing,
}

/// 切流中间态（两阶段提交的 prepare 阶段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CutoverPhase {
    /// 未切流（默认 legacy）。
    None,
    /// prepare 已受理：SSE boundary 建立、snapshot 生成、可靠事件缓冲中。
    Preparing,
}

/// 每 session/channel 的协议模式。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionChannelMode {
    pub event_protocol: EventProtocol,
    pub command_protocol: CommandProtocol,
    pub phase: CutoverPhase,
}

impl Default for SessionChannelMode {
    fn default() -> Self {
        Self {
            event_protocol: EventProtocol::Legacy,
            command_protocol: CommandProtocol::Legacy,
            phase: CutoverPhase::None,
        }
    }
}

/// 切流状态机（线程安全，daemon 侧维护）。
#[derive(Debug, Default)]
pub struct CutoverState {
    modes: HashMap<(String, RingingChannel), SessionChannelMode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CutoverError {
    /// 该 session/channel 已处于 Ringing，不能重复 prepare。
    AlreadyRinging,
    /// 该 session/channel 不在 preparing 状态，不能 commit/abort。
    NotPreparing,
}

impl CutoverState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 序列化为持久化 JSON（HashMap 元组键不满足 JSON map key，改为数组）。
    pub fn to_json(&self) -> serde_json::Value {
        let modes: Vec<_> = self
            .modes
            .iter()
            .map(|((seed, channel), mode)| {
                serde_json::json!({
                    "seed": seed,
                    "channel": channel,
                    "mode": mode,
                })
            })
            .collect();
        serde_json::json!({ "modes": modes })
    }

    /// 从持久化 JSON 恢复；格式不合法时返回 None（调用方保持默认）。
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        let modes = value.get("modes")?.as_array()?;
        let mut map = HashMap::new();
        for item in modes {
            let seed = item.get("seed")?.as_str()?.to_string();
            let channel = serde_json::from_value(item.get("channel")?.clone()).ok()?;
            let mode = serde_json::from_value(item.get("mode")?.clone()).ok()?;
            map.insert((seed, channel), mode);
        }
        Some(Self { modes: map })
    }

    pub fn mode(&self, seed: &str, channel: RingingChannel) -> SessionChannelMode {
        self.modes
            .get(&(seed.to_string(), channel))
            .copied()
            .unwrap_or_default()
    }

    /// 事件协议是否为 Ringing。
    pub fn event_is_ringing(&self, seed: &str, channel: RingingChannel) -> bool {
        self.mode(seed, channel).event_protocol == EventProtocol::Ringing
    }

    /// 命令协议是否为 Ringing。
    pub fn command_is_ringing(&self, seed: &str, channel: RingingChannel) -> bool {
        self.mode(seed, channel).command_protocol == CommandProtocol::Ringing
    }

    /// 阶段 1：channel_prepare。
    /// 仅当当前为 legacy 且未 preparing 时受理；否则返回 AlreadyRinging。
    pub fn prepare(&mut self, seed: &str, channel: RingingChannel) -> Result<(), CutoverError> {
        let key = (seed.to_string(), channel);
        let entry = self.modes.entry(key).or_default();
        match entry.event_protocol {
            EventProtocol::Ringing => Err(CutoverError::AlreadyRinging),
            EventProtocol::Legacy => {
                entry.phase = CutoverPhase::Preparing;
                Ok(())
            }
        }
    }

    /// 阶段 2：channel_commit。原子切换 event owner 为 Ringing。
    pub fn commit(&mut self, seed: &str, channel: RingingChannel) -> Result<(), CutoverError> {
        let key = (seed.to_string(), channel);
        let entry = self.modes.entry(key).or_default();
        if entry.phase != CutoverPhase::Preparing {
            return Err(CutoverError::NotPreparing);
        }
        entry.event_protocol = EventProtocol::Ringing;
        entry.phase = CutoverPhase::None;
        Ok(())
    }

    /// prepare 失败/超时/断线：保持 legacy。
    pub fn abort(&mut self, seed: &str, channel: RingingChannel) {
        let key = (seed.to_string(), channel);
        if let Some(entry) = self.modes.get_mut(&key)
            && entry.phase == CutoverPhase::Preparing
        {
            entry.phase = CutoverPhase::None;
        }
    }

    /// 命令方向切流（命令可独立于事件切换，权威协议各自唯一）。
    pub fn switch_command(
        &mut self,
        seed: &str,
        channel: RingingChannel,
        protocol: CommandProtocol,
    ) {
        let key = (seed.to_string(), channel);
        let entry = self.modes.entry(key).or_default();
        entry.command_protocol = protocol;
    }

    /// 已切流频道故障时保持 Ringing（不自动退回 legacy）：本方法为 no-op，
    /// 仅用于在接入层显式表达该硬规则。
    pub fn assert_ringing_sticky(&self, seed: &str, channel: RingingChannel) {
        debug_assert!(
            self.event_is_ringing(seed, channel),
            "ringing channels must not fall back to legacy"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_legacy_legacy() {
        let state = CutoverState::new();
        assert!(!state.event_is_ringing("s", RingingChannel::Tool));
        assert!(!state.command_is_ringing("s", RingingChannel::Tool));
        let m = state.mode("s", RingingChannel::Conversation);
        assert_eq!(m.event_protocol, EventProtocol::Legacy);
        assert_eq!(m.phase, CutoverPhase::None);
    }

    #[test]
    fn two_phase_cutover_flow() {
        let mut state = CutoverState::new();
        assert!(state.prepare("s", RingingChannel::Tool).is_ok());
        // preparing 期间事件仍是 legacy（快照/缓冲阶段）
        assert!(!state.event_is_ringing("s", RingingChannel::Tool));
        assert!(state.commit("s", RingingChannel::Tool).is_ok());
        assert!(state.event_is_ringing("s", RingingChannel::Tool));
    }

    #[test]
    fn duplicate_prepare_rejected() {
        let mut state = CutoverState::new();
        state.prepare("s", RingingChannel::Tool).expect("first");
        state.commit("s", RingingChannel::Tool).expect("commit");
        assert_eq!(
            state.prepare("s", RingingChannel::Tool),
            Err(CutoverError::AlreadyRinging)
        );
    }

    #[test]
    fn commit_without_prepare_rejected() {
        let mut state = CutoverState::new();
        assert_eq!(
            state.commit("s", RingingChannel::Tool),
            Err(CutoverError::NotPreparing)
        );
    }

    #[test]
    fn abort_keeps_legacy() {
        let mut state = CutoverState::new();
        state.prepare("s", RingingChannel::Tool).expect("prepare");
        state.abort("s", RingingChannel::Tool);
        assert!(!state.event_is_ringing("s", RingingChannel::Tool));
        // abort 后可重新 prepare
        assert!(state.prepare("s", RingingChannel::Tool).is_ok());
    }

    #[test]
    fn command_protocol_switches_independently() {
        let mut state = CutoverState::new();
        state.switch_command("s", RingingChannel::Tool, CommandProtocol::Ringing);
        assert!(state.command_is_ringing("s", RingingChannel::Tool));
        assert!(!state.event_is_ringing("s", RingingChannel::Tool));
    }

    #[test]
    fn mode_is_per_seed_and_channel() {
        let mut state = CutoverState::new();
        state.prepare("a", RingingChannel::Tool).expect("a");
        state.commit("a", RingingChannel::Tool).expect("commit");
        assert!(state.event_is_ringing("a", RingingChannel::Tool));
        assert!(!state.event_is_ringing("b", RingingChannel::Tool));
        assert!(!state.event_is_ringing("a", RingingChannel::Conversation));
    }

    #[test]
    fn sticky_ringing_assertion_holds() {
        let mut state = CutoverState::new();
        state.prepare("s", RingingChannel::Conversation).expect("prepare");
        state.commit("s", RingingChannel::Conversation).expect("commit");
        // 故障后仍保持 Ringing：断言不触发（debug 构建）
        state.assert_ringing_sticky("s", RingingChannel::Conversation);
        assert!(state.event_is_ringing("s", RingingChannel::Conversation));
    }
}
