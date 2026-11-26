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
    prompt.push_str(
        "\n[Output formatting]\n\
         When referencing files or URLs in your reply, use standard Markdown links:\n\
         - Workspace files: `[basename](relative/path)` or `[basename](relative/path#L10-L20)` using forward slashes.\n\
         - Files outside the workspace: `[basename](file:///absolute/path)` (forward slashes).\n\
         - Web pages / repositories: standard Markdown link or bare `https://...` URL.\n",
    );
    prompt.push_str(&format!("\n[Environment]\nworkspace: {}\n", cwd.display()));
    // widgets_dir 以正斜杠形态注入（Windows 亦然）：marker 的 JSON payload 里
    // 路径零反斜杠转义，模型可原样复制（widget-directive §5 合规最大故障源）。
    prompt.push_str(&format!(
        "widgets_dir: {}\n",
        cwd.join(".qaqh")
            .join("widgets")
            .join(session_id)
            .to_string_lossy()
            .replace('\\', "/")
    ));
    prompt.push_str(&format!("model: {}\n", spec.provider.model));
    if let Some(threshold_pct) = crate::handoff::threshold_pct(spec.auto_compact_threshold) {
        prompt.push_str(&format!("context_fold_threshold: {threshold_pct}%\n"));
    }
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
    // ADR-0033：团队协作策略——lead 与同事同一份静态文本（稳定前缀保 KV
    // cache），与协作工具声明同进同退。语义对齐 dsh team:policy 及其
    // inbox-only-agent-wait 决策：任务变更永不通知、需要行动就发明确消息；
    // 完成答复经终态合流自动到达；同事一律按 session id 寻址。
    let has_team_tools = spec
        .tools
        .as_ref()
        .is_some_and(|list| list.iter().any(|tool| tool.function.name == "task_create"));
    if has_team_tools {
        prompt.push_str(
            "\n[Team policy]\n\
             Colleagues are conversations you start with spawn_subagent; address them by \
             session id (send_message, wait_agent, interrupt_agent). Split write work into \
             disjoint scopes; record each task's expected file scope in its title or \
             acceptance criteria, and use task dependencies (task_update add_dependency) \
             when work must be ordered. Write-scope overlap is advisory, not a lock. \
             Shared-task workflow is task_list, task_claim with the current claim_epoch \
             (compare-and-set: a stale claim fails, re-read and retry), perform the work, \
             then task_close complete. Task readiness never starts an owner. Task changes \
             never notify anyone: when a teammate must act on a task, send them a message; \
             completion answers arrive automatically when their turn ends. The root agent \
             must wait_agent for required teammates (by session id) before giving the \
             final answer.\n",
        );
    }
    // 命名指令与工具声明同进同退：用户命名或子代理会话都不出现；auto 标题
    // 保留修订能力，指令随之常驻。
    let can_name_session = spec.tools.as_ref().is_some_and(|list| {
        list.iter()
            .any(|tool| tool.function.name == crate::session_title::TOOL_NAME)
    });
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

    /// 注册冒烟桩（宿主经注册期注入，v4.5 P2-1b；schema/prompt 面断言不触达执行）。
    struct StubTaskHost;
    impl qaqh_subagent::TaskBoardHost for StubTaskHost {
        fn task_create(
            &self,
            _r: qaqh_subagent::TaskCreateRequest<'_>,
        ) -> Result<qaqh_subagent::TaskBoardTask, String> {
            Err("stub".to_string())
        }
        fn task_claim(
            &self,
            _r: qaqh_subagent::TaskClaimRequest<'_>,
        ) -> Result<qaqh_subagent::TaskBoardTask, String> {
            Err("stub".to_string())
        }
        fn task_update(
            &self,
            _r: qaqh_subagent::TaskUpdateRequest<'_>,
        ) -> Result<qaqh_subagent::TaskBoardTask, String> {
            Err("stub".to_string())
        }
        fn task_close(
            &self,
            _r: qaqh_subagent::TaskCloseRequest<'_>,
        ) -> Result<qaqh_subagent::TaskBoardTask, String> {
            Err("stub".to_string())
        }
        fn task_list(
            &self,
            _r: qaqh_subagent::TaskListRequest<'_>,
        ) -> Result<qaqh_subagent::TaskListPage, String> {
            Err("stub".to_string())
        }
    }

    fn spec_with_collaboration_tools() -> ProviderRequestSpec {
        let manager = qaqh_workspace::registration::build_tool_manager(
            &[std::sync::Arc::new(|mgr: &mut qaqh_workspace::ToolManager| {
                qaqh_subagent::register_collaboration_tools(
                    mgr,
                    std::sync::Arc::new(StubTaskHost),
                );
            })],
            std::sync::Arc::new(qaqh_workspace::todo::MemoryTodoHost::default()),
        );
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

    #[test]
    fn output_formatting_section_is_present_and_static() {
        let cwd = std::env::temp_dir();
        let spec = ProviderRequestSpec::new(
            qaqh_gate::ProviderConfig::responses("https://example.com", "", "m", None),
            1024,
        );
        let p1 = system_prompt(&spec, &cwd, "s1");
        let p2 = system_prompt(&spec, &cwd, "s2");
        assert!(p1.contains("[Output formatting]"));
        assert!(p1.contains("file:///absolute/path"));
        let extract = |prompt: &str| {
            prompt
                .split("[Output formatting]")
                .nth(1)
                .and_then(|rest| rest.split("\n\n").next())
                .unwrap_or_default()
                .to_string()
        };
        assert_eq!(extract(&p1), extract(&p2));
    }

    #[test]
    fn environment_section_reports_fold_threshold_only_when_enabled() {
        let cwd = std::env::temp_dir();
        let disabled = ProviderRequestSpec::new(
            qaqh_gate::ProviderConfig::responses("https://example.com", "", "m", None),
            1024,
        );
        assert!(!system_prompt(&disabled, &cwd, "s1").contains("context_fold_threshold:"));

        let enabled = disabled.with_context(200_000, 0.75);
        assert!(system_prompt(&enabled, &cwd, "s1").contains("context_fold_threshold: 75%\n"));
    }
}
