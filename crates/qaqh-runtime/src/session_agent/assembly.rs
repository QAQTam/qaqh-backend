//! Production assembly for the thin session owner: provider request spec and
//! the tool policy snapshot are derived from the effective session config
//! instead of being hard-coded at the spawn site.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use qaqh_session::context_projection::ContextBudget;
use qaqh_types::ThinkingParamMode;

use crate::provider_job::ProviderRequestSpec;
use crate::tool_job::ToolPolicySnapshot;

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
    cwd.map(PathBuf::from).unwrap_or_else(|| fallback.to_path_buf())
}
