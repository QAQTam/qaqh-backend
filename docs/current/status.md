# 当前状态

> 日期：2026-09-25
> 基线：`2.0.0-alpha2`
> 状态：implementation baseline / refactor freeze candidate

## 1. 当前结论

架构重构主线可以进入冻结：

- Ringing v2 单流与 v1 硬切已完成。
- canonical session facts / projection / replay 已接入。
- tool ledger、typed tools、结构化 tool outcome 已落地。
- driver lease、canonical driver fact、过期回收与 workspace gate 已落地。
- v2 bootstrap / command / timeline / content / service 路由已落地。
- compact 第二真源已删除，归档水位推导已落地。
- comment audit 已并入主线。

当前剩余工作主要是 debug、durability、安全语义和产品裁决，不再需要开新的
架构重构主线。

## 2. 已验证门禁

```text
cargo test --workspace -- --test-threads=1              PASS
cargo clippy --workspace --all-targets -- -D warnings    PASS
cargo fmt --all -- --check                               PASS
cargo check -p qaqh-types -p qaqh-domain --all-features --all-targets  PASS
```

真实 daemon 探针：

```text
QAQH_SMOKE_LEASE_TTL_MS=30000 ./scripts/v2-smoke.sh ...
./scripts/v2-content-probe.sh ...
QAQH_CONTENT_PROBE_MODE=permission ./scripts/v2-content-probe.sh ...
./scripts/v2-compact-probe.sh ...
```

以上均通过。

## 3. 已完成的关键交付

| 领域 | 状态 | 证据 |
|---|---|---|
| v2 单流 | done | daemon routes + smoke |
| v1 硬切 | done | v1 路由 404 / 客户端硬切 |
| canonical facts/projection | done | `qaqh-session` contract tests |
| tool ledger / recovery | done | runtime tests + recovery executor |
| typed tools / display outcome | done | tool contract tests + probes |
| driver seat | done | lease/reclaim smoke |
| content ref / Range | done | content probe |
| timeline v2 | done | daemon timeline routes/tests |
| compact archive watermark | done | compact probe |
| comment audit | done | merged comment-audit changes |

## 4. 当前不作为重构目标的项

以下进入 debug/backlog，不再阻塞重构收工：

- interaction 跨 daemon 重启**继续执行**（正文已 durable，pending turn resume 仍需产品裁决）；
- permission 正文 pin/unpin（正文已 pin，ToolFinished 终结路径已接入）；
- driver `not_eligible` / 显式移交优先级；
- sandbox fallback 与 Linux 读隔离/cgroup；
- Windows 实机验证；
- timeline 性能残余；
- P6 单源/旧目录/legacy writer 清理；
- `mutilAI-SDK` bridge 接入。

具体优先级见 [`debug-backlog.md`](./debug-backlog.md)。

## 5. Subagent V2 实现状态

- Phase 0 已 accepted：spec、decisions、path grammar、communication shape 和
  canonical producer 设计已冻结。
- Phase 1 已完成：
  - `AgentPath` grammar/resolver（#364 / PR #368）。
  - 逻辑 agent catalog 与 `/root` 注册（#365 / PR #369）。
  - canonical graph store、递归 loader、post-order cascade（#366 / PR #370）。
  - `SubagentSpawned/Finished` 真实 producer（#367 / PR #371）。
  - `list_agents` path-prefix 工具面已接入。
- Phase 2 已完成：
  - canonical `InterAgentCommunication` 与 mailbox projection 已合并（#372 / PR #387）。
  - canonical `InputAccepted` producer 与 communication append 已接入。
  - subagent initial task 已通过 `InterAgentCommunication` delivery 投递。
  - `send_message` / `followup_task` / `wait_agent` / `interrupt_agent` 工具面已接入。
  - child completion 已改为 queue-only mailbox delivery，不再默认触发父 turn。
- Phase 3 进行中：
  - unloaded child 可由 delivery 经 loaded immediate parent reload，并保留 AgentPath。
  - Trigger delivery 会重新 arm collector，completion 可继续回父 mailbox。
  - idle subagent 已纳入 residency LRU，unload 后仍可经 delivery reload。
  - `list_agents` 已返回显式 status/residency；unloaded agent 仍可见。
  - parent-owned child 已拒绝无 inter-agent metadata 的 direct/app-server 输入。
  - TeamSnapshot / TeamDelta 后端 projection 已接入，roster 与 inbox 可从 canonical facts 重建。
  - runtime residency overlay 已接入；`AgentResidencyChanged` 作为 ephemeral TeamDelta 发布，重启后回到 unloaded。
- 仍待：TUI/WinUI roster/inbox 消费、depth/outbound 配额、
  大正文 `content_ref` 外置、task board 与 message board。
- Phase 4 前端壳接入和 Phase 5-7 未完成；不得用 legacy result injection 或工具卡 JSON 冒充 V2 完成。
- 权威计划：
  [`spec/2026-09-25-subagent-v2-rewrite-spec.md`](./spec/2026-09-25-subagent-v2-rewrite-spec.md)。
- 当前交接：
  [`handoff/2026-09-26-subagent-v2-current-handoff.md`](./handoff/2026-09-26-subagent-v2-current-handoff.md)。

## 6. Beta 前身份迁移门禁

- `SessionId` 必须成为唯一会话主键；新会话必须 `seed == session_id`。
- `sessions/{session_id}` 必须成为默认存储布局。
- 旧 8 位 seed 只能经 legacy resolver 访问，beta 前删除兼容映射。
- `SubagentSpawned.child_session_id` 不得再接受 8 位 seed。
- TUI/WinUI 不得假设 seed 是 8 位 hex。
- 当前状态：
  - #369/#371 的修复已随 #384/#385/#386 合并；#382 身份设计已合并。
  - canonical identity 预分配基础切片已实现：普通会话和子代理生产创建路径先分配
    `SessionId`，再以同一值作为 `seed`、目录名和 canonical id。
  - 旧目录 resolver 与启动时原子迁移已实现：迁移 journal 可恢复，
    `legacy_seed -> session_id` alias 保留只读兼容窗口。
  - 仍待：wire/runtime 字段改名、TUI/WinUI 假设清理，以及删除剩余
    `generate_seed()` 兼容路径。
- 权威迁移设计：
  [`spec/2026-09-25-session-identity-unification.md`](./spec/2026-09-25-session-identity-unification.md)。
