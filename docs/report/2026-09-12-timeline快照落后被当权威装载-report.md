# timeline 快照落后被当权威装载 → transcript 只剩第一条 user 消息（2026-09-12）

## 0. 元信息

| 项 | 值 |
|---|---|
| 报告日期 | 2026-09-12（UTC+8） |
| 分析对象 | 后端 `D:\project\QAQ-Harness` @ `b6e1d96` + 工作区未提交修复（BUG-2026-09-12-01）；前端 `D:\project\qaqh-tui-app` @ `f85c33d` + 工作区 WIP |
| 报告者 | QAQ-Harness 调试会话（AI 助手） |
| 触发方式 | 用户指定：分析「TUI 在**断线重连或重启**后只看到第一条 user 消息」 |
| 结论 | **P1 功能阻塞 / 数据可见性**。TUI 忠实显示后端给的快照；缺陷在后端：**合法但落后**的 `ringing-timeline/{seed}.json` 被当权威 restore，且恢复路径**没有任何对账**，因此落后状态跨重启自锁（永久不自愈）。真机实测 seed `6ffdb118`：归档 4 个回合，快照只有 1 个回合（第一条 user 消息、`rounds=[]`），客户端因此只剩第一条。触发源是 [BUG-2026-09-12-01](../buglist/2026-09-12-timeline死锁与debug桥token泄露-buglist.md)（持久化死锁，已修）；**结构缺陷独立存在，本次已修**。 |

## 1. 结论摘要

| ID | 严重度 | 状态 | 类型 | 位置 | 影响（一句话） |
|---|---|---|---|---|---|
| BUG-2026-09-12-04 | **P1** | 已确认 → **已修复（工作区，待提交；§5）** | 功能阻塞 / 数据可见性 | 后端 `crates/qaqh-runtime/src/ringing/timeline_hub.rs:242-268`（装载）、`:298-320`（落后判定）、`:331`（重建返回 bool） | 落后快照被当权威装载：daemon 重启 / TUI 断线重连 re-baseline 后只剩第一条 user 消息，后续回合**永久不可见**；TUI 侧无任何本地可恢复手段 |
| （放大面，未改） | P2 | open | 契约镜像 | 前端 `src/app/timeline_model.rs:346`（`apply`）、`:509`（`replace_from_page`） | ① 对**已存在回合**的 `TurnOpened` 直接忽略，与后端「原地 reopen」语义不镜像（显示的 user 文本与实际回答错位）；② re-baseline 无条件整体替换、不校验前后落差 |

## 2. 现象（用户可复现）

1. 会话已进行若干回合（本例 4 个 user 回合）。
2. TUI 断线重连（或 daemon/机器重启）。
3. transcript 只剩**第一条 user 消息**：没有回答、没有后续回合；再次重连/重启依旧是同一状态（不自愈）。

## 3. 证据链（真机，seed `6ffdb118`）

### 3.1 写侧事实是完整的，读侧投影被冻伤

| 来源 | 内容 | 时间 |
|---|---|---|
| `sessions/6ffdb118/messages.jsonl` | 481 行、**4 条 user 消息**（行 3 / 282 / 291 / 339），`user#1 = 146 chars / 300 bytes` | 更新至 02:30:53 |
| `sessions/6ffdb118/meta.json` | `message_count=481`、**`turn_count=4`**、`updated_at` → 02:36:37 | 02:36:37 |
| `ringing/ringing-timeline/6ffdb118.json` | **1 个回合**：`watermark=2`、`t1` = 第一条 user 消息、`sealed=true`、`state=completed`、**`rounds=[]`**（467 B） | **停在 01:22:43** |
| `ringing/timeline-audit/6ffdb118.jsonl` | **只有 1 行**：`{"seq":3,"ts":1789147364664,"type":"turn_opened","turn":"t1"}` | 01:22:44 |

```json
{"seed":"6ffdb118","snapshot":{"watermark":2,"turns":[{"turn_id":"t1","created_seq":1,
"user_text":"\"D:\\project\\QAQ-Harness\"对这个项目进行debug；…","sealed":true,
"state":"completed","rounds":[]}]},"journal":[]}
```

审计文件只有一行 = 该 seed 的持久化链路在 01:22:44 之后**再无任何写入**（快照文件 mtime 也定格）。

### 3.2 daemon 日志时间线（同一日志，时间已转本地时间）

```
01:21:17  daemon 启动：[ringing] lazy timeline index ready: 0 persisted timelines on disk
01:22:43  [INPUT] handle_user_input called, text_len=300        ← 300 bytes = 归档第一条 user 消息
01:22:43  [INPUT] emitting TurnStart turn_id=t1 round_num=0
01:22:43  SessionManager: replaying 1 WAL op(s) for 6ffdb118
01:22:43  [ringing] rebuilt timeline 6ffdb118 from persisted messages (BUG-006 fallback)
                ↑ 此刻归档只有这条 user 消息（回合刚被重投，尚无任何回答）→ 重建出 1 回合 / 0 rounds，
                  随后 store.persist 直接写盘（该路径不经 rehydrate，故没有死锁）
01:22:43  ERROR qaqh_message::store: MessageStore: WAL checkpoint failed for 6ffdb118: 系统找不到指定的文件。(os error 2)
01:22:43  [ringing] sealing orphan active turn t1 for 6ffdb118 (no terminal event)
01:22:44  ← 此后再无该 seed 的审计行/快照更新：
            异步 worker / persist_timeline_sync 首次走到 rehydrate_offloaded_turns
            → 同线程重入 timeline_store = BUG-2026-09-12-01 死锁（持锁死亡，锁被带进棺材）
01:22:43 → 02:30:53  t1 跑完 rounds 0..52；t2 (01:44:58) / t3 (01:52:17) / t4 (02:01:48)
                     —— 全部进了 messages.jsonl，**一个都没进 timeline 快照**
01:52:01 / 02:01:38 / 02:36:36  daemon 三次重启，每次都只看到 "1 persisted timelines on disk"
                     —— 即那条被冻伤的单回合记录，且此后每次都把它当权威 restore
```

### 3.3 客户端路径（为什么 TUI 会"忠实地"只显示第一条）

```
TUI runtime::timeline_stream（src/runtime.rs:445-502）
  └─ 每轮循环先取快照基线：GET /ringing/v1/sessions/{seed}/timeline?limit=60
      └─ daemon axum_impl/timeline_api.rs handle_timeline_snapshot
          └─ hub.timeline_snapshot(seed)                      (timeline_hub.rs:537)
              └─ ensure_timeline_loaded(seed)                 (timeline_hub.rs:190-282)
                  └─ load_seed → appender.restore(快照)        ← 落后快照在此被当权威
  └─ RuntimeMsg::TimelineRebaseline → app/mod.rs:424
      └─ timeline_model.rs:509 replace_from_page（整体替换、无条件信任）
          └─ render_transcript.rs:212 逐回合渲染（渲染层无过滤，模型里 1 个回合就只画 1 个）
```

要点：**渲染层与模型层都没有丢数据**——`sess.timeline.turns` 里就只有 1 个回合，因为服务端只给了 1 个回合。

## 4. 根因（三层，缺一不可）

1. **触发（已修）**：BUG-2026-09-12-01 的持久化死锁把快照冻结在「首回合刚开、尚无内容」那一刻，
   之后的回合无论跑多久都不再落盘（审计文件停更 = 同一事实的第二证据）。
2. **结构缺陷（本次修复）**：`timeline` 同时被当作「可重建投影」与「权威读侧」，而
   `ensure_timeline_loaded` 只在**文件缺失/损坏**时走 BUG-006 重建，**落后但合法**的快照直接 restore。
   判据缺失 → 落后状态跨进程自锁：即使触发源已修，既有冻伤快照仍会被永远装载。
3. **放大器（未改，见 §7-1）**：TUI 的 re-baseline 无条件整体替换模型，且对后端「原地 reopen
   （同 `turn_id` 换 `user_text`、清 rounds）」的语义不镜像——`apply` 里对已存在回合的
   `TurnOpened` 直接 return，界面上表现为"一直显示同一条 user 文本"。

为什么会「只剩第一条」而不是「剩最后几条」：事故快照是**首次 attach 时**按**当时归档**重建的，
当时归档只有那一条消息（回合正在被重投）。这解释了 mtime/watermark=2/rounds=[] 三个指纹。

## 5. 修复（后端，工作区）

`crates/qaqh-runtime/src/ringing/timeline_hub.rs`

1. 新增 `RECONCILE_TAIL_MESSAGES = 200`（:22）与 `persisted_timeline_is_behind()`（:298-320），**两级判定、廉价在前**：
   - **数量门**：`快照回合数 + 1 < meta.turn_count` 才继续（只读一个小 `meta.json`；`+1` 容忍"运行中回合已开、meta 尚未更新"）；
     不触发即判新鲜，零额外 I/O。
   - **同一性确认**：读归档尾部 200 条做投影，比较「归档最后一回合 `user_text` = 快照最后一回合 `user_text`」；
     快照为空而归档非空 → 落后。**只比尾部 ⇒ 幂等**：重建后的快照尾部必与归档一致，不会反复重建
     （这一条专门排除"重建窗口型快照天然少于 `turn_count`"造成的自激写盘）。
   - 无法判定（无 sessions / 无 meta / 归档投影不出回合）一律 `false`，保持"宁可少动"。
2. 装载前判落后 → **先** `rebuild_timeline_from_messages()` 再决定是否 restore（:242-268）。
   顺序关键：先 restore 会被 `rebuild_*` 内部的 `contains` 幂等门挡掉，造成"内存旧、磁盘新"分叉。
   重建成功 → 收尾孤儿 running turn（必要时同步落盘）→ 返回；重建无源可依 → 记 warn 并退回装载旧快照。
3. `rebuild_timeline_from_messages()` 返回 `bool`（:331）：调用方可区分「已用重建结果接管」与「无可重建源」。

## 6. 验证

```
cargo test -p qaqh-runtime --test timeline_stale_restore        # 新增 2/2 passed
cargo test -p qaqh-runtime --test timeline_rebuild              # BUG-006 回归 1/1 passed
cargo test -p qaqh-runtime --lib                                # 174 passed
rustfmt --edition 2024 --check（改动两文件）                     # clean
cargo clippy -p qaqh-runtime --all-targets                      # 无新增告警（既有告警见 §7-3）
```

新增 `crates/qaqh-runtime/tests/timeline_stale_restore.rs`：

- `stale_timeline_snapshot_is_rebuilt_from_messages_instead_of_restored`：
  按事故形状灌入落后快照（1 回合 / `rounds=[]` / `watermark=2`）→ 期望 `timeline_snapshot()` 返回 3 回合、
  末回合 = 归档最后一回合；**并断言自愈落盘**（重新读文件 `turns.len()==3`），再起一个 hub 复读仍正确。
- `windowed_but_tail_consistent_snapshot_is_restored_without_rebuild`：
  尾部一致的窗口化快照（2 回合 < `turn_count=3`）必须原样装载，`watermark=4242` 不得被重写 —— 锁住"不自激"。

## 7. 遗留与后续（未做，建议按序处理）

1. **TUI 契约镜像（P2，建议做）**：`src/app/timeline_model.rs:346 apply()` 对已存在回合的
   `TurnOpened` 应镜像后端 reopen 语义（刷新 `user_text`；原回合已 sealed 则清 rounds、`state=Running`、
   `failure=None`）。否则后端原地 reopen 后，界面会一直显示旧 user 文本、并把新回答挂在旧回合下。
2. **窗口型快照的 `has_more` 语义（P2）**：重建只物化 `REBUILD_RECENT_TURNS=40` 个回合，
   而客户端 `has_more` 由数组本身推导 → 窗口之前的历史在 UI 里**无法翻页找回**（只能读 `messages.jsonl`）。
   建议：快照记录"是否有更早历史"（例如持久化侧加 `truncated`/`covered_turns`），`paginate_turns` 透出。
3. **沿用 01 的遗留**：`enable_turn_offload` 死代码 + `drop(store)` 空操作告警（`timeline_hub.rs:472`）、
   offload 回调与持久化路径的 ABBA 锁序（启用 offload 前必须处理）；另 `timeline_intent_is_terminal`（`:562`）当前无调用者。
4. **既有冻伤数据的清理（运维）**：修复只在**下次装载/重启**时自愈——`6ffdb118` 这类记录需要
   至少一次 daemon 重启（或该 seed 的首次 attach）才会重建为完整窗口；也可直接删掉该 seed 的
   `ringing-timeline/{seed}.json` 强制走重建。
