//! Deterministic system prompt rebuilt at each turn from current configuration.

use std::path::Path;

use crate::provider_job::ProviderRequestSpec;

/// Maximum bytes of project instructions in the system prompt.
const MAX_AGENTS_BYTES: usize = 16 * 1024;

/// Build the deterministic system prompt for one session.
///
/// Contains workspace, model, declared tools and current project instructions.
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
    // 交接是控制面能力，与 allowlist 无关，所以这段说明无条件在场。
    prompt.push_str(
        "\n[Context handoff]\n\
         工具结果尾部可能带 `[#ctx 61% · 98k/160k]`，那是 provider 实测回传的实时占用。\n\
         占用接近上限、或你判断历史已不易自持时，**必须**调用 `handoff` 写交接文档：它会终止\n\
         本会话，并由客户端拿这份文档开新会话继续推进。\n\
         口头总结不算交接（新会话读不到它）；不要拖到请求被拒才动。\n",
    );
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
