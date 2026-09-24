# Ringing v2 canonical DriverChanged Handoff（2026-09-24）

状态：**driver 席位已收口到 canonical 单写者**，实机 smoke 通过。承接
`docs/handoff/2026-09-24-ringing-v2-p0-4-p0-5-landing-handoff.md` §4-A-1。

## 1. 背景

上一刀把 driver 做成了 daemon 内存表（`RingingDriverStore`）：语义能跑，但
席位易失（daemon 重启后 epoch 归零）、没有 reliable 事件（在线客户端只能
轮询 bootstrap）、epoch 无法与其它会话状态一起排序回放。

canonical log 是单写者模型（`CanonicalSessionStore::acquire_writer` +
`writer-fence.json`，由 session actor 的 `ToolLedger` 持有），daemon 不能直接
append。本刀按方案 A 收口：**daemon 只做准入与转发，actor 的 ToolLedger 是
席位唯一写者。**

## 2. 落地内容

### 2.1 canonical 事实

- 新增 `FactPayload::DriverChanged(DriverChanged { holder: Option<String>,
  driver_epoch: u64, changed_at_ms: i64 })`；`holder = None` 表示释放。
- `projection_slots` → CONTROL_ONLY；`FactPayload::validate` 校验
  `holder` 非空、`driver_epoch ∈ 1..=MAX_SAFE_FACT_SEQ`。
- 不 bump `SESSION_FACT_PAYLOAD_VERSION`（老日志仍可校验通过）。
- control 投影：`ControlSnapshot.driver: Option<ControlDriverState>` +
  `ControlDelta::DriverChanged { revision, holder, driver_epoch }`；因为
  `projection_events_for_fact` 一律产出 `Delivery::Reliable`，该 delta 自动成为
  带 canonical cursor 的可靠 v2 事件。

### 2.2 ToolLedger 持有席位

- `ToolLedger` 新增 `driver_holder` / `driver_epoch`，`from_store` 在重放日志时
  由最后一条 `DriverChanged` 重建 —— daemon 重启后 epoch 不归零。
- `claim_driver(holder, stale_holder, event_id, causation_id, now)`：
  - 同一 holder → `AlreadyHeld`（不写 fact）；
  - 空席 → `Claimed`（epoch + 1，写 fact）；
  - 他人持有且 `stale_holder` 命中 → `Claimed`（显式接管）；
  - 否则 → `Busy`。
- `release_driver(holder, ...)`：仅 holder 可释放，释放也推进 epoch。
- `driver_state()` 暴露 `(holder, epoch)`。

### 2.3 domain / runtime

- `ControlCommand::DriverClaim { client_session_id, stale_holder }` /
  `DriverRelease { client_session_id }`；两个身份字段由 daemon 从已认证 lease
  覆写，wire 值不可信。
- `ControlEvent::DriverChanged { holder, driver_epoch }`：canonical fact 的
  Ringing 双发，同时作为命令 receipt 的成功终态。
- `Loop::handle_driver_claim` / `handle_driver_release`：调 ledger，然后
  `DriverChanged`（成功）/ `OperationCompleted`（AlreadyHeld）/
  `OperationFailed{driver_busy|not_driver|driver_*_failed}`。

### 2.4 daemon

- **删除** `RingingDriverStore`（不再有第二真源）。
- `V2ProjectionHub::driver_state`：只读 control 投影的轻量访问器。
- `driver/claim`：
  - 已是 holder → `already_holder`（同步权威）；
  - 他人且 lease 存活 → `driver_busy`（同步权威）；
  - 空席 / holder lease 已死 → 转发 `DriverClaim`，返回
    `accepted: true, reason: "claim_requested"`；新席位经 `DriverChanged` 到达。
- `driver/release`：非 holder → `not_driver`（同步）；holder → 转发，
  `reason: "release_requested"`。
- bootstrap 的 `driver` 直接读 canonical 投影；lease 已死的 holder 呈现为
  `holder = null`。
- 命令准入 `not_driver` / `stale_driver_epoch` 改读 canonical 投影。
- 客户端若直接提交 `driver_claim` / `driver_release` 命令，daemon 覆写身份字段
  （不能替他人认领）。

### 2.5 契约变化（需要 TUI 配合）

claim/release 的 `reason` 语义变化：

```text
claim_requested   请求已转发，等待 DriverChanged
release_requested 请求已转发，等待 DriverChanged
already_holder    daemon 同步裁决
driver_busy       daemon 同步裁决
not_driver        daemon 同步裁决
```

`accepted = true` 不再等价于「席位已变更」，`holder` / `driver_epoch` 是**请求时**
的服务端视图。TUI 必须按既有规则「driver 状态只从 bootstrap / DriverChanged
更新」，在 `*_requested` 期间显示进行中而不是乐观落位。

## 3. 验证证据

```text
cargo test --workspace -- --test-threads=1            PASS（137 suites，0 failed）
cargo clippy --workspace --all-targets -- -D warnings PASS
scripts/v2-smoke.sh                                   PASS（可重复）
```

实机 smoke 现在走完整链路：daemon `session_create` → 把 canonical log 播进该
session → 真实 actor 处理 `DriverClaim` → canonical log 出现
`driver_changed`：

```text
"driver_changed","data":{"holder":"eb6f…","driver_epoch":1,...}
"driver_changed","data":{"driver_epoch":2,...}      # release
```

新增/更新用例：

```text
qaqh-session  tool_ledger::driver_seat_epoch_is_monotonic_and_survives_reopen
qaqh-daemon   v2_driver_gate_rejects_non_driver_and_stale_epoch
qaqh-daemon   v2_driver_busy_claim_is_rejected_before_dispatch
qaqh-daemon   v2_bootstrap_reports_canonical_driver_state
```

## 4. 仍未完成（alpha 迭代清单）

### A. driver 后续

1. **lease 过期主动移交**：已落地（3s 巡检、CAS、持久化 watch、重启回收；
   见 `2026-09-24-ringing-v2-driver-lease-reclaim-handoff.md`）。
2. **优先级/能力策略**：`not_eligible` 与显式移交优先级未定义（仍需裁决）。
3. **`driver_epoch` 参与指纹**：已落地，见 alpha 收口 §2.6。
4. **workspace 类命令的 gate 集合**：已补 seed-scoped service 写操作；
   全局 workspace registry 写不纳入（无 seed 归属）。

### B. P0-6 剩余 fixture

1. reliable / replaceable / ephemeral transcript fixture。
2. `ResetRequired` 剩余 reason（`cursor_expired` / `snapshot_missing` /
   `cross_session` / `v1_epoch_mismatch` / `replay_overflow`）。
3. concurrent answers 的 permission / plan 变体（当前只有 ask 路由级用例）。
4. driver handover 的 reliable 事件断言（`DriverChanged` 已可靠，但缺
   「接管后旧 epoch 命令被拒」的端到端 fixture）。
5. **v1 `Last-Event-ID` → v2 cursor 服务端映射**（未实现）。
6. Windows alpha 共用 fixture。

### C. P1 未开工

1. `/ringing/v2/service/{method}`。
2. `/ringing/v2/content`。
3. timeline v2 完整分页与重连。

### D. 其它落地细节

1. permission 的 `trust_folder` 未进 canonical decision。
2. `CommandOptions.driver_epoch` 是结构体字面量破坏性变更，TUI pin bump 时同步。
3. CNB Prepare 阶段 CPU 配额问题未解决，仍以本地全量门禁为准。

## 5. 接手注意

- driver 席位只有一个写者：session actor 的 `ToolLedger`。daemon 不得再引入
  第二份可变席位状态。
- `DriverChanged` 是 CONTROL_ONLY canonical fact；v1 折叠面刻意不消费它。
- 新 driver 语义必须同时覆盖 bootstrap、reliable 事件、重连三条路径。
- 不要 bump `SESSION_FACT_PAYLOAD_VERSION`。
