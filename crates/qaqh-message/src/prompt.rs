//! Deterministic rendering of the system prompt from an explicit environment snapshot.

const DEFAULT_PROMPT: &str = include_str!("prompts/backend_prompt.md");
const OS_ENV_TEMPLATE: &str = include_str!("prompts/os_env.md");

/// The embedded product identity and operating instructions.
pub fn system_identity() -> &'static str {
    DEFAULT_PROMPT
}

/// Inputs must be frozen by the caller for the lifetime of the context prefix.
#[derive(Debug, Clone, Copy)]
pub struct EnvironmentSnapshot<'a> {
    pub os: &'a str,
    pub shells: &'a str,
    pub toolchains: &'a str,
}

/// Render without probing the environment or reading mutable global state.
pub fn render_system_prompt(environment: EnvironmentSnapshot<'_>) -> String {
    let env_block = OS_ENV_TEMPLATE
        .replace("{{OS}}", environment.os)
        .replace("{{SHELLS}}", environment.shells)
        .replace("{{TOOLS}}", environment.toolchains);
    format!("{}\n\n{}", DEFAULT_PROMPT, env_block)
}
