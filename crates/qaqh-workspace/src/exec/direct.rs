//! exec::direct — direct_exec 主引擎 + ExecOutput（registry-native 生命周期）。

use std::io::Write;
use std::sync::{Arc, atomic::AtomicU64};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{ExecOutputStream, ExecProgressSender};

#[cfg(windows)]
use super::pipe::pipe_available_bytes;
#[cfg(unix)]
use super::pipe::set_pipe_nonblocking;
use super::pipe::{PipePumpCtx, Readiness, SEAL_JOIN_BUDGET, spawn_pipe_reader, wait_reader_done};
use super::truncate::{strip_ansi, token_truncate};

/// Internal process launch primitive. The argv is always derived from the
/// selected shell; the public exec tool no longer accepts a direct argv array.
/// Uses background threads for pipe reading and poll-based timeout.
#[cfg(test)]
#[allow(clippy::too_many_arguments)] // 参数面塑形另立项（PLAN D-5）
pub(crate) fn direct_exec(
    argv: &[String],
    env: Option<&[(String, String)]>,
    cwd: Option<&str>,
    max_output_tokens: u32,
    timeout_secs: u64,
    background_after_secs: Option<u64>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    progress_tx: Option<ExecProgressSender>,
    tool_call_id: &str,
) -> ExecOutput {
    direct_exec_inner(
        argv,
        env,
        cwd,
        max_output_tokens,
        timeout_secs,
        background_after_secs,
        cancel,
        progress_tx,
        tool_call_id,
        None,
    )
}

/// Execute through the platform sandbox helper when one is configured.
#[allow(clippy::too_many_arguments)] // 参数面塑形另立项（PLAN D-5）
pub(crate) fn direct_exec_sandboxed(
    argv: &[String],
    env: Option<&[(String, String)]>,
    cwd: Option<&str>,
    max_output_tokens: u32,
    timeout_secs: u64,
    background_after_secs: Option<u64>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    progress_tx: Option<ExecProgressSender>,
    tool_call_id: &str,
    sandbox: &qaqh_sandbox::SandboxSpec,
) -> ExecOutput {
    direct_exec_inner(
        argv,
        env,
        cwd,
        max_output_tokens,
        timeout_secs,
        background_after_secs,
        cancel,
        progress_tx,
        tool_call_id,
        Some(sandbox),
    )
}

#[allow(clippy::too_many_arguments)] // 参数面塑形另立项（PLAN D-5）
fn direct_exec_inner(
    argv: &[String],
    env: Option<&[(String, String)]>,
    cwd: Option<&str>,
    max_output_tokens: u32,
    timeout_secs: u64,
    background_after_secs: Option<u64>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    progress_tx: Option<ExecProgressSender>,
    tool_call_id: &str,
    sandbox: Option<&qaqh_sandbox::SandboxSpec>,
) -> ExecOutput {
    let start_time = std::time::Instant::now();
    let display_name = if argv.len() > 1 {
        format!("{} ...", argv[0])
    } else {
        argv[0].clone()
    };
    let mut cmd = std::process::Command::new(&argv[0]);
    if argv.len() > 1 {
        cmd.args(&argv[1..]);
    }
    let sandbox_launch = match sandbox {
        Some(spec) => match qaqh_sandbox::wrap_command(&mut cmd, argv, cwd, spec) {
            Ok(launch) => launch,
            Err(error) => {
                return ExecOutput {
                    status: "completed".to_string(),
                    command: display_name,
                    exit_code: Some(-1),
                    output: format!("SANDBOX PREPARE FAILED: {error}"),
                    stdout: String::new(),
                    stderr: String::new(),
                    truncated: false,
                    timed_out: false,
                    cancelled: false,
                    process_id: None,
                };
            }
        },
        None => qaqh_sandbox::SandboxLaunch {
            backend: qaqh_sandbox::SandboxBackend::None,
            request: None,
        },
    };
    let sandbox_backend = sandbox_launch.backend;
    let sandbox_request = sandbox_launch.request;
    if let Some(env) = env {
        cmd.envs(env.iter().map(|(k, v)| (k, v)));
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        // H6：独立进程组——取消/超时 kill 时可对整组 SIGKILL，
        // 孙进程不再持有管道写端导致 reader 永不 EOF。
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    if sandbox_request.is_some() {
        cmd.stdin(std::process::Stdio::piped());
    } else {
        cmd.stdin(std::process::Stdio::null());
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ExecOutput {
                status: "completed".to_string(),
                command: display_name,
                exit_code: Some(-1),
                output: format!("SPAWN FAILED: {e}"),
                stdout: String::new(),
                stderr: String::new(),
                truncated: false,
                timed_out: false,
                cancelled: false,
                process_id: None,
            };
        }
    };

    if let Some(request) = sandbox_request {
        let write_result = match child.stdin.take() {
            Some(mut stdin) => stdin.write_all(&request),
            None => Err(std::io::Error::other("sandbox helper stdin unavailable")),
        };
        if let Err(error) = write_result {
            let _ = child.kill();
            let _ = child.wait();
            return ExecOutput {
                status: "completed".to_string(),
                command: display_name,
                exit_code: Some(-1),
                output: format!("SANDBOX REQUEST FAILED: {error}"),
                stdout: String::new(),
                stderr: String::new(),
                truncated: false,
                timed_out: false,
                cancelled: false,
                process_id: None,
            };
        }
    }

    // 接线 ProcessRegistry：先注册（读线程捕获 proc_id），take 管道后
    // 再把子进程句柄移入注册表（poll 经 try_wait、超时移交可查）。
    let proc_id = crate::process_registry::ProcessRegistry::register(&display_name);

    // Start bounded pipe readers（registry-native，阶段 2）：
    // - 读线程有界退出（EOF / settle 到期 / 读错误，任一原因发退出信号）；
    // - 输出完整捕获进注册表（tail 视图 + captured_full 权威源）；
    // - progress sender 随线程结束必然 drop（drain 的 Disconnected 快路径）。
    // 旧"汇总信道 + handoff 善后轮询"退役：try_wait 已在任何查询路径
    // 自动置终态，善后块是死代码；汇总数据源被 captured_full 取代。
    let (stdout_done_tx, stdout_done_rx) = std::sync::mpsc::channel::<(bool, bool)>();
    let (stderr_done_tx, stderr_done_rx) = std::sync::mpsc::channel::<(bool, bool)>();
    let progress_seq = Arc::new(AtomicU64::new(0));
    let byte_cap = crate::process_registry::FULL_CAPTURE_BYTE_CAP;
    let stdout_ctx = PipePumpCtx {
        progress_tx: progress_tx.clone(),
        tool_call_id: tool_call_id.to_string(),
        output_stream: ExecOutputStream::Stdout,
        progress_seq: progress_seq.clone(),
        registry_id: proc_id,
    };
    let stderr_ctx = PipePumpCtx {
        progress_tx: progress_tx.clone(),
        tool_call_id: tool_call_id.to_string(),
        output_stream: ExecOutputStream::Stderr,
        progress_seq: progress_seq.clone(),
        registry_id: proc_id,
    };
    if let Some(p) = child.stdout.take() {
        #[cfg(unix)]
        set_pipe_nonblocking(&p);
        #[cfg(unix)]
        spawn_pipe_reader(
            p,
            byte_cap,
            stdout_ctx,
            |_stream: &mut std::process::ChildStdout| Ok(Readiness::Ready(None)),
            stdout_done_tx,
        );
        #[cfg(windows)]
        spawn_pipe_reader(
            p,
            byte_cap,
            stdout_ctx,
            |stream: &mut std::process::ChildStdout| {
                use std::os::windows::io::AsRawHandle;
                Ok(match pipe_available_bytes(stream.as_raw_handle()) {
                    Some(0) => Readiness::Empty,
                    Some(n) => Readiness::Ready(Some(n as usize)),
                    None => Readiness::Closed,
                })
            },
            stdout_done_tx,
        );
    } else {
        let _ = stdout_done_tx.send((true, false));
    }
    if let Some(p) = child.stderr.take() {
        #[cfg(unix)]
        set_pipe_nonblocking(&p);
        #[cfg(unix)]
        spawn_pipe_reader(
            p,
            byte_cap,
            stderr_ctx,
            |_stream: &mut std::process::ChildStderr| Ok(Readiness::Ready(None)),
            stderr_done_tx,
        );
        #[cfg(windows)]
        spawn_pipe_reader(
            p,
            byte_cap,
            stderr_ctx,
            |stream: &mut std::process::ChildStderr| {
                use std::os::windows::io::AsRawHandle;
                Ok(match pipe_available_bytes(stream.as_raw_handle()) {
                    Some(0) => Readiness::Empty,
                    Some(n) => Readiness::Ready(Some(n as usize)),
                    None => Readiness::Closed,
                })
            },
            stderr_done_tx,
        );
    } else {
        let _ = stderr_done_tx.send((true, false));
    }
    crate::process_registry::ProcessRegistry::attach_child(proc_id, child);

    // Poll child with timeout（子进程句柄唯一持有在注册表，经 try_wait 查询）
    let deadline = start_time + std::time::Duration::from_secs(timeout_secs);
    // 快速移交：子进程存活超过 background_after_secs 即移交后台（不等 timeout）。
    // 用于拉起长驻服务（serve/daemon/watch）——调用方希望尽快拿到
    // backgrounded tool_result，用 process(action=check/wait/kill) 接管，而不是
    // 死等到 timeout_secs 让 agent loop 阻塞。
    let handoff_deadline =
        background_after_secs.map(|secs| start_time + std::time::Duration::from_secs(secs));
    let mut exit_code: Option<i32> = None;
    let mut timed_out = false;
    let mut cancelled = false;
    loop {
        match crate::process_registry::ProcessRegistry::try_wait(proc_id) {
            Some(code) => {
                exit_code = Some(code);
                break;
            }
            None => {
                if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst))
                    || crate::is_cancel()
                {
                    // 取消 = 杀进程树（含后代），防止管道泄漏
                    crate::process_registry::ProcessRegistry::kill(proc_id);
                    cancelled = true;
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    // 超时 = 移交后台（不 kill）：进程存活、管道线程继续
                    // append_output/推流，LLM 可用 process(action=...) 接管。
                    timed_out = true;
                    break;
                }
                if handoff_deadline.is_some_and(|hd| std::time::Instant::now() >= hd) {
                    // 快速移交：进程仍在运行且已超过观察窗口 → 立即转后台。
                    timed_out = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }

    // 超时移交：不再等待管道（读取线程仍在后台 append 到注册表）
    if timed_out {
        let info = crate::process_registry::ProcessRegistry::get_info(proc_id)
            .unwrap_or_else(|| serde_json::json!({}));
        return ExecOutput {
            status: "backgrounded".to_string(),
            command: display_name,
            exit_code: None,
            output: serde_json::json!({
                "backgrounded": true,
                "process_id": proc_id,
                "transferred_after_secs": start_time.elapsed().as_secs_f64(),
                "hint": "进程已转入后台（未终止）。用 process(action=\"check\", id=process_id) 查看状态，process(action=\"wait\", id=process_id) 等待完成，process(action=\"kill\", id=process_id) 终止。",
                "info": info,
            })
            .to_string(),
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
            timed_out: true,
            cancelled: false,
            process_id: Some(proc_id),
        };
    }

    // 正常退出 / 取消：标记注册表状态
    if cancelled {
        crate::process_registry::ProcessRegistry::kill(proc_id);
    } else if let Some(code) = exit_code {
        crate::process_registry::ProcessRegistry::mark_exited(proc_id, code);
    }

    // Collect pipe output（P0 去 EOF 化，2026-09-02 冻结事故）：
    // [seal] registry-native（exec 生命周期重写阶段 2，2.1 契约）：
    // 以注册表完整捕获 `captured_full` 为权威——任何路径不得等待管道
    // EOF / 流关闭。对读线程做的是"有界 join"（SEAL_JOIN_BUDGET）：退出
    // 信号在 EOF、settle 到期、读错误时都会发出；正常路径读线程先于
    // seal 完成，首次 recv 立即返回、零额外等待；孙进程持写端路径信号
    // 来自 settle 到期（读线程确定性退出），快照始终完整可用。
    let join_deadline = std::time::Instant::now() + SEAL_JOIN_BUDGET;
    let (stdout_eof, stdout_capped) = wait_reader_done(&stdout_done_rx, join_deadline);
    let (stderr_eof, stderr_capped) = wait_reader_done(&stderr_done_rx, join_deadline);
    // O-2（issue #35）：读线程未走 `Ok(0)` 必须先落 warning 再谈截断标记。
    // 判据本身不动（保守提示），但要留下证据链——否则线上只有 `truncated:true`，
    // BUG-2026-09-12-EXEC-01 定位时被迫从结果字段反推（多绕一圈）。
    let hard_trunc = !stdout_eof || !stderr_eof || stdout_capped || stderr_capped;
    let captured_len = crate::process_registry::ProcessRegistry::captured_full(proc_id)
        .map(|(out, err)| out.len() + err.len())
        .unwrap_or(0);
    if !stdout_eof || !stderr_eof {
        log::warn!(
            "{}",
            reader_eof_warning(
                proc_id,
                &display_name,
                stdout_eof,
                stderr_eof,
                stdout_capped,
                stderr_capped,
                captured_len,
                exit_code,
                cancelled,
            )
        );
    }
    let (stdout_out, stderr_out) = crate::process_registry::ProcessRegistry::captured_full(proc_id)
        .unwrap_or_else(|| {
            // 防御性：seal 紧跟退出执行，条目惰性驱逐（10 分钟终态门槛）
            // 不可能触发；缺失意味着未知的并发破坏——占位并按截断处理。
            log::warn!("[exec] registry full capture missing for process {proc_id}");
            (
                "[WARN] registry full capture missing\n".to_string(),
                String::new(),
            )
        });
    let mut combined = String::new();
    if !stderr_out.is_empty() {
        combined.push_str(&stderr_out);
        if !stdout_out.is_empty() {
            combined.push('\n');
        }
    }
    combined.push_str(&stdout_out);
    // truncated 口径（见上方 hard_trunc）：字节预算耗尽（任一流）或读线程未以
    // EOF 收尾（settle 放弃 = 孙进程可能继续产出，保守提示输出可能不完整）。
    let cleaned = strip_ansi(&combined);
    qaqh_sandbox::record_denial_if_any(
        sandbox_backend,
        exit_code,
        &cleaned,
        tool_call_id,
        &display_name,
    );
    let total_tokens = qaqh_types::token::count_tokens(&cleaned);
    let (output_str, truncated) = if total_tokens > max_output_tokens || hard_trunc {
        (token_truncate(&cleaned, max_output_tokens), true)
    } else {
        (cleaned, false)
    };

    ExecOutput {
        status: if cancelled {
            "cancelled".to_string()
        } else {
            "completed".to_string()
        },
        command: display_name,
        exit_code,
        output: output_str,
        stdout: strip_ansi(&stdout_out),
        stderr: strip_ansi(&stderr_out),
        truncated,
        timed_out,
        cancelled,
        process_id: Some(proc_id),
    }
}

/// Structured output from a command execution.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone)]
pub struct ExecOutput {
    #[serde(default)]
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) command: String,
    #[serde(default)]
    pub(crate) exit_code: Option<i32>,
    #[serde(default)]
    pub(crate) output: String,
    /// v2 display-only streams. `#[serde(skip)]` keeps them out of model JSON;
    /// they are populated by the direct executor and consumed by `display()`.
    #[serde(skip)]
    pub(crate) stdout: String,
    #[serde(skip)]
    pub(crate) stderr: String,
    #[serde(default)]
    pub(crate) truncated: bool,
    #[serde(default)]
    pub(crate) timed_out: bool,
    #[serde(default)]
    pub(crate) cancelled: bool,
    /// 超时移交后台时的注册表进程 id（由 process 的 action 使用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) process_id: Option<u32>,
}

impl ExecOutput {
    pub(crate) fn to_json(&self) -> String {
        serde_json::to_string(self)
            .unwrap_or_else(|_| r#"{"status":"error","output":"serialization failed"}"#.into())
    }
}

/// O-2（issue #35）：读线程未走到 EOF 时的 warning 正文。
///
/// 判据（`!stdout_eof || !stderr_eof`）的定义与"哪些退出路径会让
/// `saw_eof=false`"见 [`super::pipe::drain_pipe_to_registry`] 的文档；
/// 那里是实现，这里只是渲染证据。
///
/// 单列为纯函数（而非内联 `log::warn!`）有两点理由：字段可被单测直接
/// 断言（无需日志捕获装置），以及"缺哪些证据"一眼可见——定位一次线上
/// 截断需要的 (proc_id, cmd, 两路 eof, 两路 capped, 已捕获字节,
/// exit_code, cancelled) 必须一个不少。
///
/// `truncated` 与 `direct_exec` 的 `hard_trunc` 同判据：warn 里自报的
/// 结果状态与返回给调用方的 `ExecOutput.truncated` 必须一致。
#[allow(clippy::too_many_arguments)] // 逐字段透出诊断证据，收拢成结构体只会更晦涩
pub(crate) fn reader_eof_warning(
    proc_id: u32,
    command: &str,
    stdout_eof: bool,
    stderr_eof: bool,
    stdout_capped: bool,
    stderr_capped: bool,
    captured_bytes: usize,
    exit_code: Option<i32>,
    cancelled: bool,
) -> String {
    let truncated = !stdout_eof || !stderr_eof || stdout_capped || stderr_capped;
    // exit_code 用 `{}` 而非 `{:?}`：日志正文出现 `Some(0)` 反而难 grep。
    let exit_code = exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "none".to_string());
    format!(
        "[exec] WARN reader did not reach EOF — proc_id={proc_id}, cmd={command}, \
         stdout_eof={stdout_eof}, stderr_eof={stderr_eof}, \
         stdout_capped={stdout_capped}, stderr_capped={stderr_capped}, \
         captured_bytes={captured_bytes}, exit_code={exit_code}, \
         cancelled={cancelled} — settle gave up on a still-open pipe \
         (grandchild holding the write end?); output may be incomplete \
         (truncated={truncated})"
    )
}
