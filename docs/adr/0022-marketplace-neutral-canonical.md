# ADR-0022: 插件市场中立正典与双宿主投影

状态: 已采纳（已实施） · 日期: 2026-11-15 · 关联: ADR-0008（中立 IR 先例）

## 背景

QAQ-Harness 需要一个 codex + claudecode 双宿主兼容的插件市场。调研结论（两仓库源码核实）：

- Codex（`core-plugins/src/marketplace.rs:20`）清单查找序：`.agents/plugins/marketplace.json` → `.agents/plugins/api_marketplace.json` → `.claude-plugin/marketplace.json` → `.cursor-plugin/marketplace.json`；插件 manifest 查找 `plugin.json` → `.claude-plugin/plugin.json` → `.cursor-plugin/plugin.json`（`utils/plugins/src/plugin_namespace.rs:43`）。
- Claude Code / occ 只认 `.claude-plugin/*`。
- 共同消费面：`skills/<n>/SKILL.md`、`hooks/hooks.json`（同事件名）、`.mcp.json`。Codex 会把 Claude 风格 `commands/*.md` 迁移为 skill（要求 frontmatter `description`）；`agents/` 仅 Claude 消费。

## 决策

1. **D-A（正典格式）**：市场仓库唯一清单 = `.agents/plugins/marketplace.json`（中立命名空间，与 `qaqh-skills` 的 `.agents/skills` 同族）。选它而非事实标准 `.claude-plugin/marketplace.json` 的理由：三宿主同权、Codex 查找序第一优先、本仓已有中立路径先例；代价是 occ 侧需在 `marketplaceManager.ts` 候选路径里加一项（约 2 行，属 occ 仓配套项）。
2. **D-B（分发形态）**：git 仓库即市场（`marketplace add <url|local>`）。Codex 官方 curated 市场同为 git 形态（`loader.rs:510` 的 `api_marketplace.json`），无 HTTP 索引 API；本仓 v1 不自建索引服务。
3. **wire 形状 = 双宿主真交集**：条目 source 仅 `path`（市场内相对路径，`./` 前缀合法）/ `{"source":"url"}` / `{"source":"npm"}`。Claude 的 `github`/`git` tag 与 Codex 的 `local`/`git-subdir` tag 不在交集，出现在正典清单里解析即拒——不做形态归一、不写转换层（宪法 4/9）。
4. **安装投影面**：skills → 三宿主用户级发现根（`~/.qaqh/skills`、`~/.codex/skills`、`~/.claude/skills`）；`.mcp.json` 仅 Qaqh 目标导入（复用 `mcp_import` D4 审批链 + secrets 占位符化，`ExternalSource::Marketplace` 新变体）。codex/claude 目标**不代写宿主配置文件**——外部宿主的 MCP 走其原生 marketplace 机制。`agents/`/`commands/`/`hooks/` 不投影（本仓无消费面）。
5. **接线面**：`qaqh-marketplace` 新 crate（manifest/registry/installer + 安装账本）+ daemon `CONTROL_PLANE_ROUTES` 七端点（admin 全权，不进 Ringing 契约面，无 [contract-refreeze] 义务）+ CLI `qaqh-daemon marketplace add|list|remove|update|install|installed|uninstall`。install/uninstall 默认 dry-run，`--exec` 执行；CLI 面市场 MCP server 逐个确认（供应链面），HTTP 面 admin token 即审批。
6. **安装-管理-卸载闭环**：安装账本 `~/.qaqh/marketplaces/installed.toml`（`[[installed]]`：plugin/marketplace/target/skills/mcp_servers/secret_keys）是卸载唯一事实源——`execute` 成功后追加实际写入项，`uninstall_execute` 反向销毁（skills 目录、`[mcp.servers]` 条目、secrets 键，均幂等）后删记录。前端消费经 `qaqh-client` 市场方法组（`marketplace_list/installed/add/install/uninstall/remove`，wire 直接透传控制面 JSON，client 层不二次建模），走 admin token 控制面路径（同 `/control/v1/stop` 模式），不进 Ringing 契约。

## 否决方案

- `.claude-plugin/marketplace.json` 为正典：Claude 中心主义，occ 零改动但中立性让位；如 occ 配套补丁到位，两种清单都能被三宿主读，但正典仍取中立路径。
- 写 Claude↔Codex 清单转换器：宪法 4/9 明令禁止的兼容套娃，否决。
- 安装器代写 `~/.codex/config.toml` / `~/.claude.json`：外部宿主配置是纯破坏面，否决。

## 验证

`cargo test -p qaqh-marketplace`（9 unit + 1 e2e 全流程）、`cargo test -p qaqh-config`（40 通过，`ExternalSource::Marketplace` 无回归）、`cargo check -p qaqh-daemon` 零 error。
