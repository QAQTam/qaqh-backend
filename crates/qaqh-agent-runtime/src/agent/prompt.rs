//! Process-wide probe results consumed by the frozen system prefix
//! (`{{OS}}` / `{{TOOLS}}`).
//!
//! 2026-10-31：`full_system_prompt*` / `system_prompt_for_mode` 已删除，
//! 会话提示词由 `session_agent::prompt::system_prompt` 负责。这里只剩启动期探测的两个槽位，
//! 由 `qaqh-runtime::registry::detect_os_info` / `detect_toolchains` 填充。

use std::sync::OnceLock;

/// Cached OS info string. Set at startup.
pub static OS_INFO: OnceLock<String> = OnceLock::new();

/// Cached toolchain versions. Set at startup.
pub static TOOLS_INFO: OnceLock<String> = OnceLock::new();
