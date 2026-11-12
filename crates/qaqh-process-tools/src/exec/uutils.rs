//! exec::uutils — 已装 uutils coreutils 时，让 pwsh 默认壳的裸 POSIX 命令名
//! 解析到 uutils 而非内置别名/函数。
//!
//! 背景：Windows 上 uutils 安装器只给**交互式**会话接线（改写
//! `PSConsoleHostReadLine` 的输入行），而 exec 的 pwsh 是
//! `-NoProfile -NonInteractive -EncodedCommand`，该钩子永不触发，裸 `ls`
//! 恒被默认别名 `ls→Get-ChildItem` 遮蔽。本模块在**脚本前缀**层做一次
//! 确定性手术：每次 spawn 都是独立会话，不触碰用户环境、不依赖 profile。
//! 决策与被否选项见 `docs/adr/0012`。

use std::path::{Path, PathBuf};

use super::shell::PS_UTF8_PREAMBLE;

/// 待拆除的内置别名：与已装 applet **同名**且对象管道价值低。
/// `sort/tee/where/select/diff` 等对象管道高频词刻意不拆（ADR-0012 §被否选项）。
const SHADOWED_ALIASES: &[&str] = &["ls", "cp", "mv", "rm", "cat", "echo", "pwd", "sleep", "kill"];

/// pwsh 把 `mkdir` 定义为函数（`New-Item -ItemType Directory` 包装），不是别名。
const SHADOWED_FUNCTIONS: &[&str] = &["mkdir"];

/// 探测安装器布局的 uutils coreutils。只认 `ls.exe` 这个代表位：安装器
/// 全量铺 applet，`ls.exe` 在即整目录在。每次 exec 两次 `is_file`（微秒级），
/// 刻意不做全局缓存——派生链保持纯函数，测试可注入（`None` = 未装）。
pub(crate) fn detect_coreutils_bin() -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    let mut roots = Vec::new();
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        roots.push(PathBuf::from(program_files).join("coreutils"));
    }
    // 安装器 profile 垫片同样硬编码 C: 布局，作为 env 缺失时的兜底锚点。
    roots.push(PathBuf::from(r"C:\Program Files\coreutils"));
    let ls = roots
        .iter()
        .map(|root| root.join("bin").join("ls.exe"))
        .find(|ls| ls.is_file())?;
    ls.parent().map(Path::to_path_buf)
}

/// pwsh 脚本前缀：`None` 逐字节等于历史 [`PS_UTF8_PREAMBLE`]；已装时在其后
/// 追加 PATH 前插与最小冲突别名手术（每行一条，尾部无换行，命令正文接在其后）。
pub(crate) fn pwsh_script_prefix(coreutils_bin: Option<&Path>) -> String {
    let Some(dir) = coreutils_bin else {
        return PS_UTF8_PREAMBLE.to_string();
    };
    let aliases = SHADOWED_ALIASES
        .iter()
        .map(|alias| format!("Alias:{alias}"))
        .collect::<Vec<_>>()
        .join(",");
    let functions = SHADOWED_FUNCTIONS
        .iter()
        .map(|function| format!("Function:{function}"))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{PS_UTF8_PREAMBLE}\n\
         $env:PATH = '{dir};' + $env:PATH\n\
         Remove-Item -Force -ErrorAction Ignore {aliases}\n\
         Remove-Item -Force -ErrorAction Ignore {functions}",
        dir = dir.display()
    )
}
