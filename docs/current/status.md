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

- interaction 跨 daemon 重启持久化；
- permission 正文 pin/unpin；
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
  - `AgentPath` grammar/resolver（#364）。
  - 逻辑 agent catalog 与 `/root` 注册（#365）。
  - canonical graph store、递归 loader、post-order cascade（#366）。
  - `SubagentSpawned/Finished` 真实 producer（#367）。
- Phase 2 进行中：
  - canonical `InterAgentCommunication` 与 mailbox projection 正在 #372 落地。
  - `spawn_agent` initial message、工具面和 residency reload 分别在 #373-#375。
- 仍待：`list_agents` path-prefix 工具面、child reload 后 path 恢复。
- Phase 3-7 未开始；不得用 legacy result injection 或工具卡 JSON 冒充 V2 完成。
- 权威计划：
  [`spec/2026-09-25-subagent-v2-rewrite-spec.md`](./spec/2026-09-25-subagent-v2-rewrite-spec.md)。
