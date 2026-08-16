# dsh（deepseek-harness）对标与对齐清单

> 状态：**清单已定稿，A 批随 1.0 发版，B 批发版后第一批，C 批排期**
> 日期：2026-08-13（dsh 开源当天）
> 本地源码：`F:\DeepX\.deepx-test\deepseek-harness-src\`（tarball，commit 47f9438，56MB）
> npm 包：`@deepseek-ai/dsh`（rc 线，8/10 首版，8/13 高频发版）；GitHub：`deepseek-ai/deepseek-harness`（8/13 开源，9k+ star）

## 1. 背景

DeepSeek 官方编码 agent harness（`dsh` CLI）于 2026-08-13 开源。本次调研目标：**找出"模型在特定工具中表现超过顶尖模型"的真实机制**，评估 DeepX 差距，形成分级对齐清单。结论：不是模型魔法，是「提示工程 + 运行时机制」的完整体系；DeepX 的 schema 设计不输（唯一匹配/DID YOU MEAN 更强），差距在提示层工作量和动态注入机制。

## 2. 技术栈结论（定位验证）

| 维度 | dsh | DeepX |
|---|---|---|
| 语言 | TypeScript 全栈（**1981 个 .ts，零 Rust**） | **Rust 全栈**（daemon/worker/serve/WinUI3） |
| 运行时 | Node ^22.19/\|>=24 + pnpm monorepo | 原生二进制，零运行时依赖 |
| 框架 | vendored Cordis（插件体系） | 自有 crate 架构 |
| 辅助 | Python SDK（14 个 .py，benchmark 专用 jsonrpc-agent）；native/landlock-run（Linux Landlock 沙箱，C） | 无 |
| 进程模型 | Node 单进程 + worker_threads | 多进程（daemon + worker + workspace serve） |

**结论**：DeepX 是唯一已知的纯 Rust agent harness——定位成立，发版宣传主打"纯 Rust 原生、零 Node 依赖、单二进制"。

## 3. dsh 模型增益机制（调研产出）

| # | 机制 | 要点 | DeepX 现状 |
|---|---|---|---|
| 1 | 每工具独立引导段 | 每工具 1-2 句 Claude 风格引导（"先读再写"等），system prompt 分段注册 | ❌ 仅 [FILE EDITING] 笼统一段 |
| 2 | AGENTS.md/CLAUDE.md 层级指令 | `$DSH_HOME/AGENTS.md` 全局 + 项目根→cwd 逐目录；`<system-reminder>` 帧注入 durable user message；set/replace/remove 动态事件 + SHA-1 去重 + 预算截断 | ❌ 完全没有 |
| 3 | goal 目标回合状态机 | create/get/update_goal（Codex 风格）；回合预算 maxGoalRounds；blocked 需连续 N 轮同因 + blocked_reason；"evidence before completion" | ⚠️ 手动 Goal 模式（activate/step_complete/evidence/max_auto_turns），缺 block 纪律 |
| 4 | KV cache 前缀稳定 | toolOrder 固定；动态内容 append-only 挂末尾；system prompt 字节级稳定；compaction 重放前缀复用缓存 | ⚠️ 静态 prompt 天然稳定（优势），动态注入未做 |
| 5 | 动态上下文快照 | `supersede` 语义（最新覆盖旧，物理 append-only） | ❌（`<file_state>` 账本非此机制） |
| 6 | schema 对齐 Claude Code | snake_case、唯一匹配、read-before-write | ✅ 同款且更强 |
| 7 | compaction 智能压缩 | token-meter 压力（阈值 0.8）+ tool-result pruner 修剪 + 重放前缀总结（KV 复用）+ `<compacted-summary>` 替换 + 保留 0.16 尾部 | ⚠️ engine_goal 有 compact 感知，无完整体系 |
| 8 | token-meter 用量感知 | 压力测量含全量（system prompt+工具+结果），按路由容量决策 | ❌ |
| 9 | workflow 编排 | 模型写脚本 + worker 线程（非安全沙箱） | ❌（有 subagent 无脚本编排层） |
| 10 | tool-result pruner | 超大工具结果先改写再入史 | ❌ |

**dsh 默认 persona 仅一句话**（"You are a coding agent powered by the {model} model. Your working directory is {cwd}."）——agent 化全靠运行时机制；DeepX 的 44 行强 persona 是互补优势（纯聊天不回落网页版）。

## 4. 对齐清单（分级）

### A 批：随 1.0 发版（30 分钟 - 3 小时）
| 项 | 改动点 | 预估 |
|---|---|---|
| A1 每工具引导段 | `crates/deepx-config/prompts/backend_prompt.md` 给 read_file/edit_file/exec/write 各加 1-2 句 Claude 风格引导 | 30 分钟 |
| A2 AGENTS.md 最小支持 | worker 会话启动读 workspace 根 + `~/.dsh`（或 `~/.deepx`）AGENTS.md/CLAUDE.md，`<system-reminder>` 帧注入首条 user message | 2-3 小时 |
| A3 edit 引导强化 | read-before-write 措辞进 [FILE EDITING] | 10 分钟 |

### B 批：发版后第一批（各 1-2 小时）
| 项 | 改动点 | 预估 |
|---|---|---|
| B1 goal block 纪律 | TodoStatus 增 blocked + blocked_reason；连续 N 轮（建议 3）同因才允许；"difficulty/uncertainty 不是 block" 措辞入 current_step_prompt | 1-2 小时 |
| B2 goal 证据强制 | current_step_prompt/完成路径提示"填写 evidence 证明达成" | 30 分钟 |
| B3 goal 简化 policy 段 | 手动模式下提示"goal 激活时：按序推进、完成带证据、卡住说明原因" | 30 分钟 |
| B4 tool-result pruner | exec/read 大结果进历史前截断/摘要（配合 engine_goal 既有 compact 感知） | 1-2 小时 |

### C 批：中期排期
| 项 | 内容 |
|---|---|
| C1 KV cache 纪律 | 动态注入一律挂消息流末尾（ephemeral 不落库）、工具顺序固定、system prompt 字节级稳定（已达标） |
| C2 diff 卡片 replayable 元数据 | edit/write 结果派生前端可回放 diff 卡片（SubagentCard 渲染用） |
| C3 read-before-write 抽 policy 层 | expected_hash 守卫从工具内嵌抽成可插拔策略（dsh 的 fs-policy 思路） |
| C4 MCP client | 行业标准（Anthropic 同门，dsh 有 dsh-mcp-client 参考）；Rust 侧现成 crate（rmcp/mcp）；接入外部工具服务器 |
| C5 skills 文件系统发现（可选） | 对标 dsh-skill-filesystem：项目内技能文件自动发现；**不强制**（我们的受管目录+显式激活安全性更好） |
| C6 lsp 代码语义 | 语言服务器能力（dsh `packages/lsp`） |
| C7 hooks 桥接 | Claude Code/Codex hook 兼容（dsh `packages/hooks`），拉生态用户 |

### D 批：co-work 联动（与 co-work-subagent 一起做）
| 项 | 内容 |
|---|---|
| D1 ACP server | 暴露 DeepX agent 给程序客户端（对标 dsh-acp：JSON-RPC stdio 受控方法子集） |
| D2 ACP client | 把外部 harness（dsh 等）进程拉为协作者（对标 dsh-subagent-acp：spawn + 协议驱动） |

### 明确不做
模型自动 create_goal（手动进入红利：省 authority 状态机）、authority 检查、revision/CAS（单进程 Mutex 够用）、resume disarm/rearm、todo 快照式改写（`todo_write` 全量重写 vs 我们命令式更强）、self-modification、E2B 沙箱 POC。

## 5. 关键源码索引（本地）

```
F:\DeepX\.deepx-test\deepseek-harness-src\
├── packages/core/system-prompt/src/index.ts   # 分段装配器（sections/contexts/toolOrder/变量插值）
├── packages/goal/tool-goal/src/index.ts       # goal policy 提示文本（blocked 阈值插值）
├── packages/fs/tool-fs/src/edit.ts            # edit 工具（唯一匹配 + replace_all）
├── packages/todo/tool-todo/                   # todo_write（快照式）——对比我们的命令式
├── packages/bundle/web-app/cordis.patch.yml   # 默认 persona（一句话）+ tools mode
├── native/landlock-run/                       # Linux Landlock 沙箱（= 我们 B2 文档的 Landlock 方案）
├── python/sdk + sdk-runtime/                  # benchmark 专用 jsonrpc-agent
└── BENCHMARK.md                               # 跑分方法（裁剪版 agent，非完整产品）
```

## 6. 前瞻：co-work-subagent（跨 harness 协作）

> 状态：**设计待验证**（用户将从源码做通信实现；本段是意图锚点）

**意图**：让 DeepX 的 agent 与 dsh 的 agent **跨 harness 共同开发**——不是同 harness 内的子代理（subagent），而是两个独立 agent 程序之间的协作者（co-worker）。顺带验证 "co-work-subagent" 设计。

**候选通道（dsh 侧现成面）**：
- **ACP（Agent Client Protocol）——行业标准**（Anthropic 主导，agentclientprotocol.com，JSON-RPC stdio）：dsh 已实现 server（`packages/acp/acp`，暴露 agent 给程序客户端）与 client（`packages/subagent/subagent-acp`，把外部进程当子代理驱动）两端——**参考实现完整，协议无需自研**
- `packages/hooks/` —— Claude Code/Codex hook 桥接（wire-protocol 库）
- 文件系统共享 workspace（双方已有 FS 能力，最朴素可用）

**DeepX 侧需暴露**：ACP server（对标 dsh-acp，暴露受控方法子集）或 ACP client（对标 dsh-subagent-acp，spawn dsh 进程 + 协议驱动）；Ringing V1 内部不变，ACP 作为外部边界。

**已验证的 dsh 参考语义**（subagent-acp）：子进程独立 runtime/session/model/tools；父只传 cwd、零上下文跨进程；权限自动应答（默认 reject）；dispose 阶梯（EOF 宽限→SIGTERM→SIGKILL）；每 run 新进程。

**验证目标**：
1. 跨 harness 任务委派/回传闭环（等价 B1 沙箱边界：跨 workspace 操作自动拒绝）
2. 身份与权限隔离（对方 agent 视作"受限协作者"，非本 harness worker）
3. 幂等/回放（Ringing 已建能力复用，不发明新协议——与 subagent 决策一致）
4. 完成摘要回传（参考 subagent 的 ProcessRegistry 语义）

## 7. 决策记录

1. **对齐范围 = 运行时纪律，不对齐进入机制**：手动 Goal 模式省掉 dsh 的 create_goal/authority/resume-rearm 全套；
2. **AGENTS.md 是发版后第一个"项目感知"卖点**（Claude 用户迁移第一诉求），A2 优先；
3. **不发版的内容**：exec 沙箱（B2 文档延期）、compaction 完整体系（C 批）；
4. **宣传口径**：纯 Rust 原生 + 子代理沙箱不弹窗 + Goal 证据纪律；不提未落地的 exec 沙箱。
