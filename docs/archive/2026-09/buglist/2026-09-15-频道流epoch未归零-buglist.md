# buglist（2026-09-15）— 频道流 epoch 变化未归零 cursor

> 登记规则：一行一个缺陷；**详情进 `docs/report/`**，本文件只做索引与状态跟踪。
> 状态口径：`open` / `fixed（工作区，待提交）` / `fixed @{commit}` / `verified` / `wontfix`。
>
> 本条目**未单开 report**（缺陷小且证据自足，逐条写在下方；若日后需要展开再补）。
> 发现来源：TUI 侧 T-01 阶段一迁移的可行性核实（对照 `qaqh-tui-app` 将被删除的
> `runtime.rs::stream_rebuild` 时，逐条核对后端是否有等价实现）。

## 缺陷

| ID | 状态 | 项 |
|---|---|---|
| BUG-2026-09-15-01 | `fixed @a72ce0c` | `ChannelStream` 从不比较 `server_epoch`：daemon 重启（或租约重协商）换了 epoch 后，流会带着**旧 cursor** 去新 epoch 续传 |

## 事实与证据

帧 id 是 `{epoch}:{channel}:{seq}`，两个 epoch 的 `seq` 是**各自独立**的序列。
带着旧 cursor 去新 epoch 续传，等于停在一个新 epoch 从未有过的位置：daemon 既不
补帧也不报错，而客户端状态仍报 `Open` —— 即「伪健康黑障」。

**修复前**（`ea6063c`）：

- `crates/qaqh-client/src/sse.rs`：`ChannelStream` 结构体**无 `last_epoch` 字段**，
  `connect_once` 读取 `server_epoch` 但从不与上一次比较，也从不重置 `self.cursor`。
- `crates/qaqh-client/src/timeline.rs:172-197`：**`TimelineStream` 早已实现**同一
  语义（epoch 变化 → `recover_gap()` 重定基，失败则 `cursor = 0` 兜底）。

即该不变式在后端**只覆盖了两条流中的一条**。

## 修复

`a72ce0c`：

- 新增 `ChannelStream::reconcile_epoch(last_epoch, cursor, server_epoch)`
  （`crates/qaqh-client/src/sse.rs:201`），在 `connect_once` 构造 `Last-Event-ID`
  **之前**调用（`:117`）。抽成纯函数是为了可回归测试——真正的 `connect_once`
  需要一个活着的 daemon。
- 同时抽 `timeline.rs:36` `observe_epoch`（原为内联判定），语义不变，补测试。

## 回归锁

`cargo test -p qaqh-client`：

- `sse::tests::epoch_change_resets_cursor` —— epoch 变 → 归零
- `sse::tests::same_epoch_keeps_cursor` —— 同 epoch 重连**必须保留** cursor
  （否则每次抖动都从 0 重放整条频道）
- `sse::tests::first_connect_records_epoch_without_side_effects`
- `timeline::tests::{epoch_change_requires_rebaseline, same_epoch_does_not_rebaseline,
  first_connect_does_not_rebaseline}`

对端参照：TUI `runtime.rs::stream_rebuild`（随 `56c31e7` 删除）原先锁的正是同一
不变式 —— 「epoch 变 → 归零；仅 generation/cs 变 → 保留 cursor」。

## 残余与未验证

- **端到端未跑**：`tests/lease_renegotiation.rs`（覆盖「daemon 重启 → 流恢复」）
  仍是 `#[ignore]`，需 `cargo build -p qaqh-daemon`。本次未跑的客观原因：工作树
  正被 workspace/工具侧重构占用，编 daemon 会连带编进半成品。**该重构落定后应补跑**。
- `ChannelStream` 的 epoch 归零只覆盖**频道流**；`TimelineStream` 侧的
  `recover_gap` 路径同样依赖真实 daemon 才能端到端验证。
