# 当前架构

> 日期：2026-09-25
> 基线：`f7d2d8a`
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
- `ToolResult` 的终态与展示 outcome 已结构化，不再从 `[OK]` 文本反推。
- exec 支持 stdout/stderr 分离展示。
- Linux sandbox 支持 bubblewrap 或 Landlock/seccomp；具体后端由 capability 探测决定。

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
