# Ringing v2 driver lease 过期回收 Handoff（2026-09-24）

状态：**driver 席位过期自动移交已落地**，实机 smoke 通过。承接
`docs/handoff/2026-09-24-ringing-v2-canonical-driver-changed-handoff.md` §4-A-1。

## 1. 问题

canonical 席位落地后，holder 的 lease 过期只在下一次 claim 时经 `stale_holder`
接管。对**已连接但不重新 bootstrap** 的观察者来说，席位看起来仍被死 holder
占着，规格 §9.3 的「服务端可按自身策略自动移交」没有落地。

## 2. 落地内容

### 2.1 回收 CAS

- `ControlCommand::DriverRelease` 新增 `expected_epoch: Option<u64>`。
- `ToolLedger::release_driver(holder, expected_epoch, ...)` 新增
  `DriverReleaseOutcome::StaleEpoch { driver_epoch }`：holder 匹配但 epoch 已前进
  → no-op，不写 fact。
- 这条 CAS 是必需的：回收是「读取席位 → 异步下发」的两步操作，期间 holder 可能
  重新认领（epoch 前进），没有 CAS 会误踢新 holder。

### 2.2 daemon 回收任务

- `AppState.driver_watch`：**待扫描 seed 列表**（不是席位状态）。在
  `driver/claim` 请求与 bootstrap 观察到 holder 时登记；席位变空后移除。
- 复用既有的 3s 周期任务（与僵尸 receipt 巡检同一 tick）调用
  `reclaim_dead_driver_seats`：
  - 读 canonical 席位；
  - holder 为空 → 移出扫描列表；
  - holder lease 仍活 → 跳过；
  - holder lease 已死 → 直接向 `QaqhService::send_ringing_command` 下发
    `DriverRelease { client_session_id: <dead holder>, expected_epoch: Some(n) }`
    （特权路径：绕开 daemon lease 校验，因为 holder 已经死了，走正常
    `handle_command` 会被 401 拦住）。
- 结果由 actor 落 canonical `DriverChanged { holder: null, driver_epoch: n+1 }`，
  观察者经 reliable 事件/bootstrap 看到席位空出。

### 2.3 客户端侧

- 正常 release 也带 `expected_epoch`（daemon 从读取到的 canonical epoch 填），
  语义与回收一致。
- 客户端直接提交 `driver_release` 命令时，daemon 覆写 `client_session_id` 与
  `expected_epoch`，不能替他人释放。
- 客户端不需要区分「显式 release」与「过期回收」，只跟随 `DriverChanged`。

## 3. 验证证据

```text
cargo test --workspace -- --test-threads=1            PASS（137 suites，0 failed）
cargo clippy --workspace --all-targets -- -D warnings PASS
scripts/v2-smoke.sh                                   PASS（可重复）
```

smoke 现在把完整生命周期跑一遍（daemon 用 `QAQH_TEST_LEASE_TTL_MS=6000`）：

```text
driver_changed epoch=1 holder=<A>     # claim
driver_changed epoch=2 holder=null    # 显式 release
driver_changed epoch=3 holder=<A>     # 重新 claim
driver_changed epoch=4 holder=null    # lease 过期，daemon 自动回收
```

daemon 日志：

```text
[ringing-v2] reclaiming driver seat for <seed>: holder lease expired at epoch 3
```

新增/更新用例：

```text
qaqh-session  tool_ledger::driver_seat_epoch_is_monotonic_and_survives_reopen
              （新增 StaleEpoch CAS 断言）
scripts/v2-smoke.sh  driver auto-reclaim on lease expiry
```

## 4. 仍未完成（alpha 迭代清单）

1. **daemon 重启后重建扫描列表**：`driver_watch` 是内存列表，重启后为空；此时
   过期席位要等下一次 claim 走 `stale_holder` 接管。可在启动时对已加载会话做一次
   惰性扫描，或把扫描列表持久化。
2. **回收延迟**：受 3s 巡检周期与 lease TTL 影响，席位最长空转 ~TTL+3s。
   需要更即时可改为 lease 过期事件驱动。
3. `not_eligible` 与显式移交优先级策略未定义（谁能优先接管）。
4. `driver_epoch` 未进 command fingerprint。
5. workspace 类命令的 gate 集合未纳入（当前 gate = Conversation 全量 +
   session/skill/tool-mode 控制）。
6. P0-6 剩余 fixture：reliable/replaceable/ephemeral transcript、ResetRequired
   剩余 reason、permission/plan 并发回答、v1 `Last-Event-ID` → v2 cursor 映射、
   Windows alpha。
7. P1 未开工：`/ringing/v2/service/{method}`、`/ringing/v2/content`、
   timeline v2 完整分页与重连。

## 5. 接手注意

- `driver_watch` 只是扫描列表，**不得**演变成第二份席位状态；席位真源永远是
  canonical `DriverChanged`。
- 任何「先读席位、后异步下发」的回收路径都必须带 `expected_epoch` CAS。
- 特权回收绕过 daemon lease 校验，只允许在「holder lease 已判定死亡」后使用。
- 不要 bump `SESSION_FACT_PAYLOAD_VERSION`。
