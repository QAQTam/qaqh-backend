# 刀 2：持久化收敛为"一条事件日志 + 投影"（EventLog 迁移计划）

> 状态：**阶段 1 已实现**（2026-08-18，PR `refactor/knife2-eventlog-journal-timeline`）
> 对应：`REFACTOR-ROADMAP.md` 刀 2、`bugfix-1.1-nextdev.md` BUG-005/006/007/011
> 目标判据：删除任一投影可重建；无"四套互不收敛"。
> 本文档基于对 `main @ d2c255f` 的全量只读审计（写入点/格式/读链均以行号标注）。

---

## 1. 现状：四套持久化（审计结论）

| # | 存储 | 文件 | 格式 | 写入点（见审计） | 读侧权威性 |
|---|---|---|---|---|---|
| ① | Timeline | `ringing-timeline/{seed}.json` | 单 JSON：`{seed, snapshot, journal: [TimelineEntry]}`，原子替换 | `TimelineStore::persist`（`timeline_store.rs:38-57`）；同步 terminal 写 `persist_timeline_sync`（`hub.rs:903-938`，**tokio 线程内同步 I/O+Mutex**）+ 异步 checkpoint 线程（`hub.rs:275-368`）+ 孤儿 seal + BUG-006 重建回写 | 前端 transcript **唯一权威**读侧 |
| ② | messages | `sessions/{seed}/messages.jsonl` | append-only JSONL（每行一个 `Message`）；compact 时 `rewrite_messages` 全量重写 | `store/mod.rs:45,61,79`（append_one/append_messages/rewrite_messages）；manager `save_one/save_append/save_full`（`manager.rs:432,505,561`） | session resume / conversation snapshot 读侧 **事实源** |
| ③ | compact | `sessions/{seed}/compact-context.json` | 全量 JSON `CompactContext{archive, messages, usage…}`，替换 | `write_compact_context`（`manager.rs:692-703`）；`save_compact_context/update_compact_context`（`manager.rs:177,205`） | resume 时**优先**于 ②（fail-closed 语义，BUG-007） |
| ④ | journal | `ringing/journal/{channel}/{seed}.jsonl` + `latest` 槽 | append-only JSONL（`JournalOp`，幂等）+ replaceable 槽；4MB 物理重写 | `JournalStore`（`journal_store.rs`）`append/checkpoint/compact/replaceable/rewrite`；hub `persist_*`（`hub.rs:1347+`，持 Mutex 同步 I/O） | SSE 三频道恢复 / cursor replay 权威 |

**互不收敛**：四套由三条独立链路写入（timeline 经 `paced_emitter→WriterEvent::Timeline→publish_timeline`；messages/compact 经 engine `flush_meta/snapshot_full→SessionManager.save_*`；journal 经 `publish_with_causation→persist_*`）。改任一不触发其它更新。

**已有事件溯源雏形**：
- `JournalStore`：append-only + 幂等 `JournalOp`，是**最接近统一日志的基底**（但只覆盖三频道、replaceable 独立槽、rewrite 会物理删行、有界）。
- `timeline_rebuild.rs`：timeline **可重建性已证明**（从 messages/compact 重构，但降级：全 sealed、turn 全 Completed、无逐 token delta）。
- `outbox.rs`：**与持久化无关**（每 SSE 连接发送队列），勿误判。

---

## 2. 设计决策（D-刀2-*）

- **D-1 唯一权威日志 = ①的 Timeline 事件 并入 ④的三频道 JournalStore**。
  理由：journal 已是 append-only + 幂等事件日志，且 timeline 事件（`TimelineIntent`）本来就是"每 round 一个 checkpoint"的**结果性事件**（非逐 token），天然适配日志。让 timeline 变成 journal 中 conversation 事件的可重建投影 → 四套变"一条日志(journal) + 三个投影(timeline/messages/compact)"。
- **D-2 messages/compact 保持现状为"消息事实层"**（它们是 resume 的读侧真源，且 timeline_rebuild 已消费它们）。刀 2 第一阶不合并它们，只消除 **①↔④ 的重复写与同步阻塞**。
- **D-3 不破坏 BUG-007 fail-closed**：compact 损坏仍是"回退 archive 前必须显式判断"，不允许隐式全量泄漏。
- **D-4 同步阻塞 I/O 治理**：`persist_timeline_sync`（tokio 线程内全量 JSON 序列化+原子替换）与 `JournalStore` 同步写是 BUG-011 的瓶颈——第一阶把这些全部移出 tokio 线程到专用 std 线程（timeline checkpoint 已是线程化，`persist_timeline_sync` 是残余）。

---

## 3. 分阶（每阶独立 PR、带测试、可回滚）

### 阶段 0（本次，纯文档）
- 本计划文档。

### 阶段 1：Timeline 事件并入 journal（消除①↔④双写）——**已实现**
- 让 `TimelineIntent` 也经 `publish_with_causation` 写入 ④（新增 `JournalOp::Timeline` 或复用 Conversation 事件），以 timeline 的 `turn_id/round/block` 为幂等键。
- 读取侧：`ensure_timeline_loaded` 改为**优先从 journal 重放**（有 journal 时不再读独立 `ringing-timeline/{seed}.json`），`TimelineStore` 降级为"journal 缺失时的兼容回退 + projection cache"。
- 风险：① 历史迁移（旧 `ringing-timeline/*.json` 在首次启动时重放进 journal，一次性迁移脚本）；② 前端 timeline 快照形状必须逐字不变（多前端协议兼容）。
- 验证：`qaqh-runtime` timeline/journal 全量测试 + 新增"journal→timeline 重建形状==原生写"往返测试。

**实现记录（与三频道 `JournalStore` 隔离的独立 append-only timeline 日志）**

落地形态比 D-1 的"并入 ④ 三频道 journal"更保守且无回归：timeline 事件**不进**三频道 `JournalStore`（其 4MB `rewrite/compact` 会物理删行，会破坏 timeline 的可重建性）。改为在 `TimelineStore` 下新增独立 append-only 日志 `ringing/timeline-journal/{seed}.jsonl`：

- **磁盘格式** `TimelineJournalOp`（`timeline_store.rs`，serde tag `op`）：
  - `{ "op": "snapshot", "snapshot": ... }`：完整物化恢复基点（一次性历史迁移/未来压缩写）。
  - `{ "op": "append", "entry": TimelineEntry }`：每次 persist 追加大于 watermark 的尾部记录。
- **写侧（消除①↔④冗余、建立 journal ≥ cache 不变量）**：`persist_timeline_sync` 与异步 checkpoint 线程在**同一 `timeline_store` 锁**内先 `append_timeline_journal_tail_locked`（`replay_since(seed, watermark)` 增量追加，`append_journal` 内再按 watermark 防御去重）再写 `{seed}.json` 缓存。崩溃后 journal 永不落后于缓存。
- **读侧（journal 权威、缓存加速）**：`load_timeline_persisted` 以「缓存 ∪ journal」建索引；`ensure_timeline_loaded` 优先加载：
  1. 缓存与 journal 对齐（`cache.watermark == journal_max`）→ 直接 restore 缓存（快路径，免全量重放）；
  2. 仅 journal / 缓存滞后 → `materialize_timeline_from_journal`（`timeline.rs` 纯重放 reducer）重建，并补写缓存；
  3. 无 journal → 兼容 restore 缓存 + 一次性 `backfill_journal`（`Snapshot` 基点 + `Append` 尾部，幂等）；
  4. 两者皆无 → BUG-006 从 messages/compact 重建（原逻辑，重建也 append 进 journal）。
- **重放 reducer `materialize_timeline_from_journal`**：`Snapshot` 基点内条目视为已物化（watermark 及之前的 `Append` 只保留不重放，避免折叠 delta 二次叠加）；无基点则全量重放。生成 snapshot 的排序键与 `TimelineAppender::snapshot` 一致（逐字相同），`journal` 顺序不变。宽容：缺失 turn/block 的条目跳过，绝不 panic。
- **回归面**：`TimelineAppender`、`RingingHub` 对外 API 未变；前端 timeline snapshot / replay 形状不变；`timeline_rebuild`(BUG-006)、`seal_orphan_*`、`flush_timeline_persistence` 语义保持。

**穿透链路（供评审）**：`publish_timeline → apply_intent → persist_timeline_sync/checkpoint线程 → append_journal + persist(缓存)`；重启 `ensure_timeline_loaded → (对齐→缓存 / 否则 materialize 重放) → seal_orphan → persist`。

### 阶段 2：同步 I/O 全部线程化（BUG-011）
- 移除 `persist_timeline_sync` 在 tokio 线程的同步路径（改走 checkpoint 线程/专用队列）；`JournalStore` 写改异步任务。
- 验证：多 session + 子代理并发压力路径不出现 tokio 阻塞（现有并发测试）。

### 阶段 3（可选/远期）：messages/compact 也并入同一日志
- 大改（涉及 resume/undo/多前端协议演示），单独评审。

---

## 4. 风险与对策

| 风险 | 影响 | 对策 |
|---|---|---|
| 历史 `ringing-timeline` 迁移丢失已 seal turn 终态/逐 token delta | 前端回放降级 | 阶段 1 一次性迁移只重放**结果性 TimelineIntent**（turn/round/block sealed），逐 token delta 不承诺（现重建已承认降级）；迁移前备份 |
| 前端 timeline 快照形状漂移 | 多前端协议破坏 | 阶段 1 保证 snapshot 输出形状逐字不变（复用现有 `TimelineAppender` 生成） |
| BUG-007 fail-closed 被破坏 | 数据泄漏 | compact 路径不动；journal 迁移不触碰 ②③ 读链 |
| 单点日志变单点故障 | 整体可用性 | journal 保留 4MB 重写/有界策略；timeline 投影可本地重建兜底 |
| 同步阻塞残留 | 并发长尾 | 阶段 2 专项治理 |

---

## 5. 验收清单（阶段 1）

- [ ] 删除 `ringing-timeline/{seed}.json` 后，同名 seed 仍能从 journal 重建出**逐字相同**的 timeline 快照（前端可见无变化）。
- [ ] 原生写入路径产生的 timeline 与 journal 重放路径一致（往返测试）。
- [ ] 旧历史首次启动迁移成功（`ringing-timeline` → journal），迁移幂等、可重跑。
- [ ] `cargo check --workspace`、`cargo test -p qaqh-runtime`（含新增回归）全过。
- [ ] 多前端协议（SSE timeline 快照、conversation snapshot）形状不变。

---

## 6. 待办协调

- 本刀与刀 3（传输）/刀 7（InjectionBus）在 BUG-011 有交集：同步 I/O 治理（阶段 2）与刀 3 的"手写 HTTP 现代化"互补不冲突。
- 刀 4/5（BUG-010 crate 边界/bridge）为独立重构，可并行。
