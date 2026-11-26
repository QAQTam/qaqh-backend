//! 三档强类型工具模式（`ADR-0042` 铁律 1 / `V4_6_DECISION_PACK` D1 `1C′`）。
//!
//! ## 为什么 V3 只有三档
//!
//! V2 的工具模式表（`qaqh_types::tool_mode`，**冰封中，一行不动**）里有五个名字：
//! `standard` / `minimal` / `minimal:b` / `minimal:c` / `custom`。后两个是
//! **压测档**——它们存在的唯一理由是给压测和故障注入准备更小的工具面，
//! 不是给人用的产品档位。把它们写进 V3 协议，等于让"压测开关"变成永久 ABI。
//!
//! V3 只认三档：
//! - [`ToolMode::Core`]：最小核心集（V2 `minimal` 的产品语义）；
//! - [`ToolMode::Standard`]：全量工具面；
//! - [`ToolMode::Custom`]：用户显式白名单。
//!
//! ## 压测档"隔绝"的确切含义
//!
//! [`ToolMode::from_runtime_mode`] 碰到 `minimal:b` / `minimal:c` **返回错误**，
//! 绝不静默折叠成 `Core` 或 `Standard`。静默折叠的后果是：压测档会话在 V3 客户端
//! 眼里看起来是个正常档位，行为却与档位声明不符——那是"看起来能跑"的典型烂账
//! （宪法 #3 Fail-Closed）。宁可当场拒绝，让谁也别想蒙混。

use serde::{Deserialize, Serialize};

/// V3 工具模式（三档，永久不带版本前缀）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolMode {
    /// 最小核心集：读 / 写 / 编辑 / 检索 / 执行 / 应用补丁确认。
    Core,
    /// 全量工具面。
    Standard,
    /// 用户显式白名单（白名单本身走 `ToolCatalog` 的可用性标记投影）。
    Custom,
}

/// 模式解析失败原因。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolModeError {
    /// V2 压测档被挡在 V3 之外（**故意**，见模块文档）。
    #[error("压测档 `{0}` 不属于 V3 工具模式，禁止映射")]
    LoadTestTier(String),
    /// 未知模式名。
    #[error("未知工具模式 `{0}`")]
    Unknown(String),
}

impl ToolMode {
    /// V2 运行期模式名 → V3 强类型模式（`None` 入参按 V2 的"空名单"语义取 `Standard`）。
    ///
    /// 这是**单向投影**：V3 永不反向生出 V2 名字。
    pub fn from_runtime_mode(runtime_mode: Option<&str>) -> Result<Self, ToolModeError> {
        let Some(runtime_mode) = runtime_mode.map(str::trim).filter(|mode| !mode.is_empty())
        else {
            return Ok(Self::Standard);
        };
        match runtime_mode {
            "standard" => Ok(Self::Standard),
            "minimal" => Ok(Self::Core),
            "custom" => Ok(Self::Custom),
            // 压测档：当场拒绝，不折叠。
            "minimal:b" | "minimal:c" => Err(ToolModeError::LoadTestTier(runtime_mode.to_string())),
            other => Err(ToolModeError::Unknown(other.to_string())),
        }
    }

    /// 该模式是否允许工具面被 `custom_tools` 名单裁剪（只有 `Custom` 是）。
    pub fn is_allowlist_driven(self) -> bool {
        matches!(self, Self::Custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_tiers_map_one_way() {
        assert_eq!(
            ToolMode::from_runtime_mode(None),
            Ok(ToolMode::Standard),
            "空名单 = 标准模式（V2 语义）"
        );
        assert_eq!(
            ToolMode::from_runtime_mode(Some("")),
            Ok(ToolMode::Standard)
        );
        assert_eq!(
            ToolMode::from_runtime_mode(Some(" standard ")),
            Ok(ToolMode::Standard)
        );
        assert_eq!(ToolMode::from_runtime_mode(Some("minimal")), Ok(ToolMode::Core));
        assert_eq!(ToolMode::from_runtime_mode(Some("custom")), Ok(ToolMode::Custom));
    }

    /// 反证：压测档**必须**当场失败，任何"顺手折叠"都是回归。
    #[test]
    fn load_test_tiers_are_refused_not_folded() {
        for tier in ["minimal:b", "minimal:c"] {
            match ToolMode::from_runtime_mode(Some(tier)) {
                Err(ToolModeError::LoadTestTier(rejected)) => assert_eq!(rejected, tier),
                other => panic!("{tier} 必须被拒绝，实得 {other:?}"),
            }
        }
        assert_eq!(
            ToolMode::from_runtime_mode(Some("turbo")),
            Err(ToolModeError::Unknown("turbo".into()))
        );
    }

    #[test]
    fn only_custom_is_allowlist_driven() {
        assert!(ToolMode::Custom.is_allowlist_driven());
        assert!(!ToolMode::Core.is_allowlist_driven());
        assert!(!ToolMode::Standard.is_allowlist_driven());
    }
}
