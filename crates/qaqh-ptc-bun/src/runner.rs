//! 子进程生命周期、控制通道与结果归类。

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use qaqh_ptc_core::{
    PtcBindingFn, PtcBindingNamespace, PtcError, PtcFailureKind, PtcRunFailure, PtcRunResult,
    PtcRunSpec,
};
use serde_json::Value;

use crate::protocol::{ChildError, ChildFrame, ErrorClassDecl, HostFrame, NamespaceDecl};

/// 内嵌的 runner 源码；以内容哈希命名落盘，因此升级后不会复用旧文件。
const BOOTSTRAP_TS: &str = include_str!("../runtime/ptc-bootstrap.ts");

/// spawn 前的命令改写钩子（沙箱包装、环境裁剪、可执行文件重定位）。
///
/// 钩子在 argv/cwd/env 都已设置之后调用；`Err` 会让本次执行以
/// [`PtcError::Unavailable`] 失败，而不是无约束地启动。
pub type SpawnHook = dyn Fn(&mut Command) -> Result<(), String> + Send + Sync;

/// bun 后端配置。
#[derive(Debug, Clone)]
pub struct BunConfig {
    /// bun 可执行文件；默认走 `PATH` 上的 `bun`。
    pub executable: PathBuf,
    /// 未显式给出预算时的默认经过时间上限。
    pub default_timeout: Duration,
    /// 任何请求都不超过的经过时间上限。
    pub max_timeout: Duration,
    /// 日志与完成值的合并字节上限。
    pub max_output_bytes: usize,
    /// 单条控制帧与累计未读输出的字节上限。
    pub max_message_bytes: usize,
    /// 等待 runner 报告 `ready` 的上限。
    pub startup_timeout: Duration,
}

impl Default for BunConfig {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("bun"),
            default_timeout: Duration::from_secs(120),
            max_timeout: Duration::from_secs(600),
            max_output_bytes: 16 * 1024 * 1024,
            max_message_bytes: 8 * 1024 * 1024,
            startup_timeout: Duration::from_secs(15),
        }
    }
}

/// 共享给读线程与主线程的执行状态。
#[derive(Debug)]
struct Shared {
    logs: Mutex<Vec<String>>,
    bytes: AtomicUsize,
    stderr: Mutex<String>,
}

impl Shared {
    fn new() -> Self {
        Self {
            logs: Mutex::new(Vec::new()),
            bytes: AtomicUsize::new(0),
            stderr: Mutex::new(String::new()),
        }
    }

    /// 记账一段输出；超出预算返回 `false`（调用方转成 `output-limit`）。
    fn push_log(&self, text: String, max_output_bytes: usize) -> bool {
        let added = text.len();
        let total = self.bytes.fetch_add(added, Ordering::SeqCst) + added;
        if total > max_output_bytes {
            return false;
        }
        self.logs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(text);
        true
    }

    fn append_stderr(&self, text: &str, max_bytes: usize) {
        let mut stderr = self
            .stderr
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stderr.push_str(text);
        trim_to_tail(&mut stderr, max_bytes);
    }

    fn logs(&self) -> Vec<String> {
        self.logs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn stderr(&self) -> String {
        self.stderr
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn bytes(&self) -> usize {
        self.bytes.load(Ordering::SeqCst)
    }
}

/// 读线程向主线程报告的进展。
#[derive(Debug)]
enum Event {
    Ready,
    Done {
        value: Option<Value>,
        error: Option<ChildError>,
    },
    Limit,
    Protocol(String),
    Eof,
}

/// 绑定索引：`(命名空间全局名, 成员名)` → 宿主实现。
type BindingIndex = HashMap<(String, String), Arc<PtcBindingFn>>;

/// 执行一次程序；所有程序级失败都以结果字段返回。
pub(crate) fn run(
    config: &BunConfig,
    hook: Option<&Arc<SpawnHook>>,
    spec: PtcRunSpec,
) -> Result<PtcRunResult, PtcError> {
    let bindings = binding_index(&spec.bindings)?;
    let declarations = declarations(&spec.bindings);
    let script = bootstrap_path()?;

    let mut command = Command::new(&config.executable);
    command.arg(&script);
    command.current_dir(&spec.cwd);
    command.env_clear();
    // 最小环境：runner 不需要用户的其它变量；bun 需要 PATH/HOME 才能正常解析自身缓存。
    for name in ["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "TZ"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.env("NO_COLOR", "1");
    command.env("QAQH_PTC", "1");
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(hook) = hook {
        hook(&mut command).map_err(PtcError::Unavailable)?;
    }

    let mut child = command.spawn().map_err(|error| {
        PtcError::Unavailable(format!(
            "cannot spawn {}: {error}",
            config.executable.display()
        ))
    })?;

    let Some(stdin) = child.stdin.take() else {
        stop(&mut child);
        return Err(PtcError::Unavailable(
            "bun process did not expose stdin".to_owned(),
        ));
    };
    let Some(stdout) = child.stdout.take() else {
        stop(&mut child);
        return Err(PtcError::Unavailable(
            "bun process did not expose stdout".to_owned(),
        ));
    };
    let Some(stderr) = child.stderr.take() else {
        stop(&mut child);
        return Err(PtcError::Unavailable(
            "bun process did not expose stderr".to_owned(),
        ));
    };

    let shared = Arc::new(Shared::new());
    spawn_stderr_drain(stderr, Arc::clone(&shared), config.max_message_bytes);

    let stdin = Arc::new(Mutex::new(stdin));
    let (tx, rx) = mpsc::channel();
    {
        let stdin = Arc::clone(&stdin);
        let shared = Arc::clone(&shared);
        let bindings = Arc::clone(&bindings);
        let max_message_bytes = config.max_message_bytes;
        let max_output_bytes = spec.max_output_bytes;
        thread::spawn(move || {
            read_loop(
                stdout,
                stdin,
                bindings,
                shared,
                max_message_bytes,
                max_output_bytes,
                tx,
            );
        });
    }

    // 先等 runner 装好输出补丁，再发 run；这样启动期失败能拿到 stderr 而不是超时。
    match wait_ready(&rx, &spec, config, &shared, &mut child) {
        Ok(Ready::Ready) => {}
        Ok(Ready::Aborted(kind)) => {
            stop(&mut child);
            return Ok(terminal(&shared, kind));
        }
        Err(error) => {
            stop(&mut child);
            return Err(error);
        }
    }

    let run_frame = HostFrame::Run {
        code: spec.program.clone(),
        namespaces: declarations,
        max_output_bytes: spec.max_output_bytes,
    };
    if let Err(message) = write_frame(&stdin, &run_frame, config.max_message_bytes) {
        stop(&mut child);
        return Ok(PtcRunResult::failed(
            shared.logs(),
            PtcRunFailure::new(PtcFailureKind::Protocol, message),
        ));
    }

    let outcome = drive(&rx, &spec, &shared);
    stop(&mut child);

    Ok(match outcome {
        Outcome::Done { value, error } => match error {
            Some(error) => PtcRunResult::failed(
                shared.logs(),
                PtcRunFailure::new(
                    failure_kind(&error.kind),
                    format!("{}{}", error.message, stderr_suffix(&shared)),
                ),
            ),
            None => match value {
                None => PtcRunResult::success(shared.logs(), None),
                Some(value) => {
                    let value_bytes = serde_json::to_vec(&value)
                        .map(|bytes| bytes.len())
                        .unwrap_or(usize::MAX);
                    if shared.bytes() + value_bytes > spec.max_output_bytes {
                        PtcRunResult::failed(
                            shared.logs(),
                            PtcRunFailure::new(
                                PtcFailureKind::OutputLimit,
                                format!("program output exceeded {} bytes", spec.max_output_bytes),
                            ),
                        )
                    } else {
                        PtcRunResult::success(shared.logs(), Some(value))
                    }
                }
            },
        },
        Outcome::Failed(failure) => {
            PtcRunResult::failed(shared.logs(), with_stderr(failure, &shared))
        }
    })
}

enum Ready {
    Ready,
    Aborted(PtcRunFailure),
}

enum Outcome {
    Done {
        value: Option<Value>,
        error: Option<ChildError>,
    },
    Failed(PtcRunFailure),
}

fn wait_ready(
    rx: &mpsc::Receiver<Event>,
    spec: &PtcRunSpec,
    config: &BunConfig,
    shared: &Shared,
    child: &mut std::process::Child,
) -> Result<Ready, PtcError> {
    let deadline = Instant::now()
        .checked_add(config.startup_timeout)
        .unwrap_or_else(|| Instant::now() + config.startup_timeout);
    loop {
        if spec.cancel.is_cancelled() {
            return Ok(Ready::Aborted(PtcRunFailure::new(
                PtcFailureKind::Abort,
                "execution cancelled before start",
            )));
        }
        if Instant::now() >= deadline {
            let stderr = shared.stderr();
            let mut message = format!(
                "bun did not become ready within {}ms",
                config.startup_timeout.as_millis()
            );
            if !stderr.is_empty() {
                message.push_str(&format!("\nstderr:\n{stderr}"));
            }
            return Err(PtcError::Unavailable(message));
        }
        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(Event::Ready) => return Ok(Ready::Ready),
            Ok(Event::Done { .. }) | Ok(Event::Limit) | Ok(Event::Eof) => {
                if let Ok(status) = child.try_wait() {
                    let detail = status.map(|s| format!(" ({s})")).unwrap_or_default();
                    return Ok(Ready::Aborted(PtcRunFailure::new(
                        PtcFailureKind::WorkerExit,
                        format!(
                            "bun exited before the program started{detail}{}",
                            stderr_suffix(shared)
                        ),
                    )));
                }
                return Ok(Ready::Aborted(PtcRunFailure::new(
                    PtcFailureKind::WorkerExit,
                    format!(
                        "bun closed the control channel before the program started{}",
                        stderr_suffix(shared)
                    ),
                )));
            }
            Ok(Event::Protocol(message)) => {
                return Ok(Ready::Aborted(PtcRunFailure::new(
                    PtcFailureKind::Protocol,
                    message,
                )));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Ok(Ready::Aborted(PtcRunFailure::new(
                    PtcFailureKind::WorkerExit,
                    format!(
                        "control channel closed before the program started{}",
                        stderr_suffix(shared)
                    ),
                )));
            }
        }
    }
}

fn drive(rx: &mpsc::Receiver<Event>, spec: &PtcRunSpec, shared: &Shared) -> Outcome {
    let deadline = Instant::now()
        .checked_add(spec.timeout)
        .unwrap_or_else(|| Instant::now() + spec.timeout);
    loop {
        if spec.cancel.is_cancelled() {
            return Outcome::Failed(PtcRunFailure::new(
                PtcFailureKind::Abort,
                "execution cancelled",
            ));
        }
        let now = Instant::now();
        if now >= deadline {
            return Outcome::Failed(PtcRunFailure::new(
                PtcFailureKind::Timeout,
                format!(
                    "execution deadline reached ({}ms)",
                    spec.timeout.as_millis()
                ),
            ));
        }
        let wait = (deadline - now).min(Duration::from_millis(25));
        match rx.recv_timeout(wait) {
            Ok(Event::Ready) => {}
            Ok(Event::Done { value, error }) => return Outcome::Done { value, error },
            Ok(Event::Limit) => {
                return Outcome::Failed(PtcRunFailure::new(
                    PtcFailureKind::OutputLimit,
                    format!("program output exceeded {} bytes", spec.max_output_bytes),
                ));
            }
            Ok(Event::Protocol(message)) => {
                return Outcome::Failed(PtcRunFailure::new(PtcFailureKind::Protocol, message));
            }
            Ok(Event::Eof) => {
                return Outcome::Failed(PtcRunFailure::new(
                    PtcFailureKind::WorkerExit,
                    format!(
                        "bun exited before the program settled{}",
                        stderr_suffix(shared)
                    ),
                ));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Outcome::Failed(PtcRunFailure::new(
                    PtcFailureKind::WorkerExit,
                    format!(
                        "control channel closed before the program settled{}",
                        stderr_suffix(shared)
                    ),
                ));
            }
        }
    }
}

fn terminal(shared: &Shared, failure: PtcRunFailure) -> PtcRunResult {
    PtcRunResult::failed(shared.logs(), with_stderr(failure, shared))
}

fn with_stderr(failure: PtcRunFailure, shared: &Shared) -> PtcRunFailure {
    PtcRunFailure::new(
        failure.kind,
        format!("{}{}", failure.message, stderr_suffix(shared)),
    )
}

fn stderr_suffix(shared: &Shared) -> String {
    let stderr = shared.stderr();
    if stderr.trim().is_empty() {
        String::new()
    } else {
        format!("\nstderr:\n{stderr}")
    }
}

fn failure_kind(kind: &str) -> PtcFailureKind {
    match kind {
        "invalid-output" => PtcFailureKind::InvalidOutput,
        "output-limit" => PtcFailureKind::OutputLimit,
        "protocol" => PtcFailureKind::Protocol,
        _ => PtcFailureKind::Exception,
    }
}

fn stop(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn spawn_stderr_drain(stderr: std::process::ChildStderr, shared: Arc<Shared>, max_bytes: usize) {
    thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => shared.append_stderr(&line, max_bytes),
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn read_loop(
    stdout: ChildStdout,
    stdin: Arc<Mutex<ChildStdin>>,
    bindings: Arc<BindingIndex>,
    shared: Arc<Shared>,
    max_message_bytes: usize,
    max_output_bytes: usize,
    tx: mpsc::Sender<Event>,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => {
                let _ = tx.send(Event::Eof);
                return;
            }
            Ok(_) => {}
        }
        if line.len() > max_message_bytes {
            let _ = tx.send(Event::Protocol(format!(
                "control frame exceeds {max_message_bytes} bytes"
            )));
            return;
        }
        while matches!(line.last(), Some(b'\n') | Some(b'\r')) {
            line.pop();
        }
        if line.is_empty() {
            continue;
        }
        let frame = match serde_json::from_slice::<ChildFrame>(&line) {
            Ok(frame) => frame,
            Err(error) => {
                let _ = tx.send(Event::Protocol(format!("invalid control frame: {error}")));
                return;
            }
        };
        match frame {
            ChildFrame::Ready => {
                let _ = tx.send(Event::Ready);
            }
            ChildFrame::Log { text } => {
                if !shared.push_log(text, max_output_bytes) {
                    let _ = tx.send(Event::Limit);
                    return;
                }
            }
            ChildFrame::Call {
                id,
                global,
                name,
                args,
            } => {
                let reply = match bindings.get(&(global.clone(), name.clone())) {
                    Some(call) => match call(args) {
                        Ok(value) => HostFrame::Result {
                            id,
                            ok: true,
                            value: Some(value),
                            error: None,
                        },
                        Err(message) => HostFrame::Result {
                            id,
                            ok: false,
                            value: None,
                            error: Some(message),
                        },
                    },
                    None => HostFrame::Result {
                        id,
                        ok: false,
                        value: None,
                        error: Some(format!("binding is not declared: {global}.{name}")),
                    },
                };
                if let Err(message) = write_frame(&stdin, &reply, max_message_bytes) {
                    let _ = tx.send(Event::Protocol(message));
                    return;
                }
            }
            ChildFrame::Done { value, error } => {
                let _ = tx.send(Event::Done { value, error });
                return;
            }
            ChildFrame::Limit => {
                let _ = tx.send(Event::Limit);
                return;
            }
        }
    }
}

fn write_frame(
    stdin: &Arc<Mutex<ChildStdin>>,
    frame: &HostFrame,
    max_message_bytes: usize,
) -> Result<(), String> {
    let mut bytes =
        serde_json::to_vec(frame).map_err(|error| format!("serialize control frame: {error}"))?;
    if bytes.len() > max_message_bytes {
        return Err(format!(
            "control frame exceeds {max_message_bytes} bytes before send"
        ));
    }
    bytes.push(b'\n');
    let mut guard = stdin
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .write_all(&bytes)
        .map_err(|error| format!("write control frame: {error}"))?;
    guard
        .flush()
        .map_err(|error| format!("flush control frame: {error}"))
}

fn declarations(bindings: &[PtcBindingNamespace]) -> Vec<NamespaceDecl> {
    bindings
        .iter()
        .map(|namespace| NamespaceDecl {
            global: namespace.global.clone(),
            names: namespace
                .functions
                .iter()
                .map(|member| member.name.clone())
                .collect(),
            error_class: namespace.error_class.as_ref().map(|class| ErrorClassDecl {
                name: class.name.clone(),
                member_name_property: class.member_name_property.clone(),
            }),
        })
        .collect()
}

fn binding_index(bindings: &[PtcBindingNamespace]) -> Result<Arc<BindingIndex>, PtcError> {
    validate_bindings(bindings)?;
    let mut index = HashMap::new();
    for namespace in bindings {
        for member in &namespace.functions {
            index.insert(
                (namespace.global.clone(), member.name.clone()),
                Arc::clone(&member.call),
            );
        }
    }
    Ok(Arc::new(index))
}

/// runner 占用的全局与 JS 内建名；这些名字不能用作命名空间全局。
const RESERVED_GLOBALS: &[&str] = &[
    "Bun",
    "Array",
    "Boolean",
    "Date",
    "Error",
    "Function",
    "Infinity",
    "JSON",
    "Map",
    "Math",
    "NaN",
    "Number",
    "Object",
    "Promise",
    "RegExp",
    "Set",
    "String",
    "Symbol",
    "TextDecoder",
    "TextEncoder",
    "clearTimeout",
    "console",
    "eval",
    "global",
    "globalThis",
    "process",
    "queueMicrotask",
    "self",
    "setTimeout",
    "structuredClone",
    "undefined",
    "window",
];

fn validate_bindings(bindings: &[PtcBindingNamespace]) -> Result<(), PtcError> {
    let mut globals: HashSet<&str> = HashSet::new();
    let mut errors: HashSet<&str> = HashSet::new();
    for namespace in bindings {
        if !is_portable_identifier(&namespace.global) {
            return Err(PtcError::Invalid(format!(
                "binding global {:?} is not a portable identifier",
                namespace.global
            )));
        }
        if RESERVED_GLOBALS.contains(&namespace.global.as_str()) {
            return Err(PtcError::Invalid(format!(
                "binding global {:?} is reserved by the runner",
                namespace.global
            )));
        }
        if !globals.insert(namespace.global.as_str()) {
            return Err(PtcError::Invalid(format!(
                "duplicate binding global {:?}",
                namespace.global
            )));
        }
        let mut names: HashSet<&str> = HashSet::new();
        for member in &namespace.functions {
            if member.name.is_empty() {
                return Err(PtcError::Invalid(format!(
                    "namespace {:?} has an empty member name",
                    namespace.global
                )));
            }
            if !names.insert(member.name.as_str()) {
                return Err(PtcError::Invalid(format!(
                    "namespace {:?} declares {:?} twice",
                    namespace.global, member.name
                )));
            }
        }
        if let Some(class) = &namespace.error_class {
            if !is_portable_identifier(&class.name) {
                return Err(PtcError::Invalid(format!(
                    "binding error class {:?} is not a portable identifier",
                    class.name
                )));
            }
            if RESERVED_GLOBALS.contains(&class.name.as_str()) {
                return Err(PtcError::Invalid(format!(
                    "binding error class {:?} is reserved by the runner",
                    class.name
                )));
            }
            if !errors.insert(class.name.as_str()) || globals.contains(class.name.as_str()) {
                return Err(PtcError::Invalid(format!(
                    "injected global {:?} is declared twice",
                    class.name
                )));
            }
            if class.member_name_property.is_empty() {
                return Err(PtcError::Invalid(format!(
                    "binding error class {:?} needs a member-name property",
                    class.name
                )));
            }
        }
    }
    Ok(())
}

fn is_portable_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn trim_to_tail(buffer: &mut String, max_bytes: usize) {
    if buffer.len() <= max_bytes {
        return;
    }
    let mut cut = buffer.len() - max_bytes;
    while cut < buffer.len() && !buffer.is_char_boundary(cut) {
        cut += 1;
    }
    buffer.drain(..cut);
}

/// 把 runner 源码落到临时目录，返回可直接交给 bun 的路径。
fn bootstrap_path() -> Result<PathBuf, PtcError> {
    let dir = std::env::temp_dir().join("qaqh-ptc");
    std::fs::create_dir_all(&dir).map_err(|error| {
        PtcError::Unavailable(format!("cannot create {}: {error}", dir.display()))
    })?;
    let tag = content_tag(BOOTSTRAP_TS.as_bytes());
    let path = dir.join(format!("bootstrap-{tag:016x}.ts"));
    if path.is_file() {
        return Ok(path);
    }
    let staging = dir.join(format!("bootstrap-{tag:016x}.{}.tmp", std::process::id()));
    std::fs::write(&staging, BOOTSTRAP_TS).map_err(|error| {
        PtcError::Unavailable(format!("cannot write {}: {error}", staging.display()))
    })?;
    match std::fs::rename(&staging, &path) {
        Ok(()) => Ok(path),
        Err(error) => {
            let _ = std::fs::remove_file(&staging);
            if path.is_file() {
                Ok(path)
            } else {
                Err(PtcError::Unavailable(format!(
                    "cannot materialize {}: {error}",
                    path.display()
                )))
            }
        }
    }
}

fn content_tag(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_follow_the_portable_subset() {
        for name in ["tools", "Tools2", "_x", "a_b_c"] {
            assert!(is_portable_identifier(name), "{name}");
        }
        for name in ["", "2tools", "tools-x", "工具", "tools.x"] {
            assert!(!is_portable_identifier(name), "{name}");
        }
    }

    #[test]
    fn reserved_and_duplicate_globals_are_rejected() {
        let namespace = |global: &str| PtcBindingNamespace::new(global);
        assert!(validate_bindings(&[namespace("console")]).is_err());
        assert!(validate_bindings(&[namespace("tools"), namespace("tools")]).is_err());
        assert!(validate_bindings(&[namespace("tools")]).is_ok());
    }

    #[test]
    fn error_class_must_not_collide_with_a_namespace() {
        let namespace = PtcBindingNamespace::new("tools").with_error_class("tools", "toolName");
        assert!(validate_bindings(&[namespace]).is_err());
    }

    #[test]
    fn tail_trimming_keeps_char_boundaries() {
        let mut buffer = String::from("abcdef");
        trim_to_tail(&mut buffer, 3);
        assert_eq!(buffer, "def");

        let mut buffer = String::from("中文中文");
        trim_to_tail(&mut buffer, 5);
        assert!(buffer.len() <= 5);
        assert!(buffer.is_char_boundary(0));
    }
}
