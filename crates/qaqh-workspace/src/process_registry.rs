//! ProcessRegistry — tracks child processes spawned by exec / subagent tools.
//!
//! Enables timeout → inspect → wait/kill flow instead of blind termination.
//! Thread-safe: all access through Mutex, with static convenience methods.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Status of a tracked process.
#[derive(Debug, Clone, PartialEq)]
pub enum ProcStatus {
    Running,
    Exited(i32),
    Killed,
}

/// [`ProcessRegistry::kill`] 的结果。
///
/// 用枚举而非 `bool`：`bool` 无法把「真的清理了 os 进程（组）」与「条目/墓碑
/// 存在但**没有 os_pid 可清理**」区分开，调用方（process kill 回复、subagent
/// serve 端点）会据此谎报「已杀」——PR #57 reviewer 阻断 ②。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillOutcome {
    /// 在册条目被终止（含无 os_pid 的条目：无孤儿可清，状态收敛即完成）。
    Killed,
    /// 条目已驱逐，命中墓碑且按其 os_pid 执行了清理。
    TombstoneCleaned,
    /// 条目/墓碑存在，但从未持有 os_pid（如 subagent 登记路径）——
    /// 没有可清理的残留孤儿，调用方须如实告知而不是报「已杀」。
    NoOsPid,
    /// id 从未登记过，或其墓碑已被容量淘汰。
    NotFound,
}

impl KillOutcome {
    /// 是否真的执行了（或无需执行）终止清理——`false` 只在 [`KillOutcome::NoOsPid`]。
    pub fn cleaned(self) -> bool {
        !matches!(self, KillOutcome::NoOsPid)
    }

    /// 对外文案（process kill 工具回复用），如实描述实际发生了什么。
    pub fn content(self, id: u32) -> String {
        match self {
            KillOutcome::Killed => format!("Process {id} killed."),
            KillOutcome::TombstoneCleaned => {
                format!(
                    "Process {id} entry was evicted; its orphan descendants were cleaned by os_pid."
                )
            }
            KillOutcome::NoOsPid => {
                format!("Process {id} has no os_pid to clean up; nothing to kill.")
            }
            KillOutcome::NotFound => format!("process.kill: process {id} not found"),
        }
    }
}

/// 字节预算内取尾部，起点前移到 char boundary（子进程输出是任意 UTF-8，
/// 直接按字节索引切片会在多字节字符中点 panic）。
fn char_safe_tail(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut start = s.len() - max_bytes;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    // 循环保证 start 落在边界上；get 仅为了通过 string_slice lint。
    s.get(start..).unwrap_or(s)
}

/// 前台 seal 的权威完整捕获缓冲（exec 生命周期重写阶段 2，2026-09）。
///
/// 与 `output`/`stderr` 的 tail 视图（process check 用，≤数千字符）分离：
/// 读线程把每个解码后的 chunk 同时写入 tail 与 full 捕获，seal 以
/// `captured_full` 为唯一权威结果源——任何路径不得等待管道 EOF / 流关闭。
/// 写满 `FULL_CAPTURE_BYTE_CAP` 后丢弃后续 chunk（读线程以 capped 信号上报）。
#[derive(Default)]
pub struct FullCapture {
    text: String,
}

/// 完整捕获字节上限：与旧 exec 前台 HARD_BYTE_CAP 同值，防长驻进程
/// （backgrounded 移交后读线程继续追加）内存无界。
pub const FULL_CAPTURE_BYTE_CAP: usize = 5 * 1024 * 1024;

impl FullCapture {
    fn push(&mut self, chunk: &str) {
        let remaining = FULL_CAPTURE_BYTE_CAP.saturating_sub(self.text.len());
        if remaining == 0 {
            return;
        }
        if chunk.len() <= remaining {
            self.text.push_str(chunk);
        } else {
            // 边界整块截断（不按字节硬切，避免劈开多字节字符）：上游按
            // 8KB 粒度追加，边界丢一块对 5MB 级输出无感知差异，capped
            // 由读线程上报。
            let cut = chunk
                .char_indices()
                .map(|(i, _)| i)
                .take_while(|&i| i <= remaining)
                .last()
                .unwrap_or(0);
            if cut > 0 {
                self.text.push_str(chunk.get(..cut).unwrap_or(chunk));
            }
        }
    }
}

/// One tracked process entry.
pub struct ProcEntry {
    pub id: u32,
    pub name: String,
    pub status: Arc<Mutex<ProcStatus>>,
    /// 进入终态（Exited/Killed）的时刻，`Running` 期间为 `None`。
    /// 惰性驱逐按此计时（BUG-2026-09-13-23：原实现按 `started` 计时，
    /// 长跑后刚结束的条目会被立即驱逐）。
    terminal_at: Arc<Mutex<Option<Instant>>>,
    /// 注册（spawn 受理）时刻；仅用于展示 elapsed 与测试回拨。
    pub started_at: Instant,
    pub output: Arc<Mutex<String>>,
    pub stderr: Arc<Mutex<String>>,
    /// 完整捕获（seal 权威数据源，见 `FullCapture`）。
    pub full_output: Arc<Mutex<FullCapture>>,
    pub full_stderr: Arc<Mutex<FullCapture>>,
    /// Final answer collected from subagent stdout.
    pub answer: Arc<Mutex<Option<String>>>,
    child: Arc<Mutex<Option<std::process::Child>>>,
    /// W4：OS pid 快照——child 句柄被 try_wait 回收后仍可按 pid 清理
    /// 进程树（Windows taskkill /T、Unix killpg），不再依赖句柄存活。
    os_pid: Arc<Mutex<Option<u32>>>,
    /// W4：Exited 被后续清理 kill 覆盖为 Killed 时保留原 exit code。
    last_exit_code: Arc<Mutex<Option<i32>>>,
    /// PTY stdin writer for interactive processes.
    pty_writer: Arc<Mutex<Option<Box<dyn std::io::Write + Send>>>>,
}

/// 终态条目在注册表内的存活时长（惰性驱逐门槛）。
///
/// BUG-2026-09-13-23：原实现按 `started` 计时，长跑进程一进入终态就被立刻
/// 驱逐，os_pid 随之丢失 → 孤儿孙进程无法清理。现按**终态时间**计时。
const EVICT_AFTER_TERMINAL: Duration = Duration::from_secs(600);

/// 墓碑表容量上限。
///
/// 墓碑只是「条目已驱逐，但 id 仍可解释」的短命窗口，不是第二个只进不出的
/// 注册表：无上限会把「注册表单调增长」原样搬到 `tombstones`。超限按驱逐
/// 时间 FIFO 淘汰最旧的墓碑。
///
/// 淘汰即**销毁 os_pid**（见 [`ProcessRegistry::remember_tombstone`]）：被淘汰
/// 的墓碑之后只能报 `NotFound`，绝不再按 pid 清理——pid 可能已被 OS 复用为
/// 别的进程组 id，`killpg` 会误杀活进程组。
pub const TOMBSTONE_CAPACITY: usize = 256;

/// 终态条目的墓碑（驱逐时由 os_pid 快照降级而来）。
///
/// 驱逐只释放内存中的输出快照与句柄，`os_pid` 与终态结果留下墓碑：
/// - `kill` 命中墓碑仍按 os_pid 尽力清理残留后代（原验收标准）；
/// - `get_info` 对墓碑返回 `"evicted": true` 与终态/exit code，
///   使 `process check` 能区分「已结束」与「id 无效」。
pub struct Tombstone {
    pub id: u32,
    pub name: String,
    /// 驱逐时的终态（`Exited(code)` / `Killed`）；`Running` 不可能被驱逐。
    pub status: ProcStatus,
    pub exit_code: Option<i32>,
    pub os_pid: Option<u32>,
    pub started_at: Instant,
    pub evicted_at: Instant,
}

impl Tombstone {
    /// 墓碑 kill 命中后收敛状态：状态置 `Killed`，原 exit code 保留到
    /// `exit_code`，并**销毁 os_pid**（清理只做一次；留下 stale pid 只会
    /// 在后续 kill 中重复 `killpg` 一个可能已被复用的 pid）。
    ///
    /// 状态一致性是关键：subagent 的 `RegistryRef::killed()` 只看
    /// `status == "killed"`，墓碑 kill 若不置 `Killed`，子代理永远读不到
    /// kill 请求（PR #57 reviewer 阻断 ③）。
    fn mark_killed(&mut self) {
        if let ProcStatus::Exited(code) = self.status {
            self.exit_code = Some(code);
        }
        self.status = ProcStatus::Killed;
        self.os_pid = None;
    }
}

/// Global process registry.
static REGISTRY: std::sync::LazyLock<Mutex<ProcessRegistry>> =
    std::sync::LazyLock::new(|| Mutex::new(ProcessRegistry::new()));

pub struct ProcessRegistry {
    entries: HashMap<u32, ProcEntry>,
    /// 已驱逐条目的墓碑，**按驱逐时间有序**（`push_back`），受
    /// [`TOMBSTONE_CAPACITY`] 约束：超限时 `pop_front` 淘汰最旧者。
    /// id 不复用，故一个 id 至多一条。
    tombstones: VecDeque<Tombstone>,
    next_id: u32,
}

impl ProcessRegistry {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            tombstones: VecDeque::new(),
            next_id: 1,
        }
    }

    fn with<R>(f: impl FnOnce(&mut ProcessRegistry) -> R) -> R {
        f(&mut REGISTRY.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// 惰性驱逐已进入终态超过 [`EVICT_AFTER_TERMINAL`] 的条目。
    ///
    /// 驱逐前把 `os_pid` 与终态结果降级为墓碑（见 [`Tombstone`]）：输出快照
    /// 与 Child 句柄释放，但按 os_pid 清理孤儿的路径与「已结束 vs id 无效」
    /// 的区分能力保留（BUG-2026-09-13-23）。
    fn evict_stale_entries(&mut self, now: Instant) {
        let mut stale: Vec<(u32, Tombstone)> = Vec::new();
        for (id, e) in &self.entries {
            let status = e.status.lock().unwrap_or_else(|er| er.into_inner()).clone();
            if matches!(status, ProcStatus::Running) {
                continue;
            }
            // 单次加锁读终态时刻；缺失（不应发生）则补记为当下，下一轮再驱逐。
            let terminal_at = {
                let mut guard = e.terminal_at.lock().unwrap_or_else(|er| er.into_inner());
                match *guard {
                    Some(t) => t,
                    None => {
                        *guard = Some(now);
                        continue;
                    }
                }
            };
            if now.saturating_duration_since(terminal_at) <= EVICT_AFTER_TERMINAL {
                continue;
            }
            let exit_code = *e.last_exit_code.lock().unwrap_or_else(|er| er.into_inner());
            stale.push((
                *id,
                Tombstone {
                    id: *id,
                    name: e.name.clone(),
                    status,
                    exit_code,
                    os_pid: *e.os_pid.lock().unwrap_or_else(|er| er.into_inner()),
                    started_at: e.started_at,
                    evicted_at: now,
                },
            ));
        }
        for (id, tombstone) in stale {
            if let Some(e) = self.entries.remove(&id) {
                *e.child.lock().unwrap_or_else(|er| er.into_inner()) = None;
            }
            log::debug!(
                "[registry] evicted terminal process {id} ({}), os_pid={:?} kept as tombstone",
                tombstone.name,
                tombstone.os_pid
            );
            self.remember_tombstone(tombstone);
        }
    }

    /// 记入墓碑并施加容量上限（FIFO 淘汰最旧者）。
    ///
    /// 淘汰即销毁 `os_pid` 所承载的清理能力：被淘汰的 id 之后只报
    /// [`KillOutcome::NotFound`]，**绝不**再按该 pid 做 `killpg`——驱逐出的
    /// pid 可能已被 OS 复用（PR #57 reviewer 阻断 ①）。
    fn remember_tombstone(&mut self, tombstone: Tombstone) {
        self.tombstones.push_back(tombstone);
        while self.tombstones.len() > TOMBSTONE_CAPACITY {
            if let Some(dropped) = self.tombstones.pop_front() {
                log::debug!(
                    "[registry] tombstone {} dropped (capacity {}), os_pid={:?} cleanup info discarded",
                    dropped.id,
                    TOMBSTONE_CAPACITY,
                    dropped.os_pid
                );
            }
        }
    }

    fn tombstone(&self, id: u32) -> Option<&Tombstone> {
        self.tombstones.iter().find(|t| t.id == id)
    }

    fn tombstone_mut(&mut self, id: u32) -> Option<&mut Tombstone> {
        self.tombstones.iter_mut().find(|t| t.id == id)
    }

    // ── Static convenience methods ──

    /// Register a new process. Returns the assigned id.
    pub fn register(name: &str) -> u32 {
        Self::with(|r| {
            // W6：注册表只进不出会随长会话单调涨；注册前惰性驱逐
            // 终态超过 10 分钟的条目（输出快照随之释放）。
            //
            // BUG-2026-09-13-23：门槛按**终态时间**（`terminal_at`）而非
            // 注册时间（`started`）计时——长跑后刚结束的进程其 os_pid 仍有
            // 清理价值，按 started 计时会被立刻驱逐。
            r.evict_stale_entries(std::time::Instant::now());
            let id = r.next_id;
            r.next_id = r.next_id.saturating_add(1);
            if r.next_id == u32::MAX {
                log::error!("[registry] process id space exhausted");
            }
            r.entries.insert(
                id,
                ProcEntry {
                    id,
                    name: name.to_string(),
                    status: Arc::new(Mutex::new(ProcStatus::Running)),
                    full_output: Arc::new(Mutex::new(FullCapture::default())),
                    full_stderr: Arc::new(Mutex::new(FullCapture::default())),
                    terminal_at: Arc::new(Mutex::new(None)),
                    started_at: Instant::now(),
                    output: Arc::new(Mutex::new(String::new())),
                    stderr: Arc::new(Mutex::new(String::new())),
                    answer: Arc::new(Mutex::new(None)),
                    child: Arc::new(Mutex::new(None)),
                    os_pid: Arc::new(Mutex::new(None)),
                    last_exit_code: Arc::new(Mutex::new(None)),
                    pty_writer: Arc::new(Mutex::new(None)),
                },
            );
            id
        })
    }

    /// Attach an OS child handle to an entry.
    pub fn attach_child(id: u32, child: std::process::Child) {
        Self::with(|r| {
            if let Some(entry) = r.entries.get(&id) {
                let os_pid = Some(child.id());
                *entry.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
                *entry.os_pid.lock().unwrap_or_else(|e| e.into_inner()) = os_pid;
            }
        });
    }

    /// 非阻塞查询子进程是否退出；已退出返回 exit code 并释放句柄、更新状态。
    /// 子进程句柄唯一持有在注册表（attach_child 移入），direct_exec 的
    /// poll 循环经此查询，避免 Child 双重持有。
    ///
    /// **终态自动更新**：检测到退出即置 `Exited`（幂等）。状态刷新不依赖
    /// 管道 EOF——孙进程可能持有管道写端导致 EOF 永不到达（如 cargo test
    /// 泄漏的后台 serve），若等 EOF 才 mark_exited，`process check/wait`
    /// 会永远显示 running。任何查询路径（exec 轮询、check、wait）经此刷新。
    pub fn try_wait(id: u32) -> Option<i32> {
        Self::with(|r| {
            let entry = r.entries.get(&id)?;
            // 终态缓存：child 句柄已释放，直接返回退出码（不再触碰句柄）
            match *entry.status.lock().unwrap_or_else(|e| e.into_inner()) {
                ProcStatus::Exited(code) => return Some(code),
                ProcStatus::Killed => return None,
                ProcStatus::Running => {}
            }
            let mut child_opt = entry.child.lock().unwrap_or_else(|e| e.into_inner());
            let child = child_opt.as_mut()?;
            match child.try_wait().ok()? {
                Some(status) => {
                    let code = status.code().unwrap_or(-1);
                    *child_opt = None;
                    *entry
                        .last_exit_code
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = Some(code);
                    *entry.status.lock().unwrap_or_else(|e| e.into_inner()) =
                        ProcStatus::Exited(code);
                    *entry.terminal_at.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(Instant::now());
                    Some(code)
                }
                None => None,
            }
        })
    }

    /// Write text to a process's PTY stdin. Returns true if the write succeeded.
    pub fn write_to(id: u32, text: &str) -> Result<usize, String> {
        let writer_arc = Self::with(|r| {
            r.entries.get(&id).and_then(|e| {
                if matches!(
                    *e.status.lock().unwrap_or_else(|e| e.into_inner()),
                    ProcStatus::Running
                ) {
                    Some(e.pty_writer.clone())
                } else {
                    None
                }
            })
        })
        .ok_or_else(|| format!("process {id} not found or not running"))?;

        let mut guard = writer_arc.lock().map_err(|e| format!("lock: {e}"))?;
        match guard.as_mut() {
            Some(w) => {
                // W5：write_all 保证部分写不谎报全成；WouldBlock 短暂轮询
                // 直至写完或调用方超时放弃。
                let bytes = text.as_bytes();
                let mut written = 0usize;
                loop {
                    match w.write(&bytes[written..]) {
                        Ok(0) => return Err("write: zero-length write".to_string()),
                        Ok(n) => {
                            written += n;
                            if written == bytes.len() {
                                return Ok(bytes.len());
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(20));
                        }
                        Err(e) => return Err(format!("write: {e}")),
                    }
                }
            }
            None => Err(format!("process {id} has no PTY stdin (not interactive)")),
        }
    }

    /// W-low①：读取已捕获 stdout/stderr 快照（取消收集超时兜底用）。
    pub fn captured(id: u32) -> Option<(String, String)> {
        Self::with(|r| {
            r.entries.get(&id).map(|e| {
                (
                    e.output.lock().unwrap_or_else(|er| er.into_inner()).clone(),
                    e.stderr.lock().unwrap_or_else(|er| er.into_inner()).clone(),
                )
            })
        })
    }

    /// 完整捕获快照（exec 生命周期重写阶段 2）：seal 的权威数据源。
    /// 返回 (stdout, stderr)；条目不存在（已被惰性驱逐）返回 None。
    pub fn captured_full(id: u32) -> Option<(String, String)> {
        Self::with(|r| {
            r.entries.get(&id).map(|e| {
                (
                    e.full_output
                        .lock()
                        .unwrap_or_else(|er| er.into_inner())
                        .text
                        .clone(),
                    e.full_stderr
                        .lock()
                        .unwrap_or_else(|er| er.into_inner())
                        .text
                        .clone(),
                )
            })
        })
    }

    /// 子进程是否仍在运行（读线程 settle 判定用）。
    /// Killed/Exited/条目缺失均视为"不再运行"——status 单调，一旦离开
    /// Running 不会回退。
    pub fn is_running(id: u32) -> bool {
        Self::with(|r| {
            r.entries.get(&id).is_some_and(|e| {
                matches!(
                    *e.status.lock().unwrap_or_else(|er| er.into_inner()),
                    ProcStatus::Running
                )
            })
        })
    }

    /// Mark a process as exited.
    ///
    /// 单调性守卫（T-1-5）：只有 `Running` 才允许改写为 `Exited`。`Killed` 是
    /// 由 `kill` 写入的终态，迟到的读线程 settle（管道 EOF）不得把它覆盖回
    /// `Exited`——否则 `RegistryRef::killed()`（只看 `status == "killed"`）会
    /// 丢失 kill 信号，而 `is_running` 的同文件注释已声明「status 单调，一旦
    /// 离开 Running 不会回退」。句柄释放与终态时刻仍照常维护（幂等）。
    pub fn mark_exited(id: u32, code: i32) {
        Self::with(|r| {
            if let Some(entry) = r.entries.get(&id) {
                {
                    let mut status = entry.status.lock().unwrap_or_else(|e| e.into_inner());
                    if matches!(*status, ProcStatus::Running) {
                        *status = ProcStatus::Exited(code);
                    }
                }
                *entry.child.lock().unwrap_or_else(|e| e.into_inner()) = None;
                *entry.terminal_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
            }
        });
    }

    /// Set the final answer for a subagent process.
    pub fn set_answer(id: u32, answer: String) {
        Self::with(|r| {
            if let Some(entry) = r.entries.get(&id) {
                *entry.answer.lock().unwrap_or_else(|e| e.into_inner()) = Some(answer);
            }
        });
    }

    /// Append stdout output to a tracked process.
    pub fn append_output(id: u32, chunk: &str) {
        Self::with(|r| {
            if let Some(entry) = r.entries.get(&id) {
                entry
                    .full_output
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(chunk);
                let mut out = entry.output.lock().unwrap_or_else(|e| e.into_inner());
                out.push_str(chunk);
                if out.chars().count() > 5000 {
                    // W-low②：按字符数裁剪（原实现字节计长+字符跳过，
                    // CJK 输出保留量最多缩水到 1/3 甚至清空）。
                    *out = crate::process_registry::char_safe_tail(out.as_str(), 4000).to_string();
                }
            }
        });
    }

    /// Append stderr output.
    pub fn append_stderr(id: u32, chunk: &str) {
        Self::with(|r| {
            if let Some(entry) = r.entries.get(&id) {
                entry
                    .full_stderr
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(chunk);
                let mut err = entry.stderr.lock().unwrap_or_else(|e| e.into_inner());
                err.push_str(chunk);
                if err.chars().count() > 3000 {
                    // W-low②：同上，字符口径。
                    *err = crate::process_registry::char_safe_tail(err.as_str(), 2000).to_string();
                }
            }
        });
    }

    /// Get info for a process as JSON.
    ///
    /// 已驱逐（墓碑）条目不返回 `None`：以 `"evicted": true` + 驱逐前的终态/
    /// exit code 作答，使 `process check` 能区分「已结束」与「id 无效」
    /// （BUG-2026-09-13-23）。
    pub fn get_info(id: u32) -> Option<serde_json::Value> {
        Self::with(|r| {
            let Some(entry) = r.entries.get(&id) else {
                let tombstone = r.tombstone(id)?;
                return Some(Self::tombstone_info(tombstone));
            };
            let status = entry
                .status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let output = entry
                .output
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let stderr = entry
                .stderr
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let answer = entry
                .answer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let elapsed = entry.started_at.elapsed().as_secs();

            let mut info = match status {
                ProcStatus::Exited(c) => serde_json::json!({
                    "id": id, "name": entry.name, "status": "exited",
                    "exit_code": c, "elapsed_secs": elapsed,
                    "output": output, "stderr": stderr,
                }),
                ProcStatus::Killed => serde_json::json!({
                    "id": id, "name": entry.name, "status": "killed",
                    "elapsed_secs": elapsed,
                    "exit_code": *entry.last_exit_code.lock().unwrap_or_else(|e| e.into_inner()),
                    "output": output, "stderr": stderr,
                }),
                ProcStatus::Running => serde_json::json!({
                    "id": id, "name": entry.name, "status": "running",
                    "elapsed_secs": elapsed,
                    "output_tail": if output.len() > 500 {
                        format!("...({} total)\n{}", output.len(), char_safe_tail(&output, 500))
                    } else { output.clone() },
                    "stderr_tail": if stderr.len() > 300 {
                        format!("...(stderr {} total)\n{}", stderr.len(), char_safe_tail(&stderr, 300))
                    } else { stderr.clone() },
                    "output_size": output.len(),
                }),
            };
            if let Some(ans) = answer
                && let serde_json::Value::Object(ref mut map) = info
            {
                map.insert("answer".to_string(), serde_json::json!(ans));
            }
            Some(info)
        })
    }

    /// 墓碑的对外视图（与 `get_info` 的字段形状保持一致，另加 `evicted`）。
    fn tombstone_info(tombstone: &Tombstone) -> serde_json::Value {
        let (status, exit_code) = match tombstone.status {
            ProcStatus::Exited(code) => ("exited", Some(code)),
            ProcStatus::Killed => ("killed", tombstone.exit_code),
            ProcStatus::Running => ("running", None),
        };
        serde_json::json!({
            "id": tombstone.id,
            "name": tombstone.name,
            "status": status,
            "exit_code": exit_code,
            "evicted": true,
            "elapsed_secs": tombstone.started_at.elapsed().as_secs(),
            "evicted_secs_ago": tombstone.evicted_at.elapsed().as_secs(),
            "content": format!(
                "process {}: {status} (entry evicted; os_pid cleanup info retained)",
                tombstone.id
            ),
        })
    }

    /// 按 os_pid 清理一个进程（组）：Windows `taskkill /T /F` 杀整棵树，
    /// Unix `killpg` 杀整组（spawn 侧 `process_group(0)`，见 W4/H6）。
    ///
    /// 调用方须自行确认 pid 仍然可信（在册条目 / 未被淘汰的墓碑）：
    /// 已淘汰墓碑的 pid 可能已被 OS 复用，此处不做校验。
    fn cleanup_by_pid(pid: u32) {
        #[cfg(windows)]
        {
            use std::process::Command;
            let _ = Command::new("taskkill")
                .args(["/pid", &pid.to_string(), "/T", "/F"])
                .status();
        }
        #[cfg(not(windows))]
        {
            unsafe {
                libc::killpg(pid as i32, libc::SIGKILL);
            }
        }
    }

    /// 墓碑命中路径：条目已被驱逐，但 os_pid 快照仍在表内 → 按 pid 尽力清树
    /// （BUG-2026-09-13-23 的验收标准）。
    ///
    /// - 有 os_pid：清理 + 墓碑状态收敛为 `Killed`（同时销毁 os_pid，避免
    ///   对同一（可能已被复用的）pid 重复清理）→ [`KillOutcome::TombstoneCleaned`]。
    /// - 无 os_pid（subagent 登记路径从无 `attach_child`）：**不做任何 OS 操作**，
    ///   返回 [`KillOutcome::NoOsPid`]——调用方不得据此回复「已杀」。
    ///
    /// 两条路径都**收敛状态为 `Killed`**：kill 请求在注册表层面已被受理，
    /// 状态一致性是 subagent `RegistryRef::killed()`（只看 `status == "killed"`）
    /// 能否感知 kill 的前提；「有没有真清理到 OS 进程」由返回值区分。
    fn kill_tombstoned(tombstone: &mut Tombstone) -> KillOutcome {
        let pid = tombstone.os_pid;
        tombstone.mark_killed();
        match pid {
            Some(pid) => {
                Self::cleanup_by_pid(pid);
                KillOutcome::TombstoneCleaned
            }
            None => KillOutcome::NoOsPid,
        }
    }

    /// Kill a process by id（Windows：杀整棵进程树，防止后代进程泄漏管道）。
    ///
    /// W4 语义：对仍运行的进程执行整树终止并置 Killed；对已退出（Exited）的
    /// 条目，仍按 os_pid 尽力清理残留后代（backgrounded 移交场景），状态同样
    /// 收敛为 Killed，但原 exit code 保存在 `last_exit_code` 并经 get_info
    /// 暴露——信息不再丢失。
    ///
    /// 返回 [`KillOutcome`] 而非 `bool`：调用方必须能区分「真的清理了 os 进程」
    /// 与「有 id 但无 os_pid 可清理」（PR #57 reviewer 阻断 ②）。
    pub fn kill(id: u32) -> KillOutcome {
        Self::with(|r| {
            if !r.entries.contains_key(&id) {
                return match r.tombstone_mut(id) {
                    Some(tombstone) => Self::kill_tombstoned(tombstone),
                    // id 从未登记，或墓碑已被容量淘汰（os_pid 已销毁）：
                    // 不再按 pid 清理，避免误杀复用该 pid 的活进程组。
                    None => KillOutcome::NotFound,
                };
            }
            // 上面已确认在册，故此处必然命中（同一把锁内无并发修改）。
            let Some(entry) = r.entries.get(&id) else {
                return KillOutcome::NotFound;
            };
            let mut child_opt = entry.child.lock().unwrap_or_else(|e| e.into_inner());
            match child_opt.take() {
                Some(mut c) => {
                    #[cfg(windows)]
                    {
                        use std::process::Command;
                        let _ = Command::new("taskkill")
                            .args(["/pid", &c.id().to_string(), "/T", "/F"])
                            .status();
                        let _ = c.wait();
                    }
                    #[cfg(not(windows))]
                    {
                        // H6：整组 SIGKILL（spawn 侧 process_group(0)），
                        // 孙进程释放管道写端，reader 可 EOF。
                        unsafe {
                            libc::killpg(c.id() as i32, libc::SIGKILL);
                        }
                        let _ = c.wait();
                    }
                }
                None => {
                    // 句柄已被 try_wait 回收：按 os_pid 快照尽力清树。
                    if let Some(pid) = *entry.os_pid.lock().unwrap_or_else(|e| e.into_inner()) {
                        #[cfg(windows)]
                        {
                            use std::process::Command;
                            let _ = Command::new("taskkill")
                                .args(["/pid", &pid.to_string(), "/T", "/F"])
                                .status();
                        }
                        #[cfg(not(windows))]
                        {
                            unsafe {
                                libc::killpg(pid as i32, libc::SIGKILL);
                            }
                        }
                    }
                }
            }
            if let ProcStatus::Exited(code) =
                *entry.status.lock().unwrap_or_else(|e| e.into_inner())
            {
                *entry
                    .last_exit_code
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(code);
            }
            *entry.status.lock().unwrap_or_else(|e| e.into_inner()) = ProcStatus::Killed;
            *entry.terminal_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
            KillOutcome::Killed
        })
    }

    // ── 测试专用钩子（回归夹具用；生产代码不得调用） ──
    //
    // 集成测试（tests/process_registry_tombstone.rs）需要在无真实子进程的
    // 前提下构造"久前注册、已进终态"的条目，故这些钩子必须为 pub 并随库
    // 一起编译；命名以 `_for_test` 结尾并 `#[doc(hidden)]`，不进入文档面。

    /// 测试钩子：墓碑表容量上限（阻断①回归用）。
    #[doc(hidden)]
    pub fn tombstone_capacity_for_test() -> usize {
        TOMBSTONE_CAPACITY
    }

    /// 测试钩子：当前墓碑条数（阻断①回归用）。
    #[doc(hidden)]
    pub fn tombstone_count_for_test() -> usize {
        Self::with(|r| r.tombstones.len())
    }

    /// 测试钩子：直接写入 os_pid 快照（无需真实子进程）。
    #[doc(hidden)]
    pub fn attach_os_pid_for_test(id: u32, os_pid: u32) {
        Self::with(|r| {
            if let Some(entry) = r.entries.get(&id) {
                *entry.os_pid.lock().unwrap_or_else(|e| e.into_inner()) = Some(os_pid);
            }
        });
    }

    /// 测试钩子：把注册时刻与（若有）终态时刻一并回拨 `secs` 秒——
    /// 模拟"很久以前注册、也已进入终态很久"的陈旧条目。
    #[doc(hidden)]
    pub fn age_registration_for_test(id: u32, secs: u64) {
        Self::with(|r| {
            if let Some(entry) = r.entries.get_mut(&id) {
                let Some(back) = Instant::now().checked_sub(Duration::from_secs(secs)) else {
                    return;
                };
                entry.started_at = back;
                let mut terminal = entry.terminal_at.lock().unwrap_or_else(|e| e.into_inner());
                if terminal.is_some() {
                    *terminal = Some(back);
                }
            }
        });
    }

    /// 测试钩子：仅回拨注册时刻（恒保留当前终态时刻）——用于锁定"长跑后
    /// 刚结束的条目不得按 started 计时被驱逐"。
    #[doc(hidden)]
    pub fn age_started_only_for_test(id: u32, secs: u64) {
        Self::with(|r| {
            if let Some(entry) = r.entries.get_mut(&id)
                && let Some(back) = Instant::now().checked_sub(Duration::from_secs(secs))
            {
                entry.started_at = back;
            }
        });
    }

    /// Wait for a process to exit (polling up to timeout_secs).
    ///
    /// 每次轮询先经 `try_wait` 刷新终态：子进程退出即返回，不依赖管道 EOF
    /// （孙进程可能持有管道写端，EOF 永不出现；原实现只查 status 字段，
    /// 而 backgrounded 路径的 mark_exited 在 EOF 后才执行 → 永远 running）。
    /// 取消检查（exec 生命周期重写阶段 2，报告 P1）：每次轮询同时检查
    /// per-call 取消旗标与 ambient `is_cancel()`——非 exec 阻塞工具
    /// （process wait）的飞行中取消必须立即返回，不得阻塞到 timeout。
    /// 返回的 info 中 `wait_interrupted_by_cancel = true` 标记提前返回原因。
    pub fn wait_for(
        id: u32,
        timeout_secs: u64,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Option<serde_json::Value> {
        let start = Instant::now();
        loop {
            if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst))
                || crate::is_cancel()
            {
                let mut info = Self::get_info(id)
                    .unwrap_or_else(|| serde_json::json!({ "id": id, "status": "missing" }));
                if let serde_json::Value::Object(ref mut map) = info {
                    map.insert(
                        "wait_interrupted_by_cancel".to_string(),
                        serde_json::json!(true),
                    );
                }
                return Some(info);
            }
            if start.elapsed().as_secs() > timeout_secs {
                return Self::get_info(id);
            }
            // 刷新终态（幂等；子进程已退出则自动置 Exited）
            let _ = Self::try_wait(id);
            let exited = Self::with(|r| {
                r.entries
                    .get(&id)
                    .map(|e| {
                        matches!(
                            *e.status.lock().unwrap_or_else(|e| e.into_inner()),
                            ProcStatus::Exited(_) | ProcStatus::Killed
                        )
                    })
                    .unwrap_or(true)
            });
            if exited {
                return Self::get_info(id);
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_full_grows_with_appended_chunks() {
        let id = ProcessRegistry::register("capture-test");
        ProcessRegistry::append_output(id, "hello ");
        ProcessRegistry::append_stderr(id, "warn");
        ProcessRegistry::append_output(id, "world");
        let (out, err) = ProcessRegistry::captured_full(id).expect("entry must exist");
        assert_eq!(out, "hello world");
        assert_eq!(err, "warn");
    }

    #[test]
    fn full_capture_stops_at_byte_cap() {
        let id = ProcessRegistry::register("capture-cap-test");
        let big = "x".repeat(FULL_CAPTURE_BYTE_CAP + 4096);
        ProcessRegistry::append_output(id, &big);
        let (out, _) = ProcessRegistry::captured_full(id).expect("entry must exist");
        assert_eq!(out.len(), FULL_CAPTURE_BYTE_CAP, "完整捕获必须封顶");
    }

    #[test]
    fn is_running_transitions_to_false_on_terminal_status() {
        let id = ProcessRegistry::register("running-test");
        assert!(ProcessRegistry::is_running(id));
        ProcessRegistry::mark_exited(id, 0);
        assert!(
            !ProcessRegistry::is_running(id),
            "Exited 后不得再视为 running"
        );
        assert!(
            !ProcessRegistry::is_running(u32::MAX),
            "缺失条目视为不在运行"
        );
    }

    /// T-1-5 回归：`kill` 写入的 `Killed` 终态不得被迟到的 `mark_exited`
    /// （读线程 settle 路径）覆盖回 `Exited`。未加守卫时本测试红：
    /// `status` 变回 `exited`，`RegistryRef::killed()` 丢失 kill 信号。
    #[test]
    fn mark_exited_does_not_downgrade_killed() {
        let id = ProcessRegistry::register("mark-exited-killed-test");
        // subagent 登记路径：无 os_pid/child，kill 仍把状态收敛为 Killed。
        assert_eq!(ProcessRegistry::kill(id), KillOutcome::Killed);
        let after_kill = ProcessRegistry::get_info(id).expect("entry must exist");
        assert_eq!(after_kill["status"], "killed", "前置条件：已进 Killed");

        ProcessRegistry::mark_exited(id, 0);

        let after_settle = ProcessRegistry::get_info(id).expect("entry must exist");
        assert_eq!(
            after_settle["status"], "killed",
            "mark_exited 不得把 Killed 降级为 Exited: {after_settle}"
        );
    }

    /// 对照：正常路径（Running → Exited）不受守卫影响。
    #[test]
    fn mark_exited_still_moves_running_to_exited() {
        let id = ProcessRegistry::register("mark-exited-running-test");
        ProcessRegistry::mark_exited(id, 7);
        let info = ProcessRegistry::get_info(id).expect("entry must exist");
        assert_eq!(info["status"], "exited");
        assert_eq!(info["exit_code"], 7);
    }
}
