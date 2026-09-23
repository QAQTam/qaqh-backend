# Handoff：TUI 契约测试钩子（2026-09-23）

> 状态：**P0 可交付**。独立分支 `feat/tui-contract-test-hooks`，基线
> `betav2 @ 5ec1900d6c937b6ff927d8f65fcd37d465de7988`；不混入 P3 工具迁移。
>
> 契约登记：[`2026-09-23-TUI契约测试钩子-spec.md`](../spec/2026-09-23-TUI契约测试钩子-spec.md)
> TUI 输入：[`TUI对后端的协作需求`](../../../qaqh-tui-app/docs/spec/2026-09-23-TUI对后端的协作需求-spec.md)

## 1. 已交付

- 集中 `TestHooks` registry：daemon 启动时读取一次，注入 token 一次性消费。
- plan review 真实挂起入口：
  - `QAQH_TEST_PLAN_REVIEW=1`；
  - round 0 不调用 provider，直接发真实 `PlanReviewRequested`；
  - 后续 approve/reject 走现有 `handle_plan_response`。
- SSE / timeline：
  - `QAQH_TEST_SSE_TERMINATE=lagged|<code>`；
  - `QAQH_TEST_TIMELINE_GAP=1`；
  - `QAQH_TEST_SESSION_404_SEED=<seed|*>`。
- command ack：
  - `QAQH_TEST_COMMAND_ACK=hang|<ms>`；
  - `QAQH_TEST_COMMAND_ACK_CHANNEL=...`。
- permission / ask：
  - `QAQH_TEST_INTERACTION_FAULT=permission-deny|permission-hang|ask-dismiss|ask-hang`。

## 2. 验证

后端：

```text
cargo check -p qaqh-daemon -p qaqh-runtime --all-targets  PASS
cargo test -p qaqh-daemon --bin qaqh-daemon              52 passed
cargo test -p qaqh-runtime test_hooks --lib              1 passed
```

真实 TUI PTY：

```text
QAQH_BACKEND_ROOT=../qaqh-backend-tui-contract \
QAQH_TEST_PLAN_REVIEW=1 MODE=plan \
  /tmp/e2e-v2-plan-review.sh
→ plan review modal visible / approved / no panic   PASS

QAQH_TEST_INTERACTION_FAULT=permission-deny MODE=permission ...
→ permission modal visible / approved / no panic    PASS

QAQH_TEST_INTERACTION_FAULT=ask-dismiss MODE=ask ...
→ ask modal visible / answered / no panic           PASS
```

## 3. 未收口

- TUI 仓库尚未提交正式 `MODE=plan` harness；本次只在 `/tmp` 临时脚本验证。
- `permission-hang` / `ask-hang` 只验证了 daemon 侧不返回 ack 的实现，尚未跑
  TUI 侧超时 UI 断言。
- P1 仍未做：
  - `session.meta` / `plan.*` / `stats.token_usage` / `git.*` 的 client typed 变体；
  - P3 typed todo 消费路径说明；
  - P5 wire 变更交底。

## 4. 下一步

1. 本分支开独立 PR，请 TUI 侧评审 env 名称和语义；
2. TUI 侧把 `MODE=plan` 正式并入 `scripts/e2e-v2-interactions.sh`；
3. 合并后发布新不可移动锚点（不移动旧 `tui-anchor-2026-09-23`）；
4. P1 typed service 变体另开分支，不与 P0 hook 混 PR。
