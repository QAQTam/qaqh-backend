//! Production assembly for the thin session owner: provider request spec and
//! the tool policy snapshot are derived from the effective session config
//! instead of being hard-coded at the spawn site.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use qaqh_session::canonical::CanonicalSessionIdentity;
use qaqh_session::context_projection::ContextBudget;
use qaqh_session::session_commit::SessionCommit;
use qaqh_types::ThinkingParamMode;

use crate::provider_job::ProviderRequestSpec;
use crate::tool_job::{ToolPolicySnapshot, WorkspaceToolPort};

/// Provider request parameters for one session.
///
/// The wire protocol, model, token budget, reasoning effort and tool catalog
/// are all bound into the request identity, so they must come from the
/// effective config rather than a fixed constructor.
pub fn provider_spec(
    config: &qaqh_config::Config,
    tools: Vec<qaqh_types::ToolDef>,
) -> ProviderRequestSpec {
    let base_url = if config.base_url.is_empty() {
        "https://api.openai.com/v1".to_string()
    } else {
        config.base_url.clone()
    };
    let provider = match config.wire {
        qaqh_types::Wire::Chat => qaqh_gate::ProviderConfig::chat(
            &base_url,
            &config.api_key,
            &config.model,
            None,
            None,
            ThinkingParamMode::OpenAi,
            qaqh_types::CacheTokenField::PromptCacheHitTokens,
            true,
            None,
        ),
        qaqh_types::Wire::Responses => {
            qaqh_gate::ProviderConfig::responses(&base_url, &config.api_key, &config.model, None)
        }
        qaqh_types::Wire::Messages => {
            qaqh_gate::ProviderConfig::messages(&base_url, &config.api_key, &config.model, None)
        }
        qaqh_types::Wire::Gemini => {
            qaqh_gate::ProviderConfig::gemini(&base_url, &config.api_key, &config.model, None)
        }
    };
    let mut spec = ProviderRequestSpec::new(provider, config.max_tokens.max(256));
    if !tools.is_empty() {
        spec = spec.with_tools(tools);
    }
    let effort = config.reasoning_effort.trim();
    if !effort.is_empty() {
        spec = spec.with_effort(effort.to_string());
    }
    spec
}

/// Tool execution policy for the given permission tier and sandbox role.
pub fn tool_policy(
    workspace_root: impl Into<PathBuf>,
    permission_level: u8,
    sandbox: qaqh_workspace::tool_api::SandboxMode,
    trusted_dirs: HashSet<PathBuf>,
) -> ToolPolicySnapshot {
    ToolPolicySnapshot {
        workspace_root: workspace_root.into(),
        mode: qaqh_workspace::tool_api::AgentMode::Code,
        permission_level: qaqh_workspace::PermissionLevel::from_u8(permission_level),
        sandbox,
        trusted_dirs,
    }
}

/// Per-session context budget. Sized for real multi-round tool work: the old
/// fixed 64-message ceiling turned every long conversation into a hard stop.
pub fn context_budget(config: &qaqh_config::Config) -> ContextBudget {
    let _ = config;
    default_context_budget()
}

/// Budget used by every writer of one session directory. Readers and writers
/// must agree, or a reopen would refuse a history the writer accepted.
pub fn default_context_budget() -> ContextBudget {
    ContextBudget {
        max_messages: 4096,
        max_payload_bytes: 64 * 1024 * 1024,
        max_checkpoint_bytes: 16 * 1024 * 1024,
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

/// Everything an in-process `SessionAgent` needs before it can be spawned.
///
/// 唯一装配入口：主会话与子代理只在这份输入上分叉，装配本身不再各写一遍。
/// （此前 `registry::spawn` 的 `spawn_session_inprocess` 与
/// `spawn_subagent_inprocess` 逐行重复同一段装配，只有 allowlist 与 sandbox
/// 角色不同。）
pub struct PreparedSession {
    /// Journal writer for this session (single writer per directory).
    pub commit: SessionCommit,
    /// Provider request identity (wire / model / effort / tool catalog).
    pub provider: ProviderRequestSpec,
    /// The per-session tool manager the port dispatches through.
    pub tool_manager: Arc<Mutex<qaqh_workspace::ToolManager>>,
    /// Tool execution port handed to the agent.
    pub port: Arc<WorkspaceToolPort>,
}

/// Inputs that legitimately differ between a root session and a subagent.
pub struct SpawnInput<'a> {
    pub session_id: &'a str,
    pub session_dir: &'a Path,
    /// Effective config for this session (already merged with subagent overrides).
    pub config: &'a qaqh_config::Config,
    /// Model-visible/callable tool names. Empty = no restriction
    /// (that is `ToolManager::set_allowed(vec![])` semantics, not "no tools").
    pub allowed_tools: Vec<String>,
    pub permission_level: u8,
    pub sandbox: qaqh_workspace::tool_api::SandboxMode,
    pub workspace_root: PathBuf,
}

impl<'a> SpawnInput<'a> {
    /// Root session: allowlist and permission tier come from the effective config.
    pub fn for_session(
        session_id: &'a str,
        session_dir: &'a Path,
        config: &'a qaqh_config::Config,
        workspace_root: PathBuf,
    ) -> Self {
        let allowed_tools = if config.tool_allowlist_enabled {
            config.tool_allowlist.clone()
        } else {
            Vec::new()
        };
        Self {
            session_id,
            session_dir,
            config,
            allowed_tools,
            permission_level: config.permission_level,
            sandbox: qaqh_workspace::tool_api::SandboxMode::Main,
            workspace_root,
        }
    }
}

/// Assemble one session: identity → journal writer → tool manager → provider.
///
/// Ordering matters: the identity sidecar must exist before the commit is
/// opened, and the tool manager must be initialised before the provider spec is
/// derived from `filtered_defs()` — otherwise the model is told about a tool
/// catalog the allowlist never approved.
pub fn assemble(input: SpawnInput<'_>) -> Result<PreparedSession, String> {
    let SpawnInput {
        session_id,
        session_dir,
        config,
        allowed_tools,
        permission_level,
        sandbox,
        workspace_root,
    } = input;

    let identity = CanonicalSessionIdentity::open_or_create(session_dir)
        .map_err(|error| format!("canonical identity for {session_id}: {error}"))?;
    let budget = default_context_budget();
    let commit = if session_dir.join("storage-format").exists() {
        SessionCommit::open(
            session_dir,
            identity.session_id.clone(),
            identity.log_id.clone(),
            budget,
        )
        .map_err(|error| format!("open session commit for {session_id}: {error}"))?
    } else {
        SessionCommit::create(
            session_dir,
            identity.session_id.clone(),
            identity.log_id.clone(),
            budget,
        )
        .map_err(|error| format!("create session commit for {session_id}: {error}"))?
    };

    let mut tool_manager = qaqh_workspace::registration::build_tool_manager(&[]);
    tool_manager.apply_init(allowed_tools);
    let provider = provider_spec(config, tool_defs(&tool_manager));
    let policy = tool_policy(workspace_root, permission_level, sandbox, HashSet::new());

    let tool_manager = Arc::new(Mutex::new(tool_manager));
    let port = Arc::new(WorkspaceToolPort::new(
        session_id,
        Arc::clone(&tool_manager),
        policy,
        None,
    ));
    Ok(PreparedSession {
        commit,
        provider,
        tool_manager,
        port,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_session_takes_the_allowlist_only_when_it_is_enabled() {
        let mut config = qaqh_config::Config::default();
        config.permission_level = 2;
        config.tool_allowlist = vec!["read".into(), "exec".into()];

        config.tool_allowlist_enabled = true;
        let restricted =
            SpawnInput::for_session("s", Path::new("/tmp/s"), &config, PathBuf::from("/w"));
        assert_eq!(
            restricted.allowed_tools,
            vec!["read".to_string(), "exec".to_string()]
        );
        assert_eq!(restricted.permission_level, 2);

        // Disabled allowlist = 空名单 = 不限制（不是「一个工具都不给」）。
        config.tool_allowlist_enabled = false;
        let open = SpawnInput::for_session("s", Path::new("/tmp/s"), &config, PathBuf::from("/w"));
        assert!(open.allowed_tools.is_empty());
    }
}
