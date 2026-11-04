//! qaqh-pty — unified exec 的进程会话后端（对齐 codex `codex-rs/utils/pty` 范式）。
//!
//! # 定位
//!
//! 本 crate 只回答一个问题：**把一个 argv 变成可交互的进程会话**。它不关心
//! 审批、策略、registry、工具面——那些属于上层 `qaqh-process-tools`（P2 的
//! unified-exec manager）。会话有两种对称后端：
//!
//! - [`SessionKind::Pty`]：portable-pty（Unix openpty / Windows ConPTY）。
//!   stdin/stdout/stderr 合流为单个终端流，`writer` 写入的字节就是终端输入。
//!   Unix 子进程是新会话 leader（PID == PGID）；Windows 通过 ConPTY 管理。
//! - [`SessionKind::Pipe`]：`std::process` + `process_group(0)`，stdout 与
//!   stderr 经转发线程合流为单流，是 `tty=false` 的退化路径。
//! - [`spawn_pipe_split`]：执行会话专用的 pipe 入口，stdout/stderr 保持独立，
//!   交给上层 collector 在同一 offset 序列中定序。
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
//! Unix 使用 openpty，Windows 使用 portable-pty 的 ConPTY 后端；Windows
//! ConPTY 运行行为仍需 Windows 主机验收。

#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
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

/// Pipe backend IO with source identity preserved for independent collectors.
pub struct SplitPipeIo {
    pub stdout: Box<dyn std::io::Read + Send>,
    pub stderr: Box<dyn std::io::Read + Send>,
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
    #[cfg(any(unix, windows))]
    Pty {
        child: Box<dyn portable_pty::Child + Send + Sync>,
        killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
        #[cfg(windows)]
        interrupt_writer: std::sync::Arc<std::sync::Mutex<Box<dyn std::io::Write + Send>>>,
        #[cfg(unix)]
        master_fd: OwnedFd,
    },
    #[cfg(unix)]
    Pipe { child: std::process::Child },
    #[cfg(not(unix))]
    #[allow(dead_code)]
    Unsupported,
}

#[cfg(windows)]
struct SharedPtyWriter(std::sync::Arc<std::sync::Mutex<Box<dyn std::io::Write + Send>>>);

#[cfg(windows)]
impl std::io::Write for SharedPtyWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .flush()
    }
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
            #[cfg(any(unix, windows))]
            SessionInner::Pty { child, .. } => {
                Ok(child.try_wait()?.map(|status| pty_exit_of(&status)))
            }
            #[cfg(unix)]
            SessionInner::Pipe { child } => Ok(child.try_wait()?.map(|status| SessionExit {
                code: status.code(),
            })),
            #[cfg(not(unix))]
            SessionInner::Unsupported => Err(std::io::Error::other(
                "qaqh-pty: legacy session backend is not implemented on this platform",
            )),
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

    /// Make the PTY master nonblocking so registry writes can observe
    /// cancellation/deadlines instead of sleeping inside a blocked `write(2)`.
    /// The duplicated master descriptors share this flag; the output pump must
    /// retry `WouldBlock` while waiting for new output.
    pub fn set_pty_writer_nonblocking(&self) -> std::io::Result<()> {
        #[cfg(unix)]
        if let SessionInner::Pty { master_fd, .. } = &self.inner {
            let fd = master_fd.as_raw_fd();
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if flags & libc::O_NONBLOCK != 0 {
                return Ok(());
            }
            if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
                return Err(std::io::Error::last_os_error());
            }
            return Ok(());
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "qaqh-pty: nonblocking master is only available for PTY sessions",
        ))
    }
}

/// Pipe-backed child with stdout/stderr preserved as separate streams.
///
/// Unlike [`SpawnedSession`] returned by [`spawn_pipe`], this type does not
/// merge streams. It is the execution-session backend because each stream must
/// retain its source while sharing one collector offset sequence.
#[cfg(unix)]
pub struct SpawnedPipeSession {
    pub pid: u32,
    pub stdout: Box<dyn std::io::Read + Send>,
    pub stderr: Box<dyn std::io::Read + Send>,
    pub writer: Box<dyn std::io::Write + Send>,
    child: std::process::Child,
}

#[cfg(windows)]
pub struct SpawnedPipeSession {
    pub pid: u32,
    pub stdout: Box<dyn std::io::Read + Send>,
    pub stderr: Box<dyn std::io::Read + Send>,
    pub writer: Box<dyn std::io::Write + Send>,
    child: std::process::Child,
}

#[cfg(not(any(unix, windows)))]
pub struct SpawnedPipeSession;

#[cfg(unix)]
impl std::fmt::Debug for SpawnedPipeSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnedPipeSession")
            .field("pid", &self.pid)
            .finish()
    }
}

#[cfg(unix)]
impl SpawnedPipeSession {
    /// Split stream readers and stdin from the process-control handle.
    pub fn into_parts(mut self) -> (SplitPipeIo, Self) {
        let stdout = std::mem::replace(&mut self.stdout, Box::new(std::io::empty()));
        let stderr = std::mem::replace(&mut self.stderr, Box::new(std::io::empty()));
        let writer = std::mem::replace(&mut self.writer, Box::new(std::io::sink()));
        (
            SplitPipeIo {
                stdout,
                stderr,
                writer,
            },
            self,
        )
    }

    /// Nonblocking reap: `Ok(None)` means the process is still running.
    pub fn try_wait(&mut self) -> std::io::Result<Option<SessionExit>> {
        Ok(self.child.try_wait()?.map(|status| SessionExit {
            code: status.code(),
        }))
    }

    pub fn wait(&mut self) -> std::io::Result<SessionExit> {
        let status = self.child.wait()?;
        Ok(SessionExit {
            code: status.code(),
        })
    }

    /// Send SIGINT to the child's process group.
    pub fn interrupt(&mut self) -> std::io::Result<()> {
        signal_group(self.pid, libc::SIGINT)
    }

    /// Send SIGKILL to the child's process group.
    pub fn kill_group(&mut self) -> std::io::Result<()> {
        signal_group(self.pid, libc::SIGKILL)
    }
}

#[cfg(windows)]
impl std::fmt::Debug for SpawnedPipeSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnedPipeSession")
            .field("pid", &self.pid)
            .finish()
    }
}

#[cfg(windows)]
impl SpawnedPipeSession {
    pub fn into_parts(mut self) -> (SplitPipeIo, Self) {
        let stdout = std::mem::replace(&mut self.stdout, Box::new(std::io::empty()));
        let stderr = std::mem::replace(&mut self.stderr, Box::new(std::io::empty()));
        let writer = std::mem::replace(&mut self.writer, Box::new(std::io::sink()));
        (
            SplitPipeIo {
                stdout,
                stderr,
                writer,
            },
            self,
        )
    }

    pub fn try_wait(&mut self) -> std::io::Result<Option<SessionExit>> {
        Ok(self.child.try_wait()?.map(|status| SessionExit {
            code: status.code(),
        }))
    }

    pub fn wait(&mut self) -> std::io::Result<SessionExit> {
        let status = self.child.wait()?;
        Ok(SessionExit {
            code: status.code(),
        })
    }

    /// Ask a process group created with CREATE_NEW_PROCESS_GROUP to stop, then
    /// let the execution manager escalate to tree termination if it ignores it.
    pub fn interrupt(&mut self) -> std::io::Result<()> {
        const CTRL_BREAK_EVENT: u32 = 1;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GenerateConsoleCtrlEvent(ctrl_event: u32, process_group_id: u32) -> i32;
        }
        // SAFETY: The child PID is also its process group ID because it was
        // launched with CREATE_NEW_PROCESS_GROUP.
        let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, self.pid) };
        if sent != 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// Terminate the owned process tree, including descendants holding pipes open.
    pub fn kill_group(&mut self) -> std::io::Result<()> {
        kill_windows_process_tree(self.pid).or_else(|_| self.child.kill())
    }
}

/// 阻塞收割会话。
pub fn wait_session(session: &mut SpawnedSession) -> std::io::Result<SessionExit> {
    match &mut session.inner {
        #[cfg(any(unix, windows))]
        SessionInner::Pty { child, .. } => {
            let status = child.wait()?;
            Ok(pty_exit_of(&status))
        }
        #[cfg(unix)]
        SessionInner::Pipe { child } => {
            let status = child.wait()?;
            Ok(SessionExit {
                code: status.code(),
            })
        }
        #[cfg(not(unix))]
        SessionInner::Unsupported => Err(std::io::Error::other(
            "qaqh-pty: legacy session backend is not implemented on this platform",
        )),
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

#[cfg(windows)]
fn pty_exit_of(status: &portable_pty::ExitStatus) -> SessionExit {
    SessionExit {
        code: Some(i32::try_from(status.exit_code()).unwrap_or(-1)),
    }
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
    #[cfg(windows)]
    {
        match &mut session.inner {
            SessionInner::Pty {
                interrupt_writer, ..
            } => {
                let mut writer = match interrupt_writer.try_lock() {
                    Ok(writer) => writer,
                    Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
                    Err(std::sync::TryLockError::WouldBlock) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::WouldBlock,
                            "PTY input is busy while sending Ctrl-C",
                        ));
                    }
                };
                std::io::Write::write_all(&mut **writer, &[0x03])
            }
            SessionInner::Unsupported => Err(std::io::Error::other(
                "qaqh-pty: legacy session backend is not implemented on this platform",
            )),
        }
    }
    #[cfg(not(any(unix, windows)))]
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
    #[cfg(windows)]
    {
        match &mut session.inner {
            SessionInner::Pty { killer, .. } => {
                kill_windows_process_tree(session.pid).or_else(|_| killer.kill())
            }
            SessionInner::Unsupported => Err(std::io::Error::other(
                "qaqh-pty: legacy session backend is not implemented on this platform",
            )),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = session;
        Err(std::io::Error::other(
            "qaqh-pty: kill is not supported on this platform yet",
        ))
    }
}

#[cfg(windows)]
fn kill_windows_process_tree(pid: u32) -> std::io::Result<()> {
    use std::process::{Command, Stdio};

    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "taskkill failed with status {status}"
        )))
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

#[cfg(any(unix, windows))]
fn anyhow_to_io(error: anyhow::Error) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

#[cfg(unix)]
fn duplicate_owned_fd(fd: RawFd) -> std::io::Result<OwnedFd> {
    let duplicate = unsafe { libc::dup(fd) };
    if duplicate == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}

/// Keep `Read`'s blocking contract while the shared PTY master fd is
/// nonblocking for cancellable writes.
#[cfg(unix)]
struct PollingPtyReader {
    reader: Box<dyn std::io::Read + Send>,
    poll_fd: OwnedFd,
}

#[cfg(unix)]
impl std::io::Read for PollingPtyReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            let mut descriptor = libc::pollfd {
                fd: self.poll_fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut descriptor, 1, -1) };
            if ready == -1 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }

            let terminal =
                descriptor.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0;
            match self.reader.read(buffer) {
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock && terminal => {
                    return Ok(0);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                result => return result,
            }
        }
    }
}

/// 在 **PTY** 后端上启动会话（`tty=true` 路径）。
#[cfg(any(unix, windows))]
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
    #[cfg(unix)]
    let master_raw_fd = pair
        .master
        .as_raw_fd()
        .ok_or_else(|| std::io::Error::other("portable-pty master has no raw fd"))?;
    #[cfg(unix)]
    // Keep one master descriptor in the session handle so the process executor
    // can set O_NONBLOCK; PollingPtyReader preserves blocking reads on the shared fd.
    let master_fd = duplicate_owned_fd(master_raw_fd)?;
    #[cfg(unix)]
    let reader_poll_fd = duplicate_owned_fd(master_raw_fd)?;

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

    #[cfg(unix)]
    let reader: Box<dyn std::io::Read + Send> = Box::new(PollingPtyReader {
        reader: pair.master.try_clone_reader().map_err(anyhow_to_io)?,
        poll_fd: reader_poll_fd,
    });
    #[cfg(windows)]
    let reader = pair.master.try_clone_reader().map_err(anyhow_to_io)?;
    let raw_writer = pair.master.take_writer().map_err(anyhow_to_io)?;
    #[cfg(unix)]
    let writer = raw_writer;
    #[cfg(windows)]
    let (writer, interrupt_writer) = {
        let shared_writer = std::sync::Arc::new(std::sync::Mutex::new(raw_writer));
        let interrupt_writer = std::sync::Arc::clone(&shared_writer);
        (
            Box::new(SharedPtyWriter(shared_writer)) as Box<dyn std::io::Write + Send>,
            interrupt_writer,
        )
    };
    let child = pair.slave.spawn_command(command).map_err(anyhow_to_io)?;
    let pid = child.process_id().unwrap_or_default();
    let killer = child.clone_killer();
    // On Unix the child is a new session leader (PID == PGID). ConPTY uses a
    // separate process handle; Windows tree termination is delegated to taskkill.
    // 父进程必须关闭 slave 副本，子进程退出后 master 读端才能见到 EOF。
    drop(pair.slave);

    Ok(SpawnedSession {
        pid,
        kind: SessionKind::Pty,
        reader,
        writer,
        inner: SessionInner::Pty {
            child,
            killer,
            #[cfg(windows)]
            interrupt_writer,
            #[cfg(unix)]
            master_fd,
        },
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
            // SAFETY: `write_dup` 是本线程独有的 dup 写端，未包装给任何其它所有权；
            // 交给 `File` 独占接管，线程结束时 drop 即关闭。
            let mut writer = unsafe { std::fs::File::from_raw_fd(write_dup) };
            let mut reader = source;
            let _ = std::io::copy(&mut reader, &mut writer);
            // drop(writer) 关闭本线程的写端副本。
        });
    }
    // 父进程侧关闭原始写端：EOF 判定只取决于两个转发线程。
    // SAFETY: 归还我们独有且尚未包装的 fd，立即关闭。
    unsafe { libc::close(write_fd) };

    // SAFETY: `read_fd` 由 `pipe2` 创建后尚未包装，本函数持有唯一所有权；
    // 交给 `File` 接管，随 `SpawnedSession` drop 关闭。
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

/// Spawn a pipe-backed session without combining stdout and stderr.
#[cfg(unix)]
pub fn spawn_pipe_split(spec: &SessionSpec) -> std::io::Result<SpawnedPipeSession> {
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
    command.envs(spec.env.iter().map(|(key, value)| (key, value)));
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command.spawn()?;
    let pid = child.id();
    let stdout = child.stdout.take().expect("stdout is configured as piped");
    let stderr = child.stderr.take().expect("stderr is configured as piped");
    let writer = child.stdin.take().expect("stdin is configured as piped");

    Ok(SpawnedPipeSession {
        pid,
        stdout: Box::new(stdout),
        stderr: Box::new(stderr),
        writer: Box::new(writer),
        child,
    })
}

/// Spawn a split pipe session on Windows. `taskkill /T` is the escalation path
/// for descendants; the process group also supports a graceful Ctrl+Break.
#[cfg(windows)]
pub fn spawn_pipe_split(spec: &SessionSpec) -> std::io::Result<SpawnedPipeSession> {
    use std::os::windows::process::CommandExt as _;
    use std::process::{Command, Stdio};

    if spec.argv.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "qaqh-pty: empty argv",
        ));
    }

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let mut command = Command::new(&spec.argv[0]);
    command
        .args(&spec.argv[1..])
        .env_clear()
        .envs(spec.env.iter().map(|(key, value)| (key, value)))
        .creation_flags(CREATE_NEW_PROCESS_GROUP)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }

    let mut child = command.spawn()?;
    let pid = child.id();
    let stdout = child.stdout.take().expect("stdout is configured as piped");
    let stderr = child.stderr.take().expect("stderr is configured as piped");
    let writer = child.stdin.take().expect("stdin is configured as piped");
    Ok(SpawnedPipeSession {
        pid,
        stdout: Box::new(stdout),
        stderr: Box::new(stderr),
        writer: Box::new(writer),
        child,
    })
}

/// 默认入口：PTY 后端。管道路径请显式 [`spawn_pipe`]（调用方显式声明
/// `tty=false` 的语义归属）。
#[cfg(any(unix, windows))]
pub fn spawn(spec: &SessionSpec) -> std::io::Result<SpawnedSession> {
    spawn_pty(spec)
}

#[cfg(not(any(unix, windows)))]
pub fn spawn(_spec: &SessionSpec) -> std::io::Result<SpawnedSession> {
    Err(std::io::Error::other(
        "qaqh-pty: PTY backend is not implemented on this platform yet (P3: ConPTY)",
    ))
}

#[cfg(not(any(unix, windows)))]
pub fn spawn_pty(_spec: &SessionSpec) -> std::io::Result<SpawnedSession> {
    spawn(_spec)
}

#[cfg(not(unix))]
pub fn spawn_pipe(_spec: &SessionSpec) -> std::io::Result<SpawnedSession> {
    Err(std::io::Error::other(
        "qaqh-pty: pipe backend is not implemented on this platform yet",
    ))
}

#[cfg(not(any(unix, windows)))]
pub fn spawn_pipe_split(_spec: &SessionSpec) -> std::io::Result<SpawnedPipeSession> {
    Err(std::io::Error::other(
        "qaqh-pty: split pipe backend is not implemented on this platform yet",
    ))
}
