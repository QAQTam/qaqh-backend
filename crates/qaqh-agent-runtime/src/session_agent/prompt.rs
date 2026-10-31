//! Production prompt and provider assembly for the thin session agent.
//!
//! The thin owner commits its prefix explicitly: the stable inputs are
//! materialized here as one durable `FrozenPrefix` message, which is then
//! reused byte-for-byte as the cacheable request prefix. (The legacy loop
//! rebuilt the prompt from `AgentState` every turn; that path is gone.)

use std::path::Path;

use crate::provider_job::ProviderRequestSpec;

/// Maximum bytes of project instructions injected into the frozen prefix.
const MAX_AGENTS_BYTES: usize = 16 * 1024;

/// Build the deterministic system prompt for one session.
///
/// It only contains stable inputs (workspace, model, declared tools, project
/// instructions). Per-turn state must not appear here: the message is committed
/// once and reused as the cacheable request prefix.
pub fn system_prompt(spec: &ProviderRequestSpec, cwd: &Path) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "You are QAQ, an autonomous coding agent operating on the user's workspace.\n\
         Work through the available tools. Prefer small, verifiable steps; report \
         what you actually observed and never claim work you did not perform.\n",
    );
    prompt.push_str(&format!("\n[Environment]\nworkspace: {}\n", cwd.display()));
    prompt.push_str(&format!("model: {}\n", spec.provider.model));
    if let Some(tools) = &spec.tools {
        let names: Vec<&str> = tools
            .iter()
            .map(|tool| tool.function.name.as_str())
            .collect();
        prompt.push_str(&format!("tools: {}\n", names.join(", ")));
    }
    if let Some(agents) = read_agents_instructions(cwd) {
        prompt.push_str("\n[Project instructions]\n");
        prompt.push_str(&agents);
        prompt.push('\n');
    }
    prompt
}

fn read_agents_instructions(cwd: &Path) -> Option<String> {
    let path = cwd.join("AGENTS.md");
    let bytes = std::fs::read(&path).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let end = bytes.len().min(MAX_AGENTS_BYTES);
    let mut end = end;
    while end > 0 && std::str::from_utf8(&bytes[..end]).is_err() {
        end -= 1;
    }
    String::from_utf8(bytes[..end].to_vec()).ok()
}
