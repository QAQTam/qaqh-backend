# 存储架构诊断与演进 Plan（对标 Codex）

**日期**：2026-09-10
**状态**：Phase 0 部分完成，Phase 1-5 待实施
**对照物**：`D:\project\codex-main`（OpenAI Codex 源码）

---

## 0. 背景与触发

起因：`timeline-journal/*.jsonl` 单文件膨胀至 **829 MB**，导致 daemon 内存异常
（加载时 ~1.2 GB）进而多 session 工作时前端明显阻塞。

本文档记录：
1. 实测根因（架构侧 + 算法侧）
2. Codex 的对照做法（源码实证）
3. 分阶段演进计划与预期收益

---

## 1. 实测基线（2026-09-10）

### 1.1 磁盘分布

`.qaqh` 总计 **1088.5 MB / 1767 文件**：

| 目录 | 大小 | 占比 | 性质 |
|---|---:|---:|---|
| `ringing/timeline-journal` | **1037.0 MB** | **95.3%** | ☠️ 已弃用（待删）|
| `ringing/ringing-timeline` | 18.2 MB | 1.7% | 快照（保留）|
| `sessions` | 16.2 MB | 1.5% | **权威数据** |
| `ringing/journal` | 13.3 MB | 1.2% | 三频道事件流 |
| `ringing/latest` | 0.9 MB / **1350 文件** | 0.1% | 可替换槽位 |

> **真实业务数据仅 16.2 MB，却付出 1088 MB 代价。**

### 1.2 单会话膨胀明细（seed `0b155246`）

| 指标 | 实测值 |
|---|---:|
| journal 文件 | 791.0 MB / 1,170,412 行 |
| 对应快照 | 7.1 MB |
| **真实唯一文本内容** | **4.37 MB** |
| **写放大** | **181×** |
| 加载耗时 | **28.24 秒** |
| 加载内存（Rust 结构估算） | **1,173 MB**（1.48× 文件）|

### 1.3 膨胀归因（按事件类型）

| 事件类型 | 行数 | 字节 | 占比 | B/行 |
|---|---:|---:|---:|---:|
| `block_checkpoint` | 18,402 | 570.6 MB | **71.6%** | 31,005 |
| `text_delta` | 1,148,813 | 226.7 MB | **28.4%** | 197 |
| 其余 7 类 | 3,187 | 3.4 MB | 0.05% | — |

- checkpoint 文本总量 **588.7 MB**，其中 **99.2% 是重复旧内容**
- delta 平均每行仅 **3.99 字符**，信封开销 **193 字节**（放大 49.5×）
- 最大块 127,789 字符，被重复写入累计 **561 MB**

---

## 2. 根因分析

### 2.1 算法侧

#### B1：`block_checkpoint` 全文重写 → **O(n²)**

`crates/qaqh-runtime/src/agent/turn_lap/gate.rs:51-85`：

```rust
emitter.emit_timeline(qaqh_domain::TimelineIntent::BlockCheckpoint {
    block_id: block_id.to_string(),
    text: block_text.to_string(),   // ← 截至当前的整段文本
});
```

节流条件 `CHECKPOINT_TOKEN_INTERVAL = 64` / `CHECKPOINT_INTERVAL = 2s`。
重放语义是**整体覆盖**（`timeline.rs:903-907`：`block.text = text.clone()`）。

**数学推导**：

```
块从 0 长到 S，每 k token 存一次
n = S/k 次，第 i 次写入 (i/n)·S
总写入 = Σ(i=1..n) (i/n)·S ≈ S·n/2 = S²/(2k)   →  O(S²)
```

**反直觉点**：节流间隔 k 越小（越"安全"），平方项爆炸越严重。
代码设 k=64，等于把 n 拉到最大。

实测印证：

| block_id | checkpoint 数 | 首→末体积增长 |
|---|---:|---:|
| `round-50:reasoning:664` | 501 | **×1580** |
| `round-43:reasoning:591` | 462 | ×1426 |
| `round-0:reasoning:667` | 358 | ×1131 |

#### B2：逐 token 落盘，信封开销 49.5×

```json
{"op":"append","entry":{"timeline_seq":1168152,"turn_id":"t10","round_num":14,
 "event":{"type":"text_delta","block_id":"round-14:reasoning:25",
 "fragment_seq":2240,"delta":" them"}},"ts":1789050044881}
```

`" them"`（5 字符）→ **200 字节**。

#### B3：全量反序列化恢复 → O(文件大小)

`ensure_timeline_loaded` 在**持双锁**期间调用 `store.read_journal(seed)`，
把 1,170,412 行全部 `serde_json` 反序列化成 `Vec<TimelineJournalOp>`。

#### B4/B5：无去重、无 compaction

`timeline_store.rs:19-24` 的设计声明：

> 与三频道 `JournalStore` 的 rewrite/compact 有界语义隔离——**timeline 日志从不物理删除行**。

`TimelineJournalOp::Snapshot`（压缩基点）的数据结构与重放逻辑**已完整实现**，
但运行时**从未调用**（仅 backfill 迁移与单测使用）。

### 2.2 架构侧

| # | 问题 | 位置 | 严重度 |
|---|---|---|---|
| A1 | **投影的投影的持久日志**（同一内容六份副本） | 全系统 | 🔴 |
| A2 | **全局串行锁** `lazy_load` | `timeline_hub.rs:201` | 🔴 |
| A3 | **锁内做重 I/O**（28 秒持锁）| `timeline_hub.rs:220-232` | 🔴 |
| A4 | **无内存预算**，seed 只增不减 | `TimelineAppender.seeds` | 🟠 |
| A5 | **`index.json` 用文件当索引**（全量重写 + 全局锁）| `store/mod.rs:118` | 🟠 |
| A6 | `ringing/latest` 1350 个小文件 | 可替换槽位 | 🟡 |
| A7 | 无持久化度量 | 全局 | 🟡 |
| A8 | 无冷热分层 / 压缩 | 全局 | 🟡 |

**A2 是"多 session 前端阻塞"的直接原因**：

```rust
pub(super) fn ensure_timeline_loaded(&self, seed: &str) {
    let _serial = self.lazy_load.lock()...;              // ← 全局锁
    ...
    store.read_journal(seed)                              // ← 锁内全量解析 28 秒
}
```

session A 加载时，B~Z 全部阻塞。

---

## 3. Codex 对照（源码实证）

### 3.1 数据分层

| 层 | 载体 | 职责 |
|---|---|---|
| **权威** | `rollout.jsonl` | 只存**语义事件**（非 token）|
| **索引** | SQLite（54 migrations）| 会话列表/分页/搜索/队列 |
| **投影** | `thread_items` 表 | JSONL 增量镜像（带 byte offset）|
| **冷存** | `.jsonl.zst` | 7 天后后台 zstd 压缩 |

**关键证据**（`codex-rs/state/src/lib.rs:1-5`）：

```rust
//! SQLite-backed state for rollout metadata.
//!
//! This crate is intentionally small and focused: it extracts rollout metadata
//! from JSONL rollouts and mirrors it into a local SQLite database.
```

**JSONL 是权威，SQLite 是派生**。佐证：
- `thread_history_projection_state.next_rollout_byte_offset` —— 记录"同步到哪"
- `DB_FALLBACK_METRIC` / `record_fallback()` —— DB 挂了回退 JSONL
- `apply_projection` 注释："If SQLite fails, it stays behind **the durable rollout**"

### 3.2 六个关键机制

| # | 机制 | 位置 | 解决什么 |
|---|---|---|---|
| 1 | **持久化决策层** | `rollout/src/policy.rs` | 显式区分「持久化/瞬态」|
| 2 | **反向扫描** | `rollout/src/reverse_jsonl_scanner.rs` | resume 不全量加载 |
| 3 | **ordinal + HistoryPosition** | `rollout/src/ordinal.rs`、`protocol.rs:3037` | 一致性守卫 / fork / revert |
| 4 | **分页协议** | `app-server-protocol/.../v2/thread.rs:415` | 有限首屏 + 游标翻页 |
| 5 | **per-rollout writer lock** | `rollout/src/writer_lock.rs` | 多 session 不互斥 |
| 6 | **持久化度量** | `rollout/src/persistence_metrics.rs` | 监控 pre/post filter 字节 |

#### 机制 1：持久化决策层（最核心）

```rust
/// Whether a rollout `item` should be persisted in rollout files.
pub fn is_persisted_rollout_item(item: &RolloutItem, history_mode: ThreadHistoryMode) -> bool
```

三类划分：
- **持久化**：Message / Reasoning / FunctionCall / FunctionCallOutput / Compaction / TokenUsage
- **不持久化**：AdditionalTools / CompactionTrigger / Other
- **瞬态（Transient, non-durable）**：ExecCommandBegin / ExecCommandOutputDelta / RawResponseItem / …

**Codex 不存 delta，只存最终 item。** 粒度是"一轮响应"，不是"一个 token"。

#### 机制 2：反向扫描

```rust
const READ_CHUNK_SIZE: usize = 64 * 1024;

/// Read-only scanner for newline-delimited JSON records, starting from the end.
pub struct ReverseJsonlScanner<R> { ... }

/// Skips records larger than the configured limit without buffering or parsing them.
pub fn with_max_record_bytes(mut self, max_record_bytes: usize) -> Self
```

从文件末尾往前读 64KB 块，凑够即停。`ordinal_state_for_rollout` 用它**只读最后一条**就知道写到哪了。

`model_context.rs` 的用法体现"有界恢复"：

```rust
/// Accumulates newest-to-oldest rollout items until they are sufficient to
/// reconstruct the latest model context.
pub enum ModelContextScanProgress { Continue, Complete }
```

#### 机制 3：ordinal 三道一致性守卫

```rust
// ① 投影层
if ordinal != next_ordinal { return Err(...expected ordinal {next_ordinal}, got {ordinal}); }

// ② 字节偏移层
if expected_offset != start_offset {
    return Err(ThreadStoreError::Internal {
        message: format!("thread history projection for {thread_id} is behind durable rollout"),
    });
}

// ③ 子代理继承边界
if ordinal < prefix_end {
    return Err(io::Error::other(format!(
        "...expected inherited prefix through ordinal {prefix_end}, found final durable ordinal {ordinal}")));
}
```

第三道显式检测「初始化中途崩溃」——序号不对就**拒绝 resume**，而非给出错误状态。

#### 机制 4：分页协议

```rust
pub struct ThreadResumeResponse {
    pub thread: Thread,
    pub initial_turns_page: Option<TurnsPage>,   // 有限首屏
    pub turns_backwards_cursor: Option<String>,  // 往历史翻
    pub items_backwards_cursor: Option<String>,  // 往 item 细节翻
}

pub struct TurnsPage {
    pub data: Vec<Turn>,
    pub next_cursor: Option<String>,
    pub backwards_cursor: Option<String>,
}
```

**它不假设客户端能补齐缺席的增量**，resume 直接返回「有限首屏 + 不透明游标」。

`HistoryPosition` 精确表达历史基线：

```rust
pub struct HistoryPosition {
    pub thread_id: ThreadId,              // rollout ID（物理文件）
    pub end_ordinal_exclusive: u64,       // 逻辑边界
    pub end_byte_offset: u64,             // 物理位置
}
```

注释明确：`thread_id` 实为 `rollout_id`，与 `SessionMeta::id`（稳定线程 ID）不同——
**revert 后线程 ID 不变，底层文件更换，前端无感。**

#### 机制 6：持久化度量

```rust
const ITEM_BYTES_METRIC: &str = "codex.rollout.persistence.item_bytes";
const TURN_BYTES_METRIC: &str = "codex.rollout.persistence.turn_bytes";

pub struct RolloutPersistenceBatchMeasurement {
    pub pre_filter: RolloutSizeTotals,    // 过滤前
    pub post_filter: RolloutSizeTotals,   // 过滤后
    ...
}
```

**把"过滤掉多少字节"做进产品代码**——这是防止写放大劣化的机制。

### 3.3 panic 语义对照

Codex 写入路径：

```
agent 产出 item
  → record_canonical_items()                    recorder.rs:1013
       └─ tx.send(RolloutCmd::AddItems)          mpsc channel(256)
            └─ writer task: state.add_items()
                 └─ JsonlWriter::write_line()
                      self.file.write_all(...)   // 写入 OS page cache
                      self.file.flush()          // 仅 flush，无 sync_all
```

**`JsonlWriter` 自始至终只有 `flush()`，无 `sync_all()`**（`recorder.rs:2068-2074`）。

panic 影响面：

| 层 | panic 后 |
|---|---|
| ① agent 内存态（TurnHistoryBuilder）| 丢失 |
| ② mpsc channel(256) | 丢失 |
| ③ OS page cache | **不丢**（进程崩溃不影响 page cache）|
| ④ rollout.jsonl | 持久 |
| ⑤ SQLite 投影 | 可从 ④ 重建 |

额外防线：`ensure_rollout_is_newline_terminated`（补行尾）、断句柄重试、
`terminal_failure()` 错误可见性、`load_rollout_items` 的 `parse_errors` 宽容跳过。

配置确认：
- Codex：`[profile.release]` 只设 `lto/debug/codegen-units`，**未覆盖 `panic`** → 默认 `unwind`
- QAQH：`[profile.release]` 设 `opt-level="z"/lto/strip/codegen-units`，同样未覆盖 → 默认 `unwind`

**结论：panic 持久性上两者同级。真正差距不在 fsync，而在写入量级——
Codex 是 O(n)，原 timeline-journal 是 O(n²)。**

> **启示**：只要写入量是 O(n)，"最后几秒未落盘"就是可接受窗口——因为丢得起。
> 原设计的致命处不是没 fsync，而是把 4.37 MB 真实内容写成 829 MB。

---

## 3.5 实施进展（2026-09-11 goal 模式，交接详情见 `docs/handoff-storage-goal-2026-09-11.md`）

### 已完成 ✅

| Phase | 实际实现 | 与原计划差异 | 验证 |
|---|---|---|---|
| **Phase 0 收尾** | 1.01 GB 死数据已物理删除（timeline-journal / _backup 均已不存在，实测 `.qaqh` 仅 ~1 MB） | — | 磁盘实测 |
| **Phase 1** per-seed 锁 | `lazy_load: Mutex<()>` → `lazy_loads: HashMap<String, Arc<Mutex<()>>>`（hub.rs），新增 `lazy_load_lock()` helper；timeline 与三频道 `ensure_seed_loaded` **两条路径共用** | 原计划只标 timeline_hub.rs 改动点——实测 `lazy_load` 被 timeline/三频道双路径共用，改动面扩大到 hub.rs（已验证 `load_for_resume` 纯读无重入死锁） | 新增 2 回归测试（per-seed 不互斥 / 同 seed 并发只装载一次）；qaqh-runtime 168 测试全绿 |
| **Phase 2** 有界恢复 | 新建 `qaqh-session/src/store/bounded_read.rs`（ReverseJsonlScanner 等价物：64KB 反向块读、跨块 pending 拼接、torn head 容错）；`load_recent_for_projection(seed, recent)`（compact 优先/损坏降级 fail-open，区别于 resume 的 fail-closed）；`rebuild_timeline_snapshot` 改走尾部有界读取（窗口 40 turns / 200 条消息） | 持久化字节偏移索引**不做**——反向扫描无需持久化状态、无索引失效问题（compact/rewrite 会移动偏移），复杂度更低收益同级；客户端 `recover_gap` 已是快照重基线语义无需改 | bounded_read 5 测试（含 20MB 文件只触尾部验证）+ manager 套约测试；qaqh-session 24 绿 |
| **Phase 3** 索引层 | **放弃 SQLite（owner 决策）**：`index.jsonl` append-only 增量日志（对标 Kafka log compaction）——upsert/remove O(1) append，读侧内存归并（同 seed 后行胜）+ 超 1024 行自动 compact；旧 `index.json` 首读一次性迁移后删除；IndexLock 忙等锁删除（依赖 daemon.lock 单实例假设） | 原计划对标 Codex SQLite；owner 拒绝原生依赖后改为零依赖方案，消除 O(N) 全量重写+fsync 的目标不变 | 4 专项测试（迁移/归并/tombstone/损坏行）；qaqh-session 24 绿；下游 runtime/daemon 编译无错 |

### 进行中 🔄

- **Phase 4**：`ringing/persistence_policy.rs` 已创建（决策层分类 + 2 回归测试，**测试刚修完 import 未跑**；`ringing/mod.rs` 已注册模块）。
  **待做**：① 跑 persistence_policy 测试；② TimelineAppender 内存 journal 有界化——`MAX_TIMELINE_JOURNAL_ENTRIES = 8192`（与三频道窗口对齐，~1.6MB/seed）：turn seal 即时裁剪该 turn 全部条目（与 persist 侧 `prune_sealed_timeline_journal` 语义一致）+ `next_entry` 后总量检查头部驱逐；被驱逐区间重连走 `TimelineGap → recover_gap` 重基线（Phase 0 已接受）。
  **设计定稿**：放弃"内存 TextDelta 合并"——合并会使回放 seq 跳号，客户端 `dispatch` 要求 `seq == cursor + 1`，跳号被误判为 gap。

### 待实施 ⏳

- **Phase 5**：① TimelineAppender 冷 seed 驱逐（驱逐前必须 `persist_timeline_sync` 落盘）；② 冷快照 zstd 压缩（**zstd 依赖 owner 已批准**，7 天阈值，daemon 启动 best-effort 扫描，`load_seed` 透明解压）；③ 持久化度量。
- **基准报告**：~100MB 合成数据（owner 已批准轻量版），量化 尾读 vs 全量读 / 多 seed 并发 vs 串行 / journal 有界化前后内存。
- **收尾**：`cargo test --workspace --exclude qaqh-mcp`（mcp 的 `orphan_reap.rs` Windows 既有编译失败见 §8）+ clippy + fmt + 更新本文档 + 按 owner 决策攒单 commit（含 Phase 0 未提交改动 +389/-820）。

### 既有问题（与本次无关，验证时已知）

- `qaqh-lsp` projection 2 个测试失败（`display_is_one_based` / `goto_scalar_renders_count_header`，Windows file uri 问题，不在改动文件集）
- `qaqh-mcp` `orphan_reap.rs` `#[cfg(unix)]` Windows 编译失败（§8 已记录）

---

## 4. 演进 Plan（原文，Phase 状态见 §3.5）

### Phase 0：止血（部分已完成 ✅）

| 项 | 状态 | 证据 |
|---|---|---|
| 移除 `timeline-journal` 写入与加载 | ✅ | `timeline_store.rs` / `timeline_hub.rs` 重构 |
| 移除全量重放 `materialize_timeline_from_journal` | ✅ | `timeline.rs` 净删 363 行 |
| 快照成为唯一权威 | ✅ | `ensure_timeline_loaded` 简化为纯快照装载 |
| 审计能力迁移 | ✅ | `timeline-audit/{seed}.jsonl`（60B/行，2 MiB 滚动）|
| 编译 / 测试 / clippy | ✅ | 166/166 通过，无新增警告 |
| 新 daemon 实测内存 | ✅ | **47.9 MB**（原 124 MB → 1.2 GB）|
| **1.01 GB 死数据清理** | ⏳ **待执行** | 已验证内容 100% 安全 |

清理命令（先备份，观察 1-2 天，确认无误后删除）：

```powershell
cd C:\Users\QAQTam\.qaqh\ringing
mkdir ..\_backup-timeline-journal
move timeline-journal\*.* ..\_backup-timeline-journal\
# 观察后：rmdir /s /q C:\Users\QAQTam\.qaqh\_backup-timeline-journal
```

**已知代价**：daemon 崩溃后，客户端重连无法回放上一次进程的中间帧，
由 `qaqh-client` 的 `recover_gap` 重新基线化（一次额外快照拉取）。

### Phase 1：多 session 隔离（最高 ROI）

**目标**：消除全局串行，session 间互不阻塞。

```
lazy_load: Mutex<()>
     ↓
lazy_loads: Mutex<HashMap<Seed, Arc<Mutex<()>>>>   // per-seed 锁

锁内只做状态转换；I/O（读快照）移到锁外
```

**改动点**：`crates/qaqh-runtime/src/ringing/timeline_hub.rs`
**预期**：多 session 并行加载，不再排队。

### Phase 2：有界恢复（对标反向扫描）

```
① 短期：messages.jsonl 加尾部索引（每 N 行记录偏移）
② 中期：实现 ReverseJsonlScanner 等价物（64KB 反向块读）
③ 复用已有 paginate_turns：gap 恢复只拉最近 N 轮（非全量）
```

**改动点**：`qaqh-client/src/timeline.rs` 的 `recover_gap`、`qaqh-session/src/store/mod.rs`
**预期**：resume 从 28 秒 → 毫秒级。

### Phase 3：索引层（对标 SQLite）

```sql
-- 迁移 index.json → SQLite
CREATE TABLE sessions (
    seed TEXT PRIMARY KEY, created_at INTEGER, updated_at INTEGER,
    title TEXT, archived INTEGER, message_count INTEGER,
    turn_count INTEGER, cwd TEXT, tokens_used INTEGER
);
-- 索引：created_at / updated_at / archived / cwd
```

**保留 `messages.jsonl` 为权威**（与 Codex 同构）；
SQLite 记录 byte offset 做增量投影。

**改动点**：`qaqh-session/src/store/mod.rs` 的 `read_index`/`write_index`/`upsert_index`
**预期**：列表页 O(1)；`IndexLock` 可删。

### Phase 4：写入侧收敛

```
① 加显式持久化决策层（对标 policy.rs）
     持久化：TurnOpened / BlockOpened / BlockSealed / TurnSealed / 终态文本
     瞬态：TextDelta / ToolProgress   ← 不落盘
② text_delta 若必须落盘，按 2s / 64 字符批量合并
```

**改动点**：新增 `ringing/persistence_policy.rs`；`gate.rs` 的 checkpoint 发射逻辑
**预期**：行数降 98%。

### Phase 5：内存与冷数据

```
① LRU 预算：TimelineAppender 总内存上限（建议 512 MB），超限驱逐冷 seed
② 冷快照 zstd 压缩（7 天阈值，后台 best-effort）
③ 接入持久化度量（写放大、pre/post filter 字节）
```

**改动点**：`timeline.rs` 的 `TimelineAppender`；新增 `timeline_maintenance.rs`
**预期**：内存有界；磁盘再降 5-10×。

---

## 5. 预期收益汇总

| 指标 | 当前 | Phase 1-5 后 | 提升 |
|---|---:|---:|---:|
| `.qaqh` 总占用 | 1088 MB | ~40 MB | **27×** |
| `timeline-journal` | 1037 MB | 0 | ∞ |
| 单会话加载 | 28.24 s | < 0.1 s | **280×** |
| 加载内存 | 1,173 MB | ~8 MB | **145×** |
| 写放大 | 181× | ~1.5× | **120×** |
| 多 session 加载 | 串行排队 | 并行 | — |
| 索引读写 | O(N) 全量重写 | O(log N) | — |
| daemon 常驻 | 124 MB → 1.2 GB | ~48 MB 稳定 | **25×** |

> ⚠️ **Phase 1-5 的收益数字为推算，非实测。**

---

## 6. 优先级

```
P0  Phase 0 收尾：清理 1.01 GB 死数据         ← 立即，零风险
P1  Phase 1：per-seed 锁                     ← 命中"多 session 阻塞"
P2  Phase 2：有界恢复                        ← 命中"resume 卡顿"
P3  Phase 5-①：LRU 预算                      ← 防内存反弹
P4  Phase 3：SQLite 索引                     ← 列表/搜索体验
P5  Phase 4：写入收敛                        ← 长期防劣化
```

---

## 7. 行业设计原则（本文档的依据）

这道题在工业界是经典问题，标准解法：

| 原则 | 出处 | 违反项 |
|---|---|---|
| 日志压缩（同 key 只留最新值）| Kafka Log Compaction | B1 / B4 |
| WAL + Checkpoint（WAL 可截断）| PostgreSQL / RocksDB / etcd | B5 |
| 增量帧 + 周期 I-frame | 视频编码 | B1（I-frame 过密）|
| LSM 分层压缩 | RocksDB / Cassandra | B5 |
| **写放大控制在 10-30×** | RocksDB 共识指标 | **实测 181×** |
| LRU + 内存预算 | Redis `maxmemory-policy` | A4 |
| Single-flight | Go `singleflight` | A2（做成了串行）|
| 分层存储（L1内存/L2 SSD/L3冷）| 通用 | A8 |

**最少副本原则**：

> 同一份数据最多两份：**① 权威写侧**（append-only、最小、持久）、
> **② 派生读侧**（可重建、有界、可丢弃）。

当前系统有**六份**（messages / compact-context / 三频道 journal /
timeline-journal / ringing-timeline / latest），违反该原则。

---

## 8. 风险与遗留

### 已知风险

1. **审计降级**：原 journal 的 `ts` 字段是 2026-09-02 冻结事故的定位依据。
   已用 `timeline-audit` 补偿，但粒度较粗（仅 seq/ts/type/turn，无正文）。
2. **崩溃回放丢失**：daemon 崩溃后中间帧不可回放（见 Phase 0 说明）。
3. **前端天花板**：Codex 后端设计优秀，但其 Electron 前端仍出现 4 GB / 闪退。
   **后端优化无法解决前端架构问题**——`qaqh-electron` 面临同样天花板，
   `qaqh-winui-app`（原生 WinUI，无 Chromium）是突破方向。

### 未验证项

- Phase 1-5 收益为推算，需实测确认
- Codex 前端如何消费分页协议（`app.asar` 309 MB 未解包）
- Codex 在百万 token 长上下文下的实际表现
- NTFS vs ext4 的 fsync/写行为差异

### 其它发现（不在本 Plan 范围）

- `qaqh-mcp/tests/orphan_reap.rs` 的 `pid_state` 带 `#[cfg(unix)]`，
  在 Windows 上编译失败，阻塞 `cargo check --workspace --all-targets`。
- `qaqh-session/src/store/mod.rs` 的 `save_one` / `append_one` 为死代码
  （生产路径零调用），实测 0.51 ms/条（每次 open + fsync），建议删除或标注废弃。
- `ringing/latest` 目录 1350 个小文件，可改内存 map（Phase 5 附带）。

---

## 附录 A：Codex 关键文件索引

| 主题 | 路径 |
|---|---|
| 持久化决策 | `codex-rs/rollout/src/policy.rs` |
| 反向扫描 | `codex-rs/rollout/src/reverse_jsonl_scanner.rs` |
| 压缩 | `codex-rs/rollout/src/compression.rs` |
| 度量 | `codex-rs/rollout/src/persistence_metrics.rs` |
| 序数 | `codex-rs/rollout/src/ordinal.rs` |
| 发射器 | `codex-rs/rollout/src/recorder.rs` |
| 写者锁 | `codex-rs/rollout/src/writer_lock.rs` |
| 会话索引 | `codex-rs/rollout/src/session_index.rs` |
| SQLite schema | `codex-rs/state/migrations/*.sql` |
| 历史投影 | `codex-rs/state/thread_history_migrations/0001_thread_history.sql` |
| resume 协议 | `codex-rs/app-server-protocol/src/protocol/v2/thread.rs` |
| 历史位置 | `codex-rs/protocol/src/protocol.rs:3037` |
| SQLite 说明 | `codex-rs/state/src/lib.rs:1-5` |

## 附录 B：QAQH 改动文件

```
crates/qaqh-runtime/src/timeline_store.rs          （重写，移除 journal，加审计）
crates/qaqh-runtime/src/timeline.rs                （移除全量重放 + 3 个测试）
crates/qaqh-runtime/src/ringing/timeline_hub.rs    （装载路径简化）
crates/qaqh-runtime/src/ringing/hub.rs             （持久化路径去 journal）
crates/qaqh-runtime/src/lib.rs                     （移除导出）
```

净变更：**+278 / -863 行**（约 -585 行）。
