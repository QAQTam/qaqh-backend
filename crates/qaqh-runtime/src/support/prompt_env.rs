//! Process-wide probe results consumed by the frozen system prefix
//! (`{{OS}}` / `{{TOOLS}}`).

use std::sync::OnceLock;

pub static OS_INFO: OnceLock<String> = OnceLock::new();
pub static TOOLS_INFO: OnceLock<String> = OnceLock::new();
