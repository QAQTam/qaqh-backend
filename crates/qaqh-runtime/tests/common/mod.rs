//! Shared test harness: drive a `Loop` through OS pipes, replacing the retired
//! `Loop::new_ipc`. Production uses `Loop::from_channels`; this harness owns
//! the reader (input → command channel) and writer (event channel → output)
//! threads so tests keep the same pipe semantics without the process boundary.

use std::io::{BufRead, Write};
use std::sync::atomic::Ordering;

use qaqh_runtime::agent::loop_core::{Loop, LoopChannels, ringing_command_is_interrupt};
use qaqh_runtime::agent::state::agent::AgentState;
use qaqh_runtime::agent::types::{WorkerCommand, WriterEvent};

mod wire;
use wire::read_worker_command_frame;

/// First-class pipe transport for tests: spawns the reader (input → command
/// channel) and writer (event channel → output) threads that `Loop::new_ipc`
/// used to own, then builds the loop via [`Loop::from_channels`].
///
/// Returns the loop; callers still own the pipe's `input`/`output` ends and
/// must feed command frames / read event frames exactly as before.
pub fn spawn_pipe_loop(
    agent: AgentState,
    input: impl BufRead + Send + 'static,
    output: impl Write + Send + 'static,
) -> Loop {
    let channels = LoopChannels::new();
    let cancel_for_reader = channels.cancel.clone();
    let cmd_tx = channels.cmd_tx.clone();
    let writer_dead_for_thread = channels.writer_dead.clone();

    // Reader: input JSON-LP → cmd_tx (sets cancel on interrupt commands).
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(input);
        while let Ok(Some(env)) = read_worker_command_frame(&mut reader) {
            let causation = env.command_id.clone();
            if ringing_command_is_interrupt(&env) {
                cancel_for_reader.set();
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

    // Writer: event_rx → output (JSON-LP lines).
    std::thread::spawn(move || {
        let mut writer = output;
        loop {
            let written = match channels.event_rx.recv() {
                Ok(WriterEvent::Ringing(env)) => write_event_env(&mut writer, &env),
                Ok(WriterEvent::Timeline(env)) => write_timeline_env(&mut writer, &env),
                // Titles travel through the V2 projection hub, not the worker
                // wire; the harness has nothing to serialize here.
                Ok(WriterEvent::TitleChanged { .. }) => Ok(()),
                Err(_) => break,
            };
            if written.is_err() {
                break;
            }
        }
        writer_dead_for_thread.store(true, Ordering::SeqCst);
    });

    Loop::from_channels(
        agent,
        channels.cmd_rx,
        channels.event_tx,
        channels.cancel,
        channels.writer_dead,
        std::sync::Arc::new(qaqh_runtime::agent::liveness::WorkerLiveness::new()),
        None,
    )
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

fn write_timeline_env<W: Write>(
    w: &mut W,
    env: &qaqh_ringing::RingingTimelineIntentEnvelope,
) -> std::io::Result<()> {
    let json = serde_json::to_string(env)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    writeln!(w, "{json}")?;
    w.flush()
}
