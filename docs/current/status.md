# 当前状态

> 日期：2026-09-26
> 基线：`2.0.0-alpha3`
> 状态：alpha3 released checkpoint / refactor freeze candidate

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
- Phase 3 已完成：
  - unloaded child 可由 delivery 经 loaded immediate parent reload，并保留 AgentPath。
  - Trigger delivery 会重新 arm collector，completion 可继续回父 mailbox。
  - idle subagent 已纳入 residency LRU，unload 后仍可经 delivery reload。
  - `list_agents` 已返回显式 status/residency；unloaded agent 仍可见。
  - parent-owned child 已拒绝无 inter-agent metadata 的 direct/app-server 输入。
  - TeamSnapshot / TeamDelta 后端 projection 已接入，roster 与 inbox 可从 canonical facts 重建。
  - runtime residency overlay 已接入；`AgentResidencyChanged` 作为 ephemeral TeamDelta 发布，重启后回到 unloaded。
  - max depth 默认 1 可配置；sender-target in-flight 与 sender outbound 配额已接入。
  - broadcast / `@all` 默认拒绝；`close_agent` 已不在 V2 工具表。
- 仍待：TUI/WinUI roster/inbox、task board 与 message board 消费；
  大正文 `content_ref` 已落地。
- Task board backend 已完成：canonical foundation（types / reducer / durable
  TeamStore）、runtime tools（`task_create` / `task_claim` / `task_update` /
  `task_close` / `task_list`）、daemon team snapshot endpoint 与
  `TeamDelta::TaskChanged` 单流 delta。spec 见
  [`spec/2026-09-26-team-task-board.md`](./spec/2026-09-26-team-task-board.md)。
- Message board backend 已完成：独立 `BoardFact` / `BoardStore`（channel /
  thread / post / subscription、torn-tail replay）、runtime tools
  （`board_channel_create` / `board_thread_create` / `board_post` /
  `board_subscribe` / `board_list`）、`/team` board snapshot 与
  `TeamDelta::BoardChanged` 单流 delta。notification 只对 running + loaded
  subscriber 做 queue-only best-effort，不启动 idle agent。spec 见
  [`spec/2026-09-26-team-message-board.md`](./spec/2026-09-26-team-message-board.md)。
- Phase 7 backend 已完成：`steer` / `interject` delivery、canonical
  `InputPurpose`、safe-point 优先级（interject -> steer -> queue）、单 lap
  配额与 `steer_agent` / `interject_agent` 工具已落地；`interrupt` 仍是唯一
  取消 turn 的 delivery。spec 见
  [`spec/2026-09-26-steer-interject.md`](./spec/2026-09-26-steer-interject.md)。
- Phase 4 / TEAM-01e / BOARD-01d / SUBV2-11d 的 TUI/WinUI 消费已同步完成；
  alpha3 检查点闭环。不得用 legacy result injection 或工具卡 JSON 冒充 V2 完成。
  **Phase 4 的后端契约已闭环**：`qaqh-client` 导出 Team projection typed 面
  （`Client::team_v2` + `ClientV2Team*` 类型 + 契约锁测试），TUI / WinUI
  已按该契约完成消费。工作拆分与不变量见
  [`coordination/2026-09-26-phase4-roster-inbox.md`](./coordination/2026-09-26-phase4-roster-inbox.md)。
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
  - runtime 生产路径已不再调用 `generate_seed()` / `generate_unique_seed()`；
    无 manager 的 ephemeral 路径也改用 canonical UUIDv7 SessionId。
  - 仍待：wire/runtime 字段改名、TUI/WinUI 假设清理，以及删除
    `qaqh-session` 内剩余的 legacy seed allocator / resolver。
- 权威迁移设计：
  [`spec/2026-09-25-session-identity-unification.md`](./spec/2026-09-25-session-identity-unification.md)。

## 7. Alpha3 发布检查点

判定条件：Subagent V2 Phase 0-7 的 backend + TUI/WinUI 消费全部完成。

- [x] Phase 0-3 backend
- [x] Team projection backend
- [x] Phase 5 task board backend
- [x] Phase 6 message board backend
- [x] Phase 7 steer / interject backend
- [x] Phase 4 roster / inbox / child transcript 前端消费
- [x] TEAM-01e task board 前端消费
- [x] BOARD-01d message board 前端消费
- [x] SUBV2-11d steer / interject / interrupt 前端区分
- [x] `2.0.0-alpha2` -> `2.0.0-alpha3`、release notes、smoke

发布说明：[`releases/2026-09-26-alpha3.md`](./releases/2026-09-26-alpha3.md)。

最终门禁：

```text
cargo fmt --all -- --check                                      PASS
cargo clippy --workspace --all-targets --offline -- -D warnings PASS
cargo test --workspace --offline -- --test-threads=1            PASS

QAQH_SMOKE_LEASE_TTL_MS=30000 ./scripts/v2-smoke.sh ...         PASS
./scripts/v2-content-probe.sh ...                                PASS
QAQH_CONTENT_PROBE_MODE=permission ./scripts/v2-content-probe.sh ... PASS
./scripts/v2-compact-probe.sh ...                                PASS
```

身份迁移是 Beta 硬门禁，不属于 alpha3 的 Subagent V2 检查点；不得因此把
legacy seed 带进新 roster / mailbox / Team projection。

新增 V2 工具后，默认 prompt + tool defs 预算从 20k 调整为 22k 字符
（约 5.5k tokens），仍由 `prompt_and_tool_defs_char_budget` 守住。
