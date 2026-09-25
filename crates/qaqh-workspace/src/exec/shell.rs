//! exec::shell — 平台 shell 探测与 argv 派生（Shell/detect/path/derive_exec_args_with + base64/ps_encode）。

use std::sync::OnceLock;

/// 启动期壳探测（daemon `main` 调一次，与 `cache_system_path`/`detect_os_info`
/// 同批）：显式注册的路径优先，其后由 [`Shell::detect`] 选平台默认壳并
/// 把可运行候选解析钉进 `path()`。返回钉住的可执行名（无可用壳时返回
/// 名义名，调用方无需分支）。
pub fn bootstrap() -> String {
    Shell::detect().path().to_string()
}

/// 显式钉住壳路径（embedder/测试用；`bootstrap()` 之外的第二入口）。
/// 传空串 = no-op。钉住的路径在 `Shell::path()` 中优先级最高。
pub fn register_shell(path: &str) {
    Shell::register_shell(path);
}

// ── Platform shell detection ──
// Adapted from codex-rs/shell-command/src/shell_detect.rs & core/src/shell.rs.
// Stripped to the minimum needed: pick the right shell, derive argv.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
#[allow(clippy::enum_variant_names)] // 变体名 PowerShell 含枚举名，属既有命名
pub(crate) enum Shell {
    Bash,
    Zsh,
    Sh,
    PowerShell,
    WindowsPowerShell,
    Cmd,
}

pub(crate) static DETECTED_SHELL: OnceLock<Shell> = OnceLock::new();
/// Full path to bash on Windows — avoids the WSL wrapper at System32\\bash.exe.
pub(crate) static DETECTED_BASH_PATH: OnceLock<String> = OnceLock::new();
/// 启动期显式注册的壳路径（`Shell::register_shell`）。
pub(crate) static REGISTERED_SHELL_PATH: OnceLock<String> = OnceLock::new();
/// 各壳解析出的可运行候选名缓存（`Some(None)` = 候选集全不可用）。
/// 必须按壳分槽：Bash/Zsh/Sh 共用同一候选集，但「谁命中了」对每个壳
/// 是独立事实（容器里只有 `sh` 时，Bash/Zsh 都该落到 `sh`，而不是让
/// 先探测的一方污染另一方）。
static RESOLVED_SHELL_NAME: [OnceLock<Option<String>>; 6] = [
    OnceLock::new(),
    OnceLock::new(),
    OnceLock::new(),
    OnceLock::new(),
    OnceLock::new(),
    OnceLock::new(),
];

impl Shell {
    /// Auto-detect the best available shell on this platform.
    pub(crate) fn detect() -> Self {
        *DETECTED_SHELL.get_or_init(Self::detect_uncached)
    }

    /// Resolve an explicit shell name requested by the model (exec `shell`
    /// parameter). Windows `bash` resolves to Git-for-Windows / MSYS2 when
    /// present, avoiding the WSL wrapper. Unknown names fall back to None so
    /// the caller can report a clean error.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "bash" | "bash4windows" => {
                #[cfg(windows)]
                {
                    const WIN_BASH_CANDIDATES: &[&str] = &[
                        "C:\\Program Files\\Git\\bin\\bash.exe",
                        "C:\\Program Files (x86)\\Git\\bin\\bash.exe",
                        "C:\\msys64\\usr\\bin\\bash.exe",
                    ];
                    for p in WIN_BASH_CANDIDATES {
                        if std::path::Path::new(p).is_file() {
                            DETECTED_BASH_PATH.get_or_init(|| p.to_string());
                            return Some(Shell::Bash);
                        }
                    }
                    if let Some(found) = find_bash_on_path() {
                        DETECTED_BASH_PATH.get_or_init(|| found);
                    }
                    Some(Shell::Bash)
                }
                #[cfg(not(windows))]
                {
                    Some(Shell::Bash)
                }
            }
            "zsh" => Some(Shell::Zsh),
            "sh" => Some(Shell::Sh),
            "pwsh" => Some(Shell::PowerShell),
            "powershell" => Some(Shell::WindowsPowerShell),
            "cmd" => Some(Shell::Cmd),
            _ => None,
        }
    }

    /// Platform-priority auto-detection candidates.
    pub(crate) fn auto_candidates() -> &'static [Shell] {
        #[cfg(windows)]
        {
            &[
                Shell::PowerShell,
                Shell::Bash,
                Shell::WindowsPowerShell,
                Shell::Cmd,
            ]
        }
        #[cfg(target_os = "linux")]
        {
            &[Shell::Bash, Shell::Zsh, Shell::Sh]
        }
        #[cfg(target_os = "macos")]
        {
            &[Shell::Bash, Shell::Zsh]
        }
        #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
        {
            &[Shell::Bash, Shell::Zsh, Shell::Sh]
        }
    }

    /// 启动期把「探测到的壳」钉进进程状态：注册的路径或名字在后续所有
    /// 派生中优先（Windows git-bash 绝对路径即由此进入 `path()`）。
    pub(crate) fn register_shell(path: &str) {
        if !path.is_empty() {
            let _ = REGISTERED_SHELL_PATH.set(path.to_string());
        }
    }

    /// 已解析的候选名（进程内一次性；`None` = 候选集全不可用）。
    /// 与 `available()` 共用同一份口径，保证探测与派生同源。
    fn resolved_shell_name(&self) -> Option<&'static str> {
        let slot = match self {
            Shell::Bash => 0,
            Shell::Zsh => 1,
            Shell::Sh => 2,
            Shell::PowerShell => 3,
            Shell::WindowsPowerShell => 4,
            Shell::Cmd => 5,
        };
        let cache = &RESOLVED_SHELL_NAME[slot];
        if let Some(shell) = cache.get() {
            return shell.as_deref();
        }
        let resolved = self
            .executable_candidates()
            .iter()
            .copied()
            .find(|candidate| executable_on_path(candidate));
        let _ = cache.set(resolved.map(str::to_string));
        cache.get().and_then(Option::as_deref)
    }

    pub(crate) fn detect_uncached() -> Self {
        #[cfg(windows)]
        {
            // Windows 优先级：pwsh 7 > Git for Windows bash > powershell 5.1 > cmd。
            if Shell::PowerShell.available() {
                return Shell::PowerShell;
            }
            if Shell::Bash.available() {
                return Shell::Bash;
            }
            if Shell::WindowsPowerShell.available() {
                return Shell::WindowsPowerShell;
            }
            Shell::Cmd
        }
        #[cfg(target_os = "linux")]
        {
            // Linux 优先级：bash > zsh > sh。
            for shell in [Shell::Bash, Shell::Zsh, Shell::Sh] {
                if shell.available() {
                    return shell;
                }
            }
            Shell::Sh
        }
        #[cfg(target_os = "macos")]
        {
            // macOS 优先级：bash > zsh。
            for shell in [Shell::Bash, Shell::Zsh] {
                if shell.available() {
                    return shell;
                }
            }
            Shell::Bash
        }
        #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
        {
            for shell in [Shell::Bash, Shell::Zsh, Shell::Sh] {
                if shell.available() {
                    return shell;
                }
            }
            Shell::Sh
        }
    }

    /// Path to the shell executable.
    /// 优先级：显式注册（启动期探测结果）> Windows git-bash 绝对路径 >
    /// 候选集解析出的可运行名（`resolved_shell_name`）> 名义名。
    /// 候选解析是「探测与派生同源」的关键：精简镜像只有 `sh`/`dash` 时，
    /// `Shell::Bash` 会派生 `sh` 而不是必然失败的 `bash`——于是显式
    /// `shell: "bash"` 与平台自动检测落在同一支壳上（同一 POSIX 语义），
    /// 探测口径与实际 argv 不再漂移。
    pub(crate) fn path(&self) -> &str {
        let registered = REGISTERED_SHELL_PATH.get();
        let resolved = self.resolved_shell_name();
        match self {
            Shell::Bash => registered
                .or_else(|| DETECTED_BASH_PATH.get())
                .map(String::as_str)
                .or(resolved)
                .unwrap_or("bash"),
            Shell::Zsh => resolved.unwrap_or("zsh"),
            Shell::Sh => resolved.unwrap_or("sh"),
            Shell::PowerShell => registered
                .map(String::as_str)
                .or(resolved)
                .unwrap_or("pwsh"),
            Shell::WindowsPowerShell => registered
                .map(String::as_str)
                .or(resolved)
                .unwrap_or("powershell"),
            Shell::Cmd => resolved.unwrap_or("cmd"),
        }
    }

    /// 候选可执行名集（探测与实际派生必须同源）。
    /// `path()` 非绝对路径时只是「初始候选」，真实可用性可能落在同族别名上：
    /// 精简镜像只有 `sh`/`dash`、Windows 侧 git-bash 尚未解析、pwsh 7 缺失时
    /// 落 `powershell.exe`。探测只认 `path()` 一个名字会误报 SHELL_NOT_FOUND，
    /// 而派生实际上能跑起来（O-3 的「探测 ≠ 派生」缺口）。
    pub(crate) fn executable_candidates(&self) -> &'static [&'static str] {
        match self {
            Shell::Bash => &["bash"],
            Shell::Zsh => &["zsh"],
            Shell::Sh => &["sh"],
            Shell::PowerShell => &["pwsh"],
            Shell::WindowsPowerShell => &["powershell"],
            Shell::Cmd => &["cmd"],
        }
    }

    /// shell 可用性软检测：`path()` 已是绝对路径（Windows 探到的 git-bash）时
    /// 只看它；否则按候选集探测——与 `derive_exec_args_with` 最终派生的
    /// 可执行名同源，杜绝「探测到的壳」与「实际跑的壳」不一致。
    pub(crate) fn available(&self) -> bool {
        #[cfg(windows)]
        if *self == Shell::Bash {
            if let Some(path) = find_bash_on_path() {
                DETECTED_BASH_PATH.get_or_init(|| path);
            }
            return DETECTED_BASH_PATH
                .get()
                .map(|path| std::path::Path::new(path).is_file())
                .unwrap_or(false);
        }
        let path = self.path();
        let p = std::path::Path::new(path);
        if p.is_absolute() {
            return p.is_file();
        }
        executable_on_path(path)
    }

    /// Build the argv that runs `command` through this shell.
    /// PowerShell 默认走 `-EncodedCommand`（Base64 UTF-16LE），彻底避免引号/中文/特殊字符在
    /// Win32 命令行解析中的转义地狱；stdout/stderr 仍通过管道捕获，编码不影响输出。
    /// 当 `args` 非空且为 PowerShell 时，自动走 `-CommandWithArgs`（7.6 LTS 主流），
    /// 把 `args` 原样作为 CommandParameters 填入 `$args`，避免在脚本内拼接引号。
    /// **绑定契约：只填 `$args`（`$args[0]` 起 / `$args.Count`），不产生
    /// `$arg0`/`$argN`**——`pwsh -h` 原文为 "populates the `$args` built-in
    /// variable"；脚本里写 `$argN` 恒为 `$null`（与参数是否中文无关）。
    /// 当 `args` 非空且为 POSIX shell（bash/zsh/sh）时，透传为位置参数
    /// `[sh, -c, command, _, args...]`（`$0` 占位 `_`，`args` 进 `$1/$2/$@`），
    /// 与 pwsh 侧对称：模板与数据分离，避免在脚本字符串内拼接引号。
    pub(crate) fn derive_exec_args_with(
        &self,
        command: &str,
        args: Option<&[String]>,
    ) -> Vec<String> {
        match self {
            Shell::Bash | Shell::Zsh | Shell::Sh => {
                // POSIX `sh -c 'script' name arg...`：name 占 $0，arg 进 $1/$@。
                // Harness 固定 $0 为 `_`，模型只关心 $1 起。
                let mut v = vec![
                    self.path().to_string(),
                    "-c".to_string(),
                    command.to_string(),
                ];
                if let Some(a) = args.filter(|a| !a.is_empty()) {
                    v.push("_".to_string());
                    v.extend(a.iter().cloned());
                }
                v
            }
            Shell::PowerShell | Shell::WindowsPowerShell => {
                if let Some(a) = args.filter(|a| !a.is_empty()) {
                    // -CommandWithArgs：首参是脚本，后续空格分隔的 CommandParameters 原样进 $args
                    let mut v = vec![
                        self.path().to_string(),
                        "-NoLogo".to_string(),
                        "-NoProfile".to_string(),
                        "-NonInteractive".to_string(),
                        "-ExecutionPolicy".to_string(),
                        "Bypass".to_string(),
                        "-InputFormat".to_string(),
                        "Text".to_string(),
                        "-OutputFormat".to_string(),
                        "Text".to_string(),
                        "-CommandWithArgs".to_string(),
                        command.to_string(),
                    ];
                    v.extend(a.iter().cloned());
                    v
                } else {
                    let encoded = ps_encode(command);
                    vec![
                        self.path().to_string(),
                        "-NoLogo".to_string(),
                        "-NoProfile".to_string(),
                        "-NonInteractive".to_string(),
                        "-ExecutionPolicy".to_string(),
                        "Bypass".to_string(),
                        // 强制 Text 格式：-EncodedCommand 默认在管道重定向时会以 CLIXML 序列化 ErrorRecord，
                        // 导致 Harness 捕获的 stderr 变成 XML。显式 Text 保证与 -Command 行为一致。
                        "-InputFormat".to_string(),
                        "Text".to_string(),
                        "-OutputFormat".to_string(),
                        "Text".to_string(),
                        "-EncodedCommand".to_string(),
                        encoded,
                    ]
                }
            }
            Shell::Cmd => {
                vec![
                    self.path().to_string(),
                    "/c".to_string(),
                    command.to_string(),
                ]
            }
        }
    }
}

/// PowerShell `-EncodedCommand` 要求的编码：UTF-16LE → Base64（RFC 4648）。
/// 与 `pwsh -EncodedCommand` 文档一致：`[Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($cmd))`
pub(crate) fn ps_encode(command: &str) -> String {
    let utf16le: Vec<u8> = command
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    base64_encode(&utf16le)
}

pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
pub(crate) fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for ch in input.chars() {
        if ch == '=' {
            break;
        }
        if ch.is_whitespace() {
            continue;
        }
        let val = match ch {
            'A'..='Z' => ch as u32 - 'A' as u32,
            'a'..='z' => ch as u32 - 'a' as u32 + 26,
            '0'..='9' => ch as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            _ => return Err(format!("invalid base64 char: {ch}")),
        };
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Ok(out)
}

pub(crate) fn executable_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    executable_in_dirs(name, std::env::split_paths(&path))
}

pub(crate) fn executable_in_dirs(
    name: &str,
    dirs: impl IntoIterator<Item = std::path::PathBuf>,
) -> bool {
    #[cfg(windows)]
    let candidates = if std::path::Path::new(name).extension().is_some() {
        vec![name.to_string()]
    } else {
        ["exe", "cmd", "bat", "com"]
            .into_iter()
            .map(|extension| format!("{name}.{extension}"))
            .collect()
    };
    #[cfg(not(windows))]
    let candidates = [name.to_string()];

    dirs.into_iter().any(|dir| {
        candidates
            .iter()
            .any(|candidate| is_executable_file(&dir.join(candidate)))
    })
}

/// Find `bash` on Windows PATH, skipping known WSL wrapper locations
/// (System32, WindowsApps). Returns the full path on success.
#[cfg(windows)]
pub(crate) fn find_bash_on_path() -> Option<String> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let dir_s = dir.to_string_lossy().to_lowercase();
        // Windows System32 contains WSL's bash.exe launcher — skip it.
        if dir_s.contains("\\system32") || dir_s.contains("\\windowsapps") {
            continue;
        }
        let candidate = dir.join("bash.exe");
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

pub(crate) fn is_executable_file(path: &std::path::Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    true
}
