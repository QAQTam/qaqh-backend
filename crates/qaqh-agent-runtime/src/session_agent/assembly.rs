//! Production assembly for the session loop: provider request spec and the
//! tool policy snapshot are derived from the effective session config
//! instead of being hard-coded at the spawn site.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use qaqh_session::owner::SessionOwner;
use qaqh_types::ThinkingParamMode;

use crate::provider_job::ProviderRequestSpec;
use crate::session_loop::SessionLoop;
use crate::tool_job::{ToolPolicySnapshot, WorkspaceToolPort};

/// Provider request parameters for one session.
///
/// The wire protocol, model, token budget, reasoning effort and tool catalog
/// are all bound into the request identity, so they must come from the
/// effective config rather than a fixed constructor.
pub fn provider_spec(
    config: &qaqh_config::Config,
    tools: Vec<qaqh_types::ToolDef>,
) -> Result<ProviderRequestSpec, String> {
    let deepseek_account = config.credential == qaqh_types::CredentialSource::DeepseekAccount;
    let base_url = if deepseek_account {
        qaqh_dsaccount::protocol::INFERENCE_ORIGIN.to_string()
    } else {
        config.base_url.clone()
    };
    if base_url.is_empty() {
        return Err(format!(
            "provider {}: endpoint base_url is empty; set it in providers.toml",
            config.provider_id
        ));
    }
    if config.max_tokens == 0 {
        return Err(format!(
            "provider {}: model {} has max_tokens = 0; fix the model definition in providers.toml",
            config.provider_id, config.model
        ));
    }
    let api_key = if deepseek_account {
        ""
    } else {
        config.api_key.as_str()
    };
    let provider = match config.wire {
        qaqh_types::Wire::Chat => qaqh_gate::ProviderConfig::chat(
            &base_url,
            api_key,
            &config.model,
            None,
            None,
            ThinkingParamMode::OpenAi,
            qaqh_types::CacheTokenField::PromptCacheHitTokens,
            true,
            None,
        ),
        qaqh_types::Wire::Responses => {
            qaqh_gate::ProviderConfig::responses(&base_url, api_key, &config.model, None)
        }
        qaqh_types::Wire::Messages => {
            qaqh_gate::ProviderConfig::messages(&base_url, api_key, &config.model, None)
        }
        qaqh_types::Wire::Gemini => {
            qaqh_gate::ProviderConfig::gemini(&base_url, api_key, &config.model, None)
        }
    };
    let mut provider = apply_endpoint_compat(provider, &config.compat);
    if deepseek_account && config.wire == qaqh_types::Wire::Responses {
        apply_deepseek_responses_facts(&mut provider, &config.model);
    }
    let mut spec = ProviderRequestSpec::new(provider, config.max_tokens)
        .with_credential(config.credential)
        .with_context(config.context_length, config.auto_compact_threshold);
    if !tools.is_empty() {
        spec = spec.with_tools(tools);
    }
    // `handoff` 是控制面工具，不受会话/全局 allowlist 约束：minimal 档连 `ask`
    // 都不带，若它也被过滤掉，模型就没有逃生阀，只会在撞墙时 400。
    crate::handoff::ensure_declared(&mut spec.tools);
    let effort = config.reasoning_effort.trim();
    if !effort.is_empty() {
        spec = spec.with_effort(effort.to_string());
    }
    Ok(spec)
}

/// 把 `EndpointCompat` 投影到 wire 无关的 `ProviderConfig`。
///
/// 此前 preset / 用户配置里的 `responses_*` 字段没有任何消费方：本函数曾只用
/// `ResponsesCompat::default()`，DeepSeek preset 的 `responses_effort_max = "max"`
/// 等适配配置全部落空。`apply_patch_input_format` 等原生工具位由 gate 侧单独
/// 消费，不在这张表里。
fn apply_endpoint_compat(
    mut provider: qaqh_gate::ProviderConfig,
    compat: &qaqh_types::EndpointCompat,
) -> qaqh_gate::ProviderConfig {
    provider.responses_compat = qaqh_gate::ResponsesCompat {
        web_search: compat.responses_web_search,
        echo_web_search_call: compat.responses_echo_web_search_call,
        send_include: compat.responses_send_include,
        effort_max: compat.responses_effort_max.clone(),
        supports_user: compat.responses_supports_user,
        search_function_alias: compat.responses_search_function_alias.clone(),
        echo_reasoning_content: compat.responses_echo_reasoning_content,
        siwc_plan: compat.responses_siwc_plan,
        apply_patch_freeform: compat.apply_patch_input_format
            == qaqh_types::ApplyPatchInputFormat::CodexCustom,
        ..provider.responses_compat
    };
    provider.retry = compat.retry.clone();
    if provider.responses_path.is_none() {
        provider.responses_path = compat.path.clone();
    }
    provider
}

/// DeepSeek OAuth（`api.deepseek.com` Responses）的**端点事实**。
///
/// 依据 api-docs.deepseek.com/guides/responses_api（2026-11-01）：
/// - `reasoning.effort` 支持 `none/low/high/max`——上限是 `max`，不是 OpenAI 的
///   `high`；`xhigh` 由端点自行映射为 `high`；
/// - `include` **不支持**（静默忽略）——思维链以明文 `reasoning` item 返回，
///   回放走 `echo_reasoning_content`，无需 encrypted_content；
/// - `web_search` 仅 `deepseek-v4-pro` 支持：flash 静默忽略，v4-pro 按次数计费
///   （`usage.server_tool_use`）；
/// - `apply_patch` 以 **native custom tool**（freeform 裸文本）声明：官方唯一
///   认的 custom 名称就是 `apply_patch`（其他名称 400），且官方 Codex 接入对
///   两个模型均声明 `apply_patch_tool_type: "freeform"`。回程裸补丁由 gate 的
///   `freeform_custom_blocks` 收口为 `{"patch": raw}`，执行器零改动。
///
/// 这些是端点事实而非用户偏好，且 OAuth 凭据被 origin 校验钉死在该端点上
/// （`provider_job::resolve_provider_credential`），所以在 config.compat 之后
/// 无条件收口——与 ChatGPT plan 路由的 `siwc_plan` 收口同一设计。
fn apply_deepseek_responses_facts(provider: &mut qaqh_gate::ProviderConfig, model: &str) {
    let compat = &mut provider.responses_compat;
    compat.effort_max = "max".into();
    compat.send_include = false;
    compat.web_search &= model.starts_with("deepseek-v4-pro");
    compat.apply_patch_freeform = true;
}

/// Tool execution policy for the given permission tier and sandbox role.
///
/// `os_sandbox_enabled` is the effective config switch; it is resolved once
/// here and carried in the snapshot so the tool dispatch path never rebuilds
/// the spec from a literal. Role (`SandboxMode`), permission tier and the OS
/// sandbox switch stay independent: disabling the sandbox is not a shortcut
/// past admission or approvals.
pub fn tool_policy(
    workspace_root: impl Into<PathBuf>,
    permission_level: u8,
    sandbox: qaqh_workspace::tool_api::SandboxMode,
    trusted_dirs: HashSet<PathBuf>,
    os_sandbox_enabled: bool,
) -> ToolPolicySnapshot {
    let workspace_root = workspace_root.into();
    let permission_level = qaqh_workspace::PermissionLevel::from_u8(permission_level);
    let sandbox_spec = qaqh_workspace::tool_api::SandboxSpec::for_permission(
        workspace_root.clone(),
        permission_level,
        os_sandbox_enabled,
    )
    .with_trusted_roots(trusted_dirs.iter().cloned());
    ToolPolicySnapshot {
        workspace_root,
        permission_level,
        sandbox,
        trusted_dirs,
        sandbox_spec,
    }
}

/// Enabled tool definitions for the session's tool manager.
pub fn tool_defs(manager: &qaqh_workspace::ToolManager) -> Vec<qaqh_types::ToolDef> {
    manager.filtered_defs()
}

/// Sandbox role for a subagent actor.
pub fn subagent_sandbox() -> qaqh_workspace::tool_api::SandboxMode {
    qaqh_workspace::tool_api::SandboxMode::Subagent
}

/// Resolve the workspace root a session executes in.
pub fn workspace_root(cwd: Option<String>, fallback: &Path) -> PathBuf {
    cwd.map(PathBuf::from)
        .unwrap_or_else(|| fallback.to_path_buf())
}

/// Inputs that legitimately differ between a root session and a subagent.
pub struct SpawnInput<'a> {
    pub session_id: &'a str,
    /// 会话 owner:业务落库、事件广播与工具生命周期句柄的唯一来源。
    pub owner: Arc<SessionOwner>,
    /// Effective config for this session (already merged with subagent overrides).
    pub config: &'a qaqh_config::Config,
    /// Session-level tool mode names. Empty = no session-level restriction.
    pub allowed_tools: Vec<String>,
    /// Global configuration ceiling. `Some(empty)` means no tools.
    pub global_allowed_tools: Option<Vec<String>>,
    pub permission_level: u8,
    pub sandbox: qaqh_workspace::tool_api::SandboxMode,
    pub workspace_root: PathBuf,
    pub exec_sessions: Arc<qaqh_process_tools::exec_session::ExecSessionManager>,
}

impl<'a> SpawnInput<'a> {
    /// Root session: allowlist and permission tier come from the effective config.
    pub fn for_session(
        session_id: &'a str,
        config: &'a qaqh_config::Config,
        tool_mode: &str,
        custom_tools: &[String],
        workspace_root: PathBuf,
        owner: Arc<SessionOwner>,
        exec_sessions: Arc<qaqh_process_tools::exec_session::ExecSessionManager>,
    ) -> Result<Self, String> {
        Ok(Self {
            session_id,
            owner,
            config,
            allowed_tools: session_tool_allowlist(tool_mode, custom_tools)?,
            global_allowed_tools: config
                .tool_allowlist_enabled
                .then(|| config.tool_allowlist.clone()),
            permission_level: config.permission_level,
            sandbox: qaqh_workspace::tool_api::SandboxMode::Main,
            workspace_root,
            exec_sessions,
        })
    }
}

/// Resolve the persisted session tool mode into its internal registration keys.
pub fn session_tool_allowlist(
    tool_mode: &str,
    custom_tools: &[String],
) -> Result<Vec<String>, String> {
    if tool_mode.is_empty() || tool_mode == qaqh_types::tool_mode::STANDARD {
        return Ok(Vec::new());
    }
    if let Some(tools) = qaqh_types::tool_mode::preset_tools(tool_mode) {
        return Ok(tools.iter().map(|tool| (*tool).to_string()).collect());
    }
    if tool_mode == qaqh_types::tool_mode::CUSTOM {
        if custom_tools.is_empty() {
            return Err("custom tool mode requires at least one tool in custom_tools".into());
        }
        return Ok(custom_tools.to_vec());
    }
    Err(format!("unknown session tool mode {tool_mode:?}"))
}

/// Assemble one session loop: tool manager → provider spec → tool port → loop.
///
/// Ordering matters: the tool manager must be initialised before the provider
/// spec is derived from `filtered_defs()` — otherwise the model is told about a
/// tool catalog the allowlist never approved. Business persistence, event
/// broadcast and tool lifecycle reporting all ride the owner.
pub fn assemble(input: SpawnInput<'_>) -> Result<Arc<SessionLoop>, String> {
    let SpawnInput {
        session_id,
        owner,
        config,
        allowed_tools,
        global_allowed_tools,
        permission_level,
        sandbox,
        workspace_root,
        exec_sessions,
    } = input;

    let mut tool_manager = qaqh_workspace::registration::build_tool_manager(&[]);
    tool_manager.set_global_allowed(global_allowed_tools);
    tool_manager.apply_init(allowed_tools);
    let mut provider = provider_spec(config, tool_defs(&tool_manager))?;
    // `session_title` 是根会话的自命名工具：子代理不进会话列表，声明无意义；
    // 用户命名（客户端 create 带名 / rename 过）的会话永不声明；auto 标题保留
    // 修订能力（任务需求变化时模型可重新生成）。
    if sandbox == qaqh_workspace::tool_api::SandboxMode::Main {
        let (title, source) = owner.title_state();
        if crate::session_title::should_declare(title.as_deref(), source.as_deref()) {
            crate::session_title::ensure_declared(&mut provider.tools);
        }
    }
    let policy = tool_policy(
        workspace_root.clone(),
        permission_level,
        sandbox,
        HashSet::new(),
        config.sandbox.enabled,
    );

    let port = Arc::new(WorkspaceToolPort::new(
        session_id,
        Arc::new(Mutex::new(tool_manager)),
        policy,
        owner.lifecycle(),
        exec_sessions,
    ));
    Ok(SessionLoop::new(
        session_id,
        owner,
        port,
        provider,
        workspace_root,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deepseek_responses_facts_are_pinned_to_the_official_endpoint_contract() {
        // 生产路径每次 build 都是新 provider：flash 与 v4-pro 各自收口一次。
        let flash = qaqh_gate::ProviderConfig::responses(
            "https://api.deepseek.com",
            "",
            "deepseek-flash",
            None,
        );
        let mut flash = apply_endpoint_compat(flash, &qaqh_types::EndpointCompat::default());
        apply_deepseek_responses_facts(&mut flash, "deepseek-flash");
        assert_eq!(flash.responses_compat.effort_max, "max");
        assert!(!flash.responses_compat.send_include);
        // flash：服务端 web_search 关（不支持）。
        assert!(!flash.responses_compat.web_search);
        // OAuth 路由的 apply_patch 固定走 freeform custom（官方唯一认的形状）。
        assert!(flash.responses_compat.apply_patch_freeform);

        let pro = qaqh_gate::ProviderConfig::responses(
            "https://api.deepseek.com",
            "",
            "deepseek-v4-pro",
            None,
        );
        let mut pro = apply_endpoint_compat(pro, &qaqh_types::EndpointCompat::default());
        apply_deepseek_responses_facts(&mut pro, "deepseek-v4-pro");
        assert!(pro.responses_compat.web_search);
    }

    #[test]
    fn compat_projection_carries_the_endpoint_knobs() {
        let mut compat = qaqh_types::EndpointCompat::default();
        compat.responses_effort_max = "xhigh".into();
        compat.responses_send_include = false;
        compat.apply_patch_input_format = qaqh_types::ApplyPatchInputFormat::CodexCustom;
        let provider = apply_endpoint_compat(
            qaqh_gate::ProviderConfig::responses("https://example.com", "k", "m", None),
            &compat,
        );
        assert_eq!(provider.responses_compat.effort_max, "xhigh");
        assert!(!provider.responses_compat.send_include);
        assert!(provider.responses_compat.apply_patch_freeform);
    }

    #[test]
    fn for_session_takes_the_allowlist_only_when_it_is_enabled() {
        let mut config = qaqh_config::Config::default();
        config.permission_level = 2;
        config.tool_allowlist = vec!["read".into(), "exec_command".into()];

        config.tool_allowlist_enabled = true;
        let restricted = {
            let owner = test_owner();
            SpawnInput::for_session(
                "s",
                &config,
                "standard",
                &[],
                PathBuf::from("/w"),
                owner,
                Arc::new(qaqh_process_tools::exec_session::ExecSessionManager::new()),
            )
            .unwrap()
        };
        assert!(restricted.allowed_tools.is_empty());
        assert_eq!(
            restricted.global_allowed_tools,
            Some(vec!["read".into(), "exec_command".into()])
        );
        assert_eq!(restricted.permission_level, 2);

        // Disabled allowlist = 空名单 = 不限制（不是「一个工具都不给」）。
        config.tool_allowlist_enabled = false;
        let open = {
            let owner = test_owner();
            SpawnInput::for_session(
                "s",
                &config,
                "standard",
                &[],
                PathBuf::from("/w"),
                owner,
                Arc::new(qaqh_process_tools::exec_session::ExecSessionManager::new()),
            )
            .unwrap()
        };
        assert!(open.allowed_tools.is_empty());
        assert_eq!(open.global_allowed_tools, None);
    }

    #[test]
    fn session_tool_modes_resolve_to_their_runtime_tool_sets() {
        assert_eq!(
            session_tool_allowlist("minimal:b", &[]).unwrap(),
            qaqh_types::tool_mode::MINIMAL_TOOLS_B
                .iter()
                .map(|tool| (*tool).to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            session_tool_allowlist("custom", &["exec_command".into()]).unwrap(),
            vec!["exec_command"]
        );
        assert!(session_tool_allowlist("custom", &[]).is_err());
    }

    fn test_owner() -> Arc<SessionOwner> {
        let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
        let db = Arc::new(qaqh_session::db::Db::open(dir.path().join("qaqh.db")).expect("open db"));
        Arc::new(SessionOwner::new("s", db))
    }
}
