# CNB A/B Agent 消息桥

本文件是 `tools/cnb-mcp-enhance` 中 Agent 聊天能力的快速入口。目标是让工程师 A 与工程师 B 通过 CNB issue 评论进行可等待、可回复、可审计的沟通。

## 快速开始

根目录提供无依赖启动器：

```bash
./cnb-chat --help
```

读取工程师 B 的最新消息：

```bash
./cnb-chat --chat-read \
  --issue 100 \
  --author AnyBuddy \
  --limit 10
```

监听 PR 评论时把 `--issue 100` 换成 `--pr 102`：

```bash
./cnb-chat --chat-read --pr 102 --author AnyBuddy --limit 10
```

等待一条新消息，超时后退出：

```bash
./cnb-chat --chat-wait \
  --issue 100 \
  --author AnyBuddy \
  --timeout-ms 30000 \
  --interval-ms 3000 \
  --json
```

常驻监听，只在收到新消息时输出：

```bash
./cnb-chat --chat-listen \
  --issue 100 \
  --author AnyBuddy \
  --timeout-ms 30000 \
  --interval-ms 3000 \
  --json
```

直接回复：

```bash
./cnb-chat --chat-send \
  --issue 100 \
  --body '[ACK] 已收到，按该方案执行。'
```

长正文建议使用文件，避免 shell 引号问题：

```bash
./cnb-chat --chat-send --issue 100 --body-file /tmp/reply.md
```

## 行为约定

- `--issue N`：操作 issue 评论。
- `--pr N`：操作 PR 评论。
- `--chat-read`：返回最新评论；传 `--after-id` 后只返回更新的评论。
- `--chat-wait`：首次调用以当前最新评论为游标，只等待之后的新消息；无消息时返回 `timed_out=true`。
- `--chat-listen`：持续等待并逐条输出新消息；静默超时不会刷屏。
- `--chat-send`：显式发送评论，正文为空时拒绝调用。
- `--author AnyBuddy`：只关注工程师 B 的消息。
- 评论 ID 始终按字符串处理，避免 JavaScript 大整数精度丢失。
- 默认单条正文最多输出 12000 字符，可用 `--max-chars` 调整。
- 默认轮询间隔 3000ms，最小 1000ms，避免高频请求。
- 评论分页会连续读取直到 `total`，默认上限 1000 页，并返回 `pages/total/truncated`。
- 等待路径按 ID 升序 drain，单轮最多返回 100 条，避免批次过大时跳过消息。

确定性回归测试：

```bash
node tools/cnb-mcp-enhance/chat-selftest.mjs
```

## MCP 工具

`tools/cnb-mcp-enhance/server.mjs` 同时提供：

- `cnb_chat_read`
- `cnb_chat_wait`
- `cnb_chat_send`

MCP 客户端配置：

```jsonc
{
  "mcpServers": {
    "cnb-enhance": {
      "command": "node",
      "args": ["/absolute/path/to/qaqh-backend/tools/cnb-mcp-enhance/server.mjs"],
      "env": {
        "CNB_REPO": "QAQ-Harness/qaqh-backend"
      }
    }
  }
}
```

token 复用 `~/.cnb/token`，不写入仓库，也不在输出中回显。

## 推荐协同循环

1. 在独立 PTY 会话启动 `--chat-listen --author AnyBuddy`；评审阶段使用 `--pr N`，任务讨论使用 `--issue N`。
2. 收到新评论后读取 ID、正文和状态。
3. 使用 `--chat-send` 明确回复，保持 `[ACK]/[WIP]/[BLOCKED]/[READY]` 前缀。
4. 需求与决策留在 issue，代码细节留在 PR。
5. 任务实现仍遵守一 issue、一分支、一 worktree、一 PR。

当前协同基线见 issue #100；消息桥实现任务见 issue #101。
