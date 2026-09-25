# 当前架构

> 日期：2026-09-25
> 基线：`2.0.0-alpha2`
> 状态：current

## 1. 总体分层

```text
qaqh-client / TUI / webui
        │
        ▼
qaqh-daemon (axum)
  ├── /ringing/v2/*       单一 v2 协议面
  ├── session/lease/driver
  ├── timeline/content
  └── service/control
        │
        ▼
qaqh-runtime
  ├── AgentState / Loop
  ├── TurnActor / ToolRuntime
  ├── RingingHub / timeline
  └── canonical ToolLedger
        │
        ├──────────────► qaqh-gate ──► provider HTTP
        │
        ▼
qaqh-session
  ├── canonical session facts
  ├── projections / replay
  ├── messages.jsonl / WAL / meta
  └── compact archive watermark
        │
        ▼
qaqh-types / qaqh-domain / qaqh-policy / qaqh-sandbox
```

## 2. 关键事实源

### Canonical session facts

- 位置：`crates/qaqh-session/src/session_fact_v2/`
- 角色：会话事实、提交游标、projection replay 的权威源。
- `events.jsonl` + commit marker 通过 `CanonicalSessionStore` 管理。
- 投影是派生数据，不能反向成为事实源。

### 消息归档

- 位置：`crates/qaqh-message/src/store.rs`、`crates/qaqh-session/src/manager.rs`
- `messages.jsonl` 是 append-only 消息归档。
- `meta.json` 保存消息数、turn 数、模型、usage、compact watermark 等元数据。
- `messages.wal` 覆盖 enqueue → drain 的崩溃窗口。

### Compact

- 压缩摘要作为普通 `Message` append 到 `messages.jsonl`。
- `meta.compact_covered_through_msg_id` 记录被摘要覆盖的最高 `msg_id`。
- 活跃模型视图由归档推导：
  `system + 最新摘要 + 水位后的非摘要消息`。
- 不再有 `compact-context.json` 第二真源。
- 实机验证：`scripts/v2-compact-probe.sh`。

### Ringing v2

- 位置：`crates/qaqh-ringing/src/v2/`、`crates/qaqh-runtime/src/ringing/`
- 单一 v2 流：`/ringing/v2/sessions/{seed}/events`。
- v1 三频道流和 `/ringing/v1/*` 路由已硬切删除。
- reliable / replaceable 由 canonical projection 和 replay 驱动。
- bootstrap、command、timeline、content、service 均以 v2 为唯一 wire 面。

### Driver seat

- canonical `DriverChanged` fact 是席位事实源。
- daemon lease 决定 holder 是否仍存活。
- 支持 claim、release、过期回收、daemon 重启后的启动轮转回收。
- 写入命令在已有 live holder 时受 driver gate 保护。

### Tool execution

- `ToolCallContext` 携带 session/workspace/mode/permission/sandbox/cancellation。
- `ToolCallContext` 同时携带 host 解析出的 exec 默认 shell，确保运行中配置切换即时生效。
- exec shell 优先级：显式 `shell` 参数 > `[exec].default_shell` > 平台自动探测。
- 平台自动探测顺序：
  - Windows：`pwsh` > Git for Windows bash > `powershell` 5.1 > `cmd`；
  - Linux：`bash` > `zsh` > `sh`；
  - macOS：`bash` > `zsh`。
- shell 切换不做语法翻译；命令与所选 shell 不兼容时直接返回 shell 原始错误，由模型自行纠正。

```toml
[exec]
# 空值 / "auto" = 按平台优先级自动探测
default_shell = "auto"
```

- `ToolResult` 的终态与展示 outcome 已结构化，不再从 `[OK]` 文本反推。
- exec 只接受 shell `command` 字符串，不再接受直接 `argv`。
- 进程启动层仍使用 argv，但 argv 只由所选 shell 的 `command` 参数派生，不是公开工具参数。
- exec 支持 stdout/stderr 分离展示。
- Linux sandbox 支持 bubblewrap 或 Landlock/seccomp；具体后端由 capability 探测决定。

### Subagent V2（已冻结，分阶段实现）

- `AgentControl` 负责跨 agent 的 spawn / message / interrupt / wait 命令边界。
- `AgentRegistry` 保存逻辑 agent metadata 与 loaded worker handle 的映射；
  logical identity 不随 unload 消失。
- `AgentGraphStore` 是从父 session canonical `SubagentSpawned/Finished` facts
  重建的 parent-child 索引，不是第二可写事实源。
- `Mailbox` 负责 queue/trigger/interrupt delivery；消息进入 mailbox 不等于模型已读。
- `Residency` 只描述 loaded/unloaded，与 `AgentStatus` 分离。
- `TeamSnapshot/TeamDelta` 是 TUI/WinUI 的唯一 roster/inbox 投影。
- 当前实现从 Phase 1 开始；完整契约见
  [`spec/2026-09-25-subagent-v2-rewrite-spec.md`](./spec/2026-09-25-subagent-v2-rewrite-spec.md)。

### Gate

- 位置：`crates/qaqh-gate/`
- 当前支持：
  - OpenAI Chat Completions
  - OpenAI Responses
  - Anthropic Messages
- 负责 HTTP、SSE、协议转换、重试、usage、reasoning/state 回放。
- `qaqh-gate` 是 runtime 与 provider 之间的边界，不是 agent loop。
- `mutilAI-SDK` 当前不是运行时依赖；未来只考虑通过 bridge 替换 gate 内部 adapter。

## 3. 存储布局（当前语义）

```text
{sessions_dir}/{seed}/
├── canonical-identity.json
├── events.jsonl
├── events.commit.json
├── writer-fence.json
├── messages.jsonl
├── messages.wal
├── meta.json
└── 其它 projection/cache 文件
```

`compact-context.json` 已退役，不再是当前存储布局的一部分。

## 4. 不做的事

- 不恢复 v1 兼容层。
- 不把 projection/cache 当事实源。
- 不让 SDK/agent loop 绕过 QAQH 的 canonical fact、tool ledger 和权限边界。
- 不在没有验收证据的情况下把历史文档当当前设计。
