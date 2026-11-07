use qaqh_workspace::tool_api::{
    AgentMode, CancellationToken, SandboxMode, SandboxSpec, ToolCallContext, ToolCallSource,
};

pub fn context(workspace: impl Into<std::path::PathBuf>) -> ToolCallContext {
    ToolCallContext {
        call_id: "scope-test".into(),
        session_id: "scope-session".into(),
        workspace_root: workspace.into(),
        mode: AgentMode::Code,
        permission_level: qaqh_workspace::PermissionLevel::from_u8(3),
        sandbox: SandboxMode::Main,
        sandbox_spec: SandboxSpec::disabled(),
        exec_default_shell: None,
        timeout: std::time::Duration::ZERO,
        cancellation: CancellationToken::new(),
        progress: None,
        exec_progress: None,
        exec_host: None,
        official_web: None,
        source: ToolCallSource::Model,
    }
}
