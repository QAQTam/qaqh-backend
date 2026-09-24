# 当前状态

> 日期：2026-09-25
> 基线：`f7d2d8a`
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
