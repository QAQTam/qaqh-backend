//! service::params — 会话参数与工具模式的取值/校验规则。
//!
//! 服务面的 params 一律由 [`crate::service::command`] 的强类型 DTO 解码，
//! 这里只保留**跨领域共享的规则**（工具模式白名单、可选的模式预置）。

/// 可选工具模式预置：缺省 = None（保持旧行为）；显式空串 = None（未指定，
/// 交由 SessionMeta.tool_mode 默认 standard 完成零迁移）。
/// 供 `session.create` 在 spawn 前落盘使用。
pub(crate) fn optional_tool_mode(
    tool_mode: Option<&str>,
    custom_tools: &[String],
) -> Result<Option<(String, Vec<String>)>, String> {
    let Some(tool_mode) = tool_mode else {
        return Ok(None);
    };
    // 显式空串 = 未指定（与 SessionMeta.tool_mode 的空串零迁移语义一致）。
    if tool_mode.is_empty() {
        return Ok(None);
    }
    validate_tool_mode(tool_mode)?;
    if tool_mode == "custom" && custom_tools.is_empty() {
        return Err("custom tool mode requires at least one tool in custom_tools".to_string());
    }
    Ok(Some((tool_mode.to_string(), custom_tools.to_vec())))
}

/// 工具模式白名单校验（session.create 预置与 session.settings.patch 共用）。
/// 白名单由 `qaqh_types::tool_mode::KNOWN_MODES` 单一契约提供（BUG-013）。
pub(crate) fn validate_tool_mode(tool_mode: &str) -> Result<(), String> {
    if qaqh_types::is_known(tool_mode) {
        Ok(())
    } else {
        Err(format!(
            "invalid tool_mode '{tool_mode}' (expected {})",
            qaqh_types::KNOWN_MODES.join(" | ")
        ))
    }
}
