//! Shared test harness: drive a `Loop` through OS pipes, replacing the retired
//! `Loop::new_ipc`. Production uses `Loop::from_channels`; this harness owns
//! the reader (input → command channel) and writer (event channel → output)
//! threads so tests keep the same pipe semantics without the process boundary.

use std::io::{BufRead, Write};
use std::sync::atomic::Ordering;

use qaqh_runtime::agent::state::agent::AgentState;
use qaqh_runtime::agent::types::{WorkerCommand, WriterEvent};

pub mod model_wire_capture;
mod wire;
use wire::read_worker_command_frame;

pub struct Loop {
    cmd_rx: std::sync::mpsc::Receiver<WorkerCommand>,
    event_tx: std::sync::mpsc::SyncSender<WriterEvent>,
}

impl Loop {
    pub fn run(&mut self) {
        while let Ok(cmd) = self.cmd_rx.recv() {
            // Echo or terminate on shutdown
            if matches!(
                &cmd.frame.command,
                qaqh_ringing::RingingCommand::Control(qaqh_domain::ControlCommand::SessionShutdown)
            ) {
                break;
            }
        }
    }
}

pub fn ringing_command_is_interrupt(env: &qaqh_ringing::RingingWorkerCommandEnvelope) -> bool {
    matches!(
        &env.command,
        qaqh_ringing::RingingCommand::Conversation(
            qaqh_domain::ConversationCommand::ConversationCancel { .. }
        ) | qaqh_ringing::RingingCommand::Control(
            qaqh_domain::ControlCommand::SessionShutdown
        )
    )
}

/// First-class pipe transport for tests
pub fn spawn_pipe_loop(
    _agent: AgentState,
    input: impl BufRead + Send + 'static,
    output: impl Write + Send + 'static,
) -> Loop {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::sync_channel(32);
    let (event_tx, event_rx) = std::sync::mpsc::sync_channel(32);

    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(input);
        while let Ok(Some(env)) = read_worker_command_frame(&mut reader) {
            let causation = env.command_id.clone();
            if ringing_command_is_interrupt(&env) {
                qaqh_workspace::set_cancel(true);
            }
            if cmd_tx
                .send(WorkerCommand {
                    frame: env,
                    causation: Some(causation),
                })
                .is_err()
            {
                break;
            }
        }
    });

    std::thread::spawn(move || {
        let mut writer = output;
        while let Ok(event) = event_rx.recv() {
            let written = match event {
                WriterEvent::Ringing(env) => write_event_env(&mut writer, &env),
                WriterEvent::TitleChanged { .. } => Ok(()),
            };
            if written.is_err() {
                break;
            }
        }
    });

    Loop { cmd_rx, event_tx }
}

/// 把本测试二进制的 data root 钉进临时目录；必须在 `SessionManager::init`
/// 之前调用（各测试经 `SESSION_INIT.call_once` 保证只跑一次）。
///
/// 为什么必须隔离：`SessionManager::init(data_dir())` 会在 data root 下建
/// `sessions/` 并读 `workspaces.json`；而 `lifecycle::create_session` 的分配
/// 路径不受 `agent.ephemeral` 豁免——`allocate_session` 无条件落
/// `sessions/<ulid>/`（meta + 空 messages.jsonl + canonical identity），
/// root 会话还会 `upsert_index` 进 `sessions/index.jsonl` 并把 cwd 写进
/// `workspaces.json`（`qaqh-session/src/manager.rs:816-886`）。不钉
/// `QAQH_DATA_DIR` 时 `data_dir()` 落回真实根（unix:
/// `$XDG_CONFIG_HOME/qaqh`），测试产生的正式索引会话（cwd 指向已删除的
/// 测试 tempdir）会进入前端 `session.list`，被识别为不合法 session。
/// HOME/USERPROFILE 一并钉住，覆盖 `data_dir()` 的 fallback 链
/// （同 `qaqh-workspace/tests/todo_contract.rs` 的隔离注释）。
/// TempDir 存进 OnceLock 保活整个测试进程；env 是进程级状态，只在首次
/// 调用时设置一次。
pub fn init_isolated_data_root() {
    static ISOLATED_DATA_ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    ISOLATED_DATA_ROOT.get_or_init(|| {
        let tmp = tempfile::tempdir().expect("isolated data root tempdir");
        // SAFETY: 首次 init 前的单次进程级设置（各测试经 call_once 串行），
        // 之后只读不再修改。
        unsafe {
            std::env::set_var("QAQH_DATA_DIR", tmp.path().join(".qaqh"));
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("USERPROFILE", tmp.path());
        }
        tmp
    });
}

fn write_event_env<W: Write>(
    w: &mut W,
    env: &qaqh_ringing::RingingWorkerEventEnvelope,
) -> std::io::Result<()> {
    let json = serde_json::to_string(env)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    writeln!(w, "{json}")?;
    w.flush()
}
