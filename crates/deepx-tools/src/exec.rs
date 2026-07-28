//! Command execution — direct process spawn via argv array.
//!
//! No PTY and no shell. Uses `std::process::Command` and streams pipe chunks
//! to the UI while retaining a bounded final result for the LLM.
//! Output is read via pipes (not `output()`) to prevent OOM on large outputs,
//! and truncated by actual token count using `deepx_types::token::count_tokens`.
//!
//! Two invocation modes:
//!   • `argv`  — direct exec, no shell (for simple program calls).
//!   • `command` — auto-wrapped in the platform shell, enabling pipes,
//!     redirects, and builtins without the model needing to spell out the
//!     shell executable manually.

use crate::{ExecOutputStream, ExecProgressEvent, ExecProgressSender, ToolCallCtx, ToolResult};
use serde::Serialize;
use std::io::Read;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

// ── Platform shell detection ──
// Adapted from codex-rs/shell-command/src/shell_detect.rs & core/src/shell.rs.
// Stripped to the minimum needed: pick the right shell, derive argv.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum Shell {
    Bash,
    Zsh,
    Sh,
    PowerShell,
    Cmd,
}

impl Shell {
    /// Auto-detect the best available shell on this platform.
    fn detect() -> Self {
        #[cfg(windows)]
        {
            // Prefer bash (Git for Windows / MSYS2 / WSL interop),
            // then pwsh, falling back to cmd.
            if std::process::Command::new("bash")
                .arg("-c")
                .arg("exit 0")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok()
            {
                return Shell::Bash;
            }
            if std::process::Command::new("pwsh")
                .arg("-NoProfile")
                .arg("-Command")
                .arg("exit 0")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok()
            {
                return Shell::PowerShell;
            }
            Shell::Cmd
        }
        #[cfg(not(windows))]
        {
            if std::process::Command::new("bash")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok()
            {
                return Shell::Bash;
            }
            Shell::Sh
        }
    }

    /// Path to the shell executable.
    fn path(&self) -> &'static str {
        match self {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
            Shell::Sh => "sh",
            Shell::PowerShell => "pwsh",
            Shell::Cmd => "cmd",
        }
    }

    /// Build the argv that runs `command` through this shell.
    fn derive_exec_args(&self, command: &str) -> Vec<String> {
        match self {
            Shell::Bash | Shell::Zsh | Shell::Sh => {
                vec![self.path().to_string(), "-c".to_string(), command.to_string()]
            }
            Shell::PowerShell => {
                vec![
                    self.path().to_string(),
                    "-NoProfile".to_string(),
                    "-Command".to_string(),
                    command.to_string(),
                ]
            }
            Shell::Cmd => {
                vec![
                    self.path().to_string(),
                    "/c".to_string(),
                    command.to_string(),
                ]
            }
        }
    }
}

/// Stream read from a pipe, capped at `max_bytes`.
///
/// Every retained chunk is also forwarded to the UI progress channel. Once the
/// cap is reached, the rest of the pipe is drained without forwarding so the
/// child cannot block on a full OS pipe.
fn read_stream(
    stream: impl Read,
    max_bytes: usize,
    progress_tx: Option<ExecProgressSender>,
    tool_call_id: String,
    output_stream: ExecOutputStream,
    progress_seq: Arc<AtomicU64>,
) -> (Vec<u8>, bool) {
    let mut reader = std::io::BufReader::new(stream);
    let mut buf = vec![0u8; 8192];
    let mut out = Vec::new();
    let mut pending_utf8 = Vec::new();
    let mut truncated = false;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let retained = n.min(max_bytes.saturating_sub(out.len()));
                if retained > 0 {
                    let chunk = &buf[..retained];
                    out.extend_from_slice(chunk);
                    forward_progress(
                        &mut pending_utf8,
                        chunk,
                        progress_tx.as_ref(),
                        &tool_call_id,
                        output_stream,
                        &progress_seq,
                    );
                }
                if retained < n {
                    truncated = true;
                    std::io::copy(&mut reader, &mut std::io::sink()).ok();
                    break;
                }
            }
            Err(_) => break,
        }
    }
    if !pending_utf8.is_empty() {
        send_progress(
            progress_tx.as_ref(),
            &tool_call_id,
            output_stream,
            &progress_seq,
            String::from_utf8_lossy(&pending_utf8).into_owned(),
        );
    }
    (out, truncated)
}

/// Forward only complete text units. A command may split one Chinese character
/// across pipe reads; keeping its suffix here avoids replacement glyphs in UI.
/// On Windows, non-UTF-8 console output falls back to the active OEM code page.
fn forward_progress(
    pending: &mut Vec<u8>,
    bytes: &[u8],
    tx: Option<&ExecProgressSender>,
    tool_call_id: &str,
    stream: ExecOutputStream,
    seq: &Arc<AtomicU64>,
) {
    pending.extend_from_slice(bytes);
    loop {
        match std::str::from_utf8(pending) {
            Ok(valid) => {
                send_progress(tx, tool_call_id, stream, seq, valid.to_owned());
                pending.clear();
                return;
            }
            Err(error) if error.valid_up_to() > 0 => {
                let valid_up_to = error.valid_up_to();
                let prefix =
                    String::from_utf8(pending[..valid_up_to].to_vec()).expect("valid UTF-8 prefix");
                pending.drain(..valid_up_to);
                send_progress(tx, tool_call_id, stream, seq, prefix);
            }
            Err(error) if error.error_len().is_some() => {
                #[cfg(windows)]
                if let Some(decoded) = decode_windows_oem(pending) {
                    pending.clear();
                    send_progress(tx, tool_call_id, stream, seq, decoded);
                    return;
                }
                let invalid_len = error.error_len().expect("checked above");
                let replacement = String::from_utf8_lossy(&pending[..invalid_len]).into_owned();
                pending.drain(..invalid_len);
                send_progress(tx, tool_call_id, stream, seq, replacement);
            }
            Err(_) => return, // incomplete character at end; wait for next read.
        }
    }
}

/// Decode the final capture using UTF-8 first, then the Windows console OEM
/// code page (for example GBK/936 on Simplified-Chinese Windows).
fn decode_captured(bytes: &[u8]) -> String {
    if let Ok(utf8) = std::str::from_utf8(bytes) {
        return utf8.to_owned();
    }
    #[cfg(windows)]
    if let Some(oem) = decode_windows_oem(bytes) {
        return oem;
    }
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(windows)]
fn decode_windows_oem(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return Some(String::new());
    }
    if bytes.len() > i32::MAX as usize {
        return None;
    }

    // SAFETY: These are well-known Kernel32 FFI functions with stable ABIs.
    // `GetOEMCP` takes no parameters. `MultiByteToWideChar` operates on
    // caller-provided buffers — we pass null for the sizing call, then a
    // properly-sized `Vec<u16>` for the real conversion. All pointer/length
    // pairs are derived from safe Rust slices. The `MB_ERR_INVALID_CHARS`
    // flag prevents silent substitution of invalid sequences (a split DBCS
    // byte at the end of a pipe read is not an error — it waits for the next
    // chunk).  TODO(migration): replace with `windows` crate's
    // `GetOEMCP` / `MultiByteToWideChar` bindings when `deepx-tools` gains
    // a `windows` dependency.
    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn GetOEMCP() -> u32;
        fn MultiByteToWideChar(
            code_page: u32,
            flags: u32,
            multi_byte: *const u8,
            multi_byte_len: i32,
            wide_char: *mut u16,
            wide_char_len: i32,
        ) -> i32;
    }

    // MB_ERR_INVALID_CHARS lets a split GBK/DBCS sequence wait for the next
    // read instead of emitting a replacement glyph mid-stream.
    const MB_ERR_INVALID_CHARS: u32 = 0x0000_0008;

    // SAFETY: `GetOEMCP` is a parameterless Kernel32 query with no
    // preconditions on global state.  A returned value of 0 means the OEM
    // code page is unavailable (system corruption or minimal WinPE env) —
    // fall back to UTF-8 lossy decoding.
    let code_page = unsafe { GetOEMCP() };
    if code_page == 0 {
        return None;
    }

    let byte_len = bytes.len() as i32;

    // SAFETY: Sizing call — `wide_char` is null, `wide_char_len` is 0.
    // `bytes` is a valid Rust slice; `byte_len` equals its length.
    let wide_len = unsafe {
        MultiByteToWideChar(
            code_page,
            MB_ERR_INVALID_CHARS,
            bytes.as_ptr(),
            byte_len,
            std::ptr::null_mut(),
            0,
        )
    };
    if wide_len <= 0 {
        return None;
    }
    let mut wide = vec![0u16; wide_len as usize];

    // SAFETY: Real conversion — `wide` is a `Vec<u16>` with exactly
    // `wide_len` elements. `bytes.as_ptr()` and `byte_len` match the input
    // slice. The sizing call above guarantees the buffer is large enough.
    let written = unsafe {
        MultiByteToWideChar(
            code_page,
            MB_ERR_INVALID_CHARS,
            bytes.as_ptr(),
            byte_len,
            wide.as_mut_ptr(),
            wide_len,
        )
    };
    (written == wide_len).then(|| String::from_utf16_lossy(&wide))
}

fn send_progress(
    tx: Option<&ExecProgressSender>,
    tool_call_id: &str,
    stream: ExecOutputStream,
    seq: &Arc<AtomicU64>,
    chunk: String,
) {
    if chunk.is_empty() {
        return;
    }
    if let Some(tx) = tx {
        tx.try_send(ExecProgressEvent {
            tool_call_id: tool_call_id.to_string(),
            stream,
            seq: seq.fetch_add(1, Ordering::Relaxed),
            chunk,
        });
    }
}

/// CJK character ranges used for token-count estimation.
/// CJK characters consume ~1.67 tokens each, vs ~3.3 for ASCII.
const fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{4e00}'..='\u{9fff}' | '\u{3400}'..='\u{4dbf}'
        | '\u{3000}'..='\u{303f}' | '\u{ff00}'..='\u{ffef}'
        | '\u{3040}'..='\u{30ff}'
    )
}

/// Find byte index for `target` tokens walking forward.
fn find_token_boundary(text: &str, target_tokens: u32) -> usize {
    let target_f64 = target_tokens as f64;
    let mut char_count = 0usize;
    let mut cjk_count = 0usize;
    for (i, c) in text.char_indices() {
        if is_cjk(c) {
            cjk_count += 1;
        } else {
            char_count += 1;
        }
        let est = char_count as f64 / 3.3 + cjk_count as f64 / 1.67;
        if est >= target_f64 {
            return i;
        }
    }
    text.len()
}

/// Find byte index for `target` tokens walking backward from end.
fn find_token_boundary_reverse(text: &str, target_tokens: u32) -> usize {
    let target_f64 = target_tokens as f64;
    let mut char_count = 0usize;
    let mut cjk_count = 0usize;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    for (i, c) in chars.iter().rev() {
        if is_cjk(*c) {
            cjk_count += 1;
        } else {
            char_count += 1;
        }
        let est = char_count as f64 / 3.3 + cjk_count as f64 / 1.67;
        if est >= target_f64 {
            return *i;
        }
    }
    0
}

/// Token-aware smart truncation: keeps head (70%) + tail (30%).
fn token_truncate(text: &str, max_tokens: u32) -> String {
    let total = deepx_types::token::count_tokens(text);
    if total <= max_tokens {
        return text.to_string();
    }
    let head_tokens = (max_tokens as f64 * 0.7).max(1.0) as u32;
    let tail_tokens = (max_tokens as f64 * 0.3).max(1.0) as u32;
    let head_end = find_token_boundary(text, head_tokens);
    let tail_start = find_token_boundary_reverse(text, tail_tokens);
    if head_end >= tail_start {
        let end = find_token_boundary(text, max_tokens);
        format!(
            "{}\n...[TRUNCATED: {}/{} tokens. Call exec again with narrower argv or a filtering command.]",
            &text[..end],
            max_tokens,
            total
        )
    } else {
        let tail = &text[tail_start..];
        format!(
            "{}\n\n...[TRUNCATED: {}/{} tokens, {} lines dropped. Call exec again with narrower argv or a filtering command.]\n\n{}",
            &text[..head_end],
            max_tokens,
            total,
            text[head_end..tail_start].lines().count(),
            tail.trim_start(),
        )
    }
}

/// Direct command execution: argv array, no shell.
/// Uses background threads for pipe reading and poll-based timeout.
fn direct_exec(
    argv: &[String],
    cwd: Option<&str>,
    max_output_tokens: u32,
    timeout_secs: u64,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    progress_tx: Option<ExecProgressSender>,
    tool_call_id: &str,
) -> ExecOutput {
    let start_time = std::time::Instant::now();
    let display_name = if argv.len() > 1 {
        format!("{} ...", argv[0])
    } else {
        argv[0].clone()
    };
    const HARD_BYTE_CAP: usize = 5 * 1024 * 1024;

    let mut cmd = std::process::Command::new(&argv[0]);
    if argv.len() > 1 {
        cmd.args(&argv[1..]);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ExecOutput {
                status: "completed",
                command: display_name,
                exit_code: Some(-1),
                output: format!("SPAWN FAILED: {e}"),
                wall_time_seconds: 0.0,
                original_tokens: 0,
                truncated: false,
                timed_out: false,
                cancelled: false,
                stdout_bytes: 0,
                stderr_bytes: 0,
                ui_dropped_bytes: 0,
            };
        }
    };

    // Start background pipe readers
    let (stdout_tx, stdout_rx) = std::sync::mpsc::channel();
    let (stderr_tx, stderr_rx) = std::sync::mpsc::channel();
    let progress_seq = Arc::new(AtomicU64::new(0));
    if let Some(p) = child.stdout.take() {
        let progress_tx = progress_tx.clone();
        let tool_call_id = tool_call_id.to_string();
        let progress_seq = progress_seq.clone();
        std::thread::spawn(move || {
            let (s, t) = read_stream(
                p,
                HARD_BYTE_CAP,
                progress_tx,
                tool_call_id,
                ExecOutputStream::Stdout,
                progress_seq,
            );
            let _ = stdout_tx.send((s, t));
        });
    } else {
        let _ = stdout_tx.send((Vec::new(), false));
    }
    if let Some(p) = child.stderr.take() {
        let progress_tx = progress_tx.clone();
        let tool_call_id = tool_call_id.to_string();
        let progress_seq = progress_seq.clone();
        std::thread::spawn(move || {
            let (s, t) = read_stream(
                p,
                HARD_BYTE_CAP,
                progress_tx,
                tool_call_id,
                ExecOutputStream::Stderr,
                progress_seq,
            );
            let _ = stderr_tx.send((s, t));
        });
    } else {
        let _ = stderr_tx.send((Vec::new(), false));
    }

    // Poll child with timeout
    let deadline = start_time + std::time::Duration::from_secs(timeout_secs);
    let mut exit_code: Option<i32> = None;
    let mut timed_out = false;
    let mut cancelled = false;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit_code = status.code();
                break;
            }
            Ok(None) => {
                if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst))
                    || crate::CANCEL.load(std::sync::atomic::Ordering::SeqCst)
                {
                    let _ = child.kill();
                    let _ = child.wait();
                    cancelled = true;
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => break,
        }
    }

    // Collect pipe output (threads finish after child exits)
    let (stdout_out, stdout_trunc) = stdout_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap_or_else(|_| (b"[WARN] stdout pipe timed out\n".to_vec(), true));
    let (stderr_out, stderr_trunc) = stderr_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap_or_else(|_| (b"[WARN] stderr pipe timed out\n".to_vec(), true));

    let stdout_bytes = stdout_out.len() as u64;
    let stderr_bytes = stderr_out.len() as u64;
    let ui_dropped_bytes = progress_tx
        .as_ref()
        .map_or(0, ExecProgressSender::dropped_bytes);
    let stdout_out = decode_captured(&stdout_out);
    let stderr_out = decode_captured(&stderr_out);

    let mut combined = String::new();
    if !stderr_out.is_empty() {
        combined.push_str(&stderr_out);
        if !stdout_out.is_empty() {
            combined.push('\n');
        }
    }
    combined.push_str(&stdout_out);

    let hard_trunc = stderr_trunc || stdout_trunc;
    let cleaned = strip_ansi(&combined);
    let total_tokens = deepx_types::token::count_tokens(&cleaned);
    let (output_str, truncated) = if total_tokens > max_output_tokens || hard_trunc {
        (token_truncate(&cleaned, max_output_tokens), true)
    } else {
        (cleaned, false)
    };

    ExecOutput {
        status: if cancelled { "cancelled" } else { "completed" },
        command: display_name,
        exit_code,
        output: output_str,
        wall_time_seconds: start_time.elapsed().as_secs_f64(),
        original_tokens: total_tokens,
        truncated,
        timed_out,
        cancelled,
        stdout_bytes,
        stderr_bytes,
        ui_dropped_bytes,
    }
}

/// Structured output from a command execution.
#[derive(Serialize, Debug, Clone)]
pub(crate) struct ExecOutput {
    status: &'static str,
    command: String,
    exit_code: Option<i32>,
    output: String,
    wall_time_seconds: f64,
    original_tokens: u32,
    truncated: bool,
    timed_out: bool,
    cancelled: bool,
    stdout_bytes: u64,
    stderr_bytes: u64,
    /// Bytes not sent to the UI because its bounded event queue was full.
    ui_dropped_bytes: u64,
}

impl ExecOutput {
    fn to_json(&self) -> String {
        serde_json::to_string(self)
            .unwrap_or_else(|_| r#"{"status":"error","output":"serialization failed"}"#.into())
    }
}

// ── Tool handler ──

pub(super) fn handle_run(ctx: ToolCallCtx) -> ToolResult {
    // ── Resolve argv ──
    // Two modes: `command` (auto-wrapped in platform shell) or `argv` (direct exec).
    let argv: Vec<String> = if let Some(command) = ctx.get_str("command") {
        if command.is_empty() {
            return ToolResult {
                success: false,
                content: crate::json_err(
                    "EMPTY_COMMAND",
                    "command string is empty",
                    "Provide a shell command string.",
                ),
            };
        }
        Shell::detect().derive_exec_args(command)
    } else {
        match ctx.args.get("argv").and_then(|v| v.as_array()) {
            Some(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            None => {
                return ToolResult {
                    success: false,
                    content: crate::json_err(
                        "MISSING_ARGV",
                        "exec requires argv or command",
                        "Example: {\"argv\": [\"cargo\", \"check\"]} or {\"command\": \"cargo check\"}",
                    ),
                };
            }
        }
    };
    if argv.is_empty() {
        return ToolResult {
            success: false,
            content: crate::json_err(
                "EMPTY_ARGV",
                "argv array is empty",
                "Provide at least one element.",
            ),
        };
    }
    let max_output_tokens = ctx
        .get_u64("max_output_tokens")
        .filter(|&n| n >= 100 && n <= 50000)
        .unwrap_or(10000) as u32;
    let timeout_secs = ctx
        .get_u64("timeout_secs")
        .filter(|&n| n > 0 && n <= 3600)
        .unwrap_or_else(|| ctx.timeout_secs.unwrap_or(30).clamp(1, 3600));
    // Fall back to workspace root when the caller doesn't supply cwd
    let cwd: Option<String> = ctx.get_str("cwd").map(String::from).or_else(|| {
        let ws = crate::CURRENT_WORKSPACE.read().ok()?;
        if ws.is_empty() || *ws == "." {
            None
        } else {
            Some(ws.clone())
        }
    });
    let cwd_ref: Option<&str> = cwd.as_deref();
    let result = direct_exec(
        &argv,
        cwd_ref,
        max_output_tokens,
        timeout_secs,
        Some(ctx.cancel.as_ref()),
        ctx.tx_progress.clone(),
        &ctx.id,
    );
    let success = match result.exit_code {
        Some(0) => true,
        Some(_) => false,
        None => !result.timed_out && !result.cancelled,
    };
    ToolResult {
        success,
        content: result.to_json(),
    }
}

// ── Output helpers ──

/// Strip ANSI escape sequences from output.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                while let Some(next) = chars.next() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            Some(']' | 'P' | '_' | '^') => {
                while let Some(next) = chars.next() {
                    if next == '\x07' {
                        break;
                    }
                    if next == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

// ── Registration ──

use crate::{ToolHandler, ToolRisk};
use std::time::Duration;

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register(ToolHandler {
        key: "exec".to_string(),
        description: "Execute a command. Two modes: (1) {\"argv\": [\"program\", \"arg1\", ...]} — direct exec, no shell. (2) {\"command\": \"pipeline | grep foo\"} — auto-wrapped in the platform shell (bash -c / pwsh -Command / cmd /c), enabling pipes, redirects, and shell builtins. For the argv mode, the first element is the executable, the rest are arguments. Returns {\"status\": \"completed\", \"exit_code\": 0, \"output\": \"...\", \"wall_time_seconds\": 0.5, \"timed_out\": false}",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "argv": { "type": "array", "items": {"type": "string"}, "description": "Command as array of strings. argv[0]=executable, argv[1..]=args. Example: [\"cargo\",\"check\"]" },
                "command": { "type": "string", "description": "Shell command string. Auto-wrapped in platform shell (bash -c, pwsh -Command, or cmd /c). Use for pipes, redirects, or one-liners. Example: \"ls -la | grep foo\"" },
                "cwd": {"type": "string", "description": "Working directory (optional). Defaults to workspace root."},
                "timeout_secs": {"type": "integer", "description": "Timeout in seconds (1-3600, default 30)"},
                "max_output_tokens": { "type": "integer", "description": "Max tokens of output before smart truncation (head 70% + tail 30%). Default 10000, min 100, max 50000." }
            },
            "required": [],
            "additionalProperties": false,
            "oneOf": [
                {"required": ["argv"]},
                {"required": ["command"]}
            ]
        }),
        handler: handle_run,
        risk: ToolRisk::Destructive,
        default_timeout: Duration::from_secs(30),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_git_status_returns_output() {
        let argv = vec!["git".to_string(), "status".to_string()];
        let result = direct_exec(&argv, None, 10000, 10, None, None, "test");
        eprintln!(
            "exit_code={:?} timed_out={} time={:.3}s tokens={}",
            result.exit_code, result.timed_out, result.wall_time_seconds, result.original_tokens
        );
        assert!(!result.timed_out, "timed out");
        assert!(!result.output.is_empty(), "no output");
    }

    #[test]
    fn test_git_diff_returns_output() {
        let argv = vec!["git".to_string(), "diff".to_string(), "--stat".to_string()];
        let result = direct_exec(&argv, None, 10000, 10, None, None, "test");
        eprintln!(
            "exit_code={:?} timed_out={} time={:.3}s tokens={}",
            result.exit_code, result.timed_out, result.wall_time_seconds, result.original_tokens
        );
        assert!(!result.timed_out, "timed out");
    }

    #[test]
    fn test_cargo_check_returns_output() {
        let argv = vec![
            "cargo".to_string(),
            "check".to_string(),
            "-p".to_string(),
            "deepx-types".to_string(),
        ];
        let result = direct_exec(&argv, None, 10000, 60, None, None, "test");
        eprintln!(
            "exit_code={:?} timed_out={} time={:.3}s tokens={}",
            result.exit_code, result.timed_out, result.wall_time_seconds, result.original_tokens
        );
        assert!(!result.timed_out, "timed out");
        assert!(!result.output.is_empty(), "no output");
    }

    #[cfg(windows)]
    #[test]
    fn per_call_cancel_stops_only_the_running_command() {
        let argv = vec![
            "cmd".to_string(),
            "/C".to_string(),
            "ping -n 6 127.0.0.1 >NUL".to_string(),
        ];
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let signal = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            signal.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        let result = direct_exec(&argv, None, 100, 10, Some(cancel.as_ref()), None, "test");
        assert!(
            result.cancelled,
            "per-call cancellation should stop the child"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn per_call_cancel_stops_only_the_running_command() {
        let argv = vec![
            "sleep".to_string(),
            "6".to_string(),
        ];
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let signal = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            signal.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        let result = direct_exec(&argv, None, 100, 10, Some(cancel.as_ref()), None, "test");
        assert!(
            result.cancelled,
            "per-call cancellation should stop the child"
        );
    }

    #[test]
    fn truncated_output_instructs_the_model_to_retry_narrowly() {
        let text = "token ".repeat(1_000);
        let truncated = token_truncate(&text, 10);
        assert!(
            truncated.contains("Call exec again with narrower argv or a filtering command.")
        );
    }

    #[test]
    fn pipe_reader_forwards_retained_chunks_with_the_call_id() {
        let (tx, rx) = crate::bounded_exec_progress_channel();
        let (output, truncated) = read_stream(
            std::io::Cursor::new(b"first\nsecond\n".to_vec()),
            1024,
            Some(tx),
            "call-stream-1".to_string(),
            ExecOutputStream::Stdout,
            Arc::new(AtomicU64::new(0)),
        );

        let chunks: Vec<_> = rx.try_iter().collect();
        assert_eq!(output, b"first\nsecond\n");
        assert!(!truncated);
        assert_eq!(
            chunks,
            vec![ExecProgressEvent {
                tool_call_id: "call-stream-1".to_string(),
                stream: ExecOutputStream::Stdout,
                seq: 0,
                chunk: "first\nsecond\n".to_string(),
            }]
        );
    }

    #[cfg(windows)]
    #[test]
    fn exec_forwards_stdout_to_the_progress_channel_before_returning() {
        let argv = vec![
            "cmd".to_string(),
            "/C".to_string(),
            "echo streamed-output".to_string(),
        ];
        let (tx, rx) = crate::bounded_exec_progress_channel();

        let result = direct_exec(&argv, None, 100, 10, None, Some(tx), "call-stream-2");
        let chunks: Vec<_> = rx.try_iter().collect();

        assert!(result.output.contains("streamed-output"));
        assert!(chunks.iter().any(|event| {
            event.tool_call_id == "call-stream-2"
                && event.stream == ExecOutputStream::Stdout
                && event.chunk.contains("streamed-output")
        }));
    }

    #[test]
    fn pipe_reader_keeps_split_utf8_characters_intact_for_the_ui() {
        let (tx, rx) = crate::bounded_exec_progress_channel();
        let mut input = vec![b'a'; 8191];
        input.extend_from_slice("中".as_bytes());
        let (_output, truncated) = read_stream(
            std::io::Cursor::new(input),
            16 * 1024,
            Some(tx),
            "utf8".to_string(),
            ExecOutputStream::Stdout,
            Arc::new(AtomicU64::new(0)),
        );
        assert!(!truncated);
        let text: String = rx.try_iter().map(|event| event.chunk).collect();
        assert!(text.ends_with('中'));
        assert!(!text.contains('\u{fffd}'));
    }

    #[cfg(windows)]
    #[test]
    fn windows_oem_output_is_decoded_without_utf8_beta_mode() {
        // GBK/936 for "正在", representative of cmd.exe ping output.
        assert_eq!(decode_captured(&[0xD5, 0xFD, 0xD4, 0xDA]), "正在");
    }

    #[test]
    fn bounded_progress_queue_drops_updates_without_blocking_pipe_readers() {
        let (tx, _rx) = crate::bounded_exec_progress_channel();
        for seq in 0..=crate::EXEC_PROGRESS_CHANNEL_CAPACITY {
            tx.try_send(ExecProgressEvent {
                tool_call_id: "bounded".to_string(),
                stream: ExecOutputStream::Stdout,
                seq: seq as u64,
                chunk: "x".to_string(),
            });
        }
        assert_eq!(tx.dropped_bytes(), 1);
    }
}
