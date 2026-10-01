//! exec::pipe — 管道泵读线程族（PipePumpCtx/drain_pipe_to_registry/spawn_pipe_reader/forward_progress/send_progress）。

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use crate::{ExecOutputStream, ExecProgressEvent, ExecProgressSender};

/// 读线程共享上下文（收拢参数列表，亦便于 per-stream 构造）。
pub(crate) struct PipePumpCtx {
    pub(crate) progress_tx: Option<ExecProgressSender>,
    pub(crate) tool_call_id: String,
    pub(crate) output_stream: ExecOutputStream,
    pub(crate) progress_seq: Arc<AtomicU64>,
    pub(crate) registry_id: u32,
}
/// 平台 readiness 探测结果（`drain_pipe_to_registry` 的平台胶水协议）。
/// Empty/Closed 仅在 windows 探测路径构造（unix 走 read→WouldBlock）。
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) enum Readiness {
    /// 有数据可读。`Some(n)` = 平台报告的可用字节数（Windows
    /// `PeekNamedPipe`）——读线程据此一次读空管道，避免 8 KiB/次的
    /// 碎片化读写把大输出拖成 50ms×N 轮；`None` = 无可用量信息
    /// （unix 非阻塞 read 自身探路），退回定长读。
    Ready(Option<usize>),
    /// 管道当前为空（Windows PeekNamedPipe 为 0；unix 走 read→WouldBlock）。
    Empty,
    /// 写端已关闭——**不代表数据已读完**：管道内可能仍有已缓冲字节，
    /// 读线程必须继续 read 排空，`Ok(0)` 才是真 EOF。
    Closed,
}

/// 有界管道泵（读线程主循环，exec 生命周期重写阶段 2）。
///
/// 职责：把子进程输出解码后逐 chunk 推给 progress 通道（流式 UX），同时
/// 写入注册表（tail 视图 + full 捕获——seal 以 `captured_full` 为权威）。
///
/// **退出条件（以实现为准；旧版本文档写"settle 到期即退出…绝不等待
/// EOF"，与 30a011b 后的语义相反，已作废）**：
/// - EOF（`Ok(0)`）——**正常出口，也是唯一"数据读完"的判据**。子进程
///   退出即达，零额外等待；
/// - 读错误（`WouldBlock`/`Interrupted` 除外）；
/// - **settle 到期（兜底，非主出口）**：只在"放弃了"的两种形态生效——
///   ① `Empty`（探测无数据）且子进程终态、`READER_SETTLE_BUDGET` 已过；
///   ② `Closed`（写端全关）且子进程**仍存活**、`READER_SETTLE_BUDGET`
///   已过。读到任何数据都不会打断排空（快路径 `continue`，不睡轮询周期），
///   所以**只要子进程退出后管道仍有数据、且读得动，就一定能读到 `Ok(0)`**。
///   兜底的意义是：孙进程持有写端时读线程必须确定性退出并释放 progress
///   sender（阶段 1 的 1.3 遗留驻留治愈），"静默丢"降级为"有界丢"。
///
/// 三个容易反着记的点：
/// - **`Empty` / `Closed` 都不等于"数据读完"**：`Closed`（Windows
///   `PeekNamedPipe` 失败）在每次正常收尾都会发生，而缓冲可能仍是满的；
/// - **字节预算耗尽不停止排空**：超限 chunk 就地丢弃（`capped=true`），
///   但仍要读到 `Ok(0)`——只丢不读会让管道填满、反压卡住写端；
/// - **settle 计时不会因"读到数据"而重置**：它按 `exit_seen` 起算，读得
///   动就不会走到判据点，无需重置。
///
/// 返回 `(saw_eof, capped)`。`saw_eof=false` 只来自上面两条放弃路径
/// （settle 兜底、读错误），**没有第三条**；seal 据此判定 `truncated`，
/// 并由 [`super::direct::reader_eof_warning`] 落 warning 证据链。
pub(crate) fn drain_pipe_to_registry<S: std::io::Read>(
    stream: &mut S,
    max_bytes: usize,
    ctx: &PipePumpCtx,
    readiness: &mut dyn FnMut(&mut S) -> std::io::Result<Readiness>,
) -> (bool, bool) {
    let mut buf = vec![0u8; 8192];
    let mut pending_utf8 = Vec::new();
    let mut captured_bytes = 0usize;
    let mut capped = false;
    let mut saw_eof = false;
    let mut exit_seen: Option<std::time::Instant> = None;
    // 单次 read 的字节数：`Ready(Some(n))` 带上 Peek 报告的可用量，
    // 一次就把管道读空（否则 64 KiB 缓冲要 8 次 read + 8 次 50ms 轮询）。
    // `Readiness::Ready`（unix 路径）无可用量信息，退回定长 8 KiB。
    // 上限 64 KiB：一次 read 的临时缓冲不至于过大，同时避免单次占据过久。
    const MAX_READ_CHUNK: usize = 64 * 1024;
    let mut want = buf.len();
    loop {
        // ── 顺序至关重要：先按 readiness 定本次读取量，**再**做退出判定 ──
        // 反过来的话，只要子进程已退出且 settle 预算到期，就会在"管道里还有
        // 没读出来的数据"时直接 break（实测：72,973 字节的 `Out-String` 输出
        // 恒定少 8,193 字节）。settle 的职责只是"孙进程持有写端时读线程必须
        // 确定性退出"，不是"子进程一退出就停止排空"。
        let mut empty_and_settled = false;
        match readiness(stream) {
            Ok(Readiness::Ready(available)) => {
                // Windows Peek 报告的可用量：一次读空管道，避免 8 KiB/次的
                // 碎片化读写。None（unix 路径）退回定长读。
                want = available
                    .map(|n| n.clamp(1, MAX_READ_CHUNK))
                    .unwrap_or(buf.len());
            }
            Ok(Readiness::Empty) => {
                // 瞬时无数据。**不等于** stdout 已结束：子进程可能正处于写入
                // 间隙。只有"子进程已死 + settle 预算到期"才允许放弃。
                empty_and_settled = child_settled(&mut exit_seen, ctx);
            }
            Ok(Readiness::Closed) => {
                // 写端已关闭（Windows `PeekNamedPipe` 失败）**不等于"数据读完了"**：
                // 关闭瞬间管道里仍可能有已缓冲的字节。此处直接 break 会静默丢弃
                // 整段输出（实测：阻塞读总量 72,876 字节，旧实现只取到 13,315；
                // 极端情形 0 字节 + `truncated=true` 假警报）。改为**尽力排空**：
                // 继续 read，拿到数据就继续、`Ok(0)` 才算真 EOF。
                //
                // 阻塞安全性：`PeekNamedPipe` 失败当且仅当**写端全部关闭**
                // （ERROR_BROKEN_PIPE）或句柄异常。写端全关时 read 只会返回缓冲
                // 数据或 EOF，不会无限阻塞；孙进程仍持写端的情形 Peek 报 0
                // （走 Empty），不会进这里。
                //
                // 仍然加一道门：子进程**仍在运行**时不在本分支做阻塞 read
                // （保守，避免任何"Peek 瞬时失败 + 孙进程持写端"的挂起窗口
                // 重新引入读线程驻留）。`is_running` 为假时排空是安全的——这与
                // settle 预算的意图（孙进程持写端 → 只再排空 300ms）一致。
                if crate::process_registry::ProcessRegistry::is_running(ctx.registry_id) {
                    if child_settled(&mut exit_seen, ctx) {
                        break;
                    }
                    std::thread::sleep(READER_POLL_TICK);
                    continue;
                }
                want = buf.len();
            }
            Err(_) => break,
        }
        if empty_and_settled {
            break;
        }
        // 先定下本次读取的字节数，避免 `&mut buf[..]` 的借用与后续对 `want` 的
        // 比较交叠（NLL 下 `want` 的读取会被判为与 buf 的可变借用冲突）。
        let cap_now = want.min(buf.len());
        match stream.read(&mut buf[..cap_now]) {
            Ok(0) => {
                saw_eof = true;
                break;
            }
            Ok(n) => {
                let retained = n.min(max_bytes.saturating_sub(captured_bytes));
                if retained > 0 {
                    forward_progress(
                        &mut pending_utf8,
                        &buf[..retained],
                        ctx.progress_tx.as_ref(),
                        &ctx.tool_call_id,
                        ctx.output_stream,
                        &ctx.progress_seq,
                        Some(ctx.registry_id),
                    );
                    captured_bytes += retained;
                }
                if retained < n {
                    capped = true;
                }
                // 读到数据即说明还在排空：立刻再读一轮（不睡 50ms）。这是
                // "突发大输出一次排空"的快路径；`Ok(0)` 才是终点。
                continue;
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::Interrupted =>
            {
                if child_settled(&mut exit_seen, ctx) {
                    break;
                }
                std::thread::sleep(READER_POLL_TICK);
                continue;
            }
            Err(_) => break,
        }
    }
    if !pending_utf8.is_empty() {
        send_progress(
            ctx.progress_tx.as_ref(),
            &ctx.tool_call_id,
            ctx.output_stream,
            &ctx.progress_seq,
            String::from_utf8_lossy(&pending_utf8).into_owned(),
        );
        append_registry(
            ctx.registry_id,
            ctx.output_stream,
            &String::from_utf8_lossy(&pending_utf8),
        );
    }
    (saw_eof, capped)
}

/// 读线程 settle 判定：子进程终态后起算 `READER_SETTLE_BUDGET`，到期 true。
/// status 单调（Running→Exited/Killed 不可逆），再见 Running 即重置计时
/// （防御性；正常时序下不可达）。
pub(crate) fn child_settled(exit_seen: &mut Option<std::time::Instant>, ctx: &PipePumpCtx) -> bool {
    if crate::process_registry::ProcessRegistry::is_running(ctx.registry_id) {
        *exit_seen = None;
        return false;
    }
    let seen = exit_seen.get_or_insert_with(std::time::Instant::now);
    seen.elapsed() >= READER_SETTLE_BUDGET
}

/// 生成读线程：泵循环 + 退出信号（seal 有界 join 的对象）。
/// sender（progress 与 done 信道）随线程结束必然 drop——这是
/// "读线程生命周期有界"的可观测保证。
pub(crate) fn spawn_pipe_reader<S>(
    stream: S,
    max_bytes: usize,
    ctx: PipePumpCtx,
    readiness: impl FnMut(&mut S) -> std::io::Result<Readiness> + Send + 'static,
    done_tx: std::sync::mpsc::Sender<(bool, bool)>,
) where
    S: std::io::Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut stream = stream;
        let mut readiness = readiness;
        let outcome = drain_pipe_to_registry(&mut stream, max_bytes, &ctx, &mut readiness);
        let _ = done_tx.send(outcome);
    });
}

/// unix：读端 fd 置非阻塞（poll 化读循环的前提）。
/// O_NONBLOCK 挂在"打开文件描述"上——父进程的读端与子进程的写端是
/// 两个独立描述，互不影响。
#[cfg(unix)]
pub(crate) fn set_pipe_nonblocking<P: std::os::fd::AsRawFd + ?Sized>(pipe: &P) {
    let fd = pipe.as_raw_fd();
    // SAFETY: fcntl on an fd we exclusively own; F_GETFL/F_SETFL are
    // parameterless-in/out queries on that descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags >= 0 {
        // SAFETY: same descriptor; enabling O_NONBLOCK only changes the
        // blocking behaviour of our own read end.
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    }
}

/// windows：PeekNamedPipe 查询可读字节数（匿名管道不支持 O_NONBLOCK，
/// 这是 poll 化读循环的等价物）。None = 探测失败（非管道句柄等），
/// 调用方按 Closed 处理——宁可放弃也不退回无限阻塞读。
#[cfg(windows)]
pub(crate) fn pipe_available_bytes(handle: std::os::windows::io::RawHandle) -> Option<u32> {
    // SAFETY: PeekNamedPipe is a well-known Kernel32 API with stable ABI.
    // We pass null buffers and only request the total-bytes-available
    // counter; the handle comes from std's piped stdio (a valid anonymous
    // pipe handle we exclusively read from).
    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn PeekNamedPipe(
            named_pipe: *mut core::ffi::c_void,
            buffer: *mut core::ffi::c_void,
            buffer_size: u32,
            bytes_read: *mut u32,
            total_bytes_avail: *mut u32,
            bytes_left_this_message: *mut u32,
        ) -> i32;
    }
    let mut avail: u32 = 0;
    let ok = unsafe {
        PeekNamedPipe(
            handle,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut avail,
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(avail)
}

/// seal 侧有界 join：等待读线程退出信号 (saw_eof, capped)。
/// 超时或读线程意外消失（panic → 信道 Disconnected）一律按 (false, false)
/// 处理 → truncated 保守提示；数据本身以 captured_full 为权威，不受影响。
///
/// 注意"(false, false) 兜底"是 **seal 侧**的第三类 `saw_eof=false` 来源，
/// 与读线程内部的两条放弃路径（settle 兜底、读错误）无关：此时读线程可能
/// 只是没赶上 `SEAL_JOIN_BUDGET`，数据仍在后台继续 append 到注册表。
pub(crate) fn wait_reader_done(
    rx: &std::sync::mpsc::Receiver<(bool, bool)>,
    deadline: std::time::Instant,
) -> (bool, bool) {
    let now = std::time::Instant::now();
    if now >= deadline {
        return (false, false);
    }
    rx.recv_timeout(deadline - now).unwrap_or_default()
}

/// Forward only complete text units. A command may split one Chinese character
/// across pipe reads; keeping its suffix here avoids replacement glyphs in UI.
/// On Windows, non-UTF-8 console output falls back to the active OEM code page.
pub(crate) fn forward_progress(
    pending: &mut Vec<u8>,
    bytes: &[u8],
    tx: Option<&ExecProgressSender>,
    tool_call_id: &str,
    stream: ExecOutputStream,
    seq: &Arc<AtomicU64>,
    registry_id: Option<u32>,
) {
    pending.extend_from_slice(bytes);
    loop {
        match std::str::from_utf8(pending) {
            Ok(valid) => {
                send_progress(tx, tool_call_id, stream, seq, valid.to_owned());
                if let Some(id) = registry_id {
                    append_registry(id, stream, valid);
                }
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
                    // OEM 解码块同样必须进注册表：captured_full 是工具结果的
                    // 权威源，漏追加 = 非 UTF-8（GBK 等码页）输出整段丢失
                    //（实测：pwsh 在 CP936 下 Write-Output 中文 → 工具结果
                    // 只剩 "\r\n"，G5 补跑 exec 断言红即此因）。
                    if let Some(id) = registry_id {
                        append_registry(id, stream, &decoded);
                    }
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

/// 将已解码的输出块追加到进程注册表（backgrounded 后 process_check 可查 tail）。
pub(crate) fn append_registry(id: u32, stream: ExecOutputStream, chunk: &str) {
    match stream {
        ExecOutputStream::Stdout => {
            crate::process_registry::ProcessRegistry::append_output(id, chunk)
        }
        ExecOutputStream::Stderr => {
            crate::process_registry::ProcessRegistry::append_stderr(id, chunk)
        }
    }
}

#[cfg(windows)]
pub(crate) fn decode_windows_oem(bytes: &[u8]) -> Option<String> {
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
    // `GetOEMCP` / `MultiByteToWideChar` bindings when `qaqh-workspace` gains
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

pub(crate) fn send_progress(
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
            bytes_total: 0,
        });
    }
}

/// 读线程生命周期（exec 生命周期重写阶段 2，registry-native）。
///
/// - `READER_POLL_TICK`：管道空的轮询粒度（unix 非阻塞 WouldBlock /
///   Windows PeekNamedPipe 为 0 时的重试间隔）。
/// - `READER_SETTLE_BUDGET`：兜底预算。语义是"**放弃**的判据"，不是
///   "读完的判据"：正常路径读到 `Ok(0)` 即出口，一分钱也不多等；只有
///   "终态 + 探测空"或"存活 + 写端全关 + 探测空"持续满这个预算，读线程
///   才放弃排空（丢弃后续 chunk）并释放 progress sender（阶段 1 的 1.3
///   遗留驻留治愈）。故它是**下界而非上界**：读得动就继续读到 EOF，
///   排空时长不受此值约束，只有"放弃"这一动作落在该预算内。
/// - `SEAL_JOIN_BUDGET`：seal 侧对两个读线程退出信号的有界 join 预算，
///   需覆盖"主循环观察到退出（≤50ms）+ settle（300ms）+ 调度余量"。
pub(crate) const READER_POLL_TICK: std::time::Duration = std::time::Duration::from_millis(50);
pub(crate) const READER_SETTLE_BUDGET: std::time::Duration = std::time::Duration::from_millis(300);
pub(crate) const SEAL_JOIN_BUDGET: std::time::Duration = std::time::Duration::from_millis(500);
