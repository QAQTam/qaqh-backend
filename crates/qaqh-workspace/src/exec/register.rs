//! exec::register — exec 通用入口注册（方案 A 独占）。
//!
//! 0946afe 曾将 exec 拆分为 bash/pwsh 双工具（避免三者鼎立）；回 exec 后
//! 语义收敛为单一 `exec` + `command` + `shell` 参数（bash/zsh/sh/pwsh/
//! powershell/cmd，缺省平台自动检测）。命令始终经所选 shell 包裹，不再
//! 暴露直接 argv 模式。pwsh 特判收敛在 [`Shell`] 枚举内（路径降级链 +
//! -EncodedCommand/-CommandWithArgs），注册层不分支。

use super::handler::ExecTool;
use super::shell::Shell;

pub fn register(mgr: &mut crate::ToolManager) {
    // 触发探测缓存（Windows git-bash / pwsh 降级链），description 不再为
    // 特定 shell 背书——选壳是每次调用的运行时决策。
    let _ = Shell::detect();
    let _ = Shell::from_name("bash");
    mgr.register_typed(ExecTool);
}
