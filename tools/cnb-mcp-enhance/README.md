# cnb-mcp-enhance — CNB MCP 官方服务器的 QAQ-Harness 增强层

> 结论先行：**官方已提供 [`@cnbcool/mcp-server`](https://cnb.cool/cnb/tools/cnb-mcp-server)（开源，v0.6.7+），
> 覆盖 50+ 工具（issues/pulls/build/workspaces/知识库）。不要重复造轮子。**
> 本目录只做官方缺的 4 件事，以「随装随用的 stdio MCP server」形式提供。

## 官方覆盖 vs 本仓增强

| 能力 | 官方 @cnbcool/mcp-server | 本仓 enhance |
|---|---|---|
| issues 增删改查 + 评论 + 标签（含 `state_reason` 关单） | ✅ | — |
| PR 增删改查 + 合并 + 评论 | ✅ | — |
| build start/status/stage/logs/stop | ✅（startBuild 无 npc 参数） | — |
| `cnb_startBuild` 带 **npc.workMode**（等价 UI 勾「替我上班」） | ❌ | ✅ `cnb_npc_dispatch` |
| **npc-observability**（actions/commits/prs） | ❌（swagger 有端点，官方未封装） | ✅ 3 个工具 |
| **watch**：按 issue 列表聚合轮询构建/PR 状态，一次调用看全景 | ❌ | ✅ `cnb_watch_wave` |
| 构建失败自动拉 stage 日志尾部（省一次失败重查） | ❌ | ✅（watch 内置） |
| 传输 | stdio / streamable | stdio |

## 工具清单（本仓 enhance 提供 5 个）

| 工具 | 说明 |
|---|---|
| `cnb_npc_dispatch` | 触发 `api_trigger_wm` 流水线跑 NPC 任务（内置 QAQ-Harness 铁律 systemPrompt；userPrompt 必填）。等价 CLI `build start-build --event api_trigger_wm`，token 为可信 scope（repo-code:rw），**无需 UI 勾「替我上班」** |
| `cnb_npc_observations` | 查 npc-observability：`kind=actions/commits/prs` + 时间窗，看 NPC 接单/提交/交付记录 |
| `cnb_watch_wave` | **核心监视器**：传入 `{ "issues": [6,7,8], "repo": "QAQ-Harness/qaqh-backend" }`，聚合返回每个 issue 关联构建的最新状态（pending/running/success/error）+ 最新 NPC 评论要点 + 已开 PR 编号。一次调用看全部 |
| `cnb_build_tail` | 构建失败时拉 npc go stage 日志尾部 N 行（默认 40），附失败行高亮摘要 |
| `cnb_issue_close` | 关单一步到位：state=closed + state_reason=completed（官方 update_issue 两个参数都支持，但组合语义高频，故单独封装） |

## 安装（QAQ-Harness 仓库内零依赖）

```jsonc
// .mcp.json / claude_desktop_config.json / 任何 MCP 客户端
{
  "mcpServers": {
    "cnb": {
      // 官方全量工具（50+）
      "command": "npx",
      "args": ["-y", "-p", "@cnbcool/mcp-server", "cnb-mcp-stdio"],
      "env": { "API_TOKEN": "<your-token>" }
    },
    "cnb-enhance": {
      // 本仓增强 5 工具
      "command": "node",
      "args": ["D:/project/qaqh-backend/tools/cnb-mcp-enhance/server.mjs"],
      "env": { "CNB_TOKEN": "<your-token>", "CNB_REPO": "QAQ-Harness/qaqh-backend" }
    }
  }
}
```

两个 server 并存，`cnb_*` 前缀区分。官方管 CRUD，enhance 管 NPC 编排与监视。

## 独立监视器模式（不经 MCP 也能用）

```powershell
# 直接命令行跑 watch（输出人读格式；--json 输出机器格式）
node tools/cnb-mcp-enhance/server.mjs --watch --issues 6,7,8,10,12,13 --json > wave3.json
# 或指定 SN 清单
node tools/cnb-mcp-enhance/server.mjs --watch --builds cnb-90o-xxx,cnb-69o-xxx
```

## 安全边界

- enhance 层默认**只读**；`cnb_npc_dispatch` 是唯一写操作（触发流水线），MCP 客户端侧应保留确认门
- token 从 `CNB_TOKEN` env 读，不落盘；日志不回显 token
- 与官方 server 一致：只操作 `CNB_REPO` 默认仓库，跨仓需显式传参

## 为什么不 fork 官方仓补工具

官方仓由 CNB 团队维护（swagger 生成，API 全、更新快），fork 后要跟着上游同步。
enhance 层只依赖其 HTTP 端点（swagger.json），官方加新工具我们零成本继承。
