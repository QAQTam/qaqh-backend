//! 运行期能力 → `qaqh_core_vocab::ToolCatalog` 的**单向投影**（`v4.6 P3-2`）。
//!
//! 本模块是"V2 运行期事实"与"V3 只读目录"之间唯一的桥，且**只有单向**：
//! 运行期注册表（`ToolManager`）→ `qaqh-core-vocab` DTO，永不反向。
//! 反向桥（V3 DTO → 运行期）就是宪法 #5 判死的"V2-over-V3 转换层"，这里连
//! 半个也不留。
//!
//! ## 为什么投影要 fail-closed
//!
//! 目录是前端的工具面板真相源。任何一条条目"来源不明"（比如动态工具名不符合
//! `mcp__{server}__{tool}` 约定），前端分组与降级就会错，而错误会被"看起来能用"
//! 掩盖。所以投影在遇到注册期契约破损时**整份失败**（[`CatalogError`]），
//! 不产出一条来源可疑的条目（宪法 #3）。
//!
//! ## 禁用原因的判定顺序
//!
//! 1. **模型能力先决**（`tool_calls` / `image_input`）：缺工具调用能力时整个工具面
//!    都是空的，缺视觉输入时产图工具的结果不可渲染——这类原因比任何策略都根本；
//! 2. **全局天花板**（`global_allowed`）：组织级上限比会话级更严；
//! 3. **会话工具模式**（`allowed`）：会话自己的白名单。
//!
//! 顺序即"第一条命中的原因胜出"，因此同一快照里每条不可用条目只有一个原因——
//! 前端不需要理解优先级，只需要显示它。

use qaqh_core_vocab::{
    ToolAvailability, ToolCapabilityConstraints, ToolCatalog, ToolCatalogEntry, ToolConcurrency,
    ToolGroup, ToolOrigin, ToolRiskLevel, ToolUnavailableReason,
};
use qaqh_tool_core::ToolExposure;
use qaqh_tool_core::tool_api::{Namespace, ToolDescriptor};
use qaqh_types::ModelCapabilities;
use qaqh_types::policy::ToolCategory;

use crate::ToolRisk;
use crate::manager::ToolManager;
use qaqh_core_vocab::ToolMode;

/// 简述长度上限（字符）。目录是"一眼看懂有什么"，不是文档仓库。
const SUMMARY_MAX_CHARS: usize = 160;

/// 投影失败原因（全部是注册期契约破损，不是"数据不好看"）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CatalogError {
    /// 动态 MCP 工具的规范名不符合 `mcp__{server}__{tool}` 约定——无法给出来源。
    #[error("动态 MCP 工具 `{name}` 不符合 `mcp__{{server}}__{{tool}}` 命名约定")]
    UnnamespacedMcp { name: String },
    /// 扩展工具的规范名不符合 `extension__{id}__{tool}` 约定。
    #[error("扩展工具 `{name}` 不符合 `extension__{{id}}__{{tool}}` 命名约定")]
    UnnamespacedExtension { name: String },
}

/// 把当前注册表投影成一份只读工具目录快照。
///
/// `mode` 是**已解析**的 V3 工具模式（压测档在
/// [`ToolMode::from_runtime_mode`] 那一步就该被拒绝，走不到这里）；
/// `capabilities` 是该会话实际使用的模型能力声明。
///
/// 条目按名字升序（快照可比对、可缓存）；`Hidden` / `Internal` 暴露面不进入目录
/// ——它们本就不属于模型/客户端工具面（`ToolExposure` 的既有语义）。
pub fn project_tool_catalog(
    manager: &ToolManager,
    mode: ToolMode,
    capabilities: &ModelCapabilities,
) -> Result<ToolCatalog, CatalogError> {
    let mut entries = Vec::with_capacity(manager.builtins.len() + manager.dynamic.len());
    for (name, tool) in manager.builtins.iter().chain(manager.dynamic.iter()) {
        let descriptor = &tool.descriptor;
        if matches!(
            descriptor.exposure,
            ToolExposure::Hidden | ToolExposure::Internal
        ) {
            continue;
        }
        entries.push(project_entry(
            name,
            descriptor,
            mode_allows(manager, name),
            global_ceiling_allows(manager, name),
            capabilities,
        )?);
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(ToolCatalog::new(mode, entries))
}

/// 会话白名单判定（`None` = 不限制）。
fn mode_allows(manager: &ToolManager, name: &str) -> bool {
    manager
        .allowed
        .as_ref()
        .is_none_or(|allowed| allowed.iter().any(|candidate| candidate == name))
}

/// 全局天花板判定（`None` = 不限制）。
fn global_ceiling_allows(manager: &ToolManager, name: &str) -> bool {
    manager
        .global_allowed
        .as_ref()
        .is_none_or(|allowed| allowed.iter().any(|candidate| candidate == name))
}

fn project_entry(
    name: &str,
    descriptor: &ToolDescriptor,
    allowed_by_mode: bool,
    allowed_by_global_ceiling: bool,
    capabilities: &ModelCapabilities,
) -> Result<ToolCatalogEntry, CatalogError> {
    let constraints = ToolCapabilityConstraints {
        concurrency: match descriptor.capabilities.concurrency {
            qaqh_tool_core::tool_api::Concurrency::Parallel => ToolConcurrency::Parallel,
            qaqh_tool_core::tool_api::Concurrency::Serial => ToolConcurrency::Serial,
            qaqh_tool_core::tool_api::Concurrency::Exclusive => ToolConcurrency::Exclusive,
        },
        streaming: descriptor.capabilities.streaming,
        idempotent: descriptor.capabilities.idempotent,
        workspace_bound: descriptor.capabilities.workspace_bound,
        interactive: descriptor.capabilities.interactive,
        image_output: descriptor.capabilities.image_output,
    };
    Ok(ToolCatalogEntry {
        name: name.to_string(),
        group: group_of(descriptor.category),
        summary: summarize(&descriptor.description),
        risk: risk_of(descriptor.risk.clone()),
        constraints,
        origin: origin_of(name, descriptor)?,
        availability: availability_of(constraints.image_output, capabilities, allowed_by_mode, allowed_by_global_ceiling),
    })
}

fn availability_of(
    image_output: bool,
    capabilities: &ModelCapabilities,
    allowed_by_mode: bool,
    allowed_by_global_ceiling: bool,
) -> ToolAvailability {
    let reason = if !capabilities.tool_calls {
        Some(ToolUnavailableReason::ModelLacksToolCalls)
    } else if image_output && !capabilities.image_input {
        Some(ToolUnavailableReason::ModelLacksImageInput)
    } else if !allowed_by_global_ceiling {
        Some(ToolUnavailableReason::ExcludedByGlobalCeiling)
    } else if !allowed_by_mode {
        Some(ToolUnavailableReason::ExcludedByToolMode)
    } else {
        None
    };
    match reason {
        Some(reason) => ToolAvailability::Unavailable { reason },
        None => ToolAvailability::Available,
    }
}

fn group_of(category: ToolCategory) -> ToolGroup {
    match category {
        ToolCategory::Read => ToolGroup::Read,
        ToolCategory::Write => ToolGroup::Write,
        ToolCategory::Exec => ToolGroup::Exec,
        ToolCategory::Net => ToolGroup::Net,
    }
}

fn risk_of(risk: ToolRisk) -> ToolRiskLevel {
    match risk {
        ToolRisk::ReadOnly => ToolRiskLevel::ReadOnly,
        ToolRisk::Write => ToolRiskLevel::Write,
        ToolRisk::Destructive => ToolRiskLevel::Destructive,
        ToolRisk::Administrative => ToolRiskLevel::Administrative,
    }
}

/// 来源投影。动态工具的命名空间解析失败即整份投影失败（见模块文档）。
fn origin_of(name: &str, descriptor: &ToolDescriptor) -> Result<ToolOrigin, CatalogError> {
    match descriptor.source {
        qaqh_tool_core::tool_api::ToolSource::Builtin => Ok(ToolOrigin::Builtin),
        qaqh_tool_core::tool_api::ToolSource::Mcp => match descriptor.name.namespace() {
            Some(Namespace::Mcp { server }) => Ok(ToolOrigin::DynamicMcp { server }),
            _ => Err(CatalogError::UnnamespacedMcp {
                name: name.to_string(),
            }),
        },
        qaqh_tool_core::tool_api::ToolSource::Extension => match descriptor.name.namespace() {
            Some(Namespace::Extension { id }) => Ok(ToolOrigin::DynamicExtension { id }),
            _ => Err(CatalogError::UnnamespacedExtension {
                name: name.to_string(),
            }),
        },
    }
}

/// 一段式简述：取描述首个非空行，按字符（非字节）截断到 [`SUMMARY_MAX_CHARS`]。
fn summarize(description: &str) -> String {
    let line = description
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    if line.chars().count() <= SUMMARY_MAX_CHARS {
        return line.to_string();
    }
    let mut truncated: String = line.chars().take(SUMMARY_MAX_CHARS.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use qaqh_types::tool_mode::{MINIMAL, preset_tools};

    use super::*;
    use crate::tool_api::{FatalToolError, ToolCallContext, ToolOutcome, map_tool_result};
    use crate::{ToolManager, ToolResult, build_dynamic_tool, registration::build_tool_manager};

    fn full_registry() -> ToolManager {
        let host = Arc::new(crate::todo::MemoryTodoHost::default());
        build_tool_manager(&[], host)
    }

    /// 会调工具、会看图、不上官方网的"标准能力"模型。
    fn sighted_model() -> ModelCapabilities {
        ModelCapabilities {
            tool_calls: true,
            image_input: true,
            native_web_search: false,
            reasoning: false,
        }
    }

    fn standard_catalog(manager: &ToolManager, capabilities: &ModelCapabilities) -> ToolCatalog {
        project_tool_catalog(manager, ToolMode::Standard, capabilities).expect("catalog")
    }

    fn reason(catalog: &ToolCatalog, name: &str) -> Option<ToolUnavailableReason> {
        catalog
            .entry(name)
            .unwrap_or_else(|| panic!("目录缺条目 {name}"))
            .unavailable_reason()
    }

    fn noop(
        _name: &str,
        _context: &ToolCallContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutcome, FatalToolError> {
        Ok(map_tool_result(ToolResult::ok("noop")))
    }

    /// 快照①：无视觉能力模型 —— **只有产图工具**被标不可用，其余照常可用。
    #[test]
    fn no_vision_model_blocks_exactly_the_image_output_tools() {
        let manager = full_registry();
        let capabilities = ModelCapabilities {
            image_input: false,
            ..sighted_model()
        };
        let catalog = standard_catalog(&manager, &capabilities);

        assert_eq!(
            reason(&catalog, "read_image"),
            Some(ToolUnavailableReason::ModelLacksImageInput),
            "read_image 会返回图像，模型不吃图时必须标不可用"
        );
        assert_eq!(
            reason(&catalog, "confirm_apply"),
            Some(ToolUnavailableReason::ModelLacksImageInput),
            "confirm_apply 会把既有产图结果带回模型"
        );
        for name in ["read", "grep", "exec_command", "write"] {
            assert_eq!(reason(&catalog, name), None, "{name} 不该被视觉能力牵连");
        }
        // 有视觉输入的同模型：产图工具恢复可用（证明判定源是能力而非工具名）。
        let sighted = standard_catalog(&manager, &sighted_model());
        assert_eq!(reason(&sighted, "read_image"), None);
        assert_eq!(reason(&sighted, "confirm_apply"), None);
    }

    /// 快照①'：连工具调用都不会的模型 —— 整个工具面统一不可用。
    #[test]
    fn model_without_tool_calls_blocks_the_entire_surface() {
        let manager = full_registry();
        let capabilities = ModelCapabilities {
            tool_calls: false,
            ..sighted_model()
        };
        let catalog = standard_catalog(&manager, &capabilities);
        assert!(!catalog.is_empty(), "装置失效：注册表为空");
        assert_eq!(
            catalog.available().count(),
            0,
            "无工具调用能力时不允许任何工具可用"
        );
        for entry in catalog.unavailable() {
            assert_eq!(
                entry.unavailable_reason(),
                Some(ToolUnavailableReason::ModelLacksToolCalls),
                "{} 的原因必须是 ModelLacksToolCalls",
                entry.name
            );
        }
    }

    /// 快照②：`minimal`（V3 `Core`）模式 —— 模式外工具标 `ExcludedByToolMode`。
    #[test]
    fn core_mode_marks_the_rest_excluded_by_tool_mode() {
        let mut manager = full_registry();
        let allowlist = preset_tools(MINIMAL).expect("minimal 预设");
        manager.set_allowed(allowlist.iter().map(|name| (*name).to_string()).collect());
        let catalog =
            project_tool_catalog(&manager, ToolMode::Core, &sighted_model()).expect("catalog");

        assert_eq!(catalog.mode, ToolMode::Core);
        for &name in allowlist {
            assert_eq!(reason(&catalog, name), None, "{name} 在 minimal 名单内应可用");
        }
        for name in ["read_image", "web_fetch", "visualize", "todo_write"] {
            assert_eq!(
                reason(&catalog, name),
                Some(ToolUnavailableReason::ExcludedByToolMode),
                "{name} 不在 minimal 名单内"
            );
        }
        // 压测档**走不进**目录：V3 模式解析当场拒绝（隔绝在协议之外）。
        assert!(ToolMode::from_runtime_mode(Some("minimal:b")).is_err());
        assert!(ToolMode::from_runtime_mode(Some("minimal:c")).is_err());
    }

    /// 快照③：动态 MCP 工具 —— 来源、命名空间与分组都必须精确。
    #[test]
    fn dynamic_mcp_tools_carry_server_and_group() {
        let mut manager = full_registry();
        let (name, tool) = build_dynamic_tool(
            "demo",
            "echo",
            "echo back a payload\n\nsecond paragraph ignored by summary",
            serde_json::json!({ "type": "object", "properties": {} }),
            noop,
            ToolCategory::Exec,
            Duration::from_secs(30),
        );
        manager.register_dynamic(name.clone(), tool).expect("register");
        let catalog = standard_catalog(&manager, &sighted_model());

        let entry = catalog.entry("mcp__demo__echo").expect("动态条目");
        assert_eq!(
            entry.origin,
            ToolOrigin::DynamicMcp {
                server: "demo".into()
            }
        );
        assert_eq!(entry.group, ToolGroup::Exec);
        assert!(entry.is_dynamic());
        assert!(entry.is_available());
        assert_eq!(entry.summary, "echo back a payload");
    }

    /// 判定顺序：全局天花板先于会话模式；两者都排除时只报更严的那个。
    #[test]
    fn global_ceiling_wins_over_session_mode() {
        let mut manager = full_registry();
        manager.set_allowed(vec!["read".to_string(), "exec_command".to_string()]);
        manager.set_global_allowed(Some(vec!["read".to_string()]));
        let catalog = standard_catalog(&manager, &sighted_model());

        assert_eq!(reason(&catalog, "read"), None);
        assert_eq!(
            reason(&catalog, "exec_command"),
            Some(ToolUnavailableReason::ExcludedByGlobalCeiling),
            "两层都排除时只报更严的全局天花板"
        );
        assert_eq!(
            reason(&catalog, "write"),
            Some(ToolUnavailableReason::ExcludedByGlobalCeiling)
        );
        // 放宽天花板后，同一份名单回落到会话模式原因（证明两条判定独立）。
        manager.set_global_allowed(None);
        let relaxed = standard_catalog(&manager, &sighted_model());
        assert_eq!(
            reason(&relaxed, "exec_command"),
            None,
            "天花板放开后名单内工具恢复可用"
        );
        assert_eq!(
            reason(&relaxed, "write"),
            Some(ToolUnavailableReason::ExcludedByToolMode)
        );
    }

    /// 快照可比对：条目按名字升序、无重复、简述单行有界。
    #[test]
    fn catalog_is_a_stable_sorted_snapshot() {
        let manager = full_registry();
        let catalog = standard_catalog(&manager, &sighted_model());
        let names: Vec<&str> = catalog.entries.iter().map(|e| e.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "条目必须按名字升序");
        let unique: std::collections::BTreeSet<&str> = names.iter().copied().collect();
        assert_eq!(unique.len(), names.len(), "条目不得重名");
        for entry in &catalog.entries {
            assert!(
                !entry.summary.contains('\n'),
                "{} 的简述必须单行",
                entry.name
            );
            assert!(entry.summary.chars().count() <= SUMMARY_MAX_CHARS);
        }
        // 同输入同输出（目录是幂等快照，前端可直接整体替换）。
        let again = standard_catalog(&manager, &sighted_model());
        assert_eq!(catalog, again);
    }

    #[test]
    fn summaries_take_the_first_line_and_truncate_on_char_boundary() {
        assert_eq!(summarize("  第一行\n第二行  "), "第一行");
        assert_eq!(summarize(""), "");
        let long = "中".repeat(SUMMARY_MAX_CHARS + 10);
        let short = summarize(&long);
        assert_eq!(short.chars().count(), SUMMARY_MAX_CHARS);
        assert!(short.ends_with('…'));
    }
}
