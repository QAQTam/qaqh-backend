# Agent Hooks 调研与设计（ZCode / Codex / Grok Build → qaqh-backend）

> 2026-11-22。开工前调研（宪法规则 6）。素材来自三家开源仓的浅克隆快照
> `.qaqh/tmp/repos/{Zcode,codex,grok-build}`（只读参考，不入库）。
> 本文只做分析与设计，不改任何代码、不动契约面。

## 1. 调研来源

| 项目 | 形态 | hook 核心位置 |
|---|---|---|
| zai-org/Zcode | TS monorepo（开源仓只含模型/UI/trust 面，执行在闭源 runtime） | `packages/shared/src/hooks.ts`、`workspace-hook-config.ts`、`workspace-hook-trust-store-file.ts`、`packages/services/src/hooks/hooksService.ts`、UI `HookForm.tsx` |
| openai/codex | Rust | `codex-rs/hooks/`（types/schema/registry/engine/events）、`core/src/hook_runtime.rs`、`config/src/hook_config.rs`、`protocol/src/hooks.rs`、app-server-protocol v2 schema |
| xai-org/grok-build | Rust | `crates/codegen/xai-grok-hooks/`（config/discovery/dispatcher/matcher/runner/trust，约 1.3 万行）、shell 集成 `xai-grok-shell/src/session/…hooks.rs`、用户文档 `user-guide/10-hooks.md`（60KB，语义最完整） |

三家事件命名同源（Claude Code 系），互相兼容是明设计目标：ZCode 直接读
`.claude/settings.json` 与 `.agents/`；Grok 读 `~/.claude`、`~/.cursor/hooks.json`
并做事件名/工具名映射；Codex 输出 JSON 语义与 Claude 的
`continue/stopReason/suppressOutput/systemMessage` 对齐。

## 2. 事件面对比

| 事件 | ZCode | Codex | Grok | 语义/权力 |
|---|:-:|:-:|:-:|---|
| SessionStart / SessionEnd | ✓ | ✓ | ✓ | 被动；Grok 带 source/reason、subagentType |
| UserPromptSubmit | ✓ | ✓ | ✓ | **门**：block 拒绝 prompt（reason 只给用户，不入模型上下文/不入历史） |
| PreToolUse | ✓ | ✓ | ✓ | **门**：allow/deny/ask/defer + `updatedInput` 改写 + `additionalContext` |
| PermissionRequest | ✓ | ✓(permissionRequest) | ✓(PermissionDenied 被动) | Codex：hook 可直接 Allow/Deny 预答权限请求 |
| PostToolUse | ✓ | ✓ | ✓ | 已跑完，不 block；feedback + 输出替换（`updatedToolOutput`） |
| PostToolUseFailure | ✓ | — | ✓ | 派发失败/MCP error 结果时代替 PostToolUse |
| Stop | ✓ | ✓ | ✓(+StopFailure/StopCancelled 拆分) | **门**：block 让 agent 续跑，reason 回模型 |
| SubagentStart / SubagentStop | — | ✓ | ✓ | 子代理生命周期；SubagentStop 同 Stop 门 |
| PreCompact / PostCompact | — | ✓ | ✓ | 压缩前后 |
| Notification | — | — | ✓ | 注意力事件（idle/permission prompt/任务完成） |
| Interrupt | — | ✓ | — | 中断事件 |
| PreModelRequest / PostModelRequest | — | ✓(独有) | — | **模型请求前后**：计量、费用 hook（HookCharge） |

要点：**事件自带"能做什么"的元数据**。Grok 用 `EventTraits{gate, matcher,
hub_forward}` 宏把 GateKind(Observe/Tool/Stop/PostTool/Prompt) 编进事件定义；
Codex 每个事件一个 outcome 类型（`PreToolUseOutput{block_reason,
updated_input, invalid_reason,…}`）。这避免"所有事件共享一个万能 decision"
的拧巴设计。

## 3. Handler 与配置格式

### 3.1 handler 类型

| 类型 | ZCode | Codex | Grok | 说明 |
|---|:-:|:-:|:-:|---|
| `command` | ✓(command/process) | ✓ | ✓ | 子进程，事件 JSON 走 stdin，决策走 stdout |
| `http` | — | — | ✓ | 整个 envelope POST 到 URL |
| `mcp_tool` | — | ✓ | — | handler 调用某个 MCP server 的 tool |
| `prompt` | — | ✓ | — | LLM 当裁判的 hook |
| `agent` | — | ✓ | — | 子代理当 hook |

### 3.2 通用字段（三家并集）

`matcher`（正则，按事件测工具名/通知类型/子代理类型…）、`timeout`（秒，
Grok 默认 5s、Stop 类 600s、Prompt 类 30s）、`env`（用户变量，不能覆盖保留
变量）、`async`（fire-and-forget）、`statusMessage`、`onFailure:
continue|block`（Codex 独有：失败策略可选从 fail-open 升级为 fail-closed）、
`additionalContextLimit`（Codex：超阈值把 context 落盘溢出，占位符带回模型）。

### 3.3 配置分层（Grok 最全，Codex 同构）

| 层 | Grok | Codex | 信任 |
|---|---|---|---|
| user | `~/.grok/config.toml`、`~/.grok/hooks/*.json` | `config.toml [hooks]` | 始终可信 |
| project | `<repo>/.grok/hooks/*.json` | project config | **需 trust** |
| plugin | 插件内置 | plugin hook source | per-plugin trust（内容 hash） |
| managed | `managed_config.toml`（$GROK_HOME、/etc/grok，可 server 同步） | managed 层 | 始终可信 |
| requirements | `requirements.toml`（签名缓存） | `requirements.toml` + 顶层 `allow_managed_hooks_only=true`（只认 managed，无视 user/project/session） | 组织强制 |

`allow_managed_hooks_only` 只在 `requirements.toml` 里生效（放 config.toml
无效）——"管理员开关只认管理面"是个值得抄的细节。

### 3.4 事件 JSON（stdin / POST body）

统一 envelope + per-event payload（Grok `HookEventEnvelope` + `HookPayload`
untagged enum；Codex `HookPayload{session_id, cwd, client, triggered_at,
hook_event}`）：

```json
{
  "hookEventName": "pre_tool_use",
  "hook_event_name": "PreToolUse",
  "sessionId": "…", "cwd": "…", "workspaceRoot": "…",
  "permissionMode": "default", "promptId": "…",
  "timestamp": "…", "transcriptPath": "…",
  "toolName": "run_terminal_command", "toolInput": { "command": "npm test" }
}
```

Grok 同时带 camelCase（原生）与 snake_case（Claude 兼容别名）两套键，payload
上限 128KB。保留 env 注入：`GROK_HOOK_EVENT/NAME`、`GROK_SESSION_ID`、
`GROK_WORKSPACE_ROOT`、`CLAUDE_PROJECT_DIR`（别名）——用户 `env` 试图覆盖保留
变量会在加载期剥掉并告警。

### 3.5 决策语义（Grok 文档最严，值得照抄）

- **exit 0 + stdout JSON** = 结构化决策；**exit 2** = 无条件 deny/block（stdout
  被忽略）；**其他非零** = fail-open（记录 scrollback，不拦）。
- JSON 与 exit code 冲突有确定裁决并告警：JSON `deny` 胜过非 0/2 的 exit code；
  exit 2 胜过 JSON `allow/ask/defer`。
- `PreToolUse`：`decision: allow|deny|ask|defer`（顶层或
  `hookSpecificOutput.permissionDecision`，前者胜）。`allow` 只是"没被 hook 拦"，
  不会绕过正常权限询问。`ask` 把调用升级到权限提示（压过 always-approve/saved
  grant/safe-command，但不压过 blanket-approve 的 YOLO 模式），**不能放宽**任何
  政策。`defer` = 没意见，且作废同 hook 的其它字段。`updatedInput` 必须 JSON
  object，多个 hook 时 last-wins 且互相不可见（跑完统一应用）；改写后过不了工具
  schema ⇒ 直接算 hook deny（不能安全执行就不回退原参数）；改写不能换工具名。
  非零 exit 保 deny 但丢 `updatedInput/additionalContext`。
- `additionalContext`：有序投递（不像 rewrite 的 last-wins），包 reminder 标签
  并署名 hook；deny 时丢弃。10K 字符截断。
- `PostToolUse`：`block` = "告诉模型出问题了"（不是停止）；`updatedToolOutput`
  替换模型可见输出（MCP 工具用 `updatedMCPToolOutput` 别名，内建工具忽略之）；
  替换 last-wins、替换丢失会留痕。
- `Stop`/`SubagentStop`：`block` + reason ⇒ agent 续跑（防死循环：payload 带
  `stopHookActive` 标记，续跑一轮后不再 block）。
- `UserPromptSubmit`：block 的 prompt **不入对话历史/不入盘**，只留在前端
  scrollback 与本地 up-arrow 历史；排队的 prompt 不自动跟跑。
- hook 子进程崩了/超时/输出畸形 = fail-open（除非 `onFailure: block`）。

### 3.6 进程执行细节（Grok `runner/command.rs`）

- 命令串含 shell 元字符 ⇒ unix `sh -c` / windows 配置 shell；纯路径 ⇒ 直接
  spawn，相对路径相对 hook 源文件目录解析。
- `${VAR}`/`$VAR` 展开，未解析的必需变量 ⇒ 直接 fail（不 spawn）；
  `GROK_HOOK_DEBUG=1` 打 stdin payload trace。
- 进程组 reap（`join_hook_group_reaps`）防僵尸。

## 4. 信任模型对比（三家都把它当权限边界）

- **ZCode**：project hooks 进 review 流程；trust store =
  `~/.zcode/security/workspace-hook-trust-v1.json`，按 `workspaceIdentity` +
  `hookDeclarationDigest`（bundle digest）记录。**fail-closed**：store 损坏 ⇒
  全部拦截。他们的教训写进了注释：UI 层曾用宽松局部校验、runtime 用严格 schema，
  出现"UI 显示已信任、runtime 永不放行"的分歧——修复方式是把 parse 下沉为
  shared 单一权威实现，所有消费者同判。**这个坑对我们有直接参考价值**。
- **Codex**：`HookTrustStatus { managed, untrusted, trusted, modified }`——多一个
  **modified** 状态（信任过的 hash 之后被改 ⇒ 回到待审）。per-hook
  `state.trusted_hash` + 插件 `trusted_content_hash`（声明与内容分开审批）。
- **Grok**：**统一 folder trust**——`~/.grok/trusted_folders.toml` 一把闸管
  MCP/LSP/hooks/project instructions/skills；禁用 folder-trust 则这些面一起关。
  另有 `DisabledHooks`（按 hook 名禁用 + managed_only 模式）。
- **我们现状**：`WorkspaceToolPort::trust_dir`（tool_job.rs:445）只是内存
  policy 快照，session 级，无持久化。hooks 的 project 层 trust 需要新的持久
  store（或并入 daemon 持久状态），UI 审核可走现有 `InteractionRequested` 面。

## 5. 我们 backend 的接入点（已核实的代码事实）

1. **工具漏斗唯一**：`qaqh-agent-runtime/src/session_loop.rs:1162`
   `dispatch_tool()` —— precheck → 权限/ask 交互 → port 派发，所有普通工具都
   过这里；`ask`/`handoff`/`session_title` 是控制面工具，早退不走 port
   （session_loop.rs:1170-1188）。
2. **`WorkspaceToolPort` 是 struct**（tool_job.rs:305），非 trait：
   `precheck`(547) / `execute`(593) / `grant_approval`(453) / `trust_dir`(445)。
   hook 层不适合做它的装饰器；应插在 session_loop 调用点（三家也都是
   orchestrator 层接线）。
3. **权限交互**：`ToolAdmission::ApprovalRequired` 分支里
   `request_permission()` 走 `InteractionRequested/Resolved`——PermissionRequest
   hook 的预答点。
4. **turn 边界**：契约事件已有 `TurnStarted/TurnEnded`（qaqh-ringing-contract
   events.rs）——Stop/UserPromptSubmit hook 的挂点。
5. **Config**：`qaqh-config/src/config.rs:61` `Config` 是运行时全量解析配置
   （mcp/lsp/sandbox/goals/exec…），加 `hooks: HooksConfig` 自然落位；
   `watch.rs` 已有配置热加载基建（Grok 的 mid-session reload 同款需求）。
6. **进程基建**：`qaqh-process-tools`/`qaqh-pty`/`sbx-*` + process-wrap 10；
   workspace `Cargo.toml` 已锁 `reqwest 0.13 / process-wrap 10`——command/http
   runner 直接复用。
7. **Ringing 契约面**：现有事件无 hook 概念。加 `HookStarted/HookCompleted`
   属 wire 实变 ⇒ 宪法 15 `[contract-refreeze]`：minor bump + 金标准机器重切 +
   tag；路由只能进 `RINGING_V2_HTTP_ROUTES`/`CONTROL_PLANE_ROUTES` 两张表。

## 6. 设计方案

### 6.1 新 crate `qaqh-hooks`（对齐 xai-grok-hooks 拓扑，零依赖 agent-runtime）

```
crates/qaqh-hooks/src/
  types.rs       # HookEventName + EventTraits(gate, matcher_policy) 宏生成
  config.rs      # HooksConfig/HooksFile TOML ↔ JSON 同构；deny_unknown_fields
  discovery.rs   # user/project/managed/requirements 分层合并 + 去重 + bundle digest
  matcher.rs     # regex matcher（按事件选匹配值：工具名/通知类型/子代理类型…）
  event.rs       # HookEventEnvelope + HookPayload；camel+snake 别名；128KB 上限
  result.rs      # HookDecision{Allow,Ask,Defer,Deny} / PromptDecision / StopHookOutcome
                 # / PostToolUseHookOutcome{block_reason, additional_context, output_replacement}
  runner/
    mod.rs       # RunContext{session_id, workspace_root, disabled}; HookRunnerResult
    command.rs   # stdin JSON / exit 0+stdout JSON / exit 2 = deny / 其余 fail-open
    http.rs      # POST envelope
  trust.rs       # DisabledHooks(managed_only) + 持久 trust store（digest）
  dispatcher.rs  # dispatch_pre_tool_use / dispatch_prompt_gate / dispatch_stop /
                 # dispatch_post_tool_use / dispatch_non_blocking —— 顺序跑、首 deny 截断、
                 # fail-open、rewrite 统一应用（last-wins）、context 有序合并
```

### 6.2 agent-runtime 接线（全部在 session_loop，await-able）

- `PreToolUse`：`dispatch_tool()` 内、`port.precheck()` **之前**。
  deny ⇒ `settle_failed("hook_denied")`；ask ⇒ 强制走
  `ApprovalRequired`（hook reason 进 details）；`updatedInput` ⇒ 重写
  `ToolRequest.args` 后再 precheck（改写失败 schema 即 deny）。
- `PermissionRequest`（P1）：`request_permission` 前先问 hook，Allow/Deny 预答；
  用户仍可覆盖（保持"hook 不能放宽"语义）。
- `PostToolUse`：settle 之后跑，`additionalContext/block reason` 追加进回传模型
  文本（wrap `<system-reminder>` 风格标签 + 署名）；`updatedToolOutput` 只替换
  **模型可见文本**，DB 里的真实结果不动（对齐 Grok："real result stays on the
  record"）。
- `UserPromptSubmit`：session_loop 用户输入入口；block 的 prompt 不写 session
  db（对齐 Grok "never enters the conversation history"）。
- `Stop`：turn 收尾处；block ⇒ 以 reason 拼一条 user 侧续跑消息；payload 带
  `stop_hook_active` 防死循环。
- `SessionStart/End`：session_agent assembly/teardown；`PreCompact/PostCompact`
  跟 auto-compact 触发点走。
- 控制面工具（ask/handoff/session_title）豁免 hook（Grok 的 SDK-only 限制同理：
  观察面工具不参与门）。

### 6.3 配置与信任

- `qaqh-config::Config` 加 `hooks: HooksConfig`：支持 config.toml 内嵌
  `[[hooks.PreToolUse]]`（Grok 同款 inline form）+ `hooks.disabled` +
  `hooks.managed_only`。
- trust store：`<data>/security/workspace-hook-trust-v1.json`，按
  workspaceIdentity + hookDeclarationDigest；**解析用单一 schema 实现，所有
  消费者同判，损坏 fail-closed**（ZCode 踩过的坑直接绕开）。UI 审核走
  `InteractionRequested`。
- 保留 env：`QQH_HOOK_EVENT/NAME`、`QQH_SESSION_ID`、`QQH_WORKSPACE_ROOT` +
  `CLAUDE_PROJECT_DIR` 别名（迁移友好）。

### 6.4 RingingV2 面策略（需拍板）

- **方案 A（P0，wire 零增量）**：hook 运行只落日志/现有 TimelineEntry，不改
  events.rs。优点：不触发 refreeze；缺点：前端看不到 hook 在跑，UI 面残缺。
- **方案 B（P1，contract-refreeze）**：加 `HookStarted{hook_id, event, run_id}`
  / `HookCompleted{run_id, status, decision, summary}` 两个事件（additive），
  走宪法 15：`[contract-refreeze]` minor bump + 金标准机器重切 + tag，路由进
  现有两张表。这是符合宪法 9 的正路——不为了绕开 refreeze 写转换层。

### 6.5 分阶段

| 阶段 | 内容 | 契约 |
|---|---|---|
| P0 | qaqh-hooks crate + config + command runner + PreToolUse(deny/ask/rewrite)/Stop(block)/UserPromptSubmit(block)/PostToolUse(context) + trust store + 单测 | 无 wire 变更 |
| P1 | Ringing HookStarted/Completed + /hooks 列表与审核（走两表路由）+ http runner + PermissionRequest hook | [contract-refreeze] minor bump |
| P2 | mcp_tool/prompt/agent handler、插件来源、`.claude`/`.cursor` 兼容扫描、additionalContext 落盘溢出 | 视实现定 |

### 6.6 测试（宪法 10/12 口径）

只测改动涉及 crate：决策解析（exit 0/2/N × JSON 冲突矩阵）、fail-open、
updatedInput schema 校验失败即 deny、matcher 正则、trust store 损坏 fail-closed、
去重与顺序。不做全局回归。

## 7. 需要拍板的决策点

1. **P0 事件集**：最小 7 事件（ZCode 口径：SessionStart/UserPromptSubmit/
   PreToolUse/PermissionRequest/PostToolUse/PostToolUseFailure/Stop）还是对齐
   Grok 全集（含 Subagent*/Pre|PostCompact/Notification）？建议：枚举一次到位
   （宏生成，加事件便宜），P0 只接线上 5 个。
2. **wire 时机**：先 A 后 B，还是直接 B（hook 事件从第一天就在 Ringing 上）？
   建议：直接 B，避免 P0 先落一个"看不见的 hook 系统"再补 refreeze。
3. **trust store 形态**：独立文件（ZCode 式）vs 并入 daemon 持久状态；是否学
   Grok 把 folder trust 统一管 MCP/LSP/hooks。建议：独立文件起步，统一放 P2。
4. **matcher 语法**：regex（Grok/Codex）+ 工具名别名表（`Bash→exec_command` 等）
   是否引入？建议：regex 原生支持，别名表随 Claude 兼容层放 P2。
