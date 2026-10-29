//! qaqh-pty — unified exec 的进程会话后端（对齐 codex `codex-rs/utils/pty` 范式）。
//!
//! # 定位
//!
//! 本 crate 只回答一个问题：**把一个 argv 变成可交互的进程会话**。它不关心
//! 审批、策略、registry、工具面——那些属于上层 `qaqh-process-tools`（P2 的
//! unified-exec manager）。会话有两种对称后端：
//!
//! - [`SessionKind::Pty`]：portable-pty（Unix openpty）。子进程被建立为
//!   **新会话 leader**（PID == PGID），stdin/stdout/stderr 合流为单个终端
//!   流，`writer` 写入的字节就是终端输入——`\u{3}`（Ctrl-C）、`\u{4}`
//!   （Ctrl-D）等控制字符天然具有终端语义。
//! - [`SessionKind::Pipe`]：`std::process` + `process_group(0)`，stdout 与
//!   stderr 经转发线程合流为单流。这是 `tty=false` 的退化路径，也是
//!   Windows（ConPTY 落地前，见下）的兼容后端。
//!
//! # 为什么"合并 process 和 exec"从这里开始
//!
//! 旧 exec 的 `background_after_secs` + `process check/wait` 是两套工具两套
//! 心智；codex 的答案是**一个会话模型**：命令跑满 `yield_time_ms` 仍存活
//! 就返回 session_id，之后用 `write_stdin`（空 chars = 轮询）继续收割。
//! 本 crate 提供该模型的最底层原语：
//!
//! - 单一合流输出流（PTY 天然如此；Pipe 后端用转发线程合成）；
//! - `writer`：进程存活期间可反复写入（stdin 续写）；
//! - [`SpawnedSession::wait`] / [`SpawnedSession::interrupt`] /
//!   [`SpawnedSession::kill_group`]：收割与终止，一律按**进程组**（PTY 下
//!   即会话）操作，孙进程不再逃逸。
//!
//! # 沙箱整合（qaqh 特有）
//!
//! helper 即 PTY child：上层用 `qaqh_sandbox::prepare_pty_launch` 把 argv
//! 改写为 `[helper, __qaqh-sandbox-exec-pty, payload_file]`——helper 从
//! **文件**读策略（stdin 协议与 PTY 冲突），应用 Landlock/seccomp 后
//! `execvp` 目标，且**不**把 stdin 重定向到 /dev/null（终端必须保留）。
//! 本 crate 对此完全无感知：它只 spawn 交给它的 argv。
//!
//! # 平台状态
//!
//! Linux 是默认主路径（已测试）。Windows：P1 仅保留 Pipe 后端的编译占位
//! （返回 unsupported），ConPTY 按 `portable_pty` 的 ConPtySystem 在 P3
//! 落地——Windows 属"未验证"而非"必须验证后才合入"。

use std::path::PathBuf;

/// 会话后端。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// PTY（终端）后端。
    Pty,
    /// 管道后端（stdout/stderr 合流）。
    Pipe,
}

/// 会话启动参数。`env` 是**完整替换**的环境表（调用方负责 env_clear +
/// 白名单语义，与 direct_exec 的审计 M1 约定一致）。
#[derive(Debug, Clone)]
pub struct SessionSpec {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<PathBuf>,
    /// PTY 初始列数（默认 80）。
    pub cols: u16,
    /// PTY 初始行数（默认 24）。
    pub rows: u16,
}

impl SessionSpec {
    pub fn new(argv: Vec<String>) -> Self {
        Self {
            argv,
            env: Vec::new(),
            cwd: None,
            cols: 80,
            rows: 24,
        }
    }
}

/// 会话退出状态。`code: None` 表示被信号终止（如 kill_group）或退出码
/// 不可得——调用方按"非正常退出"处理。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionExit {
    pub code: Option<i32>,
}

/// 会话的 IO 端（[`SpawnedSession::into_parts`] 拆出，可移交泵线程）。
pub struct SessionIo {
    pub reader: Box<dyn std::io::Read + Send>,
    pub writer: Box<dyn std::io::Write + Send>,
}

/// 一个已启动的进程会话：合流输出读端 + stdin 写端 + 组级终止原语。
pub struct SpawnedSession {
    pub pid: u32,
    pub kind: SessionKind,
    pub reader: Box<dyn std::io::Read + Send>,
    pub writer: Box<dyn std::io::Write + Send>,
    inner: SessionInner,
}

enum SessionInner {
    #[cfg(unix)]
    Pty {
        child: Box<dyn portable_pty::Child + Send + Sync>,
        killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    },
    #[cfg(unix)]
    Pipe { child: std::process::Child },
}

impl std::fmt::Debug for SpawnedSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnedSession")
            .field("pid", &self.pid)
            .field("kind", &self.kind)
            .finish()
    }
}

impl SpawnedSession {
    /// 拆解出 IO 端与收割句柄：读/写端交给泵线程，句柄留在调用方做
    /// try_wait/kill 轮询（unified_exec 管理器的标准用法）。
    pub fn into_parts(mut self) -> (SessionIo, SpawnedSession) {
        let reader = std::mem::replace(&mut self.reader, Box::new(std::io::empty()));
        let writer = std::mem::replace(&mut self.writer, Box::new(std::io::sink()));
        (SessionIo { reader, writer }, self)
    }

    /// 非阻塞收割：`Ok(None)` = 仍在运行。
    pub fn try_wait(&mut self) -> std::io::Result<Option<SessionExit>> {
        match &mut self.inner {
            #[cfg(unix)]
            SessionInner::Pty { child, .. } => {
                Ok(child.try_wait()?.map(|status| pty_exit_of(&status)))
            }
            #[cfg(unix)]
            SessionInner::Pipe { child } => {
                Ok(child.try_wait()?.map(|status| SessionExit { code: status.code() }))
            }
        }
    }

    /// 阻塞收割会话。
    pub fn wait(&mut self) -> std::io::Result<SessionExit> {
        wait_session(self)
    }

    /// 向**整个进程组**发送 SIGINT（PTY 下等价于终端 Ctrl-C）。
    pub fn interrupt(&mut self) -> std::io::Result<()> {
        interrupt_session(self)
    }

    /// 向**整个进程组**发送 SIGKILL。
    pub fn kill_group(&mut self) -> std::io::Result<()> {
        kill_session_group(self)
    }
}

/// 阻塞收割会话。
pub fn wait_session(session: &mut SpawnedSession) -> std::io::Result<SessionExit> {
    match &mut session.inner {
        #[cfg(unix)]
        SessionInner::Pty { child, .. } => {
            let status = child.wait()?;
            Ok(pty_exit_of(&status))
        }
        #[cfg(unix)]
        SessionInner::Pipe { child } => {
            let status = child.wait()?;
            Ok(SessionExit { code: status.code() })
        }
    }
}

#[cfg(unix)]
fn pty_exit_of(status: &portable_pty::ExitStatus) -> SessionExit {
    // portable-pty 的信号死亡用独立的 signal() 字段表达，此时
    // exit_code() 是无意义的默认值——统一映射为 code: None。
    let code = if status.signal().is_some() {
        None
    } else {
        Some(i32::try_from(status.exit_code()).unwrap_or(-1))
    };
    SessionExit { code }
}

/// 向**整个进程组**发送 SIGINT（PTY 下等价于终端 Ctrl-C）。
pub fn interrupt_session(session: &mut SpawnedSession) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        signal_group(session.pid, libc::SIGINT)?;
        if let SessionInner::Pty { killer, .. } = &mut session.inner {
            // 兜底：若 cached PGID 已失真，直杀再试一次（NotFound = 已死，忽略）。
            let _ = killer.kill();
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = session;
        Err(std::io::Error::other(
            "qaqh-pty: interrupt is not supported on this platform yet",
        ))
    }
}

/// 向**整个进程组**发送 SIGKILL。PTY 子进程是会话 leader（PID == PGID），
/// Pipe 子进程以 `process_group(0)` 启动（PGID == PID），故两种后端统一
/// `kill(-pgid)`；孙进程（sleep、守护进程、REPL 派生）不再逃逸。
pub fn kill_session_group(session: &mut SpawnedSession) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        signal_group(session.pid, libc::SIGKILL)?;
        if let SessionInner::Pty { killer, .. } = &mut session.inner {
            let _ = killer.kill();
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = session;
        Err(std::io::Error::other(
            "qaqh-pty: kill is not supported on this platform yet",
        ))
    }
}

#[cfg(unix)]
fn signal_group(pid: u32, sig: i32) -> std::io::Result<()> {
    // SAFETY: kill(2) 是无指针系统调用；负数 pgid 表示按进程组投递。
    let rc = unsafe { libc::kill(-(pid as libc::pid_t), sig) };
    if rc == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) || err.kind() == std::io::ErrorKind::NotFound {
        // 组内已无进程（自然退出/已被收割，ESRCH）——按幂等成功处理。
        return Ok(());
    }
    Err(err)
}

#[cfg(unix)]
fn anyhow_to_io(error: anyhow::Error) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

/// 在 **PTY** 后端上启动会话（`tty=true` 路径）。
#[cfg(unix)]
pub fn spawn_pty(spec: &SessionSpec) -> std::io::Result<SpawnedSession> {
    if spec.argv.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "qaqh-pty: empty argv",
        ));
    }
    let system = portable_pty::native_pty_system();
    let pair = system
        .openpty(portable_pty::PtySize {
            rows: spec.rows,
            cols: spec.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(anyhow_to_io)?;

    let mut command = portable_pty::CommandBuilder::new(&spec.argv[0]);
    for arg in &spec.argv[1..] {
        command.arg(arg);
    }
    if let Some(cwd) = &spec.cwd {
        command.cwd(cwd);
    }
    // 与 direct_exec 同款契约：整体替换环境，绝不继承 daemon 环境。
    command.env_clear();
    for (key, value) in &spec.env {
        command.env(key, value);
    }

    // portable-pty 在 Unix 上把子进程建立为新会话 leader（PID == PGID），
    // 组级终止因此可以统一按 `kill(-pid)` 处理。
    let child = pair.slave.spawn_command(command).map_err(anyhow_to_io)?;
    let pid = child.process_id().unwrap_or_default();
    let killer = child.clone_killer();
    let reader = pair.master.try_clone_reader().map_err(anyhow_to_io)?;
    let writer = pair.master.take_writer().map_err(anyhow_to_io)?;
    // 父进程必须关闭 slave 副本，子进程退出后 master 读端才能见到 EOF。
    drop(pair.slave);

    Ok(SpawnedSession {
        pid,
        kind: SessionKind::Pty,
        reader,
        writer,
        inner: SessionInner::Pty { child, killer },
    })
}

/// 在**管道**后端上启动会话（`tty=false` 路径）。stdout 与 stderr 经转发
/// 线程合流进同一条读端；流间顺序不保证（与 PTY 合流语义一致）。
#[cfg(unix)]
pub fn spawn_pipe(spec: &SessionSpec) -> std::io::Result<SpawnedSession> {
    use std::os::fd::FromRawFd;
    use std::process::{Command, Stdio};

    if spec.argv.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "qaqh-pty: empty argv",
        ));
    }
    let mut command = Command::new(&spec.argv[0]);
    command.args(&spec.argv[1..]);
    command.env_clear();
    command.envs(spec.env.iter().map(|(k, v)| (k, v)));
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    // H6 延续：独立进程组，kill_group 可覆盖孙进程。
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command.spawn()?;

    // 合流管道：两个转发线程分别把 stdout/stderr 泵进同一条 pipe 写端。
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: pipe(2) 写入调用方提供的 2 元素数组，无指针参与。
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        let err = std::io::Error::last_os_error();
        let _ = child.kill();
        let _ = child.wait();
        return Err(err);
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);

    let mut sources: Vec<Box<dyn std::io::Read + Send>> = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        sources.push(Box::new(stdout));
    }
    if let Some(stderr) = child.stderr.take() {
        sources.push(Box::new(stderr));
    }
    for source in sources {
        // 每个转发线程持有独立 dup 的写端；线程退出即关闭，两条都关闭后
        // 读端才能见到 EOF。
        // SAFETY: dup 复制一个我们独有的合法 fd；新 fd 由 File/DupFd 接管。
        let write_dup = unsafe { libc::dup(write_fd) };
        if write_dup < 0 {
            let err = std::io::Error::last_os_error();
            let _ = child.kill();
            let _ = child.wait();
            return Err(err);
        }
        std::thread::spawn(move || {
            let mut writer = unsafe { std::fs::File::from_raw_fd(write_dup) };
            let mut reader = source;
            let _ = std::io::copy(&mut reader, &mut writer);
            // drop(writer) 关闭本线程的写端副本。
        });
    }
    // 父进程侧关闭原始写端：EOF 判定只取决于两个转发线程。
    // SAFETY: 归还我们独有且尚未包装的 fd，立即关闭。
    unsafe { libc::close(write_fd) };

    let reader = unsafe { std::fs::File::from_raw_fd(read_fd) };
    let writer = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("qaqh-pty: child stdin unavailable"))?;

    Ok(SpawnedSession {
        pid: child.id(),
        kind: SessionKind::Pipe,
        reader: Box::new(reader),
        writer: Box::new(writer),
        inner: SessionInner::Pipe { child },
    })
}

/// 默认入口：PTY 后端。管道路径请显式 [`spawn_pipe`]（调用方显式声明
/// `tty=false` 的语义归属）。
#[cfg(unix)]
pub fn spawn(spec: &SessionSpec) -> std::io::Result<SpawnedSession> {
    spawn_pty(spec)
}

#[cfg(not(unix))]
pub fn spawn(_spec: &SessionSpec) -> std::io::Result<SpawnedSession> {
    Err(std::io::Error::other(
        "qaqh-pty: PTY backend is not implemented on this platform yet (P3: ConPTY)",
    ))
}

#[cfg(not(unix))]
pub fn spawn_pty(_spec: &SessionSpec) -> std::io::Result<SpawnedSession> {
    spawn(_spec)
}

#[cfg(not(unix))]
pub fn spawn_pipe(_spec: &SessionSpec) -> std::io::Result<SpawnedSession> {
    Err(std::io::Error::other(
        "qaqh-pty: pipe backend is not implemented on this platform yet",
    ))
}
