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
scripts/v2-smoke.sh                                   PASS（可重复，含重启回收阶段）
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

## 4. 补刀：扫描列表持久化（同日）

`driver_watch` 原为纯内存列表，daemon 重启后为空，过期席位要等下一次 claim 走
`stale_holder` 接管。已改为 **`RingingDriverWatch`（`ringing-driver-watch.json`）**：

- 只在 `<data_dir>` 持久化 **seed 列表**，席位真源仍是 canonical
  `DriverChanged`；文件不可读时按空列表启动（最坏退化为旧行为）。
- 席位变空、或 seed 已无法扫描（会话被删）时自动移除，文件自清理。
- 巡检派发加 **15s 退避**（`RECLAIM_RETRY_COOLDOWN_MS`）：一次派发可能被运行时
  接受、但在 actor 内被拒（见下），sweep 观测不到失败，没有退避会每 3s 重发并
  产生一次失败事件。
- smoke 新增「重启后回收」阶段：持有席位 → kill daemon → 重启 → 不触碰会话，
  仅靠持久化列表回收（epoch 5 → 6）。

## 5. 仍未完成（alpha 迭代清单）

### A. 新发现：actor 退出不释放 canonical writer lease（已修）

> 已由 `docs/handoff/2026-09-24-canonical-writer-fence-release-handoff.md` 修复：
> `ToolLedger` 现在在 `Drop` 时释放 writer fence。

补坑过程中发现：`ToolLedger::release_writer_lease` **只有 recovery executor 调用**，
session actor 正常退出/关闭时不释放 writer fence。后果：

- daemon 重启后，新进程对该会话的 canonical 写入被上一个进程的 fence 挡住，
  直到 `tool_ledger_lease_ms()`（默认 **30s**）自然过期；
- 这不只影响 driver 回收，也影响重启后**该会话的任何 canonical 写入**
  （tool intent/finished、interaction 等），期间表现为 ledger 不可用。

建议修法：actor 退出路径（`spawn_agent` 返回前 / 会话关闭）调用
`ledger.release_writer_lease(now)`；同一 writer id 释放自身 fence 是安全的。
需要单独评估，因为它改变所有会话的 writer 生命周期语义。

当前缓解：smoke 用 `QAQH_TOOL_LEDGER_LEASE_MS=3000` 缩短窗口；回收退避保证不刷屏。

### B. driver 其它

1. **回收延迟**：3s 巡检 + 15s 退避已落地；若未来需要更即时，可改为 lease
   过期事件驱动（不是 alpha 阻塞项）。
2. `not_eligible` 与显式移交优先级策略未定义（谁能优先接管）。
3. `driver_epoch` 已进 command fingerprint（2026-09-24 alpha 收口）。
4. workspace service 写操作的 gate 已补：
   `workspace.set` / `move_session` / `detach` / `session.set_tool_mode`
   在 lease 归属校验后要求 live driver；非 holder 返回 403 `not_driver`。

### C. P0-6 剩余 fixture

1. reliable / replaceable / ephemeral transcript fixture。
2. `ResetRequired` 剩余 reason（`cursor_expired` / `snapshot_missing` /
   `cross_session` / `v1_epoch_mismatch` / `replay_overflow`）。
3. concurrent answers 的 permission / plan 变体。
4. **v1 `Last-Event-ID` → v2 cursor 服务端映射**（未实现）。
5. Windows alpha 共用 fixture。

### D. P1 未开工

`/ringing/v2/service/{method}`、`/ringing/v2/content`、timeline v2 完整分页与重连。

## 5. 接手注意

- `driver_watch` 只是扫描列表，**不得**演变成第二份席位状态；席位真源永远是
  canonical `DriverChanged`。
- 任何「先读席位、后异步下发」的回收路径都必须带 `expected_epoch` CAS。
- 特权回收绕过 daemon lease 校验，只允许在「holder lease 已判定死亡」后使用。
- 不要 bump `SESSION_FACT_PAYLOAD_VERSION`。
