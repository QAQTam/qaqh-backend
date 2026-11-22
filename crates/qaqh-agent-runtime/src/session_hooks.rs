//! 会话侧 hooks glue(ADR-0038):qaqh-hooks 机制层与 SessionLoop 的唯一桥。
//!
//! 职责:装配期做一次 discovery(trust store + config hooks + 全局/项目
//! 目录),持有 [`Dispatch`] 所需状态;运行通知经 mpsc 转发到 owner 事件面
//! (HookStarted/HookFinished,仅展示)。各门的调用点在 session_loop:
//! UserPromptSubmit / PreToolUse / PermissionRequest / PostToolUse / Stop。
//!
//! 项目作用域信任:独立 trust store(`<data_dir>/security/`),digest 匹配才
//! 放行;批准入口复用既有权限交互的 `trust_folder` 应答(session_loop 侧
//! 调 [`SessionHooks::trust_project`]),不新增 wire 命令。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use qaqh_hooks::dispatcher::HookRunNotice;
use qaqh_hooks::{
    Dispatch, Discovery, HookEventName, HookPayload, HookRegistry, RunStatus, HookTrustStore,
    RunContext,
};
use qaqh_ringing_contract::events::HookRunStatus;
use qaqh_ringing_contract::fact::{ToolCallId, TurnId};
use qaqh_session::owner::SessionOwner;
use tokio::sync::mpsc;

/// 单会话 hooks 运行时;registry 为空时不构造(零开销路径)。
pub struct SessionHooks {
    session_id: String,
    owner: Arc<SessionOwner>,
    registry: HookRegistry,
    trust: Arc<Mutex<HookTrustStore>>,
    project_root: Option<PathBuf>,
    project_digest: Option<String>,
    project_trusted: AtomicBool,
    permission_mode: &'static str,
    notices_tx: mpsc::UnboundedSender<HookRunNotice>,
}

impl SessionHooks {
    /// 装配期构造。`workspace_root` 是项目 hooks 发现与 digest 的根;
    /// 无任何 hook 声明时返回 None(运行循环零分支开销)。
    pub fn spawn(
        session_id: &str,
        owner: Arc<SessionOwner>,
        config: &qaqh_config::Config,
        workspace_root: &std::path::Path,
        permission_level: u8,
    ) -> Option<Self> {
        let data_dir = qaqh_types::platform::data_dir();
        let trust_path = data_dir.join("security").join("workspace-hook-trust-v1.json");
        let trust = Arc::new(Mutex::new(HookTrustStore::load(trust_path)));
        let discovery: Discovery = qaqh_hooks::discovery::load(
            &config.hooks,
            &data_dir,
            Some(&data_dir),
            Some(workspace_root),
            &trust.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for warning in &discovery.warnings {
            log::warn!("[hooks] {warning}");
        }
        if discovery.registry.is_empty() {
            return None;
        }
        log::info!(
            "[hooks] session {session_id}: {} hook handlers loaded (project {})",
            discovery.registry.entries().len(),
            if discovery.project_trusted {
                "trusted"
            } else {
                "untrusted"
            }
        );

        let (tx, mut rx) = mpsc::unbounded_channel::<HookRunNotice>();
        let forwarder_owner = owner.clone();
        let forwarder_session = session_id.to_string();
        let forwarder = crate::session_loop::spawn_task(async move {
            while let Some(notice) = rx.recv().await {
                emit_notice(&forwarder_owner, &forwarder_session, notice);
            }
        });
        // forwarder 与会话同生命周期:recv 随 tx 关闭而退出;drop JoinHandle
        // = detach,不取消任务。
        drop(forwarder);

        Some(Self {
            session_id: session_id.to_string(),
            owner,
            registry: discovery.registry,
            trust,
            project_root: workspace_root.to_path_buf().into(),
            project_digest: discovery.project_digest,
            project_trusted: AtomicBool::new(discovery.project_trusted),
            permission_mode: permission_mode_name(permission_level),
            notices_tx: tx,
        })
    }

    /// 构造一次门派发;cwd/turn/call 由调用点供给。
    pub fn dispatch(&self, cwd: &str) -> Dispatch<'_> {
        Dispatch {
            registry: &self.registry,
            ctx: RunContext {
                session_id: self.session_id.clone(),
                workspace_root: self
                    .project_root
                    .as_ref()
                    .map(|root| root.to_string_lossy().to_string())
                    .unwrap_or_default(),
                cwd: cwd.to_string(),
                permission_mode: Some(self.permission_mode.to_string()),
                turn_id: None,
                call_id: None,
            },
            project_trusted: self.project_trusted.load(Ordering::Acquire),
            notices: Some(self.notices_tx.clone()),
        }
    }

    /// 项目 hooks 信任批准(复用权限交互的 trust_folder 应答)。
    /// 成功 = 写独立 trust store + 原子翻转本会话放行位。
    pub fn trust_project(&self) {
        let Some(root) = self.project_root.as_ref() else {
            return;
        };
        let Some(digest) = self.project_digest.as_ref() else {
            return;
        };
        let outcome = self
            .trust
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .set_trust(root, true, digest, &qaqh_hooks::event::rfc3339_utc_now());
        match outcome {
            Ok(()) => {
                self.project_trusted.store(true, Ordering::Release);
                log::info!("[hooks] project hooks trusted for {}", root.display());
            }
            Err(error) => log::error!("[hooks] trust store write failed: {error}"),
        }
    }

    /// 项目 hooks 是否已放行(诊断)。
    pub fn project_trusted(&self) -> bool {
        self.project_trusted.load(Ordering::Acquire)
    }

    /// UserPromptSubmit 门。`typed` = 用户亲手输入(自动唤醒回合只观察,
    /// grok 同律:可 block 的只有人敲的 prompt)。
    pub async fn prompt_gate(&self, prompt: &str, typed: bool, cwd: &str) -> qaqh_hooks::PromptOutcome {
        let dispatch = self.dispatch(cwd);
        if !typed || !dispatch.has_hooks(HookEventName::UserPromptSubmit) {
            return qaqh_hooks::PromptOutcome::default();
        }
        dispatch.prompt(prompt).await
    }

    /// PreToolUse 门。返回 None = 无相关 hook(调用点零成本直通)。
    pub async fn pre_tool_use_gate(
        &self,
        tool_name: &str,
        tool_input: serde_json::Value,
        cwd: &str,
    ) -> Option<qaqh_hooks::PreToolUseOutcome> {
        let dispatch = self.dispatch(cwd);
        if !dispatch.has_hooks(HookEventName::PreToolUse) {
            return None;
        }
        Some(dispatch.pre_tool_use(tool_name, tool_input).await)
    }

    /// PermissionRequest 门(权限交互升起前预答)。
    pub async fn permission_gate(
        &self,
        tool_name: &str,
        tool_input: serde_json::Value,
        cwd: &str,
    ) -> Option<qaqh_hooks::PermissionOutcome> {
        let dispatch = self.dispatch(cwd);
        if !dispatch.has_hooks(HookEventName::PermissionRequest) {
            return None;
        }
        Some(dispatch.permission_request(tool_name, tool_input).await)
    }

    /// PostToolUse 门(工具结算后;可替换模型可见输出/追加上下文)。
    pub async fn post_tool_use_gate(
        &self,
        tool_name: &str,
        tool_input: serde_json::Value,
        tool_output: &str,
        cwd: &str,
    ) -> Option<qaqh_hooks::PostToolUseOutcome> {
        let dispatch = self.dispatch(cwd);
        if !dispatch.has_hooks(HookEventName::PostToolUse) {
            return None;
        }
        Some(dispatch.post_tool_use(tool_name, tool_input, tool_output).await)
    }

    /// Stop 门。`stop_hook_active` = 本回合已经因 Stop block 续跑过一次
    /// (防环:第二次 Stop 不再门控)。
    pub async fn stop_gate(
        &self,
        last_assistant_message: Option<&str>,
        stop_hook_active: bool,
        cwd: &str,
    ) -> Option<qaqh_hooks::StopOutcome> {
        let dispatch = self.dispatch(cwd);
        if !dispatch.has_hooks(HookEventName::Stop) || stop_hook_active {
            return None;
        }
        Some(dispatch.stop(last_assistant_message, stop_hook_active).await)
    }

    /// 观察事件(SessionStart/SessionEnd/Interrupt/StopFailure/StopCancelled/
    /// PermissionDenied/PostToolUseFailure)。
    pub async fn observe(
        &self,
        event: HookEventName,
        payload: HookPayload,
        cwd: &str,
        turn_id: Option<&str>,
        call_id: Option<&str>,
    ) {
        let dispatch = self.dispatch(cwd);
        if !dispatch.has_hooks(event) {
            return;
        }
        dispatch.observe(event, payload, turn_id, call_id).await;
    }
}

fn emit_notice(owner: &SessionOwner, session_id: &str, notice: HookRunNotice) {
    let _ = session_id;
    match notice {
        HookRunNotice::Started {
            run_id,
            event,
            hook_name,
            turn_id,
            call_id,
        } => {
            let turn_id = turn_id.map(TurnId::new);
            let call_id = call_id.map(ToolCallId::new);
            owner.hook_started(&run_id, event.key(), &hook_name, turn_id.as_ref(), call_id.as_ref());
        }
        HookRunNotice::Finished {
            run_id,
            status,
            summary,
            duration_ms,
            ..
        } => {
            let status = match status {
                RunStatus::Completed => HookRunStatus::Completed,
                RunStatus::Failed => HookRunStatus::Failed,
                RunStatus::TimedOut => HookRunStatus::TimedOut,
                RunStatus::SkippedUntrusted => HookRunStatus::SkippedUntrusted,
                RunStatus::SkippedDisabled => HookRunStatus::SkippedDisabled,
            };
            owner.hook_finished(&run_id, status, summary.as_deref(), duration_ms);
        }
    }
}

/// 权限档 → hook 信封 permission_mode 展示值。
fn permission_mode_name(level: u8) -> &'static str {
    match level {
        1 => "read-only",
        3 => "skip-permissions",
        _ => "workspace-write",
    }
}
