# buglist（2026-09-12）— timeline 快照落后被当权威装载

> 登记规则：一行一个缺陷；**详情进 `docs/report/`**，本文件只做索引与状态跟踪。
> 状态口径：`open` / `fixed（工作区，待提交）` / `fixed @{commit}` / `verified` / `wontfix`。
>
> 姊妹文件：[`2026-09-12-timeline死锁与debug桥token泄露-buglist.md`](./2026-09-12-timeline死锁与debug桥token泄露-buglist.md)
> （本条目是那条 P0 死锁的**下游后果**：死锁已修，但它冻伤的快照仍在毒害恢复路径。）

| ID | 严重度 | 状态 | 类型 | 位置 | 影响（一句话） | 报告 |
|---|---|---|---|---|---|---|
| BUG-2026-09-12-04 | P1 | fixed（工作区，待提交） | 功能阻塞 / 数据可见性 | `crates/qaqh-runtime/src/ringing/timeline_hub.rs:242-268`（装载路径）、`:298-320`（新增落后判定）、`:331`（重建返回 bool） | **合法但落后**的 `ringing-timeline/{seed}.json` 被当权威 restore 且永不修复：daemon 每次重启、TUI 每次断线重连 re-baseline 后，会话 transcript 只剩第一条 user 消息（实测 4 回合只剩 1 回合），其余回合永久不可见 | [`docs/report/2026-09-12-timeline快照落后被当权威装载-report.md`](../report/2026-09-12-timeline快照落后被当权威装载-report.md) |

## 详细状态

### BUG-2026-09-12-04（落后快照被当权威装载）

- **现象**：TUI 在**断线重连**或 daemon **重启**后，会话里只看到第一条 user 消息（无回答、无后续回合）。
- **引入**：非单一提交引入；是「timeline 是可重建投影」与「快照即权威」两条语义在 `ensure_timeline_loaded` 里的接缝缺陷
  （只在**文件缺失/损坏**时重建，落后但合法的快照直接 restore）。**触发**是 BUG-2026-09-12-01 的持久化死锁：
  它把 6ffdb118 的快照冻结在 `01:22:43`（首回合刚开、尚无内容）且此后不再更新。
- **修复**：装载前做两级落后判定（数量门 `meta.turn_count` → 尾部同一性确认），落后则走既有 BUG-006 重建路径后再装载；
  `rebuild_timeline_from_messages` 返回 `bool` 以区分「已接管」与「无源可依，退回旧快照」。
- **验证**：新增 `crates/qaqh-runtime/tests/timeline_stale_restore.rs` 2/2 passed；既有 `timeline_rebuild.rs`（BUG-006 回归）1/1 passed；
  `cargo test -p qaqh-runtime --lib` 174 passed；`rustfmt --check` 两文件绿；`cargo clippy -p qaqh-runtime --all-targets` 无新增告警。
- **遗留（未做，见 report §7）**：
  1. TUI 侧 `TimelineModel::apply` 对已存在回合的 `TurnOpened` 直接忽略，与后端「原地 reopen」语义不镜像（同一族错位症状）；
  2. 重建窗口型快照的 `has_more` 语义（客户端无法翻到窗口之前的历史，只能读 `messages.jsonl`）；
  3. `enable_turn_offload` 仍是死代码 + 持久化路径 ABBA 锁序（沿用 01 的遗留项）。
