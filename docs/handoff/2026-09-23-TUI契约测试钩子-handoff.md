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
  - `QAQH_TEST_COMMAND_ACK_CHANNEL=...`；
  - `QAQH_TEST_COMMAND_ACK_COMMAND=interaction|permission_response|ask_response|all`。
- permission / ask：
  - `QAQH_TEST_INTERACTION_FAULT=permission-deny|permission-hang|ask-dismiss|ask-hang`。

## 2. 验证

后端：

```text
cargo check -p qaqh-daemon -p qaqh-runtime --all-targets  PASS
cargo test -p qaqh-daemon --bin qaqh-daemon              56 passed
cargo test -p qaqh-runtime --test plan_review_hook       4 passed
cargo test -p qaqh-runtime test_hooks --lib              1 passed
```

评审阻断项收口：

- plan review 增加真实 Loop + mock provider 集成测试，覆盖 approve / reject
  后进入下一轮 provider 请求；
- timeline gap 不再编造 seq，改为丢弃第一条真实 entry、发送下一条真实
  entry，保证 journal/cursor 自洽；
- gap 注入前先执行 seed 归属与 cursor/dedup 判定，新增 foreign-seed 负向用例；
- gap 钩子改为「只丢一帧、不关流」：不再依赖「至少两条可投递 entry」，也不再用
  「发下一条真实 entry 后关流」的形态（那会让客户端用同一 cursor 重连并二次收到
  同一帧）；新增客户端模型级回归，cosplay `expected == cursor + 1` 判定并断言
  不重复下发；
- `parse_timeline_cursor` 对非法 `Last-Event-ID` 仍按 0 重放，但补一条 `log::warn!`
  （空 cursor 属首次连接，不告警）；
- `QAQH_TEST_COMMAND_ACK_COMMAND` 默认只命中 permission/ask 响应，避免冻结
  同 channel 上其它 command；
- `permission-deny` / `ask-dismiss` 改为一次性；`*-hang` 明确保持持续语义。

真实 TUI PTY：

> `MODE=*` 是 **TUI 侧 harness 自己的变量**（TUI 仓库 `scripts/e2e-v2-interactions.sh`
> 及其临时脚本里的取值域：`plan` / `permission` / `ask`），**不是 daemon 开关**，
> 也不在本文 spec 的表格里。daemon 只读 `QAQH_TEST_*`。TUI 侧把它正式并进脚本时
> 需要自己定值域与默认值；backend 侧不校验该变量。

```text
QAQH_BACKEND_ROOT=../qaqh-backend-tui-contract \
QAQH_TEST_PLAN_REVIEW=1 MODE=plan \
  /tmp/e2e-v2-plan-review.sh
→ plan review modal visible / approved / no panic   PASS

QAQH_TEST_INTERACTION_FAULT=permission-deny MODE=permission ...
→ permission modal visible / approved / no panic    PASS

QAQH_TEST_INTERACTION_FAULT=ask-dismiss MODE=ask ...
→ ask modal visible / answered / no panic           PASS

QAQH_TEST_INTERACTION_FAULT=permission-hang MODE=permission ...
→ modal visible / key handled / no panic / no cursor timeout PASS（清理路径冒烟）

QAQH_TEST_INTERACTION_FAULT=ask-hang MODE=ask ...
→ modal visible / key handled / no panic / no cursor timeout PASS（清理路径冒烟）
```

## 3. 未收口

- TUI 仓库尚未提交正式 `MODE=plan` harness；本次只在 `/tmp` 临时脚本验证。
- `permission-hang` / `ask-hang` 的真实 PTY 清理路径已冒烟通过，但当前 harness
  没有断言“超时提示文案出现”的 UI 终态，仍需 TUI 侧补一条定时断言。
- P1 仍未做：
  - `session.meta` / `plan.*` / `stats.token_usage` / `git.*` 的 client typed 变体；
  - P3 typed todo 消费路径说明；
  - P5 wire 变更交底。

## 4. 下一步

1. 本分支开独立 PR，请 TUI 侧评审 env 名称和语义；
2. TUI 侧把 `MODE=plan` 正式并入 `scripts/e2e-v2-interactions.sh`；
3. 合并后发布新不可移动锚点（不移动旧 `tui-anchor-2026-09-23`）；
4. P1 typed service 变体另开分支，不与 P0 hook 混 PR。
