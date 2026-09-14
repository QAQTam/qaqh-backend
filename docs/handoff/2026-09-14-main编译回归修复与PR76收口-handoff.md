# handoff：main 编译回归修复 + PR #76 收口（2026-09-14）

> 交接对象：下一个接手 qaqh-backend 开发循环的 agent 或人。
> 关联：流程手册 [`2026-09-13-CNB-NPC全流程开发管线-handoff.md`](./2026-09-13-CNB-NPC全流程开发管线-handoff.md)、
> 主报告 [`docs/report/2026-09-12-多会话高频输出热路径串行化与切会话401-report.md`](../report/)

## 1. 一句话交接

**`origin/main` 曾是坏的（自身编译不过），这是所有 PR 阻塞的总根。**
已修复并合入：`278e7ed` + `932cb04`（tool_outbox 回归）→ `fe4da88`（PR#76 收口）。
现在 `main = fe4da88`，`cargo check --workspace --all-targets` 在干净 worktree 实测无 error。
issue #31 已 closed。**剩余 1 个 open PR：#78（#28），仍 `code_conflict`。**

## 2. 根因（本次最大发现）

### 2.1 合并顺序 clobber：`#8` 覆盖 `#30` 的 tool_outbox

| 时间 | PR | 内容 |
|---|---|---|
| 15:45 | #71（Closes #30） | `tool_outbox` 锁分片 + fsync 合并写；新增 `flush` / `flush_in` / `set_fsync_hook` + `SessionWriter` 分片表 |
| 15:49 | #70（Closes #8） | 分支基于 **pre-#30** 的 `tool_outbox.rs` |

#70 合并时该文件被**整文件覆盖回旧版**（`483 +----`），删掉了 `flush`，
但 #30 加在 `loop_core.rs:477` / `state/lifecycle.rs:160` 的**调用点保留** → E0425。

而 #8 对 outbox 的改动其实有**两部分**：① 删掉 #30 的写入器（clobber）；
② 新增只读 API `executed_call_ids`（取消收割区分「已执行但结果丢失」与「从未执行」）。
**修这个坑要两边都照顾到**——只恢复 #30 会漏掉 ②，`admit.rs:64` 仍悬空
（我第一版 PR#79 就漏了 ②，靠 PR#80 补上；教训见 §4.1）。

### 2.2 PR #76 的 rebase 产物冲突解错

原分支基于 `d9f15a7`，其 rebase 把 `hub.rs` 解成**半新半旧**：字段声明取回旧的
`HashMap<Channel, HashMap<Seed, SeedChannelState>>`，而下文 `channel_shards` /
`shards_for` / `slot_if_present` / `seed_keys` 全按 main 的 `Arc<ChannelShards>`
两级锁表写 → 8 处类型不匹配；`subscribe()` 仍单键 `live.entry(channel)` 而 `live`
已按 `(channel, seed)` 分片 → E0308；`subscribe_channel` 方法在冲突中丢失但测试还在调。
另有 `flush_seed` 这个**任何版本都不存在**的函数名。

## 3. 本次做了什么

| 产物 | 内容 |
|---|---|
| PR#79 `278e7ed` | 恢复 #30 的 `tool_outbox` 分片写入器 |
| PR#80 `932cb04` | 补回 #8 的 `executed_call_ids`（main 至此恢复可构建） |
| PR#76 `fe4da88` | rebase 到修好的 main，正确解 `hub.rs`/`sse.rs` 冲突（**合并双方**：main 的 `Arc<ChannelShards>` 两级锁表 + #76 的 `live` 分片 / `live_channels` / `live_watermark` / `subscribe(channel,seed)` / `subscribe_channel`）；修 reviewer 两项阻断；采纳建议 4 |

**reviewer 两项阻断**（reviewer 已附实测复现，非静态推测，均在 `sse.rs::ShardedChannelStream::recv`）：

1. **空 `receivers` 立即 `return None`** → 活跃会话可能尚未 attach seed（先开 SSE、
   后经 SessionNew/SessionResume attach 的既有 TUI 时序），流被立即切断，之后
   attach 的 seed 事件永远收不到（分片前的频道单环无此问题，属**行为回归**）。
   → 改为挂起轮询（`IDLE_SHARD_POLL`），仅当 `refresh()` 确认会话失活才结束。
2. **某分片 `Lagged` 即 `return`** → 其它分片 `pending` 中 `stream_seq` 更小、本可
   安全交付的事件被一并丢弃。→ 改为先记 `pending_lag` 标志，待水位已覆盖的最小
   序号全部交付完再上报终止帧。
3. **建议 4**：租约对账按 `REFRESH_INTERVAL`（50ms）节流，不再逐事件取全局 lease
   锁 + clone `HashSet`（那正是本 PR 要消除的 per-event 全局锁竞争）。

回归新增：`sharded_stream_waits_when_no_seed_attached_yet`（阻断 1）、
`lagged_shard_does_not_drop_smaller_pending_events`（阻断 2）。

## 4. 踩坑记录（勿重复交学费）

1. **pwsh 没有 heredoc**：`git commit -F - <<'EOF'` 会解析失败，且**前一条
   `git add -A` 会被静默跳过**——我因此让 PR#79 漏带了 `executed_call_ids`。
   → 提交信息一律写临时文件再 `git commit -F <file>`；**提交后必须 `git show
   <sha>:<path> | grep` 复核内容真的进去了**（本次靠这步才发现漏提交）。
2. **pwsh 吞 stdout**：`Select-String` / 循环里拼字符串经常输出为空（handoff §6.5 老坑）。
   → 结果一律 `> $env:TEMP\x.txt` 再 `read`。
3. **Windows 无 `sh`**：`cancel_keeps_tool_results.rs::cancel_mid_batch_keeps_executed_tool_results`
   恒失败（`executed: [0,0,0,0]`，工具从未执行）。**这是环境项，不是回归**——
   判断前先看断言语义（此处断言「批确实跑起来了」）。
4. **rebase 冲突不要"选一边"**：`hub.rs` 的两侧改动是**正交**的（main 改 `channels`
   两级锁表，PR 改 `live` 分片），必须合并双方。同理 `sse.rs` 的 test 模块
   （main 的 timeline 测试 + PR 的 shard 测试）也必须都留。
5. **`git checkout` 会因未提交改动中止**，切分支前先 `git status`。

## 5. 当前快照（2026-09-14 17:55）

- `main = fe4da88`（origin 同步），本地工作区干净。
- open PR：**#78**（`perf/bug-2026-09-13-28-block-checkpoint-v2`，Closes #28），
  `blocked_on: code_conflict`，需按 §3 同样的方式 rebase + 解冲突。
- open issue：12 个（#6 #7 #8 #10 #11 #12 #13 #24 #28 #30 #31 #39 中 #31 已关）——
  其中 **#6/#7/#8/#10/#12/#13/#30/#39 的 PR 早已 merged，issue 却仍 open**：
  CNB squash **不会**自动关单，需手动
  `issues update-issue --number N --state closed --state-reason completed`。
  建议下一轮直接批量关掉这批「PR 已合但单未关」的。
- 本地未推送的分支：`wip/bug-2026-09-14-read-image`（`eeed218`，含 read_image
  并行/图片降级修复 BUG-2026-09-14-01/02/04 + 本地循环改动）、
  `fix/main-tool-outbox-compile`、`fix/main-tool-outbox-executed-ids`、`pr76-rebase`。
- 遗留 stash：`stash@{0}`（multi-timeline + image_models，早前会话遗留）。

## 6. 下一步（建议顺序）

1. 批量关掉「PR 已合但单未关」的 issue（§5 清单）。
2. 收口 **PR#78（#28）**：rebase 到 `fe4da88` → 解 `hub.rs` tests 模块 hunk 重叠
   （PR#76 已合，重叠面已缩小）→ 本地 `cargo test -p qaqh-runtime` + clippy →
   squash merge → 关 #28。
3. 决定 `wip/bug-2026-09-14-read-image` 的去向（提 PR 或并入后续批次）。
4. 把 `wip/bug-2026-09-14-read-image` 里 `loop_core.rs`/`lifecycle.rs` 的
   `flush` → `flush_seed` 改名**丢弃**（那是 §2.2 提到的错误产物，正确名字是 `flush`）。
