//! 门面线程态运行时快照的注入钩子。
//!
//! 文件工具组脱离门面后，少数语义仍由「执行线程的 thread-local 运行时状态」
//! 决定：agent mode、permission level、subagent sandbox 旗标。这些状态的
//! 单一事实源在门面 `qaqh_workspace::runtime` /
//! `authorization`，本 crate 不反向依赖——门面在构造 ToolManager 前一次性
//! 注册回调（OnceLock 幂等，先例：qaqh-permission 的 cancel-session resolver）。
//!
//! 回调在**调用线程**上执行，thread-local 语义与搬迁前完全一致。

use std::sync::OnceLock;

/// 工具执行线程的运行时快照。
#[derive(Clone, Copy, Debug)]
pub struct AmbientRuntime {
    /// 门面 `runtime::current_mode()`（1 = Plan，其余 = Code）。
    pub mode: u8,
    /// 门面 `runtime::context()` 的 permission_level u8。
    pub permission_level: u8,
    /// 门面 `authorization::is_subagent_sandbox()`。
    pub subagent_sandbox: bool,
}

impl Default for AmbientRuntime {
    fn default() -> Self {
        Self {
            mode: 0,
            permission_level: 0,
            subagent_sandbox: false,
        }
    }
}

static AMBIENT: OnceLock<fn() -> AmbientRuntime> = OnceLock::new();

/// 门面注册运行时快照回调（幂等；首个注册者生效）。
pub fn set_ambient_provider(provider: fn() -> AmbientRuntime) {
    let _ = AMBIENT.set(provider);
}

/// 读取执行线程的运行时快照（未注册时按最保守缺省：Code / ReadOnly / 主沙箱）。
pub fn ambient() -> AmbientRuntime {
    AMBIENT.get().map(|provider| provider()).unwrap_or_default()
}
