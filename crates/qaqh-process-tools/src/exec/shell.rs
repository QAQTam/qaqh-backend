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
/// 传空串 = no-op。钉住的路径在 `Shell::path()` 中优先于 Windows bash 钉定
/// （niubash shim 或 git-bash 绝对路径）、
/// 候选解析与名义名，但并非处处最高：`cmd`/`WindowsPowerShell` 仍先取
/// System32 系统壳固定路径（审计 M2）。此前 `Zsh`/`Sh` 完全不消费该值，
/// 使 macOS（默认壳恰为 Zsh）上的钉壳成为静默 no-op——Zsh 现与
/// Bash/PowerShell 同序消费；`Sh` 从不作为任何平台的默认壳，维持原样。
pub fn register_shell(path: &str) {
    Shell::register_shell(path);
}

/// 用户 shell 终端面（ADR-0028 §D4 修订）：把闭集壳名解析成**登录形态** argv
/// （不带 `-c`/`-Command`，spawn 后由 PTY 自然进入交互）。词表与 exec 模型面
/// 的 `shell=` 参数**同源**——[`Shell::from_name`] 同一张表、`Shell::path()` 同
/// 一条钉序（git-bash 绝对路径、cmd/WindowsPowerShell 的 System32 钉死均照抄），
/// 所以这里不构成第二条注入面，也不构成第二份探测实现。未知名返回 `None`，
/// 由调用方给出拒绝文案；本函数不猜、不落回默认壳。
pub fn user_shell_argv(name: &str) -> Option<(Vec<String>, String)> {
    let shell = Shell::from_name(name)?;
    let path = shell.path().to_string();
    let argv = match shell {
        Shell::PowerShell | Shell::WindowsPowerShell => vec![path.clone()],
        _ => vec![path.clone()],
    };
    Some((argv, path))
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
/// 必须按壳分槽：候选集按壳各自持有（见 `executable_candidates`），
/// 「谁命中/是否可用」对每个壳是独立事实，避免先探测的一方污染另一方。
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
    /// parameter). Windows `bash` resolves to the Niubash install's rubash
    /// engine shim first, else Git-for-Windows / MSYS2, avoiding the WSL
    /// wrapper. Unknown names fall back to None so the caller can report a
    /// clean error.
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "bash" | "bash4windows" => {
                #[cfg(windows)]
                {
                    if let Some(shim) = niubash_shim() {
                        DETECTED_BASH_PATH.get_or_init(|| shim.to_string());
                        return Some(Shell::Bash);
                    }
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

    /// Platform-priority auto-detection candidates. exec 的「未指定 shell」
    /// 选择循环（handler 的 find(available)）逐字消费这张表：Windows 上装了
    /// Niubash 时 Bash（钉定 = shim）压过 PowerShell，未装时维持历史序。
    pub(crate) fn auto_candidates() -> &'static [Shell] {
        #[cfg(windows)]
        {
            if niubash_shim().is_some() {
                return &[Shell::Bash, Shell::PowerShell, Shell::WindowsPowerShell, Shell::Cmd];
            }
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
            // macOS 自带 zsh（系统 bash 长期停在 3.2），默认判序 zsh > bash。
            &[Shell::Zsh, Shell::Bash]
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

    /// 已解析的候选可执行**绝对路径**（进程内一次性；`None` = 候选集全不可用）。
    /// 与 `available()` 共用同一份口径，保证探测与派生同源。
    ///
    /// 审计 M2（2026-10-01）：解析结果必须钉到绝对路径且**只查 PATH，绝不含
    /// 当前工作目录**——Windows `CreateProcess` 对裸名的搜索序包含 cwd，模型
    /// 可先在工作区投放改造过的 `pwsh.exe`，此后每次 exec 实际启动的都是投放
    /// 的 shell，壳探测的全部语义（EncodedCommand 等）即被绕过。
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
            .find_map(qaqh_types::platform::find_on_path)
            .map(|path| path.to_string_lossy().into_owned());
        let _ = cache.set(resolved);
        cache.get().and_then(Option::as_deref)
    }

    pub(crate) fn detect_uncached() -> Self {
        #[cfg(windows)]
        {
            // Windows 优先级：niubash(rubash 引擎 shim) > pwsh 7 > Git for
            // Windows bash > powershell 5.1 > cmd。shim 不在任何 PATH 目录里
            // （安装器只把安装根挂上 PATH，bash.exe 在 winuxcmd\bin），必须按
            // 布局定位；命中时钉进 DETECTED_BASH_PATH，`path()`/`available()`
            // 与派生链同源。
            if let Some(shim) = niubash_shim() {
                DETECTED_BASH_PATH.get_or_init(|| shim.to_string());
                return Shell::Bash;
            }
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
            // macOS 优先级：zsh > bash（系统 bash 长期停在 3.2，zsh 才是默认壳）。
            for shell in [Shell::Zsh, Shell::Bash] {
                if shell.available() {
                    return shell;
                }
            }
            Shell::Zsh
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
    /// 各壳的优先级并不统一：`Bash` 为 显式注册 > Windows bash 钉定
    /// （`DETECTED_BASH_PATH`：niubash shim 优先，其次 git-bash 绝对路径）
    /// > 候选集解析出的**绝对路径**（`resolved_shell_name`，仅 PATH 查找）>
    /// 名义名 `bash`；`PowerShell` 为 显式注册 > 候选解析 > `pwsh`；
    /// `cmd`/`WindowsPowerShell` 先取 Windows 系统壳固定路径（审计 M2），
    /// 再到显式注册 > 候选解析 > 名义名；`Zsh` 为 显式注册 > 候选解析 >
    /// 名义名 `zsh`（macOS 默认壳即 Zsh，钉壳必须生效）；`Sh` 只用候选
    /// 解析，回落到名义名 `sh`（不使用显式注册）。
    /// 候选解析是「探测与派生同源」的关键：探测口径与实际 argv 都经
    /// `resolved_shell_name` 落到同一绝对路径，二者不再漂移。
    pub(crate) fn path(&self) -> &str {
        let registered = REGISTERED_SHELL_PATH.get();
        let resolved = self.resolved_shell_name();
        match self {
            Shell::Bash => registered
                .or_else(|| DETECTED_BASH_PATH.get())
                .map(String::as_str)
                .or(resolved)
                .unwrap_or("bash"),
            Shell::Zsh => registered.map(String::as_str).or(resolved).unwrap_or("zsh"),
            Shell::Sh => resolved.unwrap_or("sh"),
            Shell::PowerShell => registered
                .map(String::as_str)
                .or(resolved)
                .unwrap_or("pwsh"),
            Shell::WindowsPowerShell => {
                windows_system_shell(&SYSTEM_POWERSHELL_PATH, SYSTEM_POWERSHELL_REL)
                    .as_deref()
                    .or(registered.map(String::as_str))
                    .or(resolved)
                    .unwrap_or("powershell")
            }
            Shell::Cmd => windows_system_shell(&SYSTEM_CMD_PATH, SYSTEM_CMD_REL)
                .as_deref()
                .or(registered.map(String::as_str))
                .or(resolved)
                .unwrap_or("cmd"),
        }
    }

    /// 候选可执行名集（探测与实际派生必须同源）。当前每个壳只列其自身
    /// 名义名（`bash`/`zsh`/`sh`/`pwsh`/`powershell`/`cmd`），不存在同族
    /// 别名降级；探测与派生都经此集合解析出**绝对路径**，故两者口径一致
    /// （O-3「探测 ≠ 派生」缺口由此闭合）。
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
    /// Win32 命令行解析中的转义地狱。**输出侧编码**不做假设则必坏：pwsh 以
    /// `[Console]::OutputEncoding`（缺省 = OEM 码页，中文 Windows 为 CP936）解码
    /// 子进程字节再以同码页写出，`$s = git log` 这类捕获形态得到的是「合法
    /// UTF-8 的乱码」，reader 侧 UTF-8 优先 + OEM 兜底（[`super::pipe`]）救不回
    /// ——故所有 PowerShell 系脚本一律前置 [`PS_UTF8_PREAMBLE`] 统一到 UTF-8。
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
        // 生产路径：探测已装 uutils coreutils（Windows 安装器布局）再派生。
        // 探测为每次 exec 两次 is_file（微秒级），不缓存，便于测试注入
        // （见 [`Self::derive_exec_args_with_uutils`]）。决策见 docs/adr/0012。
        self.derive_exec_args_with_uutils(
            command,
            args,
            super::uutils::detect_coreutils_bin().as_deref(),
        )
    }

    /// [`Self::derive_exec_args_with`] 的可注入形态：测试传 `None` 消除
    /// 「机器是否装有 coreutils」的差异，两侧得到同一 argv。
    pub(crate) fn derive_exec_args_with_uutils(
        &self,
        command: &str,
        args: Option<&[String]>,
        coreutils_bin: Option<&std::path::Path>,
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
                // 统一 UTF-8 preamble 进脚本正文（两条 argv 形态共用），
                // 见 [`PS_UTF8_PREAMBLE`] 的实测依据；已装 uutils coreutils 时
                // 前缀追加 PATH 前插与最小冲突别名手术（docs/adr/0012）。
                // 首行重置 `$LASTEXITCODE`（防同会话残留）+ 尾部传播段
                // （[`PS_EXIT_CODE_TAIL`]）：先查 native 码再查 `$?`，防
                // 尾随 cmdlet 把 `$?` 刷真吞掉 native 失败。
                let prefix = super::uutils::pwsh_script_prefix(coreutils_bin);
                let script =
                    format!("{prefix}\n$global:LASTEXITCODE = 0\n{command}\n{PS_EXIT_CODE_TAIL}");
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
                        script,
                    ];
                    v.extend(a.iter().cloned());
                    v
                } else {
                    let encoded = ps_encode(&script);
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

/// Windows 系统壳相对 `SystemRoot` 的固定位置（审计 M2）。
const SYSTEM_CMD_REL: &str = r"System32\cmd.exe";
const SYSTEM_POWERSHELL_REL: &str = r"System32\WindowsPowerShell\v1.0\powershell.exe";

/// 系统壳固定路径的进程内缓存（`None` = 该位置不存在）。缓存后 `path()`
/// 可以安全返回 `&'static str`。
static SYSTEM_CMD_PATH: OnceLock<Option<String>> = OnceLock::new();
static SYSTEM_POWERSHELL_PATH: OnceLock<Option<String>> = OnceLock::new();

/// Windows 系统组件壳的固定绝对路径：cmd / powershell 钉到 System32 下，
/// PATH 形态异常时也不落回裸名搜索（裸名在 Windows 会先搜当前目录）。
/// 非 Windows 平台恒 `None`（这两个壳本就只在 Windows 语义里出现）。
fn windows_system_shell(
    cache: &'static OnceLock<Option<String>>,
    relative: &str,
) -> &'static Option<String> {
    cache.get_or_init(|| {
        #[cfg(windows)]
        {
            let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
            let candidate = std::path::Path::new(&root).join(relative);
            candidate
                .is_file()
                .then(|| candidate.to_string_lossy().into_owned())
        }
        #[cfg(not(windows))]
        {
            let _ = relative;
            None
        }
    })
}

/// PowerShell 系的统一 UTF-8 前置语句（作为脚本正文首行注入，
/// `-EncodedCommand` 与 `-CommandWithArgs` 两条形态共用）。
///
/// 一条赋值链同时钉三处编码（右结合，实测 pwsh 7.6 / 5.1 均可用）：
/// - `[Console]::OutputEncoding`：pwsh 解码子进程输出字节 + 写自身重定向
///   stdout 的编码。不设置时缺省 = OEM 码页（中文 Windows CP936），git 等
///   UTF-8 程序的输出会经历「UTF-8 字节 →按 936 解码成乱码字符串 →按
///   UTF-8/936 写出」的双重转码，reader 侧任何字节级兜底都救不回
///   （实测 `$s = git log --format=%s` → `涓枃…`；裸 `git log` 因字节透传
///   侥幸正确，掩盖了这一缺口）。
/// - `[Console]::InputEncoding`：控制台语境下把子进程输入 CP 切到 65001
///   （write_stdin 会话喂中文、ConPTY 输入方向依赖它）。
/// - `$OutputEncoding`：PS 向管道下游原生程序喂 stdin 的编码。
///
/// UTF-8 无 BOM（`new($false)`）：BOM 会作为 U+FEFF 混进首行输出。
/// setter 在 stdin=null / 输出重定向 / ConPTY 三种宿主形态实测无异常
/// （pwsh 7.6.9 + 5.1，`$ErrorActionPreference=Stop` 下 exit 0）。
pub(crate) const PS_UTF8_PREAMBLE: &str = "$OutputEncoding = [Console]::OutputEncoding = [Console]::InputEncoding = [System.Text.UTF8Encoding]::new($false)";

/// PowerShell 系脚本尾部的退出码传播段（与 [`PS_UTF8_PREAMBLE`] 一样两条
/// argv 形态共用，命令正文接在其前）。
///
/// `pwsh -Command`/`-EncodedCommand` 把**最后一个语句**的成败归一成退出码，
/// native 命令的真实退出码被吞（实测 pwsh 7.7：`cmd /c exit 5` → 1；尾随
/// cmdlet 时 → 0；崩溃码 0xC0000005 → 1）。传播顺序是刻意的：**先查
/// `$LASTEXITCODE` 再查 `$?`**——codex hook 的 `if ($?) { exit 0 }` 前置版
/// 会被「native 失败后尾随 cmdlet 把 `$?` 刷真」骗回 0，对任意模型脚本不可
/// 接受。本会话六案例实测（反序版）：trail-cmdlet=5、纯 cmdlet 成功=0、
/// 显式 `exit 7` 透传、`throw`=1、崩溃码 -1073741819 原样、native 失败=9。
/// 纯 pwsh 语句，Windows PowerShell 5.1 同样可用。
pub(crate) const PS_EXIT_CODE_TAIL: &str = "\
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
if ($?) { exit 0 }
exit 1";

/// PowerShell `-EncodedCommand` 要求的编码：UTF-16LE → Base64（RFC 4648）。
/// 与 `pwsh -EncodedCommand` 文档一致：`[Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($cmd))`
pub(crate) fn ps_encode(command: &str) -> String {
    let utf16le: Vec<u8> = command
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    base64_encode(&utf16le)
}

/// RFC 4648 §4 标准字母表 Base64（带 canonical padding）。
///
/// v4.5 P2-2a（plan §三.4-1）：`base64` crate 取代原手写位循环表。
pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    STANDARD.encode(bytes)
}

#[cfg(test)]
pub(crate) fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    use base64::{Engine as _, engine::general_purpose::STANDARD_PAD_INDIFFERENT};
    // 与退役前契约一致：忽略空白，第一个 `=` 后整段丢弃，padded/无 padding
    // 皆接受。
    let head = input.split('=').next().unwrap_or(input);
    let compact: Vec<u8> = head
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    STANDARD_PAD_INDIFFERENT
        .decode(&compact)
        .map_err(|e| format!("invalid base64: {e}"))
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

/// Niubash 安装树里的 bash shim（执行语义 = 内嵌 rubash 引擎，MIT，
/// Bash 5.3 差分 244/246）。用户级安装布局
/// `%LOCALAPPDATA%\Programs\Niubash\winuxcmd\bin\bash.exe`；shim 把 `-c`
/// 按 bash 语义转发到 niu.exe 内嵌引擎，stdout/stderr 纯净、退出码与
/// `-c` 后位置参数原样透传（niubash 1.5.0 实测）。它不在 PATH 里
/// （安装器只挂安装根），PATH 查找永远发现不了，必须按布局探测。
/// LOCALAPPDATA 缺失或服务账户布局不匹配时为 `None`——探测序退回
/// pwsh 优先，与历史行为逐字节一致。
#[cfg(windows)]
pub(super) fn niubash_shim() -> Option<&'static str> {
    static SHIM: OnceLock<Option<String>> = OnceLock::new();
    SHIM.get_or_init(|| {
        let local = std::path::PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
        [
            local.join(r"Programs\Niubash\winuxcmd\bin\bash.exe"),
            local.join(r"Programs\Niubash\winuxcmd\usr\bin\bash.exe"),
        ]
        .into_iter()
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
    })
    .as_deref()
}

#[cfg(not(windows))]
#[allow(dead_code)] // 仅 Windows 消费（detect/auto_candidates 的 cfg 块内）。
pub(super) fn niubash_shim() -> Option<&'static str> {
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
