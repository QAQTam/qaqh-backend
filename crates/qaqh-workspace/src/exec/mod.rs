//! exec — 命令执行通用入口（方案 A 独占）。
//!
//! 由单文件 `exec.rs` 拆分（Phase 2-2）：各子模块按既有分段切分，对外 API 不变。
//! 0946afe 曾拆分为 bash/pwsh 双工具；现收敛回单一 `exec` + `shell` 参数
//!（bash/zsh/sh/pwsh/powershell/cmd，缺省平台自动检测）。
//! 唯一对外入口为 [`register`]（`registration.rs` 调用）与 `pub(super)` handler
//!（测试经 `super::*` 可达）。

pub mod direct;
pub mod display;
pub mod handler;
pub mod pipe;
pub mod register;
pub mod shell;
pub mod truncate;

pub use register::register;
pub use shell::{bootstrap, register_shell};

#[cfg(test)]
pub(crate) use direct::{direct_exec, reader_eof_warning};
#[cfg(test)]
pub(crate) use handler::{
    detect_background_derivation, handle_run_exec, normalize_command_rg, normalize_rg_argv,
    shell_available,
};
#[cfg(test)]
pub(crate) use pipe::{PipePumpCtx, Readiness, drain_pipe_to_registry};
// `decode_windows_oem` 本体带 `#[cfg(windows)]`：非 Windows 目标下重导出会
// 编译失败（unresolved import），重导出条件必须与定义一致。
// 注：这是 Linux 上跑 `cargo test/clippy -p qaqh-workspace` 的前置修复，
// 与账本键缺陷无关，已在 PR 正文说明。
#[cfg(all(test, windows))]
pub(crate) use pipe::decode_windows_oem;
#[cfg(test)]
pub(crate) use shell::{Shell, base64_decode, executable_in_dirs, executable_on_path, ps_encode};
#[cfg(test)]
pub(crate) use truncate::token_truncate;

#[cfg(test)]
mod tests;
