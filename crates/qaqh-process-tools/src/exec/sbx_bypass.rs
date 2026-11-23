//! Windows sbx 旁路:sandboxed exec 经 sbx-win TokenPlane/RedirectPlane 执行。
//!
//! 调研报告(win-sandbox-rs docs/qaqh-integration-survey.md)§2 的"方案 b":
//! sbx Child 不进 ProcessRegistry,在本模块内完成 wait/cancel/输出收割;
//! 注册表只做展示面的 os_pid 快照(sbx Child 的输出由 sbx 自己的读线程
//! 缓冲,注册表的流式 append 对 sandboxed exec 无意义)。
//!
//! 与 std Command 路径的已知语义偏差(dark-launch 期间接受,治理接线时收敛):
//! - 超时 = kill_tree(Job close 杀整树)+ 交回已捕获输出,不走 background
//!   移交(sbx Child 私有进程句柄,无法移交注册表);
//! - 输出无流式 progress(sbx 在进程退出后一次性收割管道);
//! - redirect turn 边界固定 merge(ConfirmDeletions 默认):删除项按 ADR-0004
//!   不落盘,`skipped_deletions` 以 WARN 记录,审批确认接线(step 4)补齐。

use qaqh_policy::SandboxSpec;

use crate::ExecProgressSender;

use super::direct::ExecOutput;

/// Windows argv → CreateProcess 命令行(标准引号/反斜杠转义规则)。
fn cmdline_of(argv: &[String]) -> String {
    let mut out = String::new();
    for (i, arg) in argv.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        quote_windows_arg(arg, &mut out);
    }
    out
}

fn quote_windows_arg(arg: &str, out: &mut String) {
    if !arg.is_empty()
        && !arg
            .bytes()
            .any(|b| matches!(b, b' ' | b'\t' | b'"' | b'\n'))
    {
        out.push_str(arg);
        return;
    }
    out.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                for _ in 0..(backslashes * 2 + 1) {
                    out.push('\\');
                }
                out.push('"');
                backslashes = 0;
            }
            _ => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                out.push(ch);
                backslashes = 0;
            }
        }
    }
    for _ in 0..(backslashes * 2) {
        out.push('\\');
    }
    out.push('"');
}

/// 子进程环境:qaqh 最小白名单透传 → sbx 重指表(TMP/TEMP/USERPROFILE/HOME
/// → scratch)→ 模型显式 env 最后覆盖(spec §5.5 顺序)。
fn child_env(
    scratch_tmp: &std::path::Path,
    scratch_home: &std::path::Path,
) -> Vec<(String, String)> {
    let mut env: std::collections::BTreeMap<String, String> =
        super::direct::minimal_child_env().into_iter().collect();
    // config [exec.env] 用户授权注入面：排在 sandbox 重指之前——scratch
    // 重指是 harness 沙箱权威，压过用户配置（overlay 不能破坏 TMP/HOME 隔离）。
    for (key, value) in crate::exec::env_overlay::snapshot() {
        env.insert(key, value);
    }
    let repoint = [
        ("TEMP", scratch_tmp.display().to_string()),
        ("TMP", scratch_tmp.display().to_string()),
        ("USERPROFILE", scratch_home.display().to_string()),
        ("HOME", scratch_home.display().to_string()),
    ];
    for (k, v) in repoint {
        env.insert(k.to_string(), v);
    }
    env.into_iter().collect()
}

fn sandboxed_failure(display_name: &str, message: String) -> ExecOutput {
    ExecOutput {
        status: "completed".to_string(),
        command: display_name.to_string(),
        exit_code: Some(-1),
        output: message,
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

/// sbx 后端执行入口。`background_after_secs` 被忽略(见模块注释)。
#[allow(clippy::too_many_arguments)] // 参数面与 direct_exec_inner 对齐
pub(crate) fn sbx_exec(
    argv: &[String],
    env: Option<&[(String, String)]>,
    cwd: Option<&str>,
    max_output_chars: u32,
    timeout_secs: u64,
    _background_after_secs: Option<u64>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    progress_tx: Option<ExecProgressSender>,
    tool_call_id: &str,
    display_name: &str,
    spec: &SandboxSpec,
    command_text: &str,
) -> ExecOutput {
    use sbx_win::events::EventSink;
    use std::io::Write;

    let _ = progress_tx; // 无流式输出:输出在进程退出后由 sbx 一次性收割
    let _ = tool_call_id;
    let start_time = std::time::Instant::now();

    let Some(policy_json) = qaqh_sandbox::sbx_map::policy_json_for_exec(spec) else {
        return sandboxed_failure(
            display_name,
            "SANDBOX PREPARE FAILED: sbx policy mapping unavailable".to_string(),
        );
    };
    let policy: sbx_win::policy::SbxPolicy = match serde_json::from_str(&policy_json) {
        Ok(p) => p,
        Err(error) => {
            return sandboxed_failure(
                display_name,
                format!("SANDBOX PREPARE FAILED: sbx policy mapping: {error}"),
            );
        }
    };
    let redirect = policy.redirect;

    // 身份:workspace 模式 SID/台账持久化(sbx cleanup 可整树撤销);
    // 无 workspace 时一次性随机 SID。
    let workspace = spec.workspace_root.clone();
    let identity = match &workspace {
        Some(ws) => match sbx_win::sidstore::load_or_create_identity(
            ws,
            sbx_win::policy::IsolationKind::Token,
        ) {
            Ok((id, _created)) => id,
            Err(e) => {
                return sandboxed_failure(
                    display_name,
                    format!("SANDBOX PREPARE FAILED: sbx sidstore: {e}"),
                );
            }
        },
        None => match sbx_win::sid::capability_sid() {
            Ok(sid) => sbx_win::sidstore::Identity {
                sid_text: sbx_win::sid::sid_to_string(&sid),
                sid,
                ac_name: None,
                created: true,
            },
            Err(e) => {
                return sandboxed_failure(
                    display_name,
                    format!("SANDBOX PREPARE FAILED: sbx sidgen: {e}"),
                );
            }
        },
    };
    let cap = identity.sid.clone();

    // 事件流:workspace 模式落 journal.jsonl(可消费),否则丢弃。
    let journal: Option<std::fs::File> = workspace.as_ref().and_then(|ws| {
        let p = sbx_win::sidstore::journal_path(ws);
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .ok()
    });
    let sink: Box<dyn EventSink> = match journal {
        Some(f) => Box::new(sbx_win::events::JsonlSink::new(f)),
        None => Box::new(sbx_win::events::NullSink),
    };

    let scratch = std::env::temp_dir().join(format!(
        "qaqh-sbx-{}-{}",
        std::process::id(),
        sbx_win::events::now_millis()
    ));
    let scratch_tmp = scratch.join("tmp");
    let scratch_home = scratch.join("home");
    let view_root = scratch.join("view");
    if let Err(e) =
        std::fs::create_dir_all(&scratch_tmp).and_then(|_| std::fs::create_dir_all(&scratch_home))
    {
        return sandboxed_failure(
            display_name,
            format!("SANDBOX PREPARE FAILED: scratch: {e}"),
        );
    }

    // 子进程令牌:token 后端 = WRITE_RESTRICTED 受限令牌(直接子令牌,免提权)。
    let child_token =
        match sbx_win::token::create_restricted_token(&[cap.clone(), sbx_win::sid::everyone_sid()])
        {
            Ok(t) => t,
            Err(e) => {
                return sandboxed_failure(
                    display_name,
                    format!("SANDBOX PREPARE FAILED: restricted token: {e}"),
                );
            }
        };

    // spawn 前 ACL 计划:redirect 下工作区内授权目标进视图(真实工作区不挂
    // 任何 cap ACE),工作区外就地;非 redirect 全部就地。
    let (mut plan, view_ops) = if redirect {
        let ws = workspace.as_deref().expect("redirect requires workspace");
        match sbx_win::redirect::split_plan(&policy, &cap, ws, &view_root) {
            Ok(s) => (s.inplace, Some(s.view)),
            Err(e) => {
                return sandboxed_failure(
                    display_name,
                    format!("SANDBOX PREPARE FAILED: split redirect plan: {e}"),
                );
            }
        }
    } else {
        (sbx_win::policy::build_ace_plan(&policy, &cap), None)
    };
    plan.push(sbx_win::policy::AceOp {
        path: scratch.clone(),
        mode: sbx_win::policy::AceMode::Allow,
        sid: cap.clone(),
        inherit_tree: true,
        precreate: sbx_win::policy::Precreate::None,
        full_mask: false,
        kind: sbx_win::policy::AceTarget::File,
    });
    let snapshots = match sbx_win::acl::apply_ops(&plan, sink.as_ref()) {
        Ok(s) => s,
        Err(e) => {
            return sandboxed_failure(
                display_name,
                format!("SANDBOX PREPARE FAILED: apply acl plan: {e}"),
            );
        }
    };

    // ACE 台账(workspace 模式;sbx cleanup 的输入,与 CLI run 同构)。
    if let Some(ws) = &workspace {
        let ledger_path = sbx_win::sidstore::ledger_path(ws);
        if let Some(dir) = ledger_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut ledger) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&ledger_path)
        {
            for (op, sddl) in plan.iter().zip(snapshots.iter()) {
                let _ = writeln!(
                    ledger,
                    "{}",
                    serde_json::json!({
                        "path": op.path.display().to_string(),
                        "sid": identity.sid_text,
                        "ts": sbx_win::events::now_millis(),
                        "sddl": sddl,
                        "kind": match op.kind {
                            sbx_win::policy::AceTarget::File => "file",
                            sbx_win::policy::AceTarget::Registry => "reg",
                        },
                    })
                );
            }
        }
    }

    // redirect:启动 turn(视图随 run 起、随 turn 止,水合由本进程服务)。
    // 契约②（CLEAN-3/T10）：turn 存续期内（直到 merge 完成随本函数返回
    // 而drop）持有 RedirectTurnGuard，spy restore 在此期间被拒绝。
    let turn = if let Some(view_ops) = &view_ops {
        let ws = workspace.as_deref().expect("redirect requires workspace");
        match sbx_win::redirect::prepare_turn(ws, &view_root, view_ops) {
            Ok(t) => Some(t),
            Err(e) => {
                return sandboxed_failure(
                    display_name,
                    format!("SANDBOX PREPARE FAILED: prepare view turn: {e}"),
                );
            }
        }
    } else {
        None
    };
    let _turn_guard = turn
        .is_some()
        .then(crate::exec::redirect_guard::RedirectTurnGuard::acquire);

    let desktop = match sbx_win::desktop::create_private_desktop(&identity.sid_text) {
        Ok(d) => d,
        Err(e) => {
            return sandboxed_failure(
                display_name,
                format!("SANDBOX PREPARE FAILED: private desktop: {e}"),
            );
        }
    };

    let mut env_pairs = child_env(&scratch_tmp, &scratch_home);
    if let Some(env) = env {
        env_pairs.extend(env.iter().cloned());
    }
    // pager/编辑器/凭据提示解除：harness 最终权威，压过模型显式 env
    // (sbx 后端与 std Command 路径同一口径)。
    env_pairs.extend(super::direct::forced_child_env());

    // cwd:redirect 下工作区内路径映射进视图;无 cwd 时非 redirect 用
    // scratch\tmp,redirect 用视图根。
    let mapped_cwd: Option<std::path::PathBuf> = match (cwd, &turn) {
        (Some(c), Some(turn)) => {
            let ws = workspace.as_deref().expect("redirect requires workspace");
            match sbx_win::redirect::workspace_rel(ws, std::path::Path::new(c)) {
                Some(rel) if !rel.is_empty() => Some(turn.root().join(rel.replace('/', "\\"))),
                Some(_) => Some(turn.root().to_path_buf()),
                None => Some(std::path::PathBuf::from(c)),
            }
        }
        (Some(c), None) => Some(std::path::PathBuf::from(c)),
        (None, Some(turn)) => Some(turn.root().to_path_buf()),
        (None, None) => Some(scratch_tmp.clone()),
    };
    let cwd_path = mapped_cwd.unwrap_or_else(|| scratch_tmp.clone());

    let cmdline = cmdline_of(argv);
    let isolation = sbx_win::spawn::Isolation::RestrictedToken {
        token: child_token.handle(),
    };
    sink.emit(
        &sbx_win::events::Event::new("spawn").with_detail(serde_json::json!({
            "cmdline": cmdline,
            "redirect": redirect,
            "cap_sid": identity.sid_text,
            "tool_call_id": tool_call_id,
        })),
    );
    let mut child = match sbx_win::spawn::spawn(
        &isolation,
        &cmdline,
        &cwd_path,
        &env_pairs,
        Some(desktop.name()),
    ) {
        Ok(c) => c,
        Err(e) => {
            return sandboxed_failure(display_name, format!("SPAWN FAILED (sbx): {e}"));
        }
    };

    // 轮询退出:超时/取消 = kill_tree(Job close 杀整树)。background 移交
    // 不支持(模块注释);sbx Child 的 try_wait 替代注册表轮询。
    let deadline = start_time + std::time::Duration::from_secs(timeout_secs);
    let mut timed_out = false;
    let mut cancelled = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if cancel.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst)) {
                    child.kill_tree();
                    cancelled = true;
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    log::warn!(
                        "[exec] sbx timeout reached, killing process tree (job close); \
                         background handoff is not supported on the Windows sbx backend"
                    );
                    child.kill_tree();
                    timed_out = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                return sandboxed_failure(display_name, format!("SANDBOX WAIT FAILED: {e}"));
            }
        }
    }
    let exit = match child.wait() {
        Ok(e) => e,
        Err(e) => {
            return sandboxed_failure(display_name, format!("SANDBOX REAP FAILED: {e}"));
        }
    };
    sink.emit(
        &sbx_win::events::Event::new("exit").with_detail(serde_json::json!({ "code": exit.code })),
    );

    let stdout_out = sbx_win::console::decode_console_bytes(&exit.stdout);
    let mut stderr_out = sbx_win::console::decode_console_bytes(&exit.stderr);

    // deny-steer:内核拒绝 → 冻结导流文案 + 疑似被拒写目标(v2a,ADR 2026-10-09)。
    if sbx_win::feedback::is_likely_sandbox_denied(exit.code, &stderr_out) {
        stderr_out.push_str(&sbx_win::feedback::denial_feedback_with_targets(
            command_text,
        ));
    }

    // turn 边界:通知流 + diff + merge(ConfirmDeletions 默认,ADR-0004)。
    // 删除项不落盘,skipped_deletions 显式回显——审批确认接线(step 4)消费。
    if let Some(turn) = &turn {
        for n in turn.take_notifications() {
            let (kind, path, dest) = match &n {
                sbx_win::projfs::Notification::PreDelete { path } => ("pre_delete", path, None),
                sbx_win::projfs::Notification::PreRename { path, dest } => {
                    ("pre_rename", path, Some(dest))
                }
                sbx_win::projfs::Notification::Renamed { path, dest } => {
                    ("renamed", path, Some(dest))
                }
                sbx_win::projfs::Notification::Overwritten { path } => ("overwritten", path, None),
                sbx_win::projfs::Notification::NewFile { path } => ("new_file", path, None),
            };
            sink.emit(
                &sbx_win::events::Event::new("view_notify").with_detail(serde_json::json!({
                    "kind": kind,
                    "path": path,
                    "dest": dest,
                })),
            );
        }
        let changes = turn.diff().unwrap_or_default();
        sink.emit(
            &sbx_win::events::Event::new("overlay_diff").with_detail(serde_json::json!({
                "count": changes.len(),
            })),
        );
        // 契约①（CLEAN-3/T10）：merge 必须在返回前同步完成——批级 tool_end
        // spy 扫描在本调用返回后才发生，merge 滞后会让扫描读到「上位盘 +
        // 视图」拼合态、产生不可归因的净变更。此处同步等待 merge 结果，
        // 失败如实进 stderr，绝不推迟到后台。
        match turn.merge() {
            Ok(r) => {
                sink.emit(&sbx_win::events::Event::new("overlay_merge").with_detail(
                    serde_json::json!({
                        "applied": r.applied,
                        "deleted": r.deleted,
                        "skipped_deletions": r.skipped_deletions,
                    }),
                ));
                if r.skipped_deletions > 0 {
                    let note = format!(
                        "[sbx] {} deletion(s) NOT applied (ADR-0004: deletions require explicit \
                         confirmation; files re-projected)",
                        r.skipped_deletions
                    );
                    log::warn!("[exec] {note} (tool_call_id={tool_call_id})");
                    stderr_out.push_str(&note);
                    stderr_out.push('\n');
                }
            }
            Err(e) => {
                let note = format!(
                    "[sbx] merge FAILED (partial state possible, manual reconciliation needed): {e}"
                );
                log::error!("[exec] {note} (tool_call_id={tool_call_id})");
                stderr_out.push_str(&note);
                stderr_out.push('\n');
            }
        }
    }

    // scratch 清理:merge 后视图 upper 已落盘;非 redirect 模式 scratch 只有
    // 重指的 tmp/home。best-effort,失败不吞结果。
    drop(turn);
    drop(child_token);
    let _ = std::fs::remove_dir_all(&scratch);

    // 审计 M1:denial 日志里的输出片段先过脱敏。
    let mut injected_secrets: Vec<String> = env
        .map(|pairs| {
            pairs
                .iter()
                .map(|(_, value)| value.clone())
                .filter(|value| value.chars().count() >= 8)
                .collect()
        })
        .unwrap_or_default();
    // [exec.env] overlay 可能携带解析后的 secret 值：一并纳入脱敏名单。
    injected_secrets.extend(
        crate::exec::env_overlay::snapshot()
            .into_iter()
            .map(|(_, value)| value)
            .filter(|value| value.chars().count() >= 8),
    );
    let combined_for_denial = format!("{stderr_out}{stdout_out}");
    qaqh_sandbox::record_denial_if_any(
        qaqh_policy::SandboxBackend::WindowsToken,
        Some(exit.code as i32),
        &combined_for_denial,
        tool_call_id,
        display_name,
        &injected_secrets,
    );

    let cleaned = super::truncate::strip_ansi(&format!("{stderr_out}{stdout_out}"));
    let total_chars = cleaned.chars().count();
    let (output_str, truncated) = if total_chars > max_output_chars as usize {
        (
            super::truncate::char_truncate(&cleaned, max_output_chars),
            true,
        )
    } else {
        (cleaned, false)
    };

    ExecOutput {
        status: if cancelled {
            "cancelled".to_string()
        } else {
            "completed".to_string()
        },
        command: display_name.to_string(),
        exit_code: Some(exit.code as i32),
        output: output_str,
        stdout: super::truncate::strip_ansi(&stdout_out),
        stderr: super::truncate::strip_ansi(&stderr_out),
        truncated,
        timed_out,
        cancelled,
        process_id: None,
        output_omitted_bytes: 0,
        protocol_dropped_bytes: 0,
    }
}

/// Interactive Windows `sbx-win` session handle used by `ExecSessionManager` (`exec_command`).
pub(crate) struct SbxInteractiveSession {
    child: sbx_win::spawn::Child,
    _token: sbx_win::token::Token,
    _desktop: sbx_win::desktop::Desktop,
    turn: Option<sbx_win::redirect::Turn>,
    _turn_guard: Option<crate::exec::redirect_guard::RedirectTurnGuard>,
    scratch: std::path::PathBuf,
    command_text: String,
    tool_call_id: String,
    settled: bool,
}

impl SbxInteractiveSession {
    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<qaqh_pty::SessionExit>> {
        match self.child.try_wait().map_err(std::io::Error::other)? {
            Some(code) => {
                self.child.mark_waited();
                Ok(Some(qaqh_pty::SessionExit {
                    code: Some(code as i32),
                }))
            }
            None => Ok(None),
        }
    }

    pub(crate) fn kill_tree(&mut self) -> std::io::Result<()> {
        self.child.kill_tree();
        Ok(())
    }

    /// Run turn merge, denial-steering feedback generation, and scratch cleanup once the child exits.
    pub(crate) fn settle_after_exit(
        &mut self,
        exit_code: Option<i32>,
        stderr_preview: &str,
    ) -> Option<String> {
        if self.settled {
            return None;
        }
        self.settled = true;

        let mut extra_stderr = String::new();
        if let Some(code) = exit_code
            && sbx_win::feedback::is_likely_sandbox_denied(code as u32, stderr_preview)
        {
            extra_stderr.push_str(&sbx_win::feedback::denial_feedback_with_targets(
                &self.command_text,
            ));
        }

        if let Some(turn) = self.turn.take() {
            let _ = turn.take_notifications();
            let _ = turn.diff();
            match turn.merge() {
                Ok(r) => {
                    if r.skipped_deletions > 0 {
                        let note = format!(
                            "[sbx] {} deletion(s) NOT applied (ADR-0004: deletions require explicit \
                             confirmation; files re-projected)",
                            r.skipped_deletions
                        );
                        log::warn!("[exec] {note} (tool_call_id={})", self.tool_call_id);
                        extra_stderr.push_str(&note);
                        extra_stderr.push('\n');
                    }
                }
                Err(e) => {
                    let note = format!(
                        "[sbx] merge FAILED (partial state possible, manual reconciliation needed): {e}"
                    );
                    log::error!("[exec] {note} (tool_call_id={})", self.tool_call_id);
                    extra_stderr.push_str(&note);
                    extra_stderr.push('\n');
                }
            }
        }
        let _ = self._turn_guard.take();
        let _ = std::fs::remove_dir_all(&self.scratch);
        (!extra_stderr.is_empty()).then_some(extra_stderr)
    }
}

impl Drop for SbxInteractiveSession {
    fn drop(&mut self) {
        if !self.settled {
            let _ = self.settle_after_exit(None, "");
        }
    }
}

/// Spawn an interactive `sbx-win` session returning `(session, stdin_writer, stdout_reader, stderr_reader)`.
pub(crate) fn spawn_sbx_interactive(
    argv: &[String],
    env: &[(String, String)],
    cwd: Option<&str>,
    spec: &SandboxSpec,
    command_text: &str,
    tool_call_id: &str,
) -> Result<
    (
        SbxInteractiveSession,
        std::fs::File,
        std::fs::File,
        std::fs::File,
    ),
    String,
> {
    use sbx_win::events::EventSink;
    use std::io::Write;

    let policy_json = qaqh_sandbox::sbx_map::policy_json_for_exec(spec)
        .ok_or_else(|| "sbx policy mapping unavailable".to_string())?;
    let policy: sbx_win::policy::SbxPolicy =
        serde_json::from_str(&policy_json).map_err(|e| format!("sbx policy mapping: {e}"))?;
    let redirect = policy.redirect;

    let workspace = spec.workspace_root.clone();
    let identity = match &workspace {
        Some(ws) => {
            sbx_win::sidstore::load_or_create_identity(ws, sbx_win::policy::IsolationKind::Token)
                .map(|(id, _)| id)
                .map_err(|e| format!("sbx sidstore: {e}"))?
        }
        None => {
            let sid = sbx_win::sid::capability_sid().map_err(|e| format!("sbx sidgen: {e}"))?;
            sbx_win::sidstore::Identity {
                sid_text: sbx_win::sid::sid_to_string(&sid),
                sid,
                ac_name: None,
                created: true,
            }
        }
    };
    let cap = identity.sid.clone();

    let journal: Option<std::fs::File> = workspace.as_ref().and_then(|ws| {
        let p = sbx_win::sidstore::journal_path(ws);
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .ok()
    });
    let sink: Box<dyn EventSink> = match journal {
        Some(f) => Box::new(sbx_win::events::JsonlSink::new(f)),
        None => Box::new(sbx_win::events::NullSink),
    };

    let scratch = std::env::temp_dir().join(format!(
        "qaqh-sbx-execs-{}-{}",
        std::process::id(),
        sbx_win::events::now_millis()
    ));
    let scratch_tmp = scratch.join("tmp");
    let scratch_home = scratch.join("home");
    let view_root = scratch.join("view");
    std::fs::create_dir_all(&scratch_tmp)
        .and_then(|_| std::fs::create_dir_all(&scratch_home))
        .map_err(|e| format!("scratch: {e}"))?;

    let child_token =
        sbx_win::token::create_restricted_token(&[cap.clone(), sbx_win::sid::everyone_sid()])
            .map_err(|e| format!("restricted token: {e}"))?;

    let (mut plan, view_ops) = if redirect {
        let ws = workspace
            .as_deref()
            .ok_or_else(|| "redirect requires workspace".to_string())?;
        let split = sbx_win::redirect::split_plan(&policy, &cap, ws, &view_root)
            .map_err(|e| format!("split redirect plan: {e}"))?;
        (split.inplace, Some(split.view))
    } else {
        (sbx_win::policy::build_ace_plan(&policy, &cap), None)
    };
    plan.push(sbx_win::policy::AceOp {
        path: scratch.clone(),
        mode: sbx_win::policy::AceMode::Allow,
        sid: cap.clone(),
        inherit_tree: true,
        precreate: sbx_win::policy::Precreate::None,
        full_mask: false,
        kind: sbx_win::policy::AceTarget::File,
    });
    let snapshots = sbx_win::acl::apply_ops(&plan, sink.as_ref())
        .map_err(|e| format!("apply acl plan: {e}"))?;

    if let Some(ws) = &workspace {
        let ledger_path = sbx_win::sidstore::ledger_path(ws);
        if let Some(dir) = ledger_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut ledger) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&ledger_path)
        {
            for (op, sddl) in plan.iter().zip(snapshots.iter()) {
                let _ = writeln!(
                    ledger,
                    "{}",
                    serde_json::json!({
                        "path": op.path.display().to_string(),
                        "sid": identity.sid_text,
                        "ts": sbx_win::events::now_millis(),
                        "sddl": sddl,
                        "kind": match op.kind {
                            sbx_win::policy::AceTarget::File => "file",
                            sbx_win::policy::AceTarget::Registry => "reg",
                        },
                    })
                );
            }
        }
    }

    let turn = if let Some(view_ops) = &view_ops {
        let ws = workspace
            .as_deref()
            .ok_or_else(|| "redirect requires workspace".to_string())?;
        Some(
            sbx_win::redirect::prepare_turn(ws, &view_root, view_ops)
                .map_err(|e| format!("prepare view turn: {e}"))?,
        )
    } else {
        None
    };
    let turn_guard = turn
        .is_some()
        .then(crate::exec::redirect_guard::RedirectTurnGuard::acquire);

    let desktop = sbx_win::desktop::create_private_desktop(&identity.sid_text)
        .map_err(|e| format!("private desktop: {e}"))?;

    let mut env_pairs = child_env(&scratch_tmp, &scratch_home);
    env_pairs.extend(env.iter().cloned());
    env_pairs.extend(super::direct::forced_child_env());

    let mapped_cwd: Option<std::path::PathBuf> = match (cwd, &turn) {
        (Some(c), Some(turn)) => {
            let ws = workspace.as_deref().expect("redirect requires workspace");
            match sbx_win::redirect::workspace_rel(ws, std::path::Path::new(c)) {
                Some(rel) if !rel.is_empty() => Some(turn.root().join(rel.replace('/', "\\"))),
                Some(_) => Some(turn.root().to_path_buf()),
                None => Some(std::path::PathBuf::from(c)),
            }
        }
        (Some(c), None) => Some(std::path::PathBuf::from(c)),
        (None, Some(turn)) => Some(turn.root().to_path_buf()),
        (None, None) => Some(scratch_tmp.clone()),
    };
    let cwd_path = mapped_cwd.unwrap_or_else(|| scratch_tmp.clone());

    let cmdline = cmdline_of(argv);
    let isolation = sbx_win::spawn::Isolation::RestrictedToken {
        token: child_token.handle(),
    };
    sink.emit(
        &sbx_win::events::Event::new("spawn").with_detail(serde_json::json!({
            "cmdline": cmdline,
            "redirect": redirect,
            "cap_sid": identity.sid_text,
            "tool_call_id": tool_call_id,
        })),
    );
    let mut child = sbx_win::spawn::spawn_interactive(
        &isolation,
        &cmdline,
        &cwd_path,
        &env_pairs,
        Some(desktop.name()),
    )
    .map_err(|e| format!("spawn_interactive (sbx): {e}"))?;
    let (stdin_file, stdout_file, stderr_file) = child
        .take_stdio()
        .ok_or_else(|| "sbx interactive child missing stdio handles".to_string())?;

    Ok((
        SbxInteractiveSession {
            child,
            _token: child_token,
            _desktop: desktop,
            turn,
            _turn_guard: turn_guard,
            scratch,
            command_text: command_text.to_string(),
            tool_call_id: tool_call_id.to_string(),
            settled: false,
        },
        stdin_file,
        stdout_file,
        stderr_file,
    ))
}
