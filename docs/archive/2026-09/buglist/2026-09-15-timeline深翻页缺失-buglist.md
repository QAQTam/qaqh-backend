# buglist（2026-09-15）— timeline 深翻页缺失：归档回合够不到

> 登记规则：一行一个缺陷；**详情进 `docs/report/`**，本文件只做索引与状态跟踪。
> 状态口径：`open` / `fixed（工作区，待提交）` / `fixed @{commit}` / `verified` / `wontfix`。
>
> 本条目**未单开 report**（证据自足并逐条内联于下）。
> 发现来源：TUI 侧 T-08 闭环时拆出——T-08 只做到「诚实可观测」，这一条是它的另一半。
> 相关：TUI `docs/buglist/2026-09-15-TUI已知缺陷跟踪-buglist.md` T-08。

## 缺陷

| ID | 状态 | 项 |
|---|---|---|
| BUG-2026-09-15-05 | `fixed @a9531ce`（TUI `084a489`） | 长会话（回合数 > `REBUILD_RECENT_TURNS`=40）在 timeline 被重建后，**归档里的更早回合没有任何接口能取到**：分页只在已物化的 snapshot 内翻。T-08 之后这一状态**可见**了（`truncated_before`），但仍然**不可达** |

## 事实与证据

调用链：

| 位置 | 事实 |
|---|---|
| `timeline_rebuild.rs:18` | `REBUILD_RECENT_TURNS = 40`，重建只物化最近 40 轮 |
| `timeline_rebuild.rs:36` | `load_recent_for_projection(seed, 200)` —— 尾部有界读取，Phase 2 有意为之（GB 级归档的重建从 O(文件) 降到 O(尾部)） |
| `timeline_api.rs::handle_timeline_snapshot` | `paginate_turns(snapshot.turns, before_turn, limit)` —— 分页**只在物化 snapshot 内**；`before_turn` 落不到的位置一律当作窗口尾（`unwrap_or(turns.len())`） |
| `timeline_hub::persisted_turn_count` | 真实回合数**读得到**（`meta.turn_count`），但只用于报告，未用于取数 |

后果：200 轮的会话，timeline 重建后客户端只能拿到最近 40 轮；往回翻到窗口开头即
`has_more=false`。T-08 让前端能提示「更早的回合未包含在本窗口（仅存于 daemon 归档，
当前无法翻到）」，但用户**读不到**那 160 轮。

触发条件（非罕见）：
- timeline 文件缺失/损坏/落后（`persisted_timeline_is_behind`）→ 重建；
- 长会话（> 40 轮）本就常见。

## 处置（闭环，2026-09-15）

三处改动（详见 `a9531ce` 的提交信息）：

1. **transcript 改读归档，不读 compact 视图**（新增 `SessionManager::load_archive_tail`）。
   timeline 是**人类 transcript**，compact 视图是**模型可见面**——后者刻意 shadow 掉被
   替换的区间。业界三家同款取舍：codex 的 `HistoryReplacement` 只换模型面、grok-build
   把 `chat_history.jsonl`（模型，整体重写）与 `updates.jsonl`（UI/replay，append-only）
   分成两个文件、deepseek-harness 的 `surface.ts` 直接写「the model-visible surface …
   is the wrong source for a human transcript」。顺带修掉「`[Compacted N turns]` 摘要
   在 UI 上变成一条用户消息」这个可见缺陷。

2. **游标改全局回合序号**（`TimelineTurn.turn_index` + `before_index`，替换
   `before_turn`）。上面「坑 1」的建议是「必须显式验证」——**验证结论是根本不能对齐**：
   worker 的计数器会复用（`TimelineAppender::open_turn` 的 reopen 注释里记着实测的
   `t14` 重启重号），而归档投影的 id 又只是「已加载消息池内的下标」——池大小取决于这次
   读了多少归档，同一回合在不同池里就是不同的 `t{n}`。故改为**全局派生**
   （`t{base + i + 1}`），任何读取窗口对同一回合都给同一个 id。

3. **有界取数**：需要的回合数可精确算出（`total - start`），据此按需加倍读归档尾部，
   触顶（4000 条消息）时如实报 `truncated_before` 并**停止宣称「还能翻」**——否则客户端
   会永远请求同一个空页（BUG-2026-09-13-18 那一族）。**不**建偏移索引：与
   `bounded_read` 模块文档「拒绝持久化字节偏移索引」的既有决策一致。

**与建议的一处偏离**：建议说「实现后 `truncated_before` 应自然收敛为 `false`」——
**未照做，且不必做**。它的定义（窗口未覆盖到开头）保持不变即可：前端把
`has_more` 与它渲染成互斥分支（`if has_more … else if truncated_before`），而
`has_more` 的语义已升级为「还有更旧的**且可达**」，于是那条警告自然不再出现。
反过来改 `truncated_before` 的定义要动 `window_metadata` 的四条契约测试，收益为零。

**回归锁**：新增 `tests/timeline_deep_paging.rs`（60 轮会话重建后逐页翻到第 1 轮，
并断言 id 是全局派生而非池内下标 `t1..t40`）；既有 `timeline_stale_restore.rs`
（窗口化但尾部一致不得触发重建）、`timeline_rebuild.rs`（offload/rehydrate）、
`window_metadata` 4 条、BUG-2026-09-13-18 的 HTTP e2e 全部保持绿。

## 原始建议（留档）

给分页加一条**读穿归档**的回退：当 `before_turn` 落在物化窗口开头之前，或
`truncated_before` 为真且请求方向已到窗口首，则直接向 `messages.jsonl` /
`compact-context.json` 投影该游标附近的页并返回，**不**改动常驻 snapshot（避免把
Phase 2 的有界读取退化回全量）。

要点与坑：
1. **turn_id 对齐**：归档投影的 id 由 `projection::build_turns` 生成（turn_id 为空时
   合成 `t{n}`），常驻 snapshot 重建时走的是同一函数，理论上可对齐——但**必须显式
   验证**，否则客户端拼页会错位。
2. **基线一致**：归档投影须与 conversation snapshot 同基线（compact 优先），
   `persisted_conversation_state` 已如此，勿另起一套。
3. **rehydrate/offload**：常驻 snapshot 只留有界壳，正文按页 rehydrate；
   读穿归档的页同样要走 `rehydrate_timeline_page` 或等价的正文恢复。
4. **不变的契约**：实现后 `has_more` 仍须满足 `has_more ⇒ 本页非空`
   （BUG-2026-09-13-18）；`truncated_before` 届时应收敛为 `false`（历史可达了）。
   前端那两条提示随之自然消失——**不要**提前为它加特判。

优先级：P2。不影响正确性（数据没丢，归档里都在），影响长会话的可用性。
