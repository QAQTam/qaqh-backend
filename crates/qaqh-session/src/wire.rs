//! 旧 bootstrap 三频道 wire 结构的临时驻留(自 qaqh-ringing-contract 迁出)。
//!
//! 仅 daemon bootstrap handler 与 client SDK 在统一 SessionState 切换完成前
//! 使用;P4 线上翻转时整体删除,不留转换层。

use qaqh_ringing::{RingingV2DriverState, RingingV2PendingInteraction};
use serde::{Deserialize, Serialize};

use crate::projection::{ControlRoundState, ControlSubagentState, ControlToolState};
use crate::session_fact_v2::{ActivityState, RecoveryOutcome, SessionId, ToolCallId, TurnId};

/// bootstrap `control.state`:canonical control 快照 + wire 化的 pending
/// 交互与 driver 席位。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct ClientV2ControlState {
    pub session_id: Option<SessionId>,
    pub activity: ActivityState,
    pub current_turn_id: Option<TurnId>,
    pub current_call_id: Option<ToolCallId>,
    pub round: Option<ControlRoundState>,
    pub tools: Vec<ControlToolState>,
    pub subagents: Vec<ControlSubagentState>,
    pub last_recovery: Option<RecoveryOutcome>,
    pub revision: u64,
    pub last_fact_seq: u64,
    pub interactions: Vec<RingingV2PendingInteraction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<RingingV2DriverState>,
}

/// bootstrap `tool.state`:与 control 快照共享 `ControlToolState` 词表。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct ClientV2ToolState {
    pub tools: Vec<ControlToolState>,
}
