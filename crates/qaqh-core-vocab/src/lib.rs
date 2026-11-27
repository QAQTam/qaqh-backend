//! `RingingV3` 纯原语底座（`ADR-0042` 铁律 1 / `V4_6_DECISION_PACK` D1 `1C′`）。
//!
//! 本 crate 承载 V3 线缆词汇表里**与传输无关**的那一层：强类型 ID、纯净域枚举、
//! 工具模式与只读工具目录。它可以被 `serde` + `postcard` 直接编解码，
//! 且**永久禁止** JSON 解析库出现在依赖树里——裸 JSON `Value` 是 V2
//! 时代最大的逃生口（任何字段都能变成"随便塞点 JSON"），封杀它就是封杀
//! "协议退化回字符串方法表"这条路。
//!
//! ## 本 crate 的四条硬纪律
//!
//! 1. **零 JSON 依赖**：依赖树里出现即 CI 失败。动态参数一律走
//!    [`JsonText`]（原始文本，不解释）或 `bytes::Bytes`（在 V3 契约层）。
//! 2. **零 serde 形状魔改**：不用跳过空字段属性（会让 `Option::None` 在
//!    `postcard` 位置编码里错位）、不用内联/无标签枚举属性
//!    （外部标记 + 定长结构才是二进制安全形状）、不用字段别名属性（别名墙
//!    是协议腐化的开端）。
//! 3. **零版本前缀、零旧代际兼容前缀**：业务域类型永久不带 `V3` 前缀；
//!    只有线缆封装（下一层的 `qaqh-ringing-v3`）才带版本。
//! 4. **不碰 V2 冰封面**：`qaqh-types` / `qaqh-ringing-contract` 一行不动，
//!    也不给它们留空壳 re-export（`1C′` 纪律：不留兼容壳，v4.9 物理处决 V2）。
//!
//! ## 模块
//!
//! - [`ids`]：`SessionId` / `TurnId` / `BlockId` / … 强类型 ID（构造即校验）；
//! - [`domain`]：`ActivityState` / `TurnTerminal` / `UsageInfo` / `ContentRef`；
//! - [`tool_mode`]：三档强类型 [`ToolMode`]（压测档 `minimal:b/c` 被拒之门外）；
//! - [`tool_catalog`]：只读 [`ToolCatalog`]（前端薄投影 + 模型面工具清单的唯一真相）。

#![cfg_attr(not(test), forbid(unsafe_code))]

pub mod domain;
pub mod ids;
pub mod tool_catalog;
pub mod tool_mode;

pub use domain::{
    ActivityState, ContentRef, CredentialSource, GoalStatus, ModelSelection, ModelSelectionOptions,
    PermissionGrantScope, SettingOverride, TurnPhase, TurnTerminal, UsageInfo, Wire,
};
pub use ids::{
    BlockId, ContentHash, ExecSessionId, IdError, InputId, InteractionId, SessionId, ToolCallId,
    TurnId, WorkspaceId,
};
pub use tool_catalog::{
    JsonText, ToolAvailability, ToolCapabilityConstraints, ToolCatalog, ToolCatalogEntry,
    ToolConcurrency, ToolGroup, ToolOrigin, ToolRiskLevel, ToolUnavailableReason,
};
pub use tool_mode::{ToolMode, ToolModeError};
