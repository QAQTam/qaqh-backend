//! capability 结构化上报(cross-review §5:替换自由文本 detail 的"不可用原因"
//! 缺口;is_available/unavailable_reason 模式)。

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Capability {
    pub backend: &'static str,
    pub filesystem_write_isolation: bool,
    pub network_isolation: bool,
    pub process_hardening: bool,
    pub available: bool,
    pub elevated: bool,
    pub detail: String,
}

pub fn detect() -> Capability {
    Capability {
        backend: "windows_restricted_token",
        filesystem_write_isolation: true,
        // M1 网络仅有 env-steering(codex legacy 同款),必须明示非强制
        network_isolation: false,
        process_hardening: true,
        available: cfg!(windows),
        elevated: crate::token::is_elevated(),
        detail: "token plane: WRITE_RESTRICTED + capability-SID DACL + private desktop".into(),
    }
}
