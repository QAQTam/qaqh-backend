# handoff：main 编译回归修复 + 远端工单/PR 全量收口（2026-09-14）

> 交接对象：下一个接手 qaqh-backend 开发循环的 agent 或人。
> 关联：流程手册 [`2026-09-13-CNB-NPC全流程开发管线-handoff.md`](../../../handoff/2026-09-13-CNB-NPC全流程开发管线-handoff.md)、
> 主报告 [`docs/archive/2026-09/report/2026-09-12-多会话高频输出热路径串行化与切会话401-report.md`](../report/2026-09-12-多会话高频输出热路径串行化与切会话401-report.md)

## 0. 最终状态（2026-09-14 18:25）

- **`main = cf2ceb8`**，干净 worktree 实测 `cargo check --workspace --all-targets` **无 error**。
- **open issue = 0，open PR = 0**（远端工单与合并请求已全部清空）。
- `cargo fmt --all --check` → **0 diff**。
- 本地遗留：未推送分支若干（见 §5）+ 两个**早前会话**留下的未跟踪文档
  （`docs/buglist|report/2026-09-14-timeline工具块内存放大-*`，记录 4 个**仍 open** 的
  timeline 内存缺陷 BUG-2026-09-14-01..04，**不在本次范围，未动**）。

本次共合并 6 个 PR：#79 / #80 / #76（#31）/ #78（#28）/ #81（handoff）/ #82（#38 fmt）。

## 1. 一句话交接

**`origin/main` 曾是坏的（自身编译不过），这是所有 PR 阻塞的总根。**
已修复并合入：`278e7ed` + `932cb04`（tool_outbox 回归）→ `fe4da88`（PR#76 / #31）
→ `33e6261`（PR#78 / #28）→ `cf2ceb8`（fmt / #38）。

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
| PR#78 `33e6261` | #28 的 rebase 收口：三处冲突（`hub.rs` 取 PR 侧的 `..` 通配；`timeline_hub.rs` **取 main 侧删除死函数 `timeline_intent_is_terminal`**；probe 取 PR 侧语义并修 `&seed` 类型错）；补 reviewer 建议 B 的竞态用例 |
| PR#82 `cf2ceb8` | `cargo fmt --all` 收口 127 处既有格式差异（47 文件，纯格式） |

**关单**：批量关闭 10 个「PR 已合但 issue 未关」的工单（#6/#7/#8/#10/#11/#12/#13/#24/#30/#39）——
每条修复提交都以 `git merge-base --is-ancestor <sha> origin/main` **逐条验证**后才关；
另关闭 #31、#28、#38。**#38 未直接关**：先实测 `cargo fmt --all --check` 确有 127 处未收口，
做完才关（不把「有单就关」当流程）。

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

## 5. 当前快照（2026-09-14 18:25）

- `main = cf2ceb8`（origin 同步），本地工作区干净（仅两个早前会话遗留的未跟踪文档，见 §0）。
- **open PR = 0；open issue = 0**（两个列表均已清空）。
- 本地未推送的分支（可清理，均已合并或已废弃）：
  - `wip/bug-2026-09-14-read-image`（`eeed218`，含 read_image 并行/图片降级修复
    BUG-2026-09-14-01/02/04 + 本地循环改动）——**待定去向**，见 §6.1；
  - `fix/main-tool-outbox-compile`、`fix/main-tool-outbox-executed-ids`、
    `pr76-rebase`、`pr78-rebase`、`chore/fmt-2026-09-14`——均已 squash 合入，可删。
- 遗留 stash：`stash@{0}`（multi-timeline + image_models，早前会话遗留）。

## 6. 下一步（建议顺序）

### 6.1 待决：`wip/bug-2026-09-14-read-image`

该分支含 **read_image 并行/图片降级**的真实修复（BUG-2026-09-14-01/02/04，见
`docs/buglist/2026-09-14-read_image并行与图片降级-buglist.md`），但混入了两处**错误产物**：
`loop_core.rs:477` / `state/lifecycle.rs:160` 的 `flush` → `flush_seed` 改名
（`flush_seed` 在任何版本都不存在；正确名字是 `flush`）。

→ 若要提 PR：**先丢弃那两处改名**，只保留 gate/message 的图片修复。

### 6.2 下一批候选工单（未建档，来自早前会话的未跟踪文档）

`docs/buglist/2026-09-14-timeline工具块内存放大-buglist.md` 记录了 4 个**仍 open** 的
timeline 内存缺陷（均已带 `file:line` 与实测数据）：

| ID | 级别 | 一句话 |
|---|---|---|
| BUG-2026-09-14-01 | P2 | `TimelineTool.summary` 直接 clone `output`（实测 100% 重复，绕过 512 字符契约） |
| BUG-2026-09-14-02 | P1 | `append_tool_progress` 无上限（实测单块 4,177,593 字符） |
| BUG-2026-09-14-03 | P1 | `enable_turn_offload` 是死代码（治本开关从未接线） |
| BUG-2026-09-14-04 | P2 | `journal_entry_payload_bytes` 漏算 `ToolUpdated`（预算形同虚设） |

建议按该文档 §「修复优先级」推进（-01/-04 改动最小可立即做）。

### 6.3 流程提醒

- 派发/收割前先确认 **main 可构建**（本次教训：main 坏了会把所有 PR 一起阻塞）。
- CNB squash **不会**自动关单：合并后必须
  `issues update-issue --number N --state closed --state-reason completed`。
