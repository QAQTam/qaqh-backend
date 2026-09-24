# canonical writer fence 退出释放 Handoff（2026-09-24）

状态：**已落地**。承接
`docs/handoff/2026-09-24-ringing-v2-driver-lease-reclaim-handoff.md` §5-A（补坑时
发现）。

## 1. 问题

canonical log 的写权是排他 writer fence（`writer-fence.json` + 租约 TTL
`tool_ledger_lease_ms()`，默认 **30s**）。`ToolLedger::release_writer_lease` 之前
只有 recovery executor 调用——**session actor 正常退出时不释放 fence**。

后果（影响面远大于 driver）：

- daemon 重启 / 会话关闭后，下一个 actor（或另一个进程）在整段 TTL 内拿不到
  writer lease，`ToolLedger::open` 直接 `WriterBusy`；
- 运行期表现为该会话 **任何 canonical 写入**都被拒：tool intent / finished、
  interaction request / resolved、driver 回收……即重启后头 30 秒会话 ledger 不可用；
- 之前 recovery executor 的注释已描述过同类现象（「恢复只借用 fence……否则 resume
  后一整个 lease 窗口内所有工具都会被 WriterBusy 拒成 LEDGER_BLOCKED」），但只修了
  recovery 自己那条路径。

## 2. 修法

`ToolLedger` 增加 `Drop` 实现：ledger 被丢弃时释放自己的 writer fence。

- 覆盖**所有**退出路径：actor 结束、会话切换（`tool_ledger = None`）、recovery
  批次、一次性 ledger——不需要在每个调用点记得手写 release。
- 释放走既有 `CanonicalSessionStore::release_writer`：只在自己仍是 fence 持有者
  （writer_id + generation_epoch + fencing_token 全匹配）时才写，天然幂等、不会
  误放别人的 fence。
- 失败只 `log::warn!` 并吞掉：`Drop` 不能 panic；失败时 fence 仍按 TTL 自然过期，
  退化为修复前的行为。
- **释放时间戳用 `i64::MIN`，不用 wall clock**：released fence 必须对任何读者都
  是「已过期」。若写入当前墙钟，而调用方用的是合成时钟（测试、回放），可能反而
  把过期时间推到调用方时钟之后，让 fence「复活」。

保留 recovery executor 里原有的显式 `release_writer_lease`（语义清晰、现在与 Drop
等价，重复释放是 no-op）。

## 3. 验证证据

```text
cargo test --workspace -- --test-threads=1            PASS（137 suites，0 failed）
cargo clippy --workspace --all-targets -- -D warnings PASS
scripts/v2-smoke.sh                                   PASS
```

新增用例：

```text
qaqh-session  tool_ledger::dropping_a_ledger_releases_the_writer_fence
```

实机证据（smoke，**默认 30s** ledger lease，未再缩短窗口）：

- 修复前：重启后的 driver 回收要等 ~30s（12 次派发才成功）。
- 修复后：重启后**第一次**巡检即成功（17s 总时长，其中包含 lease TTL 等待）。
- actor 退出后 fence 实测：

```json
{
  "writer_id": "agent-180468-53973dae",
  "generation_epoch": 3,
  "lease_expires_at_ms": -9223372036854775808
}
```

- daemon 日志：actor `exited` 后 1s 内新 actor `starting ... resume=Some(...)`
  并立即完成 canonical 写入。

## 4. 仍未完成（alpha 迭代清单）

1. **崩溃路径仍按 TTL 恢复**：`Drop` 只覆盖有序退出。进程被 SIGKILL / panic 到
   无法 unwind 时，fence 仍要等 TTL 过期。要更即时需要崩溃恢复接管（已有
   `rotate_writer_fence` / recovery 机制，可评估是否在 daemon 启动时对
   「无活 actor 的会话」主动轮转 fence）。
2. `recovery_executor` 的显式 release 现在是冗余的，可评估是否清理。
3. driver 侧剩余：回收延迟（3s 巡检）、`not_eligible`/优先级策略、
   `driver_epoch` 未进 command fingerprint、workspace 命令 gate 集合。
4. P0-6 剩余 fixture：reliable/replaceable/ephemeral transcript、ResetRequired
   剩余 reason、permission/plan 并发回答、v1 `Last-Event-ID` → v2 cursor 映射、
   Windows alpha。
5. P1 未开工：`/ringing/v2/service/{method}`、`/ringing/v2/content`、
   timeline v2 完整分页与重连。

## 5. 接手注意

- writer fence 的「释放」必须对所有时钟都表现为已过期；不要再改成写 wall clock。
- `Drop` 里不要做可能 panic 或可能重入加锁的操作（当前只做一次文件锁 + 原子写）。
- 新增 `ToolLedger` 生命周期路径时不需要再手动 release；但**崩溃路径**仍不在覆盖
  范围内。
