//! 沙箱启动分步自检状态机（ADR-0040 决策 1）。
//!
//! 前端启动时经 RingingV2 服务面 `sandbox.preflight.start` 触发后台分步实跑，
//! 并轮询 `sandbox.preflight.status` 驱动逐项检查动画与开启指引（如 Windows
//! `Client-ProjFS` 启用命令、非管理员运行建议等）。

use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{SandboxBackend, SandboxCapabilities};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxPreflightState {
    Idle,
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxPreflightStepStatus {
    Pending,
    Running,
    Passed,
    Warning,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxPreflightStep {
    pub id: String,
    pub label: String,
    pub status: SandboxPreflightStepStatus,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxPreflightReport {
    pub run_id: String,
    pub state: SandboxPreflightState,
    pub platform: String,
    pub effective_backend: SandboxBackend,
    pub filesystem_write_enforced: bool,
    pub network_isolation_enforced: bool,
    pub elevated: bool,
    pub progress_percent: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_step_id: Option<String>,
    pub steps: Vec<SandboxPreflightStep>,
    pub remediations: Vec<String>,
    pub started_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<i64>,
}

static PREFLIGHT_STATE: OnceLock<Mutex<SandboxPreflightReport>> = OnceLock::new();
static RUN_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn initial_steps() -> Vec<SandboxPreflightStep> {
    #[cfg(windows)]
    {
        return vec![
            SandboxPreflightStep {
                id: "token_elevation".into(),
                label: "进程提权与权限边界检查".into(),
                status: SandboxPreflightStepStatus::Pending,
                detail: "等待检测进程令牌提权状态".into(),
                remediation: None,
                required: false,
                duration_ms: None,
            },
            SandboxPreflightStep {
                id: "restricted_token".into(),
                label: "WRITE_RESTRICTED 受限令牌与私有桌面".into(),
                status: SandboxPreflightStepStatus::Pending,
                detail: "等待验证受限主令牌派生与窗口站私有桌面隔离".into(),
                remediation: None,
                required: true,
                duration_ms: None,
            },
            SandboxPreflightStep {
                id: "dacl_write_isolation".into(),
                label: "工作区内核 DACL 写隔离探针".into(),
                status: SandboxPreflightStepStatus::Pending,
                detail: "等待实测授权目录写入与越权路径内核拒绝".into(),
                remediation: None,
                required: true,
                duration_ms: None,
            },
            SandboxPreflightStep {
                id: "deny_carveout".into(),
                label: "敏感目录 (.git) 显式 Deny 保护探针".into(),
                status: SandboxPreflightStepStatus::Pending,
                detail: "等待实测工作区内保护子目录写拦截与读透传".into(),
                remediation: None,
                required: true,
                duration_ms: None,
            },
            SandboxPreflightStep {
                id: "projfs_redirect".into(),
                label: "ProjFS 虚拟重定向层 (Client-ProjFS)".into(),
                status: SandboxPreflightStepStatus::Pending,
                detail: "等待检测 Windows Projected File System 可选功能".into(),
                remediation: None,
                required: false,
                duration_ms: None,
            },
        ];
    }
    #[cfg(target_os = "linux")]
    {
        return vec![
            SandboxPreflightStep {
                id: "bubblewrap".into(),
                label: "Bubblewrap 命名空间沙箱探针".into(),
                status: SandboxPreflightStepStatus::Pending,
                detail: "等待检测 bwrap 与非特权用户命名空间".into(),
                remediation: None,
                required: false,
                duration_ms: None,
            },
            SandboxPreflightStep {
                id: "landlock_lsm".into(),
                label: "Landlock LSM 文件系统隔离".into(),
                status: SandboxPreflightStepStatus::Pending,
                detail: "等待检测内核 Landlock LSM 支持".into(),
                remediation: None,
                required: false,
                duration_ms: None,
            },
            SandboxPreflightStep {
                id: "seccomp_bpf".into(),
                label: "Seccomp-BPF 系统调用与网络过滤".into(),
                status: SandboxPreflightStepStatus::Pending,
                detail: "等待检测内核 Seccomp 过滤器支持".into(),
                remediation: None,
                required: false,
                duration_ms: None,
            },
        ];
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        vec![SandboxPreflightStep {
            id: "platform_support".into(),
            label: "平台沙箱后端检查".into(),
            status: SandboxPreflightStepStatus::Pending,
            detail: "等待检测当前操作系统沙箱能力".into(),
            remediation: None,
            required: false,
            duration_ms: None,
        }]
    }
}

fn new_idle_report() -> SandboxPreflightReport {
    let caps = SandboxCapabilities::detect();
    SandboxPreflightReport {
        run_id: "preflight_0".into(),
        state: SandboxPreflightState::Idle,
        platform: caps.platform_str().to_string(),
        effective_backend: caps.backend,
        filesystem_write_enforced: caps.filesystem_write_isolation,
        network_isolation_enforced: caps.network_isolation,
        elevated: is_process_elevated(),
        progress_percent: 0,
        current_step_id: None,
        steps: initial_steps(),
        remediations: Vec::new(),
        started_at_ms: 0,
        finished_at_ms: None,
    }
}

fn is_process_elevated() -> bool {
    #[cfg(windows)]
    {
        return sbx_win::token::is_elevated();
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn state_slot() -> &'static Mutex<SandboxPreflightReport> {
    PREFLIGHT_STATE.get_or_init(|| Mutex::new(new_idle_report()))
}

/// 读取最近一次已完成的自检写隔离结论（尚未完成时返回 `None`，调用方回退静态探测）。
pub fn cached_write_isolation_verified() -> Option<bool> {
    let guard = PREFLIGHT_STATE.get()?.lock().ok()?;
    match guard.state {
        SandboxPreflightState::Completed | SandboxPreflightState::Failed => {
            Some(guard.filesystem_write_enforced)
        }
        _ => None,
    }
}

/// 启动后台分步自检并返回当前快照。
///
/// - 若已在 `Running`，直接返回当前运行中快照（幂等，不重复起线程）；
/// - 若已 `Completed`/`Failed` 且 `force == false`，直接返回既有快照；
/// - 否则初始化新一轮 `run_id`，后台线程逐项执行探针并实时刷新进度。
pub fn start(force: bool) -> SandboxPreflightReport {
    let run_id = {
        let mut guard = state_slot()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.state == SandboxPreflightState::Running {
            return guard.clone();
        }
        if !force
            && matches!(
                guard.state,
                SandboxPreflightState::Completed | SandboxPreflightState::Failed
            )
        {
            return guard.clone();
        }
        let seq = RUN_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let run_id = format!("preflight_{}_{seq}", std::process::id());
        let steps = initial_steps();
        let first_step = steps.first().map(|s| s.id.clone());
        let caps = SandboxCapabilities::detect();
        *guard = SandboxPreflightReport {
            run_id: run_id.clone(),
            state: SandboxPreflightState::Running,
            platform: caps.platform_str().to_string(),
            effective_backend: caps.backend,
            filesystem_write_enforced: caps.filesystem_write_isolation,
            network_isolation_enforced: caps.network_isolation,
            elevated: is_process_elevated(),
            progress_percent: 0,
            current_step_id: first_step,
            steps,
            remediations: Vec::new(),
            started_at_ms: now_ms(),
            finished_at_ms: None,
        };
        run_id
    };

    std::thread::Builder::new()
        .name(format!("sbx-preflight-{run_id}"))
        .spawn(move || {
            execute_preflight_run(&run_id);
        })
        .ok();

    status()
}

/// 查询当前沙箱自检进度快照。若尚未启动（`Idle`），自动触发一次后台自检。
pub fn status() -> SandboxPreflightReport {
    let is_idle = {
        let guard = state_slot()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.state == SandboxPreflightState::Idle
    };
    if is_idle {
        return start(false);
    }
    state_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// 同步跑完一轮自检并返回最终报告（供测试或阻塞式调用）。
pub fn run_blocking() -> SandboxPreflightReport {
    let seq = RUN_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let run_id = format!("preflight_sync_{}_{seq}", std::process::id());
    {
        let mut guard = state_slot()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let steps = initial_steps();
        let first_step = steps.first().map(|s| s.id.clone());
        let caps = SandboxCapabilities::detect();
        *guard = SandboxPreflightReport {
            run_id: run_id.clone(),
            state: SandboxPreflightState::Running,
            platform: caps.platform_str().to_string(),
            effective_backend: caps.backend,
            filesystem_write_enforced: caps.filesystem_write_isolation,
            network_isolation_enforced: caps.network_isolation,
            elevated: is_process_elevated(),
            progress_percent: 0,
            current_step_id: first_step,
            steps,
            remediations: Vec::new(),
            started_at_ms: now_ms(),
            finished_at_ms: None,
        };
    }
    execute_preflight_run(&run_id);
    state_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn mark_step_running(run_id: &str, idx: usize) {
    let mut guard = state_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.run_id != run_id {
        return;
    }
    let total = guard.steps.len().max(1);
    if let Some(step) = guard.steps.get_mut(idx) {
        step.status = SandboxPreflightStepStatus::Running;
        guard.current_step_id = Some(step.id.clone());
    }
    guard.progress_percent = ((idx * 100) / total) as u8;
}

fn complete_step(
    run_id: &str,
    idx: usize,
    status: SandboxPreflightStepStatus,
    detail: String,
    remediation: Option<String>,
    duration_ms: u64,
) {
    let mut guard = state_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.run_id != run_id {
        return;
    }
    let total = guard.steps.len().max(1);
    if let Some(step) = guard.steps.get_mut(idx) {
        step.status = status;
        step.detail = detail;
        step.remediation = remediation.clone();
        step.duration_ms = Some(duration_ms);
    }
    if let Some(rem) = remediation
        && !rem.is_empty()
        && !guard.remediations.contains(&rem)
    {
        guard.remediations.push(rem);
    }
    guard.progress_percent = (((idx + 1) * 100) / total).min(100) as u8;
    guard.current_step_id = guard.steps.get(idx + 1).map(|s| s.id.clone());
}

fn finish_run(
    run_id: &str,
    state: SandboxPreflightState,
    effective_backend: SandboxBackend,
    filesystem_write_enforced: bool,
    network_isolation_enforced: bool,
) {
    let mut guard = state_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.run_id != run_id {
        return;
    }
    guard.state = state;
    guard.effective_backend = effective_backend;
    guard.filesystem_write_enforced = filesystem_write_enforced;
    guard.network_isolation_enforced = network_isolation_enforced;
    guard.progress_percent = 100;
    guard.current_step_id = None;
    guard.finished_at_ms = Some(now_ms());
}

fn execute_preflight_run(run_id: &str) {
    #[cfg(windows)]
    {
        execute_windows_preflight(run_id);
        return;
    }
    #[cfg(target_os = "linux")]
    {
        execute_linux_preflight(run_id);
        return;
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        mark_step_running(run_id, 0);
        complete_step(
            run_id,
            0,
            SandboxPreflightStepStatus::Warning,
            "当前平台暂未接线内核级沙箱后端，命令将以当前用户权限执行".into(),
            Some("请在受支持的 Windows 或 Linux 环境中启用内核沙箱隔离。".into()),
            0,
        );
        finish_run(
            run_id,
            SandboxPreflightState::Completed,
            SandboxBackend::None,
            false,
            false,
        );
    }
}

#[cfg(windows)]
fn execute_windows_preflight(run_id: &str) {
    use sbx_win::{
        acl, desktop,
        events::NullSink,
        policy::{AceMode, AceOp, AceTarget, NetworkPolicy, Precreate, SbxPolicy, build_ace_plan},
        projfs, sid, spawn, token,
    };

    let comspec =
        std::env::var("ComSpec").unwrap_or_else(|_| "C:\\Windows\\system32\\cmd.exe".into());
    let cmd_line = |inner: &str| format!("\"{comspec}\" /c {inner}");
    let quoted = |p: &std::path::Path| format!("\"{}\"", p.display());

    // ── Step 0: token_elevation ──
    mark_step_running(run_id, 0);
    let t0 = Instant::now();
    let elevated = token::is_elevated();
    if elevated {
        complete_step(
            run_id,
            0,
            SandboxPreflightStepStatus::Warning,
            "当前进程处于管理员提权 (Elevated) 状态；Administrators 组对象可能绕过受限令牌写拦截"
                .into(),
            Some("建议以普通标准用户（非管理员）身份启动客户端与守护进程，以确保受限令牌 DACL 写隔离对所有个人文件严格生效。".into()),
            t0.elapsed().as_millis() as u64,
        );
    } else {
        complete_step(
            run_id,
            0,
            SandboxPreflightStepStatus::Passed,
            "当前进程以标准非提权用户运行，受限令牌边界正常".into(),
            None,
            t0.elapsed().as_millis() as u64,
        );
    }

    // ── Step 1: restricted_token ──
    mark_step_running(run_id, 1);
    let t1 = Instant::now();
    let cap = match sid::capability_sid() {
        Ok(cap) => cap,
        Err(err) => {
            complete_step(
                run_id,
                1,
                SandboxPreflightStepStatus::Failed,
                format!("生成 Capability SID 失败: {err}"),
                Some("请检查 Windows 安全子系统 (Advapi32) 状态。".into()),
                t1.elapsed().as_millis() as u64,
            );
            finish_run(
                run_id,
                SandboxPreflightState::Failed,
                SandboxBackend::None,
                false,
                false,
            );
            return;
        }
    };
    let cap_text = sid::sid_to_string(&cap);
    let everyone = sid::everyone_sid();
    let tok = match token::create_restricted_token(&[cap.clone(), everyone]) {
        Ok(tok) => tok,
        Err(err) => {
            complete_step(
                run_id,
                1,
                SandboxPreflightStepStatus::Failed,
                format!("派生 WRITE_RESTRICTED 受限令牌失败: {err}"),
                Some("请检查本地安全策略是否限制了 CreateRestrictedToken。".into()),
                t1.elapsed().as_millis() as u64,
            );
            finish_run(
                run_id,
                SandboxPreflightState::Failed,
                SandboxBackend::None,
                false,
                false,
            );
            return;
        }
    };
    let priv_desktop = match desktop::create_private_desktop(&cap_text) {
        Ok(d) => d,
        Err(err) => {
            complete_step(
                run_id,
                1,
                SandboxPreflightStepStatus::Failed,
                format!("创建沙箱私有桌面失败: {err}"),
                Some("请确认守护进程运行在交互式窗口站 (WinSta0) 下，而非受限服务会话。".into()),
                t1.elapsed().as_millis() as u64,
            );
            finish_run(
                run_id,
                SandboxPreflightState::Failed,
                SandboxBackend::None,
                false,
                false,
            );
            return;
        }
    };
    complete_step(
        run_id,
        1,
        SandboxPreflightStepStatus::Passed,
        format!(
            "WRITE_RESTRICTED 受限令牌与私有桌面 ({}) 就绪",
            priv_desktop.name()
        ),
        None,
        t1.elapsed().as_millis() as u64,
    );

    // 准备隔离探针临时目录
    let base = std::env::temp_dir().join(format!(
        "qaqh-sbx-preflight-{}-{}",
        std::process::id(),
        now_ms()
    ));
    let scratch = base.join("scratch");
    let ws_a = base.join("ws_a");
    let git_dir = ws_a.join(".git");
    let ws_b = base.join("ws_b");
    let protected_file = ws_b.join("protected.txt");

    let setup_res = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(scratch.join("tmp"))?;
        std::fs::create_dir_all(scratch.join("home"))?;
        std::fs::create_dir_all(&git_dir)?;
        std::fs::write(git_dir.join("config"), "gitcfg")?;
        std::fs::create_dir_all(&ws_b)?;
        std::fs::write(&protected_file, "old")?;
        Ok(())
    })();

    if let Err(err) = setup_res {
        let _ = std::fs::remove_dir_all(&base);
        complete_step(
            run_id,
            2,
            SandboxPreflightStepStatus::Failed,
            format!("创建自检临时目录失败: {err}"),
            Some("请检查系统 TEMP 目录读写权限与剩余磁盘空间。".into()),
            0,
        );
        finish_run(
            run_id,
            SandboxPreflightState::Failed,
            SandboxBackend::None,
            false,
            false,
        );
        return;
    }

    // ── Step 2: dacl_write_isolation ──
    mark_step_running(run_id, 2);
    let t2 = Instant::now();
    let policy = SbxPolicy {
        writable_roots: vec![ws_a.clone()],
        writable_files: vec![],
        deny_write_paths: vec![git_dir.clone()],
        network: NetworkPolicy::Deny,
        isolation: sbx_win::policy::IsolationKind::Token,
        readable_roots: vec![],
        capabilities: vec![],
        writable_registry_keys: vec![],
        redirect: false,
    };
    let mut plan = build_ace_plan(&policy, &cap);
    plan.push(AceOp {
        path: scratch.clone(),
        mode: AceMode::Allow,
        sid: cap.clone(),
        inherit_tree: true,
        precreate: Precreate::None,
        full_mask: false,
        kind: AceTarget::File,
    });

    if let Err(err) = acl::apply_ops(&plan, &NullSink) {
        let _ = std::fs::remove_dir_all(&base);
        complete_step(
            run_id,
            2,
            SandboxPreflightStepStatus::Failed,
            format!("挂载 Capability SID DACL 失败: {err}"),
            Some("请确认系统临时目录与工作区位于支持 DACL 的 NTFS/ReFS 分区。".into()),
            t2.elapsed().as_millis() as u64,
        );
        finish_run(
            run_id,
            SandboxPreflightState::Failed,
            SandboxBackend::None,
            false,
            false,
        );
        return;
    }

    let env_pairs = vec![
        (
            "SystemRoot".into(),
            std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into()),
        ),
        ("ComSpec".into(), comspec.clone()),
        ("TEMP".into(), scratch.join("tmp").display().to_string()),
        ("TMP".into(), scratch.join("tmp").display().to_string()),
    ];
    let run_probe = |inner: String| -> std::io::Result<spawn::Exit> {
        let child = spawn::spawn(
            &spawn::Isolation::RestrictedToken {
                token: tok.handle(),
            },
            &cmd_line(&inner),
            &scratch,
            &env_pairs,
            Some(priv_desktop.name()),
        )
        .map_err(|e| std::io::Error::other(format!("spawn: {e}")))?;
        child.wait()
    };

    let ok_file = ws_a.join("ok.txt");
    let allow_ok = run_probe(format!("echo x > {}", quoted(&ok_file)))
        .map(|e| e.code == 0 && ok_file.exists())
        .unwrap_or(false);
    let deny_outside_ok = run_probe(format!("echo x > {}", quoted(&protected_file)))
        .map(|e| {
            e.code != 0
                && std::fs::read_to_string(&protected_file)
                    .map(|c| c == "old")
                    .unwrap_or(false)
        })
        .unwrap_or(false);

    if allow_ok && deny_outside_ok {
        complete_step(
            run_id,
            2,
            SandboxPreflightStepStatus::Passed,
            "授权工作区写入成功，未授权外部路径写入已被内核 DACL 拒绝".into(),
            None,
            t2.elapsed().as_millis() as u64,
        );
    } else {
        let _ = std::fs::remove_dir_all(&base);
        complete_step(
            run_id,
            2,
            SandboxPreflightStepStatus::Failed,
            format!(
                "DACL 写隔离探针未通过 (allow_write={allow_ok}, deny_outside={deny_outside_ok})"
            ),
            Some(if elevated {
                "当前处于管理员提权状态导致受限令牌未能拦截外部写入，请降权以普通用户身份重新启动应用。".into()
            } else {
                "请确认工作区与临时目录位于支持 NTFS ACL 的磁盘分区。".into()
            }),
            t2.elapsed().as_millis() as u64,
        );
        finish_run(
            run_id,
            SandboxPreflightState::Failed,
            SandboxBackend::None,
            false,
            false,
        );
        return;
    }

    // ── Step 3: deny_carveout ──
    mark_step_running(run_id, 3);
    let t3 = Instant::now();
    let git_config = git_dir.join("config");
    let deny_git_write = run_probe(format!("echo x > {}", quoted(&git_config)))
        .map(|e| e.code != 0)
        .unwrap_or(false);
    let allow_git_read = run_probe(format!("type {}", quoted(&git_config)))
        .map(|e| e.code == 0 && String::from_utf8_lossy(&e.stdout).contains("gitcfg"))
        .unwrap_or(false);

    let _ = std::fs::remove_dir_all(&base);

    if deny_git_write && allow_git_read {
        complete_step(
            run_id,
            3,
            SandboxPreflightStepStatus::Passed,
            "敏感子目录 (.git) 显式 Deny ACE 写保护与只读透传验证通过".into(),
            None,
            t3.elapsed().as_millis() as u64,
        );
    } else {
        complete_step(
            run_id,
            3,
            SandboxPreflightStepStatus::Failed,
            format!(
                "敏感目录保护探针失败 (deny_write={deny_git_write}, allow_read={allow_git_read})"
            ),
            Some("请检查文件系统 explicit Deny ACE 顺序是否被安全软件篡改。".into()),
            t3.elapsed().as_millis() as u64,
        );
        finish_run(
            run_id,
            SandboxPreflightState::Failed,
            SandboxBackend::None,
            false,
            false,
        );
        return;
    }

    // ── Step 4: projfs_redirect ──
    mark_step_running(run_id, 4);
    let t4 = Instant::now();
    let projfs_ok = projfs::available();
    let redirect_on = crate::sbx_map::redirect_promoted();
    let effective_backend = if projfs_ok && redirect_on {
        complete_step(
            run_id,
            4,
            SandboxPreflightStepStatus::Passed,
            "Client-ProjFS 已启用，沙箱已就绪于最强 RedirectPlane (虚拟视图写重定向 + 受限令牌)"
                .into(),
            None,
            t4.elapsed().as_millis() as u64,
        );
        SandboxBackend::WindowsRedirect
    } else if !projfs_ok {
        complete_step(
            run_id,
            4,
            SandboxPreflightStepStatus::Warning,
            "Windows Client-ProjFS 可选功能尚未开启；当前已自动启用 TokenPlane (受限令牌 + DACL) 内核写隔离".into(),
            Some("建议开启 Windows ProjFS 以获得虚拟视图写重定向隔离（管理员终端执行一次）：DISM /Online /Enable-Feature /FeatureName:Client-ProjFS /NoRestart".into()),
            t4.elapsed().as_millis() as u64,
        );
        SandboxBackend::WindowsToken
    } else {
        complete_step(
            run_id,
            4,
            SandboxPreflightStepStatus::Warning,
            "环境变量 QAQH_SBX_REDIRECT=0 已显式关闭 ProjFS 重定向；当前使用 TokenPlane 内核写隔离"
                .into(),
            Some("移除环境变量 QAQH_SBX_REDIRECT=0 即可恢复 ProjFS RedirectPlane。".into()),
            t4.elapsed().as_millis() as u64,
        );
        SandboxBackend::WindowsToken
    };

    finish_run(
        run_id,
        SandboxPreflightState::Completed,
        effective_backend,
        true,
        false,
    );
}

#[cfg(target_os = "linux")]
fn execute_linux_preflight(run_id: &str) {
    let caps = SandboxCapabilities::detect();

    // Step 0: bubblewrap
    mark_step_running(run_id, 0);
    let t0 = Instant::now();
    if caps.bubblewrap {
        complete_step(
            run_id,
            0,
            SandboxPreflightStepStatus::Passed,
            format!("Bubblewrap 命名空间沙箱可用 ({})", caps.detail),
            None,
            t0.elapsed().as_millis() as u64,
        );
    } else {
        complete_step(
            run_id,
            0,
            SandboxPreflightStepStatus::Warning,
            format!("Bubblewrap 不可用: {}", caps.detail),
            Some("建议安装 bubblewrap 并开启非特权用户命名空间 (kernel.unprivileged_userns_clone=1)。".into()),
            t0.elapsed().as_millis() as u64,
        );
    }

    // Step 1: landlock
    mark_step_running(run_id, 1);
    let t1 = Instant::now();
    if caps.landlock {
        complete_step(
            run_id,
            1,
            SandboxPreflightStepStatus::Passed,
            "内核 Landlock LSM 已启用".into(),
            None,
            t1.elapsed().as_millis() as u64,
        );
    } else {
        complete_step(
            run_id,
            1,
            SandboxPreflightStepStatus::Warning,
            "内核未启用 Landlock LSM".into(),
            Some("请在内核启动参数 security= 中包含 landlock。".into()),
            t1.elapsed().as_millis() as u64,
        );
    }

    // Step 2: seccomp
    mark_step_running(run_id, 2);
    let t2 = Instant::now();
    if caps.seccomp {
        complete_step(
            run_id,
            2,
            SandboxPreflightStepStatus::Passed,
            "Seccomp-BPF 系统调用过滤可用".into(),
            None,
            t2.elapsed().as_millis() as u64,
        );
    } else {
        complete_step(
            run_id,
            2,
            SandboxPreflightStepStatus::Failed,
            "Seccomp-BPF 不可用".into(),
            Some("请启用 Linux 内核 CONFIG_SECCOMP_FILTER。".into()),
            t2.elapsed().as_millis() as u64,
        );
    }

    let state = if caps.filesystem_write_isolation {
        SandboxPreflightState::Completed
    } else {
        SandboxPreflightState::Failed
    };
    finish_run(
        run_id,
        state,
        caps.backend,
        caps.filesystem_write_isolation,
        caps.network_isolation,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preflight_runs_all_steps_and_updates_cached_report() {
        let report = run_blocking();
        assert_eq!(report.progress_percent, 100);
        assert!(matches!(
            report.state,
            SandboxPreflightState::Completed | SandboxPreflightState::Failed
        ));
        assert!(!report.steps.is_empty());
        assert!(report.finished_at_ms.is_some());
        #[cfg(windows)]
        {
            assert_eq!(report.steps.len(), 5);
            assert_eq!(report.steps[0].id, "token_elevation");
            assert_eq!(report.steps[1].id, "restricted_token");
            assert_eq!(report.steps[2].id, "dacl_write_isolation");
            assert_eq!(report.steps[3].id, "deny_carveout");
            assert_eq!(report.steps[4].id, "projfs_redirect");
            if !report.elevated {
                assert!(report.filesystem_write_enforced);
                assert!(matches!(
                    report.effective_backend,
                    SandboxBackend::WindowsToken | SandboxBackend::WindowsRedirect
                ));
            }
        }
        let cached = status();
        assert_eq!(cached.run_id, report.run_id);
        assert_eq!(
            cached_write_isolation_verified(),
            Some(report.filesystem_write_enforced)
        );
    }
}
