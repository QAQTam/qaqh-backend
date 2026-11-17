//! 用户 shell 终端注册表（ADR-0028 切片 2）。
//!
//! # 与 `ExecSessionManager` 的关系：**零共享**，这是设计而不是巧合
//!
//! | 维度 | agent exec 面（`qaqh-process-tools::exec_session`） | 用户 shell 面（本模块） |
//! |---|---|---|
//! | owner | `session_id`（会话 close/delete 连带杀，`service.rs:252-266`） | `instance_key`（device 稳定身份；会话 close **不该**牵连用户终端） |
//! | 字节去向 | `capture.rs` → DB append-only 日志 + `TimelineEntry` → 模型上下文 | 只进内存有界环 + `broadcast`；**永不**进 timeline/DB/上下文 |
//! | 视图 | 模型侧要 ANSI 剥净 + ConPTY 重排合行（`clean_pty_model_text`） | 原始字节，**绕开 reflower**（D6：模型侧判据「CUP 列参数 == 终端列数」只属于那边） |
//! | 尺寸 | 硬编码 80×24 | UI 测量 + 活 resize（切片 1 的 `SpawnedSession::resize`） |
//! | 谁能量 | agent tool（`exec_command`/`write_stdin`） | 只有配对客户端；**不注册任何 agent tool**，所以 LLM 既看不到也打不进（D2） |
//!
//! 断联不 kill：与 `ExecStopReason`（`Interrupt|Terminate|TurnCancel|Timeout|
//! Shutdown`，无 client 档）同律。回收只有三条显式路（close / device revoke /
//! daemon shutdown），外加一条 shell 独有的**无人观看 idle 回收**，由
//! `server.rs` 那条既有 60s 周期任务调 [`ShellRegistry::sweep_idle`]。

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::{broadcast, watch};

/// 用户 shell 的默认几何。与前端 `ShellTerminal.tsx` 的兜底值同律，
/// UI 测量到真实值后立刻 resize 覆盖。
pub const SHELL_DEFAULT_COLS: u16 = 100;
pub const SHELL_DEFAULT_ROWS: u16 = 24;

/// attach 时可回看的原始字节上限。与 `EXEC_OUTPUT_DEFAULT_MAX_BYTES`(256KiB)
/// 同量级 —— 但这里是**原始 ANSI 字节**，不是给模型的文本视图。
pub const SHELL_REPLAY_CAP: usize = 256 * 1024;

/// 单个观看者（ws 连接）的滞后容忍度：满了由 `broadcast::Receiver::recv()` 报
/// `RecvError::Lagged`，观看方据此重新 `attach(from_offset)` 取回放窗口 ——
/// 与既有 exec 面"落后不掐读泵"同律，慢客户端不许把读泵钉住。
const SHELL_VIEWER_CAPACITY: usize = 512;

/// 无人观看且无输入活动的回收阈值。**刻意不做成 config 字段**：`ConfigDto` /
/// `ConfigPatch` 是导出的用户空间 DTO（`qaqh-config/src/dto.rs:84,111` 的
/// `session_idle_unload_secs` 就是活证），把它塞进去等于让一个内部阈值变成
/// 又一次 wire 实变的搭车项。要用户可调时再单独立项。
pub const SHELL_IDLE_REAP_SECS: u64 = 600;

/// 打开一条 shell 的请求面。**只有几何与目录**：shell 由 daemon 决定（D4 ——
/// 少一个参数就少一条注入面，且"进程归后端所有"本来就是这次迁移的理由）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellOpen {
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    pub cwd: Option<String>,
}

/// 一帧原始输出 + 它在整条会话里的字节游标。游标只用于**回放定位**，
/// 不是 `SessionEventFrame.seq` 那种跨重连的会话事实游标（D3 约束 4）。
///
/// 这里**只有字节**：终止事实走 [`ShellAttach::stopped`] 那条 watch，不往裸字节流里
/// 塞虚构帧 —— 往 xterm 的输入流里编一个「会话结束」的字节序列就是让客户端猜。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellFrame {
    pub offset: u64,
    pub bytes: Arc<[u8]>,
}

/// shell 的可观测状态（`shell.open` / `shell.list` 的作答原料）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellInfo {
    pub id: String,
    pub pid: u32,
    pub shell: String,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    pub alive: bool,
    /// 已产出的原始字节总数（attach 回放用游标的上界）。
    pub output_offset: u64,
}

/// attach 的结果：状态 + 从 `from_offset` 起的回放段 + 实时帧订阅。
pub struct ShellAttach {
    pub info: ShellInfo,
    pub replay: Vec<u8>,
    pub from_offset: u64,
    pub next_offset: u64,
    pub frames: broadcast::Receiver<ShellFrame>,
    /// 终止事实的观测口：`None` = 还活着，`Some(reason)` = 已经死了，原因就是 D5 那
    /// 三条显式 kill 路之一或 `ChildExited`。订阅一次就拿到**当前值**（所以「子进程
    /// 自己退出后晚来的观看者」也能读到真相，不必轮询 `alive`，也不必有哨兵字节），
    /// 之后 `changed()` 只往已终止的方向走（切片 4 的 `closed` 控制帧就来自这里）。
    pub stopped: watch::Receiver<Option<ShellStopReason>>,
}

/// shell 终止原因。**没有** `ClientDisconnected` 这一档（断联不 kill）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellStopReason {
    Closed,
    IdleReaped,
    OwnerRevoked,
    DaemonShutdown,
    ChildExited,
}

impl ShellStopReason {
    /// wire 上的原因字符串口径（`ShellCloseResult.reason`）；snake_case 与
    /// `ExecStopReason` 的序列化风格同律。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::IdleReaped => "idle_reaped",
            Self::OwnerRevoked => "owner_revoked",
            Self::DaemonShutdown => "daemon_shutdown",
            Self::ChildExited => "child_exited",
        }
    }
}

/// 日志与 wire 用**同一个**拼写：`{}` 走 `as_str()`，不许再出现 `Closed` 这种
/// 与 `ShellCloseResult.reason` 不同的第二事实源（`{:?}` 留给调试用 Debug）。
impl std::fmt::Display for ShellStopReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 注册表句柄（可克隆，daemon 装配时塞进 AppState；切片 3/4 接线）。
#[derive(Clone, Default)]
pub struct ShellRegistry {
    shells: Arc<Mutex<HashMap<String, Arc<LiveShell>>>>,
}

struct LiveShell {
    id: String,
    owner_instance_key: String,
    shell: String,
    cwd: String,
    pid: u32,
    /// 控制面（resize/几何读回/收割/组杀）。与读泵分持：读泵只拿 reader，
    /// 打字只拿 `stdin`，三者互不阻塞。
    control: Mutex<qaqh_pty::SpawnedSession>,
    stdin: Mutex<Box<dyn Write + Send>>,
    /// 读泵启动时一次性取走（`take()`），此后线程独占 —— 与 control/stdin 分持，
    /// 所以 resize 与打字都不会被慢读拖住。
    reader: Mutex<Option<Box<dyn Read + Send>>>,
    output: Mutex<OutputRing>,
    frames: broadcast::Sender<ShellFrame>,
    /// 终止事实的唯一真源。用 `watch` 而不是再开一条 broadcast：`watch` 保留
    /// **当前值**，所以「先死、后 attach」的观看者也读得到原因（broadcast 会把
    /// 没订阅时发过的那帧永远丢掉）。`None` = 还活着。
    stopped: watch::Sender<Option<ShellStopReason>>,
    last_activity: Mutex<Instant>,
    alive: std::sync::atomic::AtomicBool,
    /// 最后一次**可信**的几何：open 时是请求值，之后每次从内核读成功就刷新。
    /// master 关掉/子进程已逝时 `info()` 回报这一份，而不是编一个默认尺寸
    /// （一个退出的 132×44 终端被报成 100×24 就是给用户看的谎，宪法 5）。
    geometry: Mutex<(u16, u16)>,
}

/// 有界原始字节环：保留尾部 `SHELL_REPLAY_CAP`，同时记账**全量**偏移。
/// 这里刻意不复用 `exec/head_tail.rs`——那份要按 stdout/stderr 分源、要在
/// UTF-8 边界上给模型造文本；shell 要的是一条裸字节流 + 可靠游标。
struct OutputRing {
    bytes: VecDeque<u8>,
    total: u64,
}

impl OutputRing {
    fn new() -> Self {
        Self { bytes: VecDeque::new(), total: 0 }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend(chunk);
        self.total += chunk.len() as u64;
        while self.bytes.len() > SHELL_REPLAY_CAP {
            self.bytes.pop_front();
        }
    }

    fn tail_from(&self, from_offset: u64) -> Vec<u8> {
        let oldest = self.total.saturating_sub(self.bytes.len() as u64);
        let start = from_offset.saturating_sub(oldest).min(self.bytes.len() as u64) as usize;
        self.bytes.range(start..).copied().collect()
    }
}

impl ShellRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&self, owner_instance_key: &str, request: ShellOpen) -> Result<ShellInfo, String> {
        let cols = request.cols.unwrap_or(SHELL_DEFAULT_COLS);
        let rows = request.rows.unwrap_or(SHELL_DEFAULT_ROWS);
        let (argv, shell_label) = default_shell_argv();
        let cwd = resolve_cwd(request.cwd.as_deref())?;

        // spec.env 是**完整替换表**（spawn_pty 内 env_clear）。用户 shell 的语义
        // 就是"用户自己的环境"，所以这里显式快照 daemon 进程环境 —— 与前端
        // node-pty 旧实现的 `env: {...process.env}` 同形，是功能对齐而不是破口。
        let mut spec = qaqh_pty::SessionSpec::new(argv);
        spec.cols = cols;
        spec.rows = rows;
        spec.cwd = Some(cwd.clone());
        spec.env = std::env::vars().collect();

        let session = qaqh_pty::spawn_pty(&spec).map_err(|error| error.to_string())?;
        let (io, control) = session.into_parts();
        let id = format!("shell_{}", qaqh_ringing_contract::ulid::generate_ulid());
        let pid = control.pid;
        let (frames, _) = broadcast::channel(SHELL_VIEWER_CAPACITY);
        let (stopped, _) = watch::channel(None);
        let shell = Arc::new(LiveShell {
            id: id.clone(),
            owner_instance_key: owner_instance_key.to_string(),
            shell: shell_label.clone(),
            cwd: cwd.display().to_string(),
            pid,
            control: Mutex::new(control),
            stdin: Mutex::new(io.writer),
            reader: Mutex::new(Some(io.reader)),
            output: Mutex::new(OutputRing::new()),
            frames,
            stopped,
            last_activity: Mutex::new(Instant::now()),
            alive: std::sync::atomic::AtomicBool::new(true),
            geometry: Mutex::new((cols, rows)),
        });

        // 观察者先起、后入表：反过来会有一段时间这条会话**已经能被 attach 却永远
        // 读不到字节**（宪法 5：那是个看起来能用的空壳）。起不来就当场拆会话。
        if let Err(error) = spawn_output_watchers(Arc::clone(&shell), self.clone()) {
            let mut control = shell.control.lock().unwrap_or_else(|e| e.into_inner());
            let _ = control.kill_group();
            // 必须关 master：读泵可能已经起来了，而 ConPTY 的写端由 master 持有 ——
            // 不关就是漏一条永不退出的线程（`qaqh-pty/src/lib.rs:119-127`）。
            control.close_terminal();
            drop(control);
            let _ = shell.mark_stopped(ShellStopReason::DaemonShutdown);
            return Err(format!("qaqh-shell: output watchers failed to start: {error}"));
        }
        self.shells
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), Arc::clone(&shell));
        log::info!(
            "[shell] open {} pid={} shell={} cwd={} geometry={}x{}",
            shell.id,
            shell.pid,
            shell.shell,
            shell.cwd,
            cols,
            rows
        );
        Ok(shell.info())
    }

    /// 订阅实时帧并取回从 `from_offset` 起的回放段。`from_offset` 落后于缓冲头
    /// 时按可用段给（`next_offset` 是权威续读点），调用方据此对齐。
    pub fn attach(
        &self,
        owner_instance_key: &str,
        id: &str,
        from_offset: u64,
    ) -> Result<ShellAttach, String> {
        let shell = self.find_owned(owner_instance_key, id)?;
        shell.touch();
        let frames = shell.frames.subscribe();
        // watch 的 subscribe 先于环快照：attach 之后发生的终止一定能在 receiver 里读到，
        // 已经发生过的终止则直接从当前值读到（`Option` 是单向的，只会变成 Some）。
        let stopped = shell.stopped.subscribe();
        let (replay, start, next_offset) = {
            let output = shell.output.lock().unwrap_or_else(|e| e.into_inner());
            let oldest = output.total.saturating_sub(output.bytes.len() as u64);
            (output.tail_from(from_offset), from_offset.max(oldest), output.total)
        };
        Ok(ShellAttach {
            info: shell.info(),
            replay,
            from_offset: start,
            next_offset,
            frames,
            stopped,
        })
    }

    /// 往 shell 的 stdin 注入原始字节（keystrokes 走数据面，不走 RPC —— D4）。
    pub fn write(&self, owner_instance_key: &str, id: &str, bytes: &[u8]) -> Result<usize, String> {
        let shell = self.find_owned(owner_instance_key, id)?;
        shell.touch();
        let mut stdin = shell.stdin.lock().unwrap_or_else(|e| e.into_inner());
        stdin.write_all(bytes).map_err(|error| error.to_string())?;
        stdin.flush().map_err(|error| error.to_string())?;
        Ok(bytes.len())
    }

    /// 变更几何并回报内核里的真值（不回报"请求值"：ConPTY 在退出竞态下会拒）。
    pub fn resize(
        &self,
        owner_instance_key: &str,
        id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<(u16, u16), String> {
        let shell = self.find_owned(owner_instance_key, id)?;
        shell.touch();
        let control = shell.control.lock().unwrap_or_else(|e| e.into_inner());
        control.resize(cols, rows).map_err(|error| error.to_string())?;
        let (cols, rows) = control.terminal_size().map_err(|error| error.to_string())?;
        drop(control);
        *shell.geometry.lock().unwrap_or_else(|e| e.into_inner()) = (cols, rows);
        // 尺寸变更是一次活动：否则窗口刚拉大就被 idle 回收掐掉。
        Ok((cols, rows))
    }

    pub fn list(&self, owner_instance_key: &str) -> Vec<ShellInfo> {
        let mut infos: Vec<ShellInfo> = self
            .shells
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|shell| shell.owner_instance_key == owner_instance_key)
            .map(|shell| shell.info())
            .collect();
        infos.sort_by(|a, b| a.id.cmp(&b.id));
        infos
    }

    pub fn close(&self, owner_instance_key: &str, id: &str) -> Result<ShellStopReason, String> {
        let shell = self.find_owned(owner_instance_key, id)?;
        self.stop_one(&shell, ShellStopReason::Closed)
    }

    /// device 吊销路径（`device_registry.rs:166` revoke 已覆盖 lease 强失效）：
    /// 吊销即杀掉该身份名下所有 shell。
    pub fn shutdown_owner(&self, owner_instance_key: &str) -> usize {
        let victims: Vec<Arc<LiveShell>> = self
            .shells
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|shell| shell.owner_instance_key == owner_instance_key)
            .cloned()
            .collect();
        let count = victims.len();
        for shell in victims {
            let _ = self.stop_one(&shell, ShellStopReason::OwnerRevoked);
        }
        count
    }

    /// daemon 关停（与 `ExecSessionManager::shutdown` 同律：杀干净再走）。
    pub fn shutdown_all(&self) -> usize {
        let victims: Vec<Arc<LiveShell>> =
            self.shells.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
        let count = victims.len();
        for shell in victims {
            let _ = self.stop_one(&shell, ShellStopReason::DaemonShutdown);
        }
        count
    }

    /// 无人观看且无输入活动超过 `ttl_secs` 才回收；0 = 关（与
    /// `session_idle_unload_secs` 同纪律）。挂在既有 60s 周期任务里。
    ///
    /// 条件里**不含** `alive`：已经逝了但仍挂在表里的条目（唯一的路是子进程在
    /// `open` 入表前就退出）也归这里清出去，否则那份 256KiB 环要占到 daemon 关停。
    /// `stop_one` 对逝条目只做摘表，不会再动内核（先到先得，见 `mark_stopped`）。
    pub fn sweep_idle(&self, ttl_secs: u64) -> Vec<String> {
        if ttl_secs == 0 {
            return Vec::new();
        }
        let ttl = std::time::Duration::from_secs(ttl_secs);
        let victims: Vec<Arc<LiveShell>> = self
            .shells
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|shell| {
                let unwatched = shell.frames.receiver_count() == 0;
                let idle = shell
                    .last_activity
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .elapsed()
                    >= ttl;
                unwatched && idle
            })
            .cloned()
            .collect();
        let ids: Vec<String> = victims.iter().map(|shell| shell.id.clone()).collect();
        for shell in victims {
            let _ = self.stop_one(&shell, ShellStopReason::IdleReaped);
        }
        ids
    }

    /// owner 判定在**注册表内部**做：daemon 各入口给的是 `authz.rs` 反推出来的
    /// `instance_key`，不是 params 里的自报字段（D2 的机器面）。错误文案对
    /// "不存在"与"不是你的"给同一句话，避免跨 device 的存在性探针。
    fn find_owned(&self, owner_instance_key: &str, id: &str) -> Result<Arc<LiveShell>, String> {
        let found = self
            .shells
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned();
        match found {
            Some(shell) if shell.owner_instance_key == owner_instance_key => Ok(shell),
            _ => Err(format!(
                "qaqh-shell: no such shell for this caller: {id}"
            )),
        }
    }

    fn stop_one(&self, shell: &Arc<LiveShell>, reason: ShellStopReason) -> Result<ShellStopReason, String> {
        // 先从表里摘掉：并发 attach/write 立刻拿到 unknown shell，而不是
        // 对着一个正在死的会话继续打字。
        self.shells.lock().unwrap_or_else(|e| e.into_inner()).remove(&shell.id);
        // 先到先得：谁写下终止事实，谁负责把内核侧收干净 —— 同一次终止只允许一条路
        // 动手（读泵把子进程自己退出算作 `ChildExited`，显式 close 算作 `Closed`）。
        let (reason, won) = shell.mark_stopped(reason);
        if !won {
            log::info!(
                "[shell] stop {} pid={} skipped: 已由 {reason} 终止",
                shell.id, shell.pid
            );
            return Ok(reason);
        }
        {
            let mut control = shell.control.lock().unwrap_or_else(|e| e.into_inner());
            let _ = control.kill_group();
            // 读泵靠这里拿到 EOF：ConPTY 的写端由 master 持有，子进程退出
            // 不产生 EOF（qaqh-pty lib.rs:119-127 的根因注记）。
            control.close_terminal();
        }
        // 不发哨兵字节帧：观看者从 `ShellAttach::stopped` 这条 watch 读原因
        // （切片 4 的 ws `closed` 控制帧）。往裸字节流里编一帧「会话结束」会让
        // xterm 猜不透，而且那份原因就成了第二事实源。
        log::info!("[shell] stopped {} pid={} reason={}", shell.id, shell.pid, reason.as_str());
        Ok(reason)
    }

    /// 读泵在子进程退出后摘表：只摘自己那一份 `Arc`，并发 close 已经摘过就不动。
    /// **先摘表再碰 control** —— `stop_one` 的顺序是「map → control」，反序即死锁。
    fn forget(&self, shell: &Arc<LiveShell>) {
        let mut shells = self.shells.lock().unwrap_or_else(|e| e.into_inner());
        if shells.get(&shell.id).is_some_and(|live| Arc::ptr_eq(live, shell)) {
            shells.remove(&shell.id);
        }
    }
}

impl LiveShell {
    /// 先到先得地写终止事实，返回 `(权威原因, 是否本次写入)`。
    ///
    /// 为什么必须是「先到先得」：`shell.close` 打到一条刚刚自己退出过的会话时，
    /// 回给调用方的若是 `closed` 就是给用户看的第二个版本的事实（宪法 5）；读泵
    /// 抢到 `ChildExited` 之后任何路都不许改写它。`watch` 一旦成 `Some` 就永不
    /// 回退，所以写完立刻读必然拿得到原因。
    fn mark_stopped(&self, reason: ShellStopReason) -> (ShellStopReason, bool) {
        let won = self.stopped.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(reason);
            true
        });
        if won {
            self.alive.store(false, std::sync::atomic::Ordering::Release);
        }
        let recorded = self.stop_reason().expect(
            "qaqh-shell: mark_stopped 之后 watch 里必有原因,读不到即为逻辑错",
        );
        (recorded, won)
    }

    fn stop_reason(&self) -> Option<ShellStopReason> {
        *self.stopped.borrow()
    }

    fn touch(&self) {
        *self.last_activity.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }

    fn output_total(&self) -> u64 {
        self.output.lock().unwrap_or_else(|e| e.into_inner()).total
    }

    fn info(&self) -> ShellInfo {
        let control = self.control.lock().unwrap_or_else(|e| e.into_inner());
        let geometry = match control.terminal_size() {
            Ok(geometry) => {
                *self.geometry.lock().unwrap_or_else(|e| e.into_inner()) = geometry;
                geometry
            }
            // 读不到内核真值（会话已收割、master 已关）→ 用最后一次可信值。
            Err(_) => *self.geometry.lock().unwrap_or_else(|e| e.into_inner()),
        };
        let (cols, rows) = geometry;
        drop(control);
        ShellInfo {
            id: self.id.clone(),
            pid: self.pid,
            shell: self.shell.clone(),
            cwd: self.cwd.clone(),
            cols,
            rows,
            alive: self.alive.load(std::sync::atomic::Ordering::Relaxed),
            output_offset: self.output_total(),
        }
    }
}

/// 子进程**自己**退出的观察间隔。与 exec 面 `PROCESS_WATCH_INTERVAL`（`manager.rs:25`）
/// 同值同律。
///
/// 为什么必须有这条线程而不是「等读泵拿到 EOF」：ConPTY 的输出管道写端由 master
/// 持有，子进程退出**不产生 EOF**（`qaqh-pty/src/lib.rs:119-127` 的根因注记）。
/// 只盯 read 循环的话，用户敲 `exit` 之后这条会话会永远挂在「还活着」上 —— 那既是
/// 谎（`shell.list` 报 alive），也是泄漏（256KiB 的环 + 表项没人收，`sweep_idle`
/// 按 D5 只动活会话，永远不动它）。
const SHELL_EXIT_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// 每会话两条线程：读泵搬字节，观察线程盯退出。两条都要，是因为上面那段 EOF 纪律。
///
/// 句柄全部丢弃 = 线程脱管(detached)：会话生命周期由 close/sweep/revoke/shutdown
/// 驱动，不靠 join；子进程一逝两条自己收。
fn spawn_output_watchers(shell: Arc<LiveShell>, registry: ShellRegistry) -> std::io::Result<()> {
    spawn_read_pump(Arc::clone(&shell), registry.clone())?;
    let id = shell.id.clone();
    std::thread::Builder::new()
        .name(format!("shell-reap-{id}"))
        .spawn(move || {
            loop {
                let exited = {
                    let mut control = shell.control.lock().unwrap_or_else(|e| e.into_inner());
                    // Err 也按「已逝」处理：ECHILD 之类只可能是别人已经收割过。
                    match control.try_wait() {
                        Ok(Some(_)) | Err(_) => true,
                        Ok(None) => false,
                    }
                };
                if exited || !shell.alive.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                std::thread::sleep(SHELL_EXIT_POLL);
            }
            harvest_child_exit(&shell, &registry);
            // 读泵不靠这里叫醒：上面那次 close_terminal 就是它要的 EOF。
        })
        .map(|_| ())
}

/// 读泵：裸字节进环 + 广播。reader 在 `control` 之外的独立所有权里（`into_parts`
/// 交出来的），所以打字与 resize 都不会被慢读拖住。
fn spawn_read_pump(shell: Arc<LiveShell>, registry: ShellRegistry) -> std::io::Result<()> {
    let mut reader = shell
        .reader
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .expect("shell reader is taken exactly once, right after open");
    std::thread::Builder::new()
        .name(format!("shell-pump-{}", shell.id))
        .spawn(move || {
            let mut buffer = vec![0u8; 8 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        let chunk = &buffer[..read];
                        let offset = {
                            let mut output = shell.output.lock().unwrap_or_else(|e| e.into_inner());
                            output.push(chunk);
                            output.total
                        };
                        let frame = ShellFrame { offset, bytes: Arc::from(chunk) };
                        // 无观看者时 send 失败属正常（环仍在，attach 能回放）。
                        let _ = shell.frames.send(frame);
                    }
                }
            }
            // Unix 上子进程退出会直接给 EOF，于是这条路也可能先到 —— 收摊口是同一个，
            // 先到先得，不会与观察线程双杀。
            harvest_child_exit(&shell, &registry);
        })
        .map(|_| ())
}

/// 子进程自己退出这条路的收尾。**只有抢到终止事实的那一条动手**：摘表、收割、
/// 关 master（`close_terminal` 只能在收割之后调 —— `qaqh-pty/src/lib.rs:316-321`）。
/// 显式 `close`/`revoke`/`shutdown` 已经写下原因的话，这里连日志都不许再造一个事实。
fn harvest_child_exit(shell: &Arc<LiveShell>, registry: &ShellRegistry) {
    let (reason, won) = shell.mark_stopped(ShellStopReason::ChildExited);
    if !won {
        log::info!("[shell] {} 已由 {reason} 收摊，本次不重复动手", shell.id);
        return;
    }
    // 先摘表再碰 control：`stop_one` 的取锁顺序是 map → control，反序即死锁。
    registry.forget(shell);
    {
        let mut control = shell.control.lock().unwrap_or_else(|e| e.into_inner());
        let _ = control.try_wait();
        control.close_terminal();
    }
    log::info!("[shell] exited {} pid={} reason={}", shell.id, shell.pid, reason.as_str());
}

/// daemon 侧决定用哪个 shell —— 客户端不传可执行名（D4：少一个参数少一条注入面）。
///
/// 刻意**不**复用 `registry::detect_shell()`：那个解析的是 agent 的**命令执行**
/// shell（`bootstrap_exec_shell()`，给 `exec_command` 拼 `-c` 用的），与"用户打开
/// 自己终端时要哪个登录 shell"是两个面（D2/D6）。这里的语义与前端 node-pty 旧
/// 实现对齐：Windows 给 `powershell.exe`，Unix 给 `$SHELL`，落回 `/bin/bash`。
fn default_shell_argv() -> (Vec<String>, String) {
    if cfg!(windows) {
        return (vec!["powershell.exe".into(), "-NoLogo".into()], "powershell.exe".into());
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    (vec![shell.clone()], shell)
}

fn resolve_cwd(requested: Option<&str>) -> Result<PathBuf, String> {
    let path = match requested {
        Some(raw) if !raw.trim().is_empty() => PathBuf::from(raw),
        _ => home_dir(),
    };
    if !path.is_dir() {
        return Err(format!("qaqh-shell: cwd is not a directory: {}", path.display()));
    }
    Ok(path)
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests;

