# DeepX Daemon 未来优化文档

> 状态：2026-08-03 编写。本文档记录 daemon 冷启动内存问题的**已完成优化**（P0/P1）与**后续优化方向**（P2+），供后续迭代参考。

---

## 0. 背景与已完成优化摘要

### 问题

`deepx-daemon run` 冷启动内存高达 **158.9 MB**（WorkingSet），而干净的 Rust 进程基线仅 **14.4 MB**。根因是启动时把 `~/.deepx/ringing` 下 **150 MB 历史数据全量反序列化并常驻内存**，且 Rust 无 GC、永不释放：

| 数据源 | 磁盘体积 | 常驻内存（修复前） |
|---|---|---|
| `ringing/journal/**/*.jsonl`（可靠事件 WAL，append-only） | 110.7 MB | ~110 MB（重放 + 无界 projection） |
| `ringing/ringing-timeline/*.json`（transcript 快照 + replay tail） | 38.1 MB | ~110 MB（反序列化膨胀 3 倍） |

### 已完成修复（2026-08-03）

#### P0：journal 物理压缩触发条件修复

**缺陷**：`maybe_rewrite_journal` 只在 `RoundCompleted → compact_round_deltas → removed > 0` 时触发。轮次未完成/卡死时 `RoundDelta` 持续 append 但永不触发重写，磁盘文件无界增长（实测单文件 82 MB / 16 万行，**0 条 compact 记录**）。

**修复**（`crates/deepx-runtime/src/ringing/hub.rs`、`journal_store.rs`）：

- `JournalStore` 新增 `pending_bytes` 内存计数（`write_line` 累加、`rewrite` 清零），热路径**零 I/O 门控**；
- `maybe_rewrite_journal` → `rewrite_if_oversized(force)`：每次 reliable append 后检查，`pending ≥ 阈值` 才 stat + 整文件重写；重写以内存有界 journal（≤8192 条）为权威，超大文件收敛到窗口大小；
- 懒加载 seed 时 `force` 模式按物理大小直接检查，**历史超大文件加载即压缩**。

#### P1：journal + timeline 双懒加载

**缺陷**：`load_persisted` / `load_timeline_persisted` 启动时全量读取并常驻所有历史会话，而实际消费者只有"用户当前打开的 1-2 个会话"。

**修复**：

- 启动只扫描磁盘 seed 清单（`list_seeds`，纯目录遍历，毫秒级），**不重放任何事件**；
- 新增 `ensure_seed_loaded(channel, seed)` / `ensure_timeline_loaded(seed)`：首次访问（publish / replay / snapshot / checkpoint / bootstrap）时按需从磁盘恢复，精确恢复 sequencer 水位，并顺带收尾孤儿 running turn；
- `timeline_root` 独立保存（`start_timeline_persistence` 会 take 掉 `timeline_store`，懒加载必须用独立路径）；
- 全部访问点接入：`publish_with_causation`、`replay_since`、`snapshot`、`conversation_snapshot`、`checkpoint`、`last_stream_seq`、`publish_timeline`、`timeline_snapshot`、`timeline_replay_since`；
- `replay_channel_since` 保持"只回放已加载 seed"语义（客户端连接前必经 open/bootstrap 触发加载）。

### 实测结果（150.6 MB 真实历史数据复本，release 二进制）

| 场景 | WorkingSet | Private |
|---|---|---|
| 修复前（全量加载） | **158.9 MB** | 156.8 MB |
| 修复后（双懒加载，冷启动） | **15.2 MB** | **6.6 MB** |
| **降幅** | **90.4%** | 95.8% |

- 测试覆盖：`deepx-runtime` 97 个单测全过（含新增 `lazy_load_defers_history_until_first_access`、`journal_rewrite_converges_on_lazy_load_without_round_completed`）；`deepx-daemon` 19 个单测 + 1 个集成测试（ignored）通过；
- 契约未变：存储格式（JSONL / JSON）、wire 协议、replay 语义、幂等窗口、孤儿 turn 收尾全部保持。

---

## 1. 后续优化方向（P2+）

### P2-1：会话关闭时释放内存态（LRU 卸载）

**现状**：seed 一旦懒加载即常驻（journal state + timeline appender + projection），会话关闭后不释放。长期运行打开过多个会话后内存会累积。

**方案**：会话关闭（`SessionStateChanged { Closed }`）或长时间无 lease 时，将该 seed 的 `SeedChannelState` / timeline 从内存移除（磁盘数据保留，再次访问走懒加载恢复）。需要：

- 卸载前 flush 持久化（timeline sync persist + journal 已同步 append，天然安全）；
- 移除后 sequencer 水位保留（防止 stream_seq 冲突）；
- 恢复成本 = 一次懒加载（已被 P1 实现）。

**收益**：内存随活跃会话数伸缩，而不是只增不减。**风险**：低。**工作量**：0.5-1 天。

### P2-2：journal 架构收敛为"快照 + 增量 tail"

**现状**：jsonl 是 append-only 全量事件日志（历史可达 82 MB/会话），reliable 回放只需要有界窗口（8192 条），事件存档另有 `sessions/` 消息库与 timeline 快照。三份数据存在职责重叠。

**方案**：周期性把内存存活事件物化为 checkpoint 记录（jsonl 内追加 `Checkpoint` 行 = 权威基线 + 之后增量），重放时从最近 checkpoint 起步而非全量；配合 P0 的 rewrite，磁盘与装载成本与"自 checkpoint 以来的增量"成正比。

**收益**：单会话磁盘从 82 MB 收敛到 ~4 MB（8192 条窗口）；冷启动 / 懒加载 I/O 再降一个数量级。**风险**：中（需要与 `CursorExpired` / `reset_required` 语义对齐）。**工作量**：1-2 天。

### P2-3：worker 拉起时后台预加载

**现状**：懒加载是同步的——resume 会话的 worker 首个事件触发 `ensure_seed_loaded`，82 MB 文件读取 + 重放会阻塞该次 publish（数百 ms 级）。

**方案**：`registry.spawn` 时异步预加载目标 seed（后台线程/任务），publish 到达时通常已就绪；`ensure_*` 保留为兜底。**收益**：首事件延迟消除。**风险**：低。**工作量**：0.5 天。

### P2-4：移除 TimelineStore 全量读取 API

**现状**：`TimelineStore::load` 已标记 `#[cfg(test)]`，但测试仍依赖全量读。建议测试改为 `load_seed` 循环，彻底移除全量路径，防止未来误用。**工作量**：0.5 小时。

### P2-5：一次性历史数据收敛工具

**现状**：存量 150 MB 历史数据（82 MB 级超大 jsonl）只有被访问时才会被懒加载压缩；从未访问的会话文件保持巨大。

**方案**：daemon 启动后台任务或独立 CLI（`deepx-daemon gc-journals`）遍历 `journal/`，对超过阈值的文件执行"读尾 + 重放 + rewrite"（复用 P0 的收敛逻辑，无需完整内存态）。**收益**：磁盘立即回收 ~100 MB。**风险**：低（rewrite 原子替换）。**工作量**：0.5-1 天。

### P2-6：rewrite 后仍超阈值时的兜底

**现状**：`rewrite_if_oversized` 重写后若 entries（8192 条）序列化体积仍 ≥ 阈值（大事件场景），下次 append 会再次触发重写，产生无收益的重复 I/O（频率 ≈ 每 4 MB 一次，尚可接受）。

**方案**：重写后记录时间戳，冷却期内跳过（如 30 s）；或阈值检查改用"磁盘大小 - 上次重写大小"增量判断。**工作量**：0.5 小时。

### P2-7：内存监控与诊断

**方案**：

- daemon `status` / `debug` 页暴露：已加载 seed 数、磁盘 seed 清单数、journal/timeline 常驻字节估算、懒加载命中次数；
- 阈值参数化（`JOURNAL_REWRITE_THRESHOLD_BYTES` 目前是 const，测试用 OnceLock 覆盖；可提升为 config 项）。

**收益**：可观测性，便于复现与验收。**工作量**：0.5 天。

### P2-8：事件双写合并（journal ↔ timeline）

**现状**：同一轮次事件同时写入 `journal/*.jsonl`（reliable append）与 `ringing-timeline/*.json`（快照 + tail），存在冗余。

**方案**：评估 timeline 是否可成为 journal 的权威物化视图（或反之），消除一份写入路径。**注意**：这是**架构级**改动，需先完成 P2-2 再评估。**风险**：高。**工作量**：3-5 天。

---

## 2. 优先级建议

| 优先级 | 项目 | 理由 |
|---|---|---|
| 高 | P2-5 历史数据收敛工具 | 存量磁盘立即回收，无需等用户访问 |
| 高 | P2-1 会话关闭卸载 | 防长期运行内存累积（懒加载的互补面） |
| 中 | P2-3 后台预加载 | 消除 resume 首事件延迟 |
| 中 | P2-2 快照 + 增量 tail | 磁盘/装载成本质变，但需谨慎对齐契约 |
| 低 | P2-4 / P2-6 / P2-7 | 清理与可观测性 |
| 观望 | P2-8 双写合并 | 架构级，依赖 P2-2 落地后再评估 |

## 3. 验收基线（每次改动后复测）

1. 用本文档背景章节的复本方法（临时 `USERPROFILE` + 重写 `data-root` marker）冷启动，WorkingSet ≤ 20 MB；
2. `deepx-runtime` / `deepx-daemon` 全量测试通过；
3. 打开历史会话：transcript 完整、SSE 重连回放正确、孤儿 turn 收尾为 Cancelled；
4. 无 `RoundCompleted` 的会话持续运行后，jsonl 大小保持有界（≤ 阈值 + 窗口体积）。
