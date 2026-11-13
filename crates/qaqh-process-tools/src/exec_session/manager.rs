//! Session-scoped process execution, bounded capture, and lifecycle publication.

use std::collections::HashMap;
use std::io::{Read, Write};
#[cfg(windows)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use qaqh_ringing_contract::events::ExecOutputSource;
use qaqh_ringing_contract::fact::{SessionId, ToolCallId, TurnId};
use qaqh_ringing_contract::state::{
    ExecCancelReason, ExecInterruptReason, ExecSessionOutcome, ExecSessionState, ExecSessionView,
    ExecStopReason,
};
use qaqh_types::{ExecSessionId, ToolCallOutcome, ToolCancellationReason, ToolInterruptionReason};

use super::capture::{Capture, ModelRead, OutputChunk, OutputSnapshot, OutputSource};

const SESSION_LIMIT: usize = 8;
const GLOBAL_LIMIT: usize = 32;
const TERMINAL_RETENTION: Duration = Duration::from_secs(10 * 60);
const OUTPUT_EOF_SETTLE: Duration = Duration::from_millis(500);
const PROCESS_WATCH_INTERVAL: Duration = Duration::from_millis(100);
const MAX_STDIN_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopResult {
    StopRequested,
    AlreadyFinished,
    NotFound,
}

#[derive(Debug, Clone)]
pub struct ExecSessionStart {
    pub owner_session_id: String,
    pub origin_turn_id: String,
    pub origin_call_id: String,
    pub command: String,
    pub cwd: Option<String>,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub tty: bool,
    pub timeout_ms: Option<u64>,
    pub yield_time_ms: u32,
    pub max_output_bytes: usize,
    pub cancellation: Arc<AtomicBool>,
}

#[derive(Debug, Clone)]
pub struct ExecSessionResult {
    pub outcome: ToolCallOutcome,
    pub exec_session_id: Option<ExecSessionId>,
    pub output: String,
    pub output_truncated: bool,
    pub omitted_bytes: u64,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub input_bytes_written: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct ExecSessionPoll {
    pub exec_session_id: ExecSessionId,
    pub owner_session_id: String,
    pub chars: String,
    pub yield_time_ms: u32,
    pub max_output_bytes: usize,
    pub cancellation: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecManagerErrorKind {
    QuotaExceeded,
    NotFound,
    WrongOwner,
    InputUnsupported,
    InputTooLarge,
    InputWrite,
    Spawn,
    Shutdown,
}

#[derive(Debug, Clone)]
pub struct ExecManagerError {
    pub kind: ExecManagerErrorKind,
    pub message: String,
    pub bytes_written: usize,
}

impl std::fmt::Display for ExecManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ExecManagerError {}

/// The publisher is session-local. Implementations must enqueue lifecycle facts
/// into the owner pump and must not retain a manager lock while doing so.
pub trait ExecSessionPublisher: std::fmt::Debug + Send + Sync + 'static {
    fn upsert(&self, view: ExecSessionView);
    fn output(
        &self,
        exec_session_id: &ExecSessionId,
        start_offset: u64,
        end_offset: u64,
        source: ExecOutputSource,
        text: String,
        snapshot: OutputSnapshot,
        stream_dropped_bytes: u64,
    );
    fn finish(&self, view: ExecSessionView, transcript: String, truncated: bool);
}

#[derive(Debug, Clone)]
pub struct ExecSessionManager {
    inner: Arc<ManagerInner>,
}

#[derive(Debug, Default)]
struct ManagerInner {
    state: Mutex<ManagerState>,
}

#[derive(Default)]
struct ManagerState {
    sessions: HashMap<String, Arc<ManagedSession>>,
    active_by_owner: HashMap<String, usize>,
    active_total: usize,
    shutting_down: bool,
}

impl std::fmt::Debug for ManagerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagerState")
            .field("sessions", &self.sessions.len())
            .field("active_total", &self.active_total)
            .field("shutting_down", &self.shutting_down)
            .finish()
    }
}

struct ManagedSession {
    id: ExecSessionId,
    owner_session_id: String,
    origin_turn_id: String,
    origin_call_id: String,
    command: String,
    cwd: Option<String>,
    tty: bool,
    started_at_ms: i64,
    publisher: Arc<dyn ExecSessionPublisher>,
    core: Mutex<SessionCore>,
    changed: Condvar,
    interaction: Mutex<()>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    #[cfg(windows)]
    input_write_in_flight: Arc<AtomicBool>,
    #[cfg(any(unix, windows))]
    process: Mutex<ProcessControl>,
}

struct SessionCore {
    capture: Capture,
    state: ExecSessionState,
    stop_reason: Option<ExecStopReason>,
    stop_started: Option<Instant>,
    exit: Option<qaqh_pty::SessionExit>,
    terminal: Option<ExecSessionOutcome>,
    eof_sources: usize,
    expected_sources: usize,
    finalized: bool,
    active_slot_released: bool,
    finished_at_ms: Option<i64>,
    stream_dropped_bytes: u64,
}

#[cfg(any(unix, windows))]
enum ProcessControl {
    #[cfg(any(unix, windows))]
    Pty(qaqh_pty::SpawnedSession),
    Pipe(qaqh_pty::SpawnedPipeSession),
}

#[cfg(any(unix, windows))]
impl ProcessControl {
    fn try_wait(&mut self) -> std::io::Result<Option<qaqh_pty::SessionExit>> {
        match self {
            #[cfg(any(unix, windows))]
            Self::Pty(process) => process.try_wait(),
            Self::Pipe(process) => process.try_wait(),
        }
    }

    fn interrupt(&mut self) -> std::io::Result<()> {
        match self {
            #[cfg(any(unix, windows))]
            Self::Pty(process) => process.interrupt(),
            Self::Pipe(process) => process.interrupt(),
        }
    }

    fn kill_group(&mut self) -> std::io::Result<()> {
        match self {
            #[cfg(any(unix, windows))]
            Self::Pty(process) => process.kill_group(),
            Self::Pipe(process) => process.kill_group(),
        }
    }

    /// Post-reap console teardown: on Windows closes the ConPTY master so
    /// the terminal source reaches EOF (see qaqh_pty::SpawnedSession).
    fn close_terminal(&mut self) {
        match self {
            #[cfg(any(unix, windows))]
            Self::Pty(process) => process.close_terminal(),
            Self::Pipe(_) => {}
        }
    }
}

impl ExecSessionManager {
    pub fn new() -> Self {
        let manager = Self {
            inner: Arc::new(ManagerInner::default()),
        };
        let weak = Arc::downgrade(&manager.inner);
        std::thread::Builder::new()
            .name("exec-session-retention".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(30));
                    let Some(inner) = weak.upgrade() else {
                        break;
                    };
                    reap_expired(&inner);
                }
            })
            .expect("start execution session retention worker");
        manager
    }

    pub fn start(
        &self,
        request: ExecSessionStart,
        publisher: Arc<dyn ExecSessionPublisher>,
    ) -> Result<ExecSessionResult, ExecManagerError> {
        #[cfg(any(unix, windows))]
        {
            self.start_platform(request, publisher)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (request, publisher);
            Err(manager_error(
                ExecManagerErrorKind::Spawn,
                "execution sessions require the split pipe backend on this platform".into(),
                0,
            ))
        }
    }

    #[cfg(any(unix, windows))]
    fn start_platform(
        &self,
        request: ExecSessionStart,
        publisher: Arc<dyn ExecSessionPublisher>,
    ) -> Result<ExecSessionResult, ExecManagerError> {
        self.reserve(&request.owner_session_id)?;
        let timeout_ms = request.timeout_ms;
        let yield_time_ms = request.yield_time_ms;
        let max_output_bytes = request.max_output_bytes;
        let cancellation = Arc::clone(&request.cancellation);
        let id = ExecSessionId::new(format!(
            "execs_{}",
            qaqh_ringing_contract::ulid::generate_ulid()
        ));
        let spec = qaqh_pty::SessionSpec {
            argv: request.argv.clone(),
            env: request.env.clone(),
            cwd: request.cwd.as_deref().map(std::path::PathBuf::from),
            cols: 80,
            rows: 24,
        };
        let (process, writer, readers, expected_sources) = if request.tty {
            let process = qaqh_pty::spawn_pty(&spec).map_err(|error| {
                self.release_reservation(&request.owner_session_id);
                manager_error(
                    ExecManagerErrorKind::Spawn,
                    format!("spawn PTY command: {error}"),
                    0,
                )
            })?;
            #[cfg(unix)]
            let process = {
                let mut process = process;
                if let Err(error) = process.set_pty_writer_nonblocking() {
                    let _ = process.kill_group();
                    self.release_reservation(&request.owner_session_id);
                    return Err(manager_error(
                        ExecManagerErrorKind::Spawn,
                        format!("configure PTY input: {error}"),
                        0,
                    ));
                }
                process
            };
            let (io, process) = process.into_parts();
            (
                ProcessControl::Pty(process),
                io.writer,
                vec![(io.reader, OutputSource::Terminal)],
                1,
            )
        } else {
            let process = qaqh_pty::spawn_pipe_split(&spec).map_err(|error| {
                self.release_reservation(&request.owner_session_id);
                manager_error(
                    ExecManagerErrorKind::Spawn,
                    format!("spawn pipe command: {error}"),
                    0,
                )
            })?;
            let (io, process) = process.into_parts();
            (
                ProcessControl::Pipe(process),
                io.writer,
                vec![
                    (io.stdout, OutputSource::Stdout),
                    (io.stderr, OutputSource::Stderr),
                ],
                2,
            )
        };

        let session = Arc::new(ManagedSession {
            id: id.clone(),
            owner_session_id: request.owner_session_id.clone(),
            origin_turn_id: request.origin_turn_id,
            origin_call_id: request.origin_call_id,
            command: request.command,
            cwd: request.cwd,
            tty: request.tty,
            started_at_ms: now_ms(),
            publisher,
            core: Mutex::new(SessionCore {
                capture: Capture::default(),
                state: ExecSessionState::Running,
                stop_reason: None,
                stop_started: None,
                exit: None,
                terminal: None,
                eof_sources: 0,
                expected_sources,
                finalized: false,
                active_slot_released: false,
                finished_at_ms: None,
                stream_dropped_bytes: 0,
            }),
            changed: Condvar::new(),
            interaction: Mutex::new(()),
            writer: Arc::new(Mutex::new(writer)),
            #[cfg(windows)]
            input_write_in_flight: Arc::new(AtomicBool::new(false)),
            process: Mutex::new(process),
        });

        {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            state.sessions.insert(id.to_string(), Arc::clone(&session));
        }
        session.publisher.upsert(session.view());

        for (reader, source) in readers {
            spawn_reader(Arc::clone(&session), reader, source);
        }
        let manager = Arc::clone(&self.inner);
        let watcher_session = Arc::clone(&session);
        let watcher = std::thread::Builder::new()
            .name(format!("exec-watch-{}", id.as_str()))
            .spawn(move || watch_process(manager, watcher_session, timeout_ms));
        if let Err(error) = watcher {
            log::error!("exec session {} watcher failed to start: {error}", id);
            self.request_stop(&session, ExecStopReason::Terminate);
            force_shutdown_finalize(Arc::clone(&self.inner), &session);
            return Ok(session.consume_result(max_output_bytes, true, None));
        }

        let yield_window = Duration::from_millis(yield_time_ms as u64);
        let _interaction = session
            .interaction
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let result = wait_for_call(
            &session,
            yield_window,
            max_output_bytes,
            Some(&cancellation),
            true,
        );
        Ok(result)
    }

    pub fn poll(&self, request: ExecSessionPoll) -> Result<ExecSessionResult, ExecManagerError> {
        let session = self.lookup(&request.owner_session_id, &request.exec_session_id)?;
        let _interaction = session
            .interaction
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        if request.chars.len() > MAX_STDIN_BYTES {
            return Err(manager_error(
                ExecManagerErrorKind::InputTooLarge,
                format!("stdin input exceeds {MAX_STDIN_BYTES} UTF-8 bytes"),
                0,
            ));
        }

        if !request.chars.is_empty() && request.chars == "\u{3}" {
            self.request_stop(&session, ExecStopReason::Interrupt);
        } else if !request.chars.is_empty() {
            if !session.tty {
                return Err(manager_error(
                    ExecManagerErrorKind::InputUnsupported,
                    "stdin input requires a PTY execution session".into(),
                    0,
                ));
            }
            if session.is_finished() {
                return Ok(session.consume_result(request.max_output_bytes, false, None));
            }
            let bytes_written = write_input(&session, request.chars.as_bytes()).map_err(
                |(message, bytes_written)| {
                    manager_error(ExecManagerErrorKind::InputWrite, message, bytes_written)
                },
            )?;
            let result = wait_for_call(
                &session,
                Duration::from_millis(request.yield_time_ms as u64),
                request.max_output_bytes,
                Some(&request.cancellation),
                false,
            );
            return Ok(ExecSessionResult {
                input_bytes_written: Some(bytes_written),
                ..result
            });
        }

        if request.cancellation.load(Ordering::SeqCst) {
            return Ok(session.cancelled_without_consuming());
        }
        Ok(wait_for_call(
            &session,
            Duration::from_millis(request.yield_time_ms as u64),
            request.max_output_bytes,
            Some(&request.cancellation),
            false,
        ))
    }

    pub fn stop(
        &self,
        owner_session_id: &str,
        id: &ExecSessionId,
        reason: ExecStopReason,
    ) -> StopResult {
        let session = {
            let state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            state.sessions.get(id.as_str()).cloned()
        };
        let Some(session) = session else {
            return StopResult::NotFound;
        };
        if session.owner_session_id != owner_session_id {
            return StopResult::NotFound;
        }
        if self.request_stop(&session, reason) {
            StopResult::StopRequested
        } else {
            StopResult::AlreadyFinished
        }
    }

    pub fn list(&self, owner_session_id: &str) -> Vec<ExecSessionView> {
        let sessions = {
            let state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            state
                .sessions
                .values()
                .filter(|session| session.owner_session_id == owner_session_id)
                .cloned()
                .collect::<Vec<_>>()
        };
        sessions.into_iter().map(|session| session.view()).collect()
    }

    pub fn owner_session_id(&self, id: &ExecSessionId) -> Option<String> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sessions
            .get(id.as_str())
            .map(|session| session.owner_session_id.clone())
    }

    /// Stop every live child during daemon shutdown and wait for final owner facts.
    pub fn shutdown(&self) {
        let sessions = {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            state.shutting_down = true;
            state.sessions.values().cloned().collect::<Vec<_>>()
        };
        self.stop_and_wait(&sessions, ExecStopReason::Shutdown);
    }

    /// Terminate only this conversation's processes before its rows are removed.
    pub fn shutdown_session(&self, owner_session_id: &str) -> Result<(), String> {
        let sessions = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .sessions
            .values()
            .filter(|session| session.owner_session_id == owner_session_id)
            .cloned()
            .collect::<Vec<_>>();
        self.stop_and_wait(&sessions, ExecStopReason::Terminate);
        for session in &sessions {
            if session
                .core
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .exit
                .is_none()
            {
                return Err(format!(
                    "execution session {} exit is unconfirmed",
                    session.id
                ));
            }
        }
        self.inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .sessions
            .retain(|_, session| session.owner_session_id != owner_session_id);
        Ok(())
    }

    fn stop_and_wait(
        &self,
        sessions: &[Arc<ManagedSession>],
        reason: ExecStopReason,
    ) {
        for session in sessions {
            self.request_stop(session, reason);
        }
        let shutdown_deadline = Instant::now() + Duration::from_secs(3);
        for session in sessions {
            let mut core = session.core.lock().unwrap_or_else(|e| e.into_inner());
            while core.terminal.is_none() && Instant::now() < shutdown_deadline {
                let wait = shutdown_deadline.saturating_duration_since(Instant::now());
                let (next, _) = session
                    .changed
                    .wait_timeout(core, wait)
                    .unwrap_or_else(|e| e.into_inner());
                core = next;
            }
            #[cfg(any(unix, windows))]
            let needs_forced_finalize = core.terminal.is_none();
            drop(core);
            #[cfg(any(unix, windows))]
            if needs_forced_finalize {
                force_shutdown_finalize(Arc::clone(&self.inner), session);
            }
        }
    }

    fn reserve(&self, owner_session_id: &str) -> Result<(), ExecManagerError> {
        let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.shutting_down {
            return Err(manager_error(
                ExecManagerErrorKind::Shutdown,
                "execution manager is shutting down".into(),
                0,
            ));
        }
        let owner_count = state
            .active_by_owner
            .get(owner_session_id)
            .copied()
            .unwrap_or(0);
        if owner_count >= SESSION_LIMIT || state.active_total >= GLOBAL_LIMIT {
            return Err(manager_error(
                ExecManagerErrorKind::QuotaExceeded,
                format!(
                    "execution session quota reached (session {owner_count}/{SESSION_LIMIT}, daemon {}/{GLOBAL_LIMIT})",
                    state.active_total
                ),
                0,
            ));
        }
        state.active_total += 1;
        *state
            .active_by_owner
            .entry(owner_session_id.to_string())
            .or_default() += 1;
        Ok(())
    }

    fn release_reservation(&self, owner_session_id: &str) {
        release_active(&self.inner, owner_session_id);
    }

    fn lookup(
        &self,
        owner_session_id: &str,
        id: &ExecSessionId,
    ) -> Result<Arc<ManagedSession>, ExecManagerError> {
        let session = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sessions
            .get(id.as_str())
            .cloned()
            .ok_or_else(|| {
                manager_error(
                    ExecManagerErrorKind::NotFound,
                    format!("execution session {id} not found"),
                    0,
                )
            })?;
        if session.owner_session_id != owner_session_id {
            return Err(manager_error(
                ExecManagerErrorKind::WrongOwner,
                "execution session belongs to a different conversation".into(),
                0,
            ));
        }
        Ok(session)
    }

    fn request_stop(&self, session: &Arc<ManagedSession>, reason: ExecStopReason) -> bool {
        let (changed, accepted) = {
            let mut core = session.core.lock().unwrap_or_else(|e| e.into_inner());
            if core.finalized || core.exit.is_some() {
                (false, false)
            } else if core.stop_reason.is_some() {
                (false, true)
            } else {
                core.stop_reason = Some(reason);
                core.stop_started = Some(Instant::now());
                core.state = ExecSessionState::Stopping { reason };
                (true, true)
            }
        };
        if !accepted {
            return false;
        }
        if changed {
            session.publisher.upsert(session.view());
        } else {
            return true;
        }
        #[cfg(any(unix, windows))]
        {
            let mut process = session.process.lock().unwrap_or_else(|e| e.into_inner());
            let result = match reason {
                ExecStopReason::Interrupt | ExecStopReason::TurnCancel => process.interrupt(),
                ExecStopReason::Terminate | ExecStopReason::Timeout | ExecStopReason::Shutdown => {
                    process.kill_group()
                }
            };
            if let Err(error) = result {
                log::debug!("exec session {} stop signal: {error}", session.id);
            }
        }
        session.changed.notify_all();
        true
    }
}

impl ManagedSession {
    fn view(&self) -> ExecSessionView {
        let core = self.core.lock().unwrap_or_else(|e| e.into_inner());
        let snapshot = core.capture.snapshot();
        ExecSessionView {
            id: self.id.clone(),
            owner_session_id: SessionId::new(self.owner_session_id.clone()),
            origin_turn_id: TurnId::new(self.origin_turn_id.clone()),
            origin_call_id: ToolCallId::new(self.origin_call_id.clone()),
            state: core.state.clone(),
            tty: self.tty,
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            started_at_ms: self.started_at_ms,
            finished_at_ms: core.finished_at_ms,
            next_offset: snapshot.next_offset,
            output_preview: snapshot.output_preview,
            preview_start_offset: snapshot.preview_start_offset,
            output_complete: core.finalized && core.eof_sources == core.expected_sources,
            capture_omitted_bytes: snapshot.capture_omitted_bytes,
            stream_dropped_bytes: core.stream_dropped_bytes,
            output: None,
        }
    }

    fn is_finished(&self) -> bool {
        self.core
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .terminal
            .is_some()
    }

    fn consume_result(
        &self,
        max_output_bytes: usize,
        starting_call: bool,
        input_bytes_written: Option<usize>,
    ) -> ExecSessionResult {
        let mut core = self.core.lock().unwrap_or_else(|e| e.into_inner());
        let read = core.capture.take_model_pending(max_output_bytes);
        result_from_core(&self.id, &core, read, starting_call, input_bytes_written)
    }

    fn cancelled_without_consuming(&self) -> ExecSessionResult {
        ExecSessionResult {
            outcome: ToolCallOutcome::Cancelled {
                reason: ToolCancellationReason::TurnCancel,
            },
            exec_session_id: Some(self.id.clone()),
            output: String::new(),
            output_truncated: false,
            omitted_bytes: 0,
            exit_code: None,
            timed_out: false,
            cancelled: true,
            input_bytes_written: None,
        }
    }
}

fn result_from_core(
    id: &ExecSessionId,
    core: &SessionCore,
    read: ModelRead,
    starting_call: bool,
    input_bytes_written: Option<usize>,
) -> ExecSessionResult {
    let output = read
        .chunks
        .iter()
        .map(|chunk| chunk.text.as_str())
        .collect::<String>();
    // tty 会话的模型侧出口必须剥控制序列（与 exec::pty_exec 的 strip_ansi
    // 契约同形）：pwsh 在 ConPTY 下会注入 DEC 私有模式（?9001/?1004/?25）、
    // OSC 标题与彩色表格帧，pipe 会话里原生程序也可能自发上色。前端
    // progress/transcript 面保持原样 ANSI，渲染层自行消费；预算与偏移按
    // 原始字节计，此处只净化文本。
    let output = crate::exec::truncate::strip_ansi(&output);
    let exit_code = core.exit.as_ref().and_then(|exit| exit.code);
    let timed_out = matches!(core.terminal, Some(ExecSessionOutcome::TimedOut { .. }));
    let cancelled = matches!(core.terminal, Some(ExecSessionOutcome::Cancelled { .. }));
    let outcome = if core.terminal.is_none() {
        ToolCallOutcome::Yielded {
            exec_session_id: id.clone(),
        }
    } else if !starting_call {
        ToolCallOutcome::Completed
    } else {
        match core.terminal.as_ref().expect("terminal outcome checked") {
            ExecSessionOutcome::Exited { exit_code: 0 } => ToolCallOutcome::Completed,
            ExecSessionOutcome::Exited { exit_code } => failed_outcome(
                "exit_nonzero",
                format!(
                    "command exited with code {exit_code}{}",
                    status_suffix(*exit_code)
                ),
            ),
            ExecSessionOutcome::TimedOut { .. } => {
                failed_outcome("timed_out", "command exceeded timeout_ms".into())
            }
            ExecSessionOutcome::Cancelled {
                reason: ExecCancelReason::TurnCancel,
                ..
            } => ToolCallOutcome::Cancelled {
                reason: ToolCancellationReason::TurnCancel,
            },
            ExecSessionOutcome::Cancelled { reason, .. } => {
                failed_outcome("execution_stopped", format!("command stopped: {reason:?}"))
            }
            ExecSessionOutcome::Interrupted { reason } => ToolCallOutcome::Interrupted {
                reason: match reason {
                    ExecInterruptReason::DaemonRestart => ToolInterruptionReason::DaemonRestart,
                    ExecInterruptReason::UpgradeStateUnknown => {
                        ToolInterruptionReason::LegacyStateUnknown
                    }
                    ExecInterruptReason::LostExecution => ToolInterruptionReason::LostExecution,
                },
            },
        }
    };
    ExecSessionResult {
        outcome,
        exec_session_id: Some(id.clone()),
        output,
        output_truncated: read.omitted_bytes > 0,
        omitted_bytes: read.omitted_bytes,
        exit_code,
        timed_out,
        cancelled,
        input_bytes_written,
    }
}

fn wait_for_call(
    session: &Arc<ManagedSession>,
    window: Duration,
    max_output_bytes: usize,
    cancellation: Option<&Arc<AtomicBool>>,
    starting_call: bool,
) -> ExecSessionResult {
    let deadline = Instant::now() + window;
    let mut core = session.core.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if core.terminal.is_some() {
            let read = core.capture.take_model_pending(max_output_bytes);
            return result_from_core(&session.id, &core, read, starting_call, None);
        }
        if cancellation.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            drop(core);
            if starting_call {
                #[cfg(any(unix, windows))]
                request_stop_inner(session, ExecStopReason::TurnCancel);
            } else {
                return session.cancelled_without_consuming();
            }
            core = session.core.lock().unwrap_or_else(|e| e.into_inner());
        }
        let now = Instant::now();
        if now >= deadline {
            let read = core.capture.take_model_pending(max_output_bytes);
            return result_from_core(&session.id, &core, read, starting_call, None);
        }
        let slice = deadline
            .saturating_duration_since(now)
            .min(Duration::from_millis(100));
        let (next, _) = session
            .changed
            .wait_timeout(core, slice)
            .unwrap_or_else(|e| e.into_inner());
        core = next;
    }
}

fn spawn_reader(
    session: Arc<ManagedSession>,
    mut reader: Box<dyn Read + Send>,
    source: OutputSource,
) {
    std::thread::Builder::new()
        .name(format!("exec-read-{}", session.id.as_str()))
        .spawn(move || {
            // Invalid UTF-8 expands to at most three output bytes per input
            // byte. This keeps each append within one public event boundary.
            let mut buffer = [0u8; 2048];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => append_output(&session, source, &buffer[..count]),
                    Err(error)
                        if error.kind() == std::io::ErrorKind::Interrupted
                            || error.kind() == std::io::ErrorKind::WouldBlock =>
                    {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => {
                        log::debug!("exec session {} output read: {error}", session.id);
                        break;
                    }
                }
            }
            finish_source(&session, source);
        })
        .expect("spawn execution output collector");
}

fn append_output(session: &ManagedSession, source: OutputSource, bytes: &[u8]) {
    let mut core = session.core.lock().unwrap_or_else(|e| e.into_inner());
    if core.finalized {
        return;
    }
    let chunks = core.capture.append_bytes(source, bytes);
    publish_chunks(session, &chunks, &core);
    drop(core);
    session.changed.notify_all();
}

fn finish_source(session: &ManagedSession, source: OutputSource) {
    let mut core = session.core.lock().unwrap_or_else(|e| e.into_inner());
    if core.finalized {
        return;
    }
    let chunks = core.capture.finish_source(source);
    publish_chunks(session, &chunks, &core);
    core.eof_sources += 1;
    drop(core);
    session.changed.notify_all();
}

fn publish_chunks(session: &ManagedSession, chunks: &[OutputChunk], core: &SessionCore) {
    let snapshot = core.capture.snapshot();
    for chunk in chunks {
        session.publisher.output(
            &session.id,
            chunk.start_offset,
            chunk.end_offset,
            contract_source(chunk.source),
            chunk.text.clone(),
            snapshot.clone(),
            core.stream_dropped_bytes,
        );
    }
}

fn contract_source(source: OutputSource) -> ExecOutputSource {
    match source {
        OutputSource::Stdout => ExecOutputSource::Stdout,
        OutputSource::Stderr => ExecOutputSource::Stderr,
        OutputSource::Terminal => ExecOutputSource::Terminal,
    }
}

#[cfg(any(unix, windows))]
fn watch_process(inner: Arc<ManagerInner>, session: Arc<ManagedSession>, timeout_ms: Option<u64>) {
    let deadline = timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
    let mut watch_failed = false;
    let exit = loop {
        let finalized = session.is_finished();
        let result = {
            let mut process = session.process.lock().unwrap_or_else(|e| e.into_inner());
            let result = process.try_wait();
            if let Ok(Some(status)) = &result {
                let mut core = session.core.lock().unwrap_or_else(|e| e.into_inner());
                core.exit = Some(status.clone());
            }
            result
        };
        match result {
            Ok(Some(status)) => {
                if finalized {
                    return;
                }
                break status;
            }
            Ok(None) if finalized => {
                std::thread::sleep(PROCESS_WATCH_INTERVAL);
                continue;
            }
            Ok(None) => {}
            Err(error) => {
                log::error!("exec session {} wait failed: {error}", session.id);
                watch_failed = true;
                let mut process = session.process.lock().unwrap_or_else(|e| e.into_inner());
                let _ = process.kill_group();
            }
        }

        if deadline.is_some_and(|at| Instant::now() >= at) {
            request_stop_inner(&session, ExecStopReason::Timeout);
        }
        let (stop_reason, stop_started) = {
            let core = session.core.lock().unwrap_or_else(|e| e.into_inner());
            (core.stop_reason, core.stop_started)
        };
        if matches!(
            stop_reason,
            Some(ExecStopReason::Interrupt | ExecStopReason::TurnCancel)
        ) && stop_started.is_some_and(|at| at.elapsed() >= Duration::from_secs(2))
        {
            let _ = session
                .process
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .kill_group();
        }
        std::thread::sleep(PROCESS_WATCH_INTERVAL);
    };

    {
        let mut core = session.core.lock().unwrap_or_else(|e| e.into_inner());
        core.exit = Some(exit.clone());
    }
    session.changed.notify_all();

    // ConPTY 契约：子进程退出不产生输出管道 EOF（写端由 master 侧持有）。
    // 收割即关终端，EOF 确定性到来；Unix 下是 no-op。
    {
        let mut process = session.process.lock().unwrap_or_else(|e| e.into_inner());
        process.close_terminal();
    }

    let mut eof_deadline = Instant::now() + OUTPUT_EOF_SETTLE;
    let mut escalated = false;
    loop {
        let mut core = session.core.lock().unwrap_or_else(|e| e.into_inner());
        if core.eof_sources == core.expected_sources {
            break;
        }
        let now = Instant::now();
        if now >= eof_deadline {
            drop(core);
            if escalated {
                break;
            }
            let _ = session
                .process
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .kill_group();
            escalated = true;
            eof_deadline = Instant::now() + OUTPUT_EOF_SETTLE;
            continue;
        }
        let (next, _) = session
            .changed
            .wait_timeout(core, eof_deadline.saturating_duration_since(now))
            .unwrap_or_else(|e| e.into_inner());
        core = next;
        if core.eof_sources == core.expected_sources {
            break;
        }
    }
    finalize_session(inner, &session, Some(exit), watch_failed);
}

#[cfg(any(unix, windows))]
fn request_stop_inner(session: &ManagedSession, reason: ExecStopReason) {
    let changed = {
        let mut core = session.core.lock().unwrap_or_else(|e| e.into_inner());
        if core.finalized || core.exit.is_some() {
            return;
        }
        if core.stop_reason == Some(reason) {
            false
        } else if core.stop_reason.is_some()
            && reason != ExecStopReason::Timeout
            && reason != ExecStopReason::Shutdown
        {
            false
        } else {
            core.stop_reason = Some(reason);
            core.stop_started = Some(Instant::now());
            core.state = ExecSessionState::Stopping { reason };
            true
        }
    };
    if changed {
        session.publisher.upsert(session.view());
    } else {
        return;
    }
    let mut process = session.process.lock().unwrap_or_else(|e| e.into_inner());
    let result = match reason {
        ExecStopReason::Interrupt | ExecStopReason::TurnCancel => process.interrupt(),
        ExecStopReason::Terminate | ExecStopReason::Timeout | ExecStopReason::Shutdown => {
            process.kill_group()
        }
    };
    if let Err(error) = result {
        log::debug!("exec session {} stop signal: {error}", session.id);
    }
    session.changed.notify_all();
}

#[cfg(any(unix, windows))]
fn force_shutdown_finalize(inner: Arc<ManagerInner>, session: &Arc<ManagedSession>) {
    let observed_exit = {
        let mut process = session.process.lock().unwrap_or_else(|e| e.into_inner());
        let _ = process.kill_group();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match process.try_wait() {
                Ok(Some(exit)) => break Some(exit),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(PROCESS_WATCH_INTERVAL);
                }
                Ok(None) => break None,
                Err(error) => {
                    log::error!("exec session {} shutdown reap failed: {error}", session.id);
                    break None;
                }
            }
        }
    };
    let deadline = Instant::now() + OUTPUT_EOF_SETTLE;
    let mut core = session.core.lock().unwrap_or_else(|e| e.into_inner());
    while core.eof_sources < core.expected_sources && Instant::now() < deadline {
        let wait = deadline.saturating_duration_since(Instant::now());
        let (next, _) = session
            .changed
            .wait_timeout(core, wait)
            .unwrap_or_else(|e| e.into_inner());
        core = next;
    }
    drop(core);
    let watch_failed = observed_exit.is_none();
    finalize_session(inner, session, observed_exit, watch_failed);
}

fn finalize_session(
    inner: Arc<ManagerInner>,
    session: &ManagedSession,
    exit: Option<qaqh_pty::SessionExit>,
    watch_failed: bool,
) {
    let (view, mut transcript, truncated, release_slot) = {
        let mut core = session.core.lock().unwrap_or_else(|e| e.into_inner());
        if core.finalized {
            return;
        }
        core.exit = exit.clone().or(core.exit.clone());
        let complete = core.eof_sources == core.expected_sources;
        for source in [
            OutputSource::Terminal,
            OutputSource::Stdout,
            OutputSource::Stderr,
        ] {
            if (source == OutputSource::Terminal && session.tty)
                || (source != OutputSource::Terminal && !session.tty)
            {
                let chunks = core.capture.finish_source(source);
                publish_chunks(session, &chunks, &core);
            }
        }
        let exit_observed = exit.is_some() || core.exit.is_some();
        let output = match (
            core.stop_reason,
            exit.and_then(|status| status.code),
            watch_failed,
        ) {
            (Some(ExecStopReason::Timeout), code, _) => {
                ExecSessionOutcome::TimedOut { exit_code: code }
            }
            (Some(ExecStopReason::Shutdown), code, _) if exit_observed => {
                ExecSessionOutcome::Cancelled {
                    reason: cancel_reason(ExecStopReason::Shutdown),
                    exit_code: code,
                }
            }
            (Some(ExecStopReason::Shutdown), _, _) => ExecSessionOutcome::Interrupted {
                reason: ExecInterruptReason::LostExecution,
            },
            (Some(reason), code, _) => ExecSessionOutcome::Cancelled {
                reason: cancel_reason(reason),
                exit_code: code,
            },
            (None, Some(exit_code), false) => ExecSessionOutcome::Exited { exit_code },
            _ => ExecSessionOutcome::Interrupted {
                reason: ExecInterruptReason::LostExecution,
            },
        };
        core.terminal = Some(output.clone());
        core.state = ExecSessionState::Finished { outcome: output };
        core.finished_at_ms = Some(now_ms());
        core.finalized = true;
        let snapshot = core.capture.snapshot();
        let mut transcript = snapshot
            .transcript_head
            .iter()
            .map(|chunk| chunk.text.as_str())
            .collect::<String>();
        if snapshot.capture_omitted_bytes > 0 {
            transcript.push_str(&format!(
                "\n[{} bytes omitted from captured transcript]\n",
                snapshot.capture_omitted_bytes
            ));
        }
        transcript.extend(
            snapshot
                .transcript_tail
                .iter()
                .map(|chunk| chunk.text.as_str()),
        );
        let truncated = snapshot.capture_omitted_bytes > 0 || !complete;
        let release_slot = !core.active_slot_released;
        core.active_slot_released = true;
        let view = ExecSessionView {
            id: session.id.clone(),
            owner_session_id: SessionId::new(session.owner_session_id.clone()),
            origin_turn_id: TurnId::new(session.origin_turn_id.clone()),
            origin_call_id: ToolCallId::new(session.origin_call_id.clone()),
            state: core.state.clone(),
            tty: session.tty,
            command: session.command.clone(),
            cwd: session.cwd.clone(),
            started_at_ms: session.started_at_ms,
            finished_at_ms: core.finished_at_ms,
            next_offset: snapshot.next_offset,
            output_preview: snapshot.output_preview,
            preview_start_offset: snapshot.preview_start_offset,
            output_complete: complete,
            capture_omitted_bytes: snapshot.capture_omitted_bytes,
            stream_dropped_bytes: core.stream_dropped_bytes,
            output: None,
        };
        (view, transcript, truncated, release_slot)
    };

    session
        .publisher
        .finish(view, std::mem::take(&mut transcript), truncated);
    session.changed.notify_all();
    if release_slot {
        release_active(&inner, &session.owner_session_id);
    }
}

impl Default for ExecSessionManager {
    fn default() -> Self {
        Self::new()
    }
}

fn cancel_reason(reason: ExecStopReason) -> ExecCancelReason {
    match reason {
        ExecStopReason::Interrupt => ExecCancelReason::Interrupt,
        ExecStopReason::Terminate => ExecCancelReason::Terminate,
        ExecStopReason::TurnCancel => ExecCancelReason::TurnCancel,
        ExecStopReason::Shutdown => ExecCancelReason::Shutdown,
        ExecStopReason::Timeout => ExecCancelReason::Shutdown,
    }
}

fn write_input(session: &ManagedSession, input: &[u8]) -> Result<usize, (String, usize)> {
    #[cfg(windows)]
    {
        return write_input_windows(session, input);
    }
    #[cfg(not(windows))]
    write_input_nonblocking(session, input)
}

#[cfg(not(windows))]
fn write_input_nonblocking(
    session: &ManagedSession,
    input: &[u8],
) -> Result<usize, (String, usize)> {
    let mut writer = session.writer.lock().unwrap_or_else(|e| e.into_inner());
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut written = 0;
    while written < input.len() {
        match writer.write(&input[written..]) {
            Ok(0) => return Err(("stdin closed before all input was written".into(), written)),
            Ok(count) => written += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err((
                        format!("stdin write timed out after {written} bytes: {error}"),
                        written,
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                return Err((
                    format!("stdin write failed after {written} bytes: {error}"),
                    written,
                ));
            }
        }
    }
    Ok(written)
}

#[cfg(windows)]
struct InputWriteFlight(Arc<AtomicBool>);

#[cfg(windows)]
impl Drop for InputWriteFlight {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[cfg(windows)]
fn write_input_windows(session: &ManagedSession, input: &[u8]) -> Result<usize, (String, usize)> {
    if session
        .input_write_in_flight
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err((
            "a previous stdin write is still blocked; the execution is stopping".into(),
            0,
        ));
    }

    let writer = Arc::clone(&session.writer);
    let input = input.to_vec();
    let input_len = input.len();
    let progress = Arc::new(AtomicUsize::new(0));
    let worker_progress = Arc::clone(&progress);
    let worker_flight = Arc::clone(&session.input_write_in_flight);
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name(format!("exec-input-{}", session.id.as_str()))
        .spawn(move || {
            let flight = InputWriteFlight(worker_flight);
            let result = write_input_blocking(&writer, &input, &worker_progress);
            drop(flight);
            let _ = sender.send(result);
        });
    if let Err(error) = worker {
        session
            .input_write_in_flight
            .store(false, Ordering::Release);
        return Err((format!("start stdin writer: {error}"), 0));
    }

    match receiver.recv_timeout(Duration::from_secs(5)) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            let written = progress.load(Ordering::Acquire);
            if written == input_len {
                return Ok(written);
            }
            terminate_after_unsettled_input(session);
            let _ = receiver.recv_timeout(Duration::from_secs(1));
            Err((
                format!("stdin write timed out after {written} bytes; execution was terminated"),
                written,
            ))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            let written = progress.load(Ordering::Acquire);
            terminate_after_unsettled_input(session);
            Err((
                format!("stdin writer stopped after {written} bytes; execution was terminated"),
                written,
            ))
        }
    }
}

#[cfg(windows)]
fn write_input_blocking(
    writer: &Arc<Mutex<Box<dyn Write + Send>>>,
    input: &[u8],
    progress: &AtomicUsize,
) -> Result<usize, (String, usize)> {
    let mut writer = writer.lock().unwrap_or_else(|error| error.into_inner());
    let mut written = 0;
    while written < input.len() {
        match writer.write(&input[written..]) {
            Ok(0) => return Err(("stdin closed before all input was written".into(), written)),
            Ok(count) => {
                written += count;
                progress.store(written, Ordering::Release);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                return Err((
                    format!("stdin write failed after {written} bytes: {error}"),
                    written,
                ));
            }
        }
    }
    Ok(written)
}

#[cfg(windows)]
fn terminate_after_unsettled_input(session: &ManagedSession) {
    request_stop_inner(session, ExecStopReason::Terminate);
    let force_kill = {
        let core = session
            .core
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        !core.finalized
            && core.exit.is_none()
            && matches!(
                core.stop_reason,
                Some(ExecStopReason::Interrupt | ExecStopReason::TurnCancel)
            )
    };
    if force_kill {
        let _ = session
            .process
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .kill_group();
    }
}

fn release_active(inner: &ManagerInner, owner_session_id: &str) {
    let mut state = inner.state.lock().unwrap_or_else(|e| e.into_inner());
    state.active_total = state.active_total.saturating_sub(1);
    if let Some(count) = state.active_by_owner.get_mut(owner_session_id) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            state.active_by_owner.remove(owner_session_id);
        }
    }
}

fn reap_expired(inner: &ManagerInner) {
    let cutoff = now_ms().saturating_sub(TERMINAL_RETENTION.as_millis() as i64);
    let mut state = inner.state.lock().unwrap_or_else(|e| e.into_inner());
    state.sessions.retain(|_, session| {
        let core = session.core.lock().unwrap_or_else(|e| e.into_inner());
        core.terminal.is_none()
            || core
                .finished_at_ms
                .map_or(true, |finished| finished > cutoff)
    });
}

fn failed_outcome(code: &str, message: String) -> ToolCallOutcome {
    ToolCallOutcome::Failed {
        error: qaqh_types::SessionToolError {
            code: code.to_string(),
            message,
            retryable: false,
            details_ref: None,
        },
    }
}

/// 负退出码 = Windows NTSTATUS（0xC0000142 加载/控制台初始化失败、
/// 0xC0000005 访问冲突…）。十进制 i32 对人不友好，补一个十六进制位型，
/// 让「进程根本没跑起来」与「命令返回非零」在错误信息里可分辨。
fn status_suffix(exit_code: i32) -> String {
    if exit_code < 0 {
        format!(" (0x{:08X})", exit_code as u32)
    } else {
        String::new()
    }
}

fn manager_error(
    kind: ExecManagerErrorKind,
    message: String,
    bytes_written: usize,
) -> ExecManagerError {
    ExecManagerError {
        kind,
        message,
        bytes_written,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[derive(Debug, Default)]
    struct RecordingPublisher {
        outputs: Mutex<Vec<(u64, u64, ExecOutputSource, String)>>,
    }

    impl ExecSessionPublisher for RecordingPublisher {
        fn upsert(&self, _view: ExecSessionView) {}

        fn output(
            &self,
            _exec_session_id: &ExecSessionId,
            start_offset: u64,
            end_offset: u64,
            source: ExecOutputSource,
            text: String,
            _snapshot: OutputSnapshot,
            _stream_dropped_bytes: u64,
        ) {
            self.outputs
                .lock()
                .unwrap()
                .push((start_offset, end_offset, source, text));
        }

        fn finish(&self, _view: ExecSessionView, _transcript: String, _truncated: bool) {}
    }

    fn request(command: &str, yield_time_ms: u32, timeout_ms: Option<u64>) -> ExecSessionStart {
        ExecSessionStart {
            owner_session_id: "SES-EXEC-TEST".into(),
            origin_turn_id: "TURN-EXEC-TEST".into(),
            origin_call_id: "CALL-EXEC-TEST".into(),
            command: command.into(),
            cwd: None,
            argv: vec!["/bin/sh".into(), "-c".into(), command.into()],
            env: Vec::new(),
            tty: false,
            timeout_ms,
            yield_time_ms,
            max_output_bytes: 4096,
            cancellation: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn split_pipe_captures_both_streams_and_exit_status() {
        let manager = ExecSessionManager::new();
        let success_publisher = Arc::new(RecordingPublisher::default());
        let success = manager
            .start(
                request("printf diagnostic >&2; exit 0", 1000, None),
                success_publisher.clone(),
            )
            .expect("successful command");
        assert_eq!(success.outcome, ToolCallOutcome::Completed);
        assert_eq!(success.exit_code, Some(0));
        assert!(success_publisher.outputs.lock().unwrap().iter().any(
            |(_, _, source, text)| *source == ExecOutputSource::Stderr && text == "diagnostic"
        ));

        let failed_publisher = Arc::new(RecordingPublisher::default());
        let failed = manager
            .start(
                request("printf stdout; printf stderr >&2; exit 7", 1000, None),
                failed_publisher.clone(),
            )
            .expect("nonzero command");
        assert_eq!(failed.exit_code, Some(7));
        assert!(matches!(
            failed.outcome,
            ToolCallOutcome::Failed { ref error } if error.code == "exit_nonzero"
        ));

        let mut outputs = failed_publisher.outputs.lock().unwrap().clone();
        outputs.sort_by_key(|(start, _, _, _)| *start);
        assert!(outputs.iter().any(|(_, _, source, text)| {
            *source == ExecOutputSource::Stdout && text == "stdout"
        }));
        assert!(outputs.iter().any(|(_, _, source, text)| {
            *source == ExecOutputSource::Stderr && text == "stderr"
        }));
        for (start, end, _, text) in &outputs {
            assert_eq!(end - start, text.len() as u64);
        }
        for pair in outputs.windows(2) {
            assert_eq!(
                pair[0].1, pair[1].0,
                "global output offsets must be contiguous"
            );
        }
        manager.shutdown();
    }

    #[test]
    fn shutdown_session_stops_only_its_owned_processes() {
        let manager = ExecSessionManager::new();
        let a = manager
            .start(
                request("sleep 30", 20, None),
                Arc::new(RecordingPublisher::default()),
            )
            .expect("start A");
        assert!(matches!(a.outcome, ToolCallOutcome::Yielded { .. }));
        let mut b_request = request("sleep 30", 20, None);
        b_request.owner_session_id = "OTHER-SESSION".into();
        let b = manager
            .start(b_request, Arc::new(RecordingPublisher::default()))
            .expect("start B");
        assert!(matches!(b.outcome, ToolCallOutcome::Yielded { .. }));
        manager
            .shutdown_session("SES-EXEC-TEST")
            .expect("confirmed stop");
        assert!(manager.list("SES-EXEC-TEST").is_empty());
        let other = manager.list("OTHER-SESSION");
        assert_eq!(other.len(), 1);
        assert!(matches!(other[0].state, ExecSessionState::Running));
        manager.shutdown();
    }

    #[test]
    fn yielded_pipe_poll_collects_new_output_and_finishes() {
        let manager = ExecSessionManager::new();
        let publisher = Arc::new(RecordingPublisher::default());
        let initial = manager
            .start(
                request("printf first; sleep 0.2; printf second", 20, None),
                publisher,
            )
            .expect("yielding command");
        let exec_session_id = match initial.outcome {
            ToolCallOutcome::Yielded { exec_session_id } => exec_session_id,
            outcome => panic!("expected yielded outcome, got {outcome:?}"),
        };
        let polled = manager
            .poll(ExecSessionPoll {
                exec_session_id,
                owner_session_id: "SES-EXEC-TEST".into(),
                chars: String::new(),
                yield_time_ms: 1000,
                max_output_bytes: 4096,
                cancellation: Arc::new(AtomicBool::new(false)),
            })
            .expect("poll yielded command");
        assert_eq!(polled.outcome, ToolCallOutcome::Completed);
        let output = format!("{}{}", initial.output, polled.output);
        assert!(
            output.contains("first"),
            "initial output was lost: {output:?}"
        );
        assert!(
            output.contains("second"),
            "polled output was lost: {output:?}"
        );

        let background = manager
            .start(
                request("sleep 0.1", 20, None),
                Arc::new(RecordingPublisher::default()),
            )
            .expect("background command");
        assert!(matches!(
            background.outcome,
            ToolCallOutcome::Yielded { .. }
        ));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !manager
            .list("SES-EXEC-TEST")
            .iter()
            .any(|view| view.command == "sleep 0.1" && view.state.is_terminal())
        {
            assert!(
                Instant::now() < deadline,
                "background exit was not published"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        manager.shutdown();
    }

    #[test]
    fn pty_poll_writes_input_and_returns_the_response() {
        let manager = ExecSessionManager::new();
        let mut start = request(r#"read -r value; printf 'reply:%s\n' "$value""#, 20, None);
        start.tty = true;
        let initial = manager
            .start(start, Arc::new(RecordingPublisher::default()))
            .expect("interactive command");
        let exec_session_id = match initial.outcome {
            ToolCallOutcome::Yielded { exec_session_id } => exec_session_id,
            outcome => panic!("expected yielded outcome, got {outcome:?}"),
        };
        let polled = manager
            .poll(ExecSessionPoll {
                exec_session_id,
                owner_session_id: "SES-EXEC-TEST".into(),
                chars: "hello\n".into(),
                yield_time_ms: 1000,
                max_output_bytes: 4096,
                cancellation: Arc::new(AtomicBool::new(false)),
            })
            .expect("write terminal input and poll");
        assert_eq!(polled.outcome, ToolCallOutcome::Completed);
        assert!(polled.input_bytes_written.unwrap_or_default() >= 6);
        assert!(
            polled.output.contains("reply:hello"),
            "response missing: {:?}",
            polled.output
        );
        manager.shutdown();
    }

    #[test]
    fn hard_timeout_kills_the_command_and_returns_timed_out() {
        let manager = ExecSessionManager::new();
        let result = manager
            .start(
                request("sleep 5", 1000, Some(50)),
                Arc::new(RecordingPublisher::default()),
            )
            .expect("timed out command");
        assert!(result.timed_out);
        assert!(matches!(
            result.outcome,
            ToolCallOutcome::Failed { ref error } if error.code == "timed_out"
        ));
        manager.shutdown();
    }
}
