use std::collections::HashMap;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use qaqh_session::session_fact_v2::{AgentMetadata, AgentPath, SessionId};
use qaqh_subagent::{AgentOutcome, ListedAgent, ListedAgentResidency, ListedAgentStatus};

#[derive(Clone, Debug)]
pub struct SubagentSpawnSpec {
    pub tools: Vec<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub max_tokens: Option<u32>,
    pub permission_level: u8,
}
use crate::agent_catalog::AgentCatalog;
use crate::quota_ledger::{QuotaKind, QuotaLedger, QuotaLimits, QuotaReservation, ReleaseReason};
use crate::subagent_supervisor::{LifecycleEvent, SubagentSupervisor};
use crate::RingingHub;

static SYSTEM_PATH: OnceLock<String> = OnceLock::new();

pub fn cache_system_path() {
    // `mut` 只被下方 Windows 注册表探测分支消费（非 Windows 编译只读）。
    #[cfg_attr(not(target_os = "windows"), allow(unused_mut))]
    let mut path = std::env::var("PATH").unwrap_or_default();
    #[cfg(target_os = "windows")]
    for key in [
        r"HKCU\Environment",
        r"HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment",
    ] {
        let mut command = background_command("reg");
        command.args(["query", key, "/v", "Path"]);
        if let Some(output) = probe_output(command, PROBE_TIMEOUT) {
            let text = String::from_utf8_lossy(&output.stdout);
            if let Some(value) = text
                .lines()
                .find(|line| line.contains("REG_"))
                .and_then(|line| {
                    line.split_once("REG_EXPAND_SZ")
                        .or_else(|| line.split_once("REG_SZ"))
                })
                .map(|(_, value)| value.trim())
            {
                for segment in value.split(';').filter(|value| !value.is_empty()) {
                    if !path
                        .split(';')
                        .any(|current| current.eq_ignore_ascii_case(segment))
                    {
                        if !path.is_empty() {
                            path.push(';')
                        }
                        path.push_str(segment)
                    }
                }
            }
        }
    }
    let _ = SYSTEM_PATH.set(path.clone());
    // SAFETY: 只在进程启动期（单线程初始化窗口）改写 PATH，此刻尚无并发
    // 线程读取环境变量；此后本函数幂等返回，不再写入。
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("PATH", path);
    }
}

/// 启动期壳探测（exec 的可用性口径与实际派生同源）。幂等，可重复调用。
pub fn detect_shell() {
    let shell = qaqh_workspace::bootstrap_exec_shell();
    log::info!("[runtime] exec shell bootstrap: {shell}");
}

/// 探测型命令的单次超时上界。
///
/// `--version` / `uname` 这类探测本该毫秒级返回，2s 已是极大宽限。
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// 整份工具快照（6 次探测）的总预算；超出即停止探测、用已有结果降级。
///
/// 探测结果只喂给提示词里的工具快照（`crate::agent::prompt::TOOLS_INFO`），
/// **不值得为它拖慢启动**——何况它跑在 daemon 开始服务之前。
const PROBE_BUDGET: Duration = Duration::from_secs(6);

/// 等子进程退出时的轮询间隔。
const PROBE_POLL: Duration = Duration::from_millis(10);

pub fn detect_os_info() {
    #[cfg(target_os = "windows")]
    let info = {
        let mut command = background_command("cmd");
        command.args(["/d", "/c", "ver"]);
        probe_output(command, PROBE_TIMEOUT)
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| format!("windows {}", std::env::consts::ARCH))
    };
    #[cfg(not(target_os = "windows"))]
    let info = {
        let mut command = Command::new("uname");
        command.arg("-a");
        probe_output(command, PROBE_TIMEOUT)
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| format!("{} {}", std::env::consts::OS, std::env::consts::ARCH))
    };
    let _ = crate::agent::prompt::OS_INFO.set(info);
    // Toolchain snapshot. Each probe lists candidate program names tried in
    // order (first success wins): e.g. Windows ships `python` while most
    // Linux distros only provide `python3`.
    let tool_probes: [(&[&str], &[&str]); 6] = [
        (&["git"], &["--version"]),
        (&["cargo"], &["--version"]),
        (&["node"], &["--version"]),
        (&["python", "python3"], &["--version"]),
        (&["rustc"], &["--version"]),
        (&["pnpm"], &["--version"]),
    ];
    let budget = Instant::now() + PROBE_BUDGET;
    let mut tools = Vec::new();
    'probes: for (programs, args) in tool_probes {
        for program in programs {
            let mut command = background_command(program);
            command.args(args);
            if let Some(output) = probe_output(command, PROBE_TIMEOUT) {
                let value = if output.stdout.is_empty() {
                    &output.stderr
                } else {
                    &output.stdout
                };
                let value = String::from_utf8_lossy(value).trim().to_string();
                if !value.is_empty() {
                    tools.push(value);
                    break;
                }
            }
            if Instant::now() >= budget {
                log::warn!(
                    "[runtime] 工具探测超出 {PROBE_BUDGET:?} 总预算，放弃剩余探测（已得 {} 项）",
                    tools.len()
                );
                break 'probes;
            }
        }
    }
    let _ = crate::agent::prompt::TOOLS_INFO.set(tools.join(", "));
}

/// 跑一个探测型命令并取回输出，**带超时**。
///
/// 为什么不能用 `Command::output()`：它的契约是「等子进程退出 **且** 把
/// stdout/stderr 管道读到 EOF」。只要有任何后代进程继承并持有管道的写端，
/// EOF 就永不出现——**即便被探测的程序本身早已退出**。把它放在 daemon 开始
/// 服务**之前**的同步路径上，就得到一个不报错、不退出、无日志的永久挂起
/// （BUG-2026-09-15-02）。
///
/// 这里三重设防：stdin 接空设备（不再有交互等待）、等退出用轮询限时、读管道
/// 另起线程且同样限时。任一环节超时即 `kill` 并回收子进程、返回 `None`——
/// **探测失败必须降级，不得阻塞启动**。
fn probe_output(mut command: Command, timeout: Duration) -> Option<Output> {
    let deadline = Instant::now() + timeout;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let stdout_rx = drain(child.stdout.take());
    let stderr_rx = drain(child.stderr.take());

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait(); // 回收，避免僵尸
                    log::warn!(
                        "[runtime] 探测命令超时（{timeout:?}），已终止并跳过：{:?}",
                        command.get_program()
                    );
                    return None;
                }
                std::thread::sleep(PROBE_POLL);
            }
            Err(_) => return None,
        }
    };

    // 子进程已退出 **≠** 管道已到 EOF：后代可能仍持有写端（同一失败模式在
    // `qaqh-workspace/src/process_registry.rs:354` 已有认知）。故这里同样限时，
    // 拿多少算多少——绝不回到无限等待。
    let left = deadline.saturating_duration_since(Instant::now());
    Some(Output {
        status,
        stdout: stdout_rx.recv_timeout(left).unwrap_or_default(),
        stderr: stderr_rx.recv_timeout(left).unwrap_or_default(),
    })
}

/// 另起线程把 `reader` 读干，结果经 channel 回传。
///
/// 读取必须离开主线程：`read_to_end` 会一直阻塞到 EOF，而 EOF 正是此处不可信
/// 的东西。读线程可能永远收不到 EOF（后代持有写端），但它只是**有界**地泄漏
/// 一个线程，而主线程的等待是限时的。
fn drain<R: std::io::Read + Send + 'static>(
    reader: Option<R>,
) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(mut reader) = reader {
        let _ = std::thread::Builder::new()
            .name("qaqh-probe-io".to_string())
            .spawn(move || {
                let mut buf = Vec::new();
                let _ = reader.read_to_end(&mut buf);
                let _ = tx.send(buf);
            });
    }
    rx
}

fn background_command(program: &str) -> Command {
    // `mut` 只被 Windows 的 CREATE_NO_WINDOW 调整消费（非 Windows 编译只读）。
    #[cfg_attr(not(target_os = "windows"), allow(unused_mut))]
    let mut command = Command::new(program);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// How an agent worker is attached to this daemon process.
enum AgentTransport {
    /// owner 驱动的回合循环(`qaqh_agent_runtime::session_loop`)。
    Loop { loop_: Arc<qaqh_agent_runtime::session_loop::SessionLoop> },
}

enum AgentKind {
    Session,
    Subagent(SubagentSpawnSpec),
}

pub struct AgentInstance {
    session_id: String,
    transport: AgentTransport,
    kind: AgentKind,
}

/// Logical identity allocated for a newly spawned subagent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnedSubagentInfo {
    pub child_session_id: SessionId,
    pub parent_agent_path: AgentPath,
    pub child_agent_path: AgentPath,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SubagentSpawnOptions<'a> {
    pub(crate) tools: &'a [String],
    pub(crate) model: Option<&'a str>,
    pub(crate) base_url: Option<&'a str>,
    pub(crate) max_tokens: Option<u32>,
    /// Workspace the caller explicitly requested for the child session.
    /// `None` = inherit the parent workspace / default session root.
    pub(crate) workspace: Option<&'a str>,
    pub(crate) ephemeral: bool,
    /// P1-⑤：父会话档位；legacy/bootstrap 路径回落全局 config。
    pub(crate) permission_level: u8,
}

pub(crate) struct CollectorArmSpec {
    pub(crate) child_session_id: String,
    pub(crate) name: String,
    pub(crate) parent_session_id: String,
    pub(crate) parent_call_id: String,
    pub(crate) timeout_secs: u64,
    pub(crate) root_session_id: String,
    pub(crate) parent_agent_path: String,
    pub(crate) child_agent_path: String,
}

pub struct AgentRegistry {
    instances: HashMap<String, AgentInstance>,
    /// 会话存储句柄（PR-3-1 注入化；spawn 诊断读 meta 用）。
    sessions: Arc<qaqh_session::SessionManager>,
    /// 会话 owner 目录:每会话业务顺序与工具生命周期的落点。
    owners: Arc<qaqh_session::owner::OwnerDirectory>,
    /// Ringing 运行时；None = 未启用 legacy worker-only 模式。
    hub: Option<Arc<RingingHub>>,
    /// daemon 正在关闭：worker 退出是预期的，禁止自动重生。
    shutting_down: bool,
    /// 最近一次 spawn 时间（防崩溃-重启风暴：同一 seed 1 秒内不重复拉起）。
    last_spawn: HashMap<String, std::time::Instant>,
    /// P2-5：daemon 级 parent/child edge 与 unload 顺序状态机。
    supervisor: SubagentSupervisor,
    /// Subagent V2：逻辑 agent metadata；worker handle 仍由 `instances` 管理。
    agent_catalog: AgentCatalog,
    /// Explicit residency state for logical agents. Kept separate from
    /// `instances` so `list_agents` reads lifecycle state rather than guessing
    /// from the worker table.
    residency: HashMap<String, ListedAgentResidency>,
    /// P2-7：root session tree 的 durable quota owner。
    quota_ledgers: HashMap<String, QuotaLedger>,
    quota_limits: QuotaLimits,
    /// Subagent V2：树深上限。root depth = 0；默认 1（只允许 root -> child）。
    /// Subagent V2：agent 树最大深度（`/root` 的 depth 为 0）。
    ///
    /// 默认 1 = 只允许 `root -> child`；daemon 启动时由
    /// `config.subagent.max_depth` 覆盖（`service.rs`），改动需重启 daemon。
    /// 拓扑本身（`AgentPath` 的 parent/child/is_descendant_of）支持任意深度，
    /// 这个值只是策略闸门。
    max_depth: usize,
    /// Subagent V2：同一 sender-target 的 queued message 上限。0 = unlimited。
    message_in_flight_per_pair: u64,
    /// Subagent V2：单个 sender 的累计 outbound attempt 上限。0 = unlimited。
    message_outbound_per_sender: u64,
    /// Runtime-only outbound attempt counters: root -> author path -> count.
    /// 消息配额是安全阀，不要求跨 daemon 重启持久化。
    outbound_attempts: HashMap<String, HashMap<String, u64>>,
    /// Child result collectors currently armed for a running/reloaded turn.
    /// 已武装的子代理收集器:child id → 关联的父工具 call id。
    armed_collectors: HashMap<String, String>,
    /// V2 子代理终态摘要（**结果面**）：`child_session_id` → outcome。
    ///
    /// 结果不再寄存在 `ProcessRegistry` 的 `answer` 槽：状态面自己承载最终
    /// 答复，由 `list_agents` 直接返回（与 codex `AgentStatus::Completed`
    /// 同构）。键是 canonical 主键，reload 不会换 id。
    subagent_outcomes: HashMap<String, AgentOutcome>,
    /// 子代理 spawn 参数(reload 用):child id → spawn spec。
    spawn_configs: HashMap<String, SubagentSpawnSpec>,
}

impl AgentRegistry {
    pub fn new(
        sessions: Arc<qaqh_session::SessionManager>,
        owners: Arc<qaqh_session::owner::OwnerDirectory>,
    ) -> Self {
        Self {
            instances: HashMap::new(),
            sessions,
            owners,
            hub: None,
            shutting_down: false,
            last_spawn: HashMap::new(),
            supervisor: SubagentSupervisor::default(),
            agent_catalog: AgentCatalog::default(),
            residency: HashMap::new(),
            quota_ledgers: HashMap::new(),
            quota_limits: QuotaLimits::unlimited(),
            max_depth: 1,
            message_in_flight_per_pair: 16,
            message_outbound_per_sender: 1024,
            outbound_attempts: HashMap::new(),
            armed_collectors: HashMap::new(),
            subagent_outcomes: HashMap::new(),
            spawn_configs: HashMap::new(),
        }
    }

    /// 挂载 Ringing 运行时。Ringing worker 事件只进入 native hub。
    pub fn attach_ringing(&mut self, hub: Arc<RingingHub>) {
        self.hub = Some(hub);
    }
}

mod lifecycle;
mod messaging;
mod queries;
mod spawn;

impl AgentInstance {
    fn is_dead(&self) -> bool {
        // 回合循环无独立执行线程可崩:任务失败由循环自身收尾,实例常驻。
        false
    }

    fn wait_until_stopped(&self, _session_id: &str) -> bool {
        true
    }

    fn kind_name(&self) -> AgentKind {
        match &self.kind {
            AgentKind::Session => AgentKind::Session,
            AgentKind::Subagent(spec) => AgentKind::Subagent(spec.clone()),
        }
    }

    fn signal_shutdown(&mut self) {
        let AgentTransport::Loop { loop_ } = &self.transport;
        loop_.abandon();
        qaqh_workspace::set_session_cancel(&self.session_id, true);
    }

    /// Unload 收尾:owner 泵线程持有全部落库职责,实例 drop 即完成,
    /// 无线程可 join。回合任务感知取消后自行结算挂起交互并结束。
    fn finish_shutdown(&mut self) {
        log::info!("stopped agent {}", self.session_id);
    }

    fn shutdown(mut self) {
        self.signal_shutdown();
        self.finish_shutdown();
    }
}

/// Stable name for a command the thin actor does not implement. Returning an
/// error instead of `Ok(())` keeps entry acceptance from lying about execution.
fn ringing_command_name(command: &qaqh_ringing::RingingCommand) -> &'static str {
    match command {
        qaqh_ringing::RingingCommand::Conversation(_) => "conversation",
        qaqh_ringing::RingingCommand::Control(_) => "control",
        qaqh_ringing::RingingCommand::Tool(_) => "tool",
    }
}

/// Stable, grammar-valid name for legacy direct `spawn_subagent` callers.
fn legacy_child_name(session_id: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    session_id.hash(&mut hasher);
    format!("sub_{:016x}", hasher.finish())
}

/// Millisecond wall clock for logical agent registration.
fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

/// Broadcast 命令 id（时间戳十六进制，语义同 service.rs 的 `command_id`）。
fn broadcast_command_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("bcast-{nanos:x}")
}

#[cfg(test)]
mod tests;
