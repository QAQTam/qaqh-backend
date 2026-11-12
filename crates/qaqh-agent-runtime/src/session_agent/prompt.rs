//! Deterministic system prompt rebuilt at each turn from current configuration.

use std::path::Path;

use crate::provider_job::ProviderRequestSpec;

/// Maximum bytes of project instructions in the system prompt.
const MAX_AGENTS_BYTES: usize = 16 * 1024;

/// Build the deterministic system prompt for one session.
///
/// Contains workspace, model, declared tools and current project instructions.
pub fn system_prompt(spec: &ProviderRequestSpec, cwd: &Path, session_id: &str) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "You are QAQ, an autonomous coding agent operating on the user's workspace.\n\
         Work through the available tools. Prefer small, verifiable steps; report \
         what you actually observed and never claim work you did not perform.\n",
    );
    prompt.push_str(&format!("\n[Environment]\nworkspace: {}\n", cwd.display()));
    prompt.push_str(&format!(
        "widgets_dir: {}\n",
        cwd.join(".qaqh").join("widgets").join(session_id).display()
    ));
    prompt.push_str(&format!("model: {}\n", spec.provider.model));
    if let Some(tools) = &spec.tools {
        let names: Vec<&str> = tools
            .iter()
            .map(|tool| tool.function.name.as_str())
            .collect();
        prompt.push_str(&format!("tools: {}\n", names.join(", ")));
    }
    // 技能目录 + 嵌入 bundled 正文：会话初始化注入（system 段），每回合
    // 由确定性 prompt 重建携带；模型经 skill_list 列资源、read 直读文件。
    prompt.push_str(&qaqh_skills::render_system_section(cwd));
    // 交接是控制面能力，与 allowlist 无关，所以这段说明无条件在场。
    prompt.push_str(
        "\n[Context handoff]\n\
         工具结果尾部可能带 `[#ctx 61% · 98k/160k]`，那是 provider 实测回传的实时占用。\n\
         占用接近上限、或你判断历史已不易自持时，**必须**调用 `handoff` 写交接文档：它会终止\n\
         本会话，并由客户端拿这份文档开新会话继续推进。\n\
         口头总结不算交接（新会话读不到它）；不要拖到请求被拒才动。\n",
    );
    // ADR-0014 切片F：团队协作策略——lead 与子级同一份静态文本（稳定前缀
    // 保 KV cache），与协作工具声明同进同退。借鉴 dsh team:policy，适配本仓
    // 工具名（claim_epoch CAS；无 task_get，task_list 直返全量）。writeScopes
    // 结构化字段属 v3.1 wire 实变，落地前以标题/验收 criteria 承载 scope。
    let has_team_tools = spec
        .tools
        .as_ref()
        .is_some_and(|list| list.iter().any(|tool| tool.function.name == "task_create"));
    if has_team_tools {
        prompt.push_str(
            "\n[Team policy]\n\
             Split write work into disjoint scopes; record each task's expected file scope in its \
             title or acceptance criteria, and use task dependencies (task_update add_dependency) \
             when work must be ordered. Write-scope overlap is advisory, not a lock. Shared-task \
             workflow is task_list, task_claim with the current claim_epoch (compare-and-set: a \
             stale claim fails, re-read and retry), perform the work, then task_close complete. \
             Task readiness never starts an owner. The root agent must wait_agent for required \
             teammates before giving the final answer. Task changes do not end wait_agent calls, \
             add inbox messages, or start idle agents. Use send_message when another agent needs \
             to act on a task.\n",
        );
    }
    // 命名指令与工具声明同进同退：用户命名或子代理会话都不出现；auto 标题
    // 保留修订能力，指令随之常驻。
    let can_name_session = spec
        .tools
        .as_ref()
        .is_some_and(|list| list.iter().any(|tool| tool.function.name == crate::session_title::TOOL_NAME));
    if can_name_session {
        prompt.push_str(
            "\n[Session title]\n\
             明确任务后，请调用 `session_title` 根据用户需求给会话命名（名词短语、≤20 字、\n\
             写意图不写结论）；任务需求变化时可重新生成标题。命名成功即落库，客户端会话\n\
             列表实时可见；不调用不算错误，列表会回退到目录名。\n",
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_with_collaboration_tools() -> ProviderRequestSpec {
        let manager = qaqh_workspace::registration::build_tool_manager(&[
            qaqh_subagent::register_collaboration_tools,
        ]);
        let mut spec = ProviderRequestSpec::new(
            qaqh_gate::ProviderConfig::responses("https://example.com", "", "m", None),
            1024,
        );
        spec.tools = Some(manager.all_defs());
        spec
    }

    /// 切片F 闸门：协作工具在场 → policy 在场；工具缺席 → policy 缺席。
    /// lead 与子级拿到逐字节相同的 policy 段（静态文本，无会话插值）。
    #[test]
    fn team_policy_rides_with_the_collaboration_tool_surface() {
        let cwd = std::env::temp_dir();
        let with_tools = system_prompt(&spec_with_collaboration_tools(), &cwd, "s1");
        assert!(with_tools.contains("[Team policy]"));
        assert!(with_tools.contains("task_claim with the current claim_epoch"));

        let bare = ProviderRequestSpec::new(
            qaqh_gate::ProviderConfig::responses("https://example.com", "", "m", None),
            1024,
        );
        let without_tools = system_prompt(&bare, &cwd, "s2");
        assert!(!without_tools.contains("[Team policy]"));

        // 稳定前缀：两个会话的 policy 段逐字节一致。
        let other = system_prompt(&spec_with_collaboration_tools(), &cwd, "s2");
        let extract = |prompt: &str| {
            prompt
                .split("[Team policy]")
                .nth(1)
                .and_then(|rest| rest.split("\n\n").next())
                .unwrap_or_default()
                .to_string()
        };
        assert_eq!(extract(&with_tools), extract(&other));
    }
}
