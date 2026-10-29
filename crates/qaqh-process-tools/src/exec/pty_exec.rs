//! exec::pty_exec — tty=true 的 PTY 执行路径（codex unified_exec oneshot 范式）。
//!
//! # 与 direct_exec（tty=false 管道路径）的分工
//!
//! 同一 `exec` 工具的两种对称后端（codex `tools/handlers/shell_spec.rs` 的
//! `tty` 参数语义：*"True allocates a PTY; false or omitted uses plain
//! pipes"*）：
//!
//! - **tty=false（缺省）**：继续走 `direct_exec` 的成熟管道路径——
//!   stdout/stderr 分流、有界泵读、settle 兜底、`captured_full` 权威。
//!   stdio/err 行为与替换前完全一致（验证点 1）。
//! - **tty=true**：本模块。qaqh-pty PTY 后端 + `qaqh_sandbox::prepare_pty_launch`
//!   （helper 从文件读 policy、不重定向 stdin）。单合流终端流，进度事件
//!   **原样携带 ANSI 转义**推给前端（验证点 2：渲染层拿到的是实时终端流）；
//!   模型侧 tool result 经 `strip_ansi` + `token_truncate`（验证点 4：
//!   meta 与实际输出一致，不带控制序列噪声）。
//!
//! # 本轮刻意采用的 oneshot 生命周期（codex `oneshot.rs` 对齐）
//!
//! timeout 到点 = `kill_group` 终止并收割（**不**移交后台）；`cancellation`
//! 同理。`background_after_secs` 在 tty 路径直接报参数错——它依赖
//! `ProcessRegistry` 托管 `std::process::Child`，PTY 会话的后台移交归入
//! 下一轮（write_stdin / 严格 timeout / 进程监听）统一研判，避免半吊子
//! 双语义。孙进程持 slave 导致 master 不 EOF 的情形按"有界 join +
//! truncated 保守提示"处理（与管道路径的 settle 语义同形）。
//!
//! # 权限与沙箱
//!
//! 审批面不变（`ToolRisk::Destructive` + `ToolCategory::Exec` 在 handler
//! 层，本模块不可见）；沙箱经 `prepare_pty_launch` 保持强制（Landlock/
//! seccomp/bwrap），失败 fail closed（验证点 3）。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{ExecOutputStream, ExecProgressSender};

use super::truncate::{strip_ansi, token_truncate};

/// PTY 路径的轮询粒度（与 direct_exec 主循环一致）。
const POLL_TICK: std::time::Duration = std::time::Duration::from_millis(50);

/// tty=true 的 exec 执行入口。参数面与 `direct_exec_sandboxed` 对齐，但
/// 没有 `background_after_secs`（见模块文档）。
#[allow(clippy::too_many_arguments)] // 参数面塑形归 P3（unified_exec manager）
pub(crate) fn pty_exec(
    argv: &[String],
    env: Option<&[(String, String)]>,
    cwd: Option<&str>,
    max_output_tokens: u32,
    timeout_secs: u64,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    progress_tx: Option<ExecProgressSender>,
    tool_call_id: &str,
    sandbox: Option<&qaqh_sandbox::SandboxSpec>,
) -> super::direct::ExecOutput {
    let start_time = std::time::Instant::now();
    let display_name = if argv.len() > 1 {
        format!("{} ...", argv[0])
    } else {
        argv[0].clone()
    };

    let Some(spec) = sandbox else {
        // tty 路径必须有 spec（含 None 后端）以便统一走 prepare_pty_launch；
        // handler 层 ctx.sandbox_spec() 恒有值，这里只做防御。
        return failure_output(&display_name, "SANDBOX SPEC MISSING: tty path requires a sandbox spec");
    };
    let launch = match qaqh_sandbox::prepare_pty_launch(argv, cwd, spec) {
        Ok(launch) => launch,
        Err(error) => {
            // fail closed：策略解析/降级失败绝不放行。
            return failure_output(&display_name, &format!("SANDBOX PREPARE FAILED: {error}"));
        }
    };
    let sandbox_backend = launch.backend;
    let payload_path = launch.payload_path.clone();

    // 审计 M1 延续：env_clear + 最小白名单；PTY 需要 TERM 才有正常终端行为
    // （daemon 常无 TERM），缺省补 xterm-256color。
    let mut session_spec = qaqh_pty::SessionSpec::new(launch.argv);
    let mut env_pairs = super::direct::minimal_child_env();
    if !env_pairs.iter().any(|(key, _)| key == "TERM") {
        env_pairs.push(("TERM".to_string(), "xterm-256color".to_string()));
    }
    if let Some(env) = env {
        env_pairs.extend(env.iter().cloned());
    }
    session_spec.env = env_pairs;
    session_spec.cwd = cwd.map(std::path::PathBuf::from);

    let session = match qaqh_pty::spawn_pty(&session_spec) {
        Ok(session) => session,
        Err(error) => {
            if let Some(path) = &payload_path {
                let _ = std::fs::remove_file(path);
            }
            return failure_output(&display_name, &format!("SPAWN FAILED: {error}"));
        }
    };
    let (io, mut handle) = session.into_parts();

    // 合流读线程：PTY master → forward_progress（ANSI 原样合帧推流，
    // HeadTailBuffer 有界捕获经信道交回）。master EOF（子进程退出且
    // slave 全关）即正常出口；孙进程持 slave 时由 seal 侧有界 join 兜底。
    let proc_id = crate::process_registry::ProcessRegistry::register(&display_name);
    // os_pid（会话 leader）与 stdin writer 入册：现有 kill 路径的 killpg
    // 由此覆盖 PTY 会话组；write_stdin 时代 writer 已就位。
    crate::process_registry::ProcessRegistry::attach_pty_session(
        proc_id,
        handle.pid,
        io.writer,
    );
    let (done_tx, done_rx) = std::sync::mpsc::channel::<PumpOutcome>();
    let reader_tool_call_id = tool_call_id.to_string();
    let reader_seq = Arc::new(AtomicU64::new(0));
    let _reader_handle = std::thread::spawn(move || {
        pump_pty_output(
            io.reader,
            progress_tx,
            reader_tool_call_id,
            reader_seq,
            done_tx,
        )
    });

    // 轮询收割（oneshot 语义：超时/取消 = kill_group 终止）。
    let deadline = start_time + std::time::Duration::from_secs(timeout_secs);
    let mut exit_code: Option<i32> = None;
    let mut timed_out = false;
    let mut cancelled = false;
    loop {
        match handle.try_wait() {
            Ok(Some(exit)) => {
                exit_code = exit.code;
                break;
            }
            Ok(None) => {
                if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) || crate::is_cancel() {
                    let _ = handle.kill_group();
                    let _ = handle.wait();
                    cancelled = true;
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    let _ = handle.kill_group();
                    let _ = handle.wait();
                    timed_out = true;
                    break;
                }
                std::thread::sleep(POLL_TICK);
            }
            Err(error) => {
                log::warn!("[exec:pty] try_wait failed: {error}");
                let _ = handle.kill_group();
                let _ = handle.wait();
                break;
            }
        }
    }

    // seal：有界等待读线程信号（EOF / kill 后 slave 关闭 → 通常立即返回；
    // 孙进程持 slave → SEAL_JOIN_BUDGET 到期放弃，truncated 保守提示）。
    // 对齐 direct.rs / ddcd606 契约：绝不调用 reader_handle.join()，防止孙进程持 slave 导致读线程阻塞时主线程永久挂死。
    let join_deadline = std::time::Instant::now() + super::pipe::SEAL_JOIN_BUDGET;
    let outcome = recv_pump(&done_rx, join_deadline).unwrap_or_default();
    let hard_trunc = !outcome.saw_eof;

    crate::process_registry::ProcessRegistry::mark_exited(proc_id, exit_code.unwrap_or(-1));

    // denial 证据链与管道路径同形（M1 脱敏）。
    let injected_secrets: Vec<String> = env
        .map(|pairs| {
            pairs
                .iter()
                .map(|(_, value)| value.clone())
                .filter(|value| value.chars().count() >= 8)
                .collect()
        })
        .unwrap_or_default();
    let cleaned = strip_ansi(&outcome.text);
    qaqh_sandbox::record_denial_if_any(
        sandbox_backend,
        exit_code,
        &cleaned,
        tool_call_id,
        &display_name,
        &injected_secrets,
    );
    if hard_trunc {
        log::warn!(
            "[exec:pty] WARN reader did not reach EOF — proc_id={proc_id}, cmd={display_name}, \
             saw_eof={}, captured_bytes={}, omitted_bytes={}, protocol_dropped_bytes={}, \
             exit_code={}, timed_out={timed_out}, cancelled={cancelled} — \
             grandchild holding the PTY slave?; output may be incomplete",
            outcome.saw_eof,
            outcome.bytes,
            outcome.omitted,
            outcome.protocol_dropped,
            exit_code.map(|code| code.to_string()).unwrap_or_else(|| "none".to_string()),
        );
    }
    let total_tokens = qaqh_types::token::count_tokens(&cleaned);
    // truncated 口径与管道路径对齐：硬截（未 EOF）或捕获省略（洪峰中段
    // 被丢弃）都必须让模型知道"看到的不完整"。
    let (output_str, truncated) =
        if total_tokens > max_output_tokens || hard_trunc || outcome.omitted > 0 {
            (token_truncate(&cleaned, max_output_tokens), true)
        } else {
            (cleaned.clone(), false)
        };

    let status = if cancelled { "cancelled" } else { "completed" };
    super::direct::ExecOutput {
        status: status.to_string(),
        command: display_name,
        exit_code,
        output: output_str,
        // 合流终端流没有独立的 stderr；display 面以 stdout 呈现（与
        // codex unified_exec 的单流响应一致）。
        stdout: cleaned,
        stderr: String::new(),
        truncated,
        timed_out,
        cancelled,
        process_id: Some(proc_id),
        output_omitted_bytes: outcome.omitted as u64,
        protocol_dropped_bytes: outcome.protocol_dropped,
    }
}

fn failure_output(command: &str, message: &str) -> super::direct::ExecOutput {
    super::direct::ExecOutput {
        status: "completed".to_string(),
        command: command.to_string(),
        exit_code: Some(-1),
        output: message.to_string(),
        stdout: String::new(),
        stderr: String::new(),
        truncated: false,
        timed_out: false,
        cancelled: false,
        process_id: None,
        output_omitted_bytes: 0,
        protocol_dropped_bytes: 0,
    }
}

/// 读线程的产出（经信道交回主线程，避免任何共享可变态）。
#[derive(Default)]
struct PumpOutcome {
    saw_eof: bool,
    /// lossy 文本（HeadTailBuffer 头尾拼合）。
    text: String,
    /// 观测总字节（含被缓冲丢弃的中段）。
    bytes: usize,
    /// 捕获缓冲丢弃的中段字节（HeadTailBuffer omitted）。
    omitted: usize,
    /// 协议侧丢弃字节（progress 通道满被丢——前端有界消费）。
    protocol_dropped: u64,
}

/// seal 侧有界 recv：超时或线程消失按放弃处理（Outcome 走 default →
/// saw_eof=false → truncated 保守提示）。
fn recv_pump(
    rx: &std::sync::mpsc::Receiver<PumpOutcome>,
    deadline: std::time::Instant,
) -> Option<PumpOutcome> {
    let now = std::time::Instant::now();
    if now >= deadline {
        return None;
    }
    rx.recv_timeout(deadline - now).ok()
}

/// 合帧阈值：攒够即发（大输出时帧率受此约束的下一档）。
const FRAME_FLUSH_BYTES: usize = 64 * 1024;
/// 合帧窗口：不足阈值也按时冲刷（交互式输出的实时性下限）。
const FRAME_FLUSH_WINDOW: std::time::Duration = std::time::Duration::from_millis(50);

/// 合流泵：master 读端 → HeadTailBuffer 有界捕获（codex `OutputBuffers`
/// 语义，1 MiB 顶）+ 合帧 progress 推流（ANSI 原样；帧率 ≤
/// max(4KB/帧, 20 帧/s)，洪峰撞缓冲不撞协议）。
///
/// 退出条件与 `drain_pipe_to_registry` 同构：`Ok(0)` = 真 EOF；其他读错误
/// 放弃（saw_eof=false）。
fn pump_pty_output(
    mut reader: Box<dyn std::io::Read + Send>,
    progress_tx: Option<ExecProgressSender>,
    tool_call_id: String,
    seq: Arc<AtomicU64>,
    done_tx: std::sync::mpsc::Sender<PumpOutcome>,
) {
    let mut capture = super::head_tail::HeadTailBuffer::default();
    let mut pending_utf8 = Vec::new();
    let mut frame: Vec<u8> = Vec::with_capacity(FRAME_FLUSH_BYTES);
    let mut window_start: Option<std::time::Instant> = None;
    let mut saw_eof = false;

    let mut flush_frame = |frame: &mut Vec<u8>,
                           window_start: &mut Option<std::time::Instant>| {
        if frame.is_empty() {
            return;
        }
        super::pipe::forward_progress(
            &mut pending_utf8,
            frame,
            progress_tx.as_ref(),
            &tool_call_id,
            ExecOutputStream::Stdout,
            &seq,
            None,
        );
        frame.clear();
        *window_start = None;
    };

    let mut buf = vec![0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                saw_eof = true;
                break;
            }
            Ok(n) => {
                capture.push_bytes(&buf[..n]);
                frame.extend_from_slice(&buf[..n]);
                if frame.len() >= FRAME_FLUSH_BYTES {
                    flush_frame(&mut frame, &mut window_start);
                } else {
                    let start = *window_start.get_or_insert_with(std::time::Instant::now);
                    if start.elapsed() >= FRAME_FLUSH_WINDOW {
                        flush_frame(&mut frame, &mut window_start);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    flush_frame(&mut frame, &mut window_start);
    if !pending_utf8.is_empty() {
        let tail = String::from_utf8_lossy(&pending_utf8).into_owned();
        super::pipe::send_progress(
            progress_tx.as_ref(),
            &tool_call_id,
            ExecOutputStream::Stdout,
            &seq,
            tail.clone(),
        );
    }
    let protocol_dropped = progress_tx
        .as_ref()
        .map(|tx| tx.dropped_bytes())
        .unwrap_or(0);
    let _ = done_tx.send(PumpOutcome {
        saw_eof,
        text: capture.to_text(),
        bytes: capture.total_bytes(),
        omitted: capture.omitted_bytes(),
        protocol_dropped,
    });
}
