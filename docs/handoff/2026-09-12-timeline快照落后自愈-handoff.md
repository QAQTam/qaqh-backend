# handoff：timeline 落后快照自愈（2026-09-12）

> 关联：[`docs/report/2026-09-12-timeline快照落后被当权威装载-report.md`](../report/2026-09-12-timeline快照落后被当权威装载-report.md)、
> [`docs/buglist/2026-09-12-timeline快照落后被当权威装载-buglist.md`](../buglist/2026-09-12-timeline快照落后被当权威装载-buglist.md)、
> [`docs/buglist/2026-09-12-timeline死锁与debug桥token泄露-buglist.md`](../buglist/2026-09-12-timeline死锁与debug桥token泄露-buglist.md)

## 1. 一句话交接

TUI「断线重连/重启后只剩第一条 user 消息」的根因不在 TUI：后端把**合法但落后**的 timeline 快照当权威
装载且永不修复。工作区已加**装载前落后判定 + 走既有 BUG-006 重建路径**，测试全绿；**TUI 侧的契约镜像
（reopen 语义）尚未动**，是本 handoff 的第一待办。

## 2. 本次改动清单（工作区，未提交）

| 文件 | 位置 | 变更 |
|---|---|---|
| `crates/qaqh-runtime/src/ringing/timeline_hub.rs` | `:22` | 新增 `RECONCILE_TAIL_MESSAGES = 200`（判定用尾部窗口） |
| 同上 | `:242-268`（`ensure_timeline_loaded` 的 `Some(persisted)` 分支） | **装载前**判落后；落后则先 `rebuild_timeline_from_messages` 再决定是否 restore；重建无源可依时记 warn 并退回旧快照 |
| 同上 | `:298-320` | 新增 `persisted_timeline_is_behind()`：数量门（`meta.turn_count`）→ 尾部同一性确认（归档末回合 `user_text` vs 快照末回合） |
| 同上 | `:331` | `rebuild_timeline_from_messages()` 返回 `bool`（区分"已接管"/"无源可依"） |
| 同上 | `:271` | `None` 分支调用改为 `let _ = …`（适配新返回值） |
| `crates/qaqh-runtime/tests/timeline_stale_restore.rs` | 新文件 | 2 个回归：落后快照自愈（含落盘 + 二次启动）；尾部一致的窗口化快照**不得**重建 |

**为什么顺序关键**：`rebuild_timeline_from_messages()` 内部有 `if !appender.contains(seed)` 幂等门。
若先 `restore` 再重建，重建会被该门挡掉 → 内存是旧快照、磁盘是新快照 = 分叉。所以判定与重建都必须在
`restore` **之前**。

**为什么两级判定**：`turn_count` 门极廉（一个小 `meta.json`）；但"重建窗口"（`REBUILD_RECENT_TURNS=40`）
本身就让长会话的快照回合数少于 `turn_count`，只有"尾部同一性"能区分"正常的窗口化"与"真的落后"，
否则每次启动都会重建 → 自激写盘。只比尾部 ⇒ 幂等。

## 3. 如何验证（含既有冻伤数据）

```powershell
cd D:\project\QAQ-Harness
cargo test -p qaqh-runtime --test timeline_stale_restore   # 期望 2 passed
cargo test -p qaqh-runtime --test timeline_rebuild         # 期望 1 passed（BUG-006 回归）
cargo test -p qaqh-runtime --lib                           # 期望 174 passed
rustfmt --edition 2024 --check crates/qaqh-runtime/src/ringing/timeline_hub.rs
```

真机验证（既有冻伤会话 `6ffdb118`，`%USERPROFILE%\.qaqh`）：

1. 先记档：`ringing\ringing-timeline\6ffdb118.json` = 1 回合 / `watermark=2`；`sessions\6ffdb118\meta.json` = `turn_count=4`。
2. 用**含本修复**的构建重启 daemon（或让该 seed 首次 attach）。
3. 期望日志出现：`[ringing] timeline 6ffdb118 is behind persisted messages (1 turns in snapshot) — rebuilding projection before restore`
   → 随后 `rebuilt timeline 6ffdb118 from persisted messages (BUG-006 fallback)` + `lazily loaded timeline 6ffdb118 (rebuilt)`。
4. 期望文件：快照回合数 > 1（尾部窗口 200 条消息 / 40 回合内），TUI 重连后能看到最近若干回合（不再是第一条）。
   注意：**窗口之前的更早回合受 §4-2 限制仍不可见**，属已知遗留。
5. 兜底手段：删除 `ringing-timeline\{seed}.json` → 强制走 BUG-006 重建（等价结果）。

## 4. 待办（按建议顺序）

1. **【P2，首选】TUI 镜像后端 reopen 语义**（`D:\project\qaqh-tui-app`）：
   `src/app/timeline_model.rs:346 apply()` 中对 `TimelineEvent::TurnOpened` 的处理目前是
   `if self.find_turn_mut(turn_id).is_none() { push } ; return None;` —— **已存在回合时直接忽略**。
   后端 `crates/qaqh-runtime/src/timeline.rs:166-213 open_turn` 的契约是「同 id 且已 seal → 原地重置
   （换 `user_text`、清 `rounds`、`state=Running`、`failure=None`）」。TUI 应镜像：刷新 `user_text`，
   若原回合为终态则清 `rounds` + 置 `Running`。建议加单测：先 `TurnSealed` 再用新文本 `TurnOpened` 同 id →
   断言 `user_text` 已更新、`rounds` 为空、`is_streaming()==true`。
   *风险点*：必须确认"同一连接的活流上不会有回放的旧 TurnOpened"——当前 TUI 的 timeline SSE 是严格 +1 光标 +
   gap 即 re-baseline（`src/runtime.rs:561-566`），`replace_from_page` 已把页内回合铺好，故活流上的同 id
   `TurnOpened` 语义上就是 reopen。实现时请把这条推理写进注释。
2. **【P2】窗口型快照的 `has_more` 语义**：重建只物化 40 回合，而客户端 `has_more` 由数组长度推导
   （`timeline_api.rs:5-26 paginate_turns`）→ 窗口前历史在 UI 里无法翻页。建议持久化侧记录覆盖范围
   （如 `covered_turns` / `truncated`），由 `handle_timeline_snapshot` 透出；注意兼容既有缓存文件（`serde(default)`）。
3. **【P3】沿用 BUG-01 遗留**：`enable_turn_offload`（死代码 + `drop(store)` 空操作，`timeline_hub.rs:472`）
   与 offload 回调 / 持久化路径的 ABBA 锁序；`timeline_intent_is_terminal`（`:562`）无调用者。
   另：`similar` 前次报告 §3.6-4 记录过同一处锁序风险。
4. **【P3】观测**：本次新增的两条 warn/info 行（`is behind persisted messages` / `lazily loaded timeline … (rebuilt)`）
   是"投影落后"的唯一外部可观测信号，事故复盘时优先 grep 它们。

## 5. 风险与注意事项

- 误判代价：判定只看"末回合 `user_text` 是否一致"。若**同一文本被连发两次**（例如连续两条"继续"）且快照恰好
  落后，则判定为"新鲜"→ 本次不重建（保守，不误伤）；反之若归档末条与快照末条文本不同则一定重建——
  重建是**幂等的可恢复动作**（重建源是 `messages.jsonl`），最坏情况只是多一次投影+写盘。
- 重建取的是**尾部窗口**（200 条消息 / 40 回合）：修复保证"最近回合回来"，不保证"全历史回来"（见待办 2）。
- 装载路径新增成本：一次 `meta.json` 读（全部 seed）；仅当数量门触发才追加"尾部 200 条读 + 投影"（窗口化长会话每次装载一次）。
- 不要把本修复理解为"持久化不会失败"：`persist` 失败仍只记日志（旁路设计），本修复只是让**落后可被检出并自愈**。
