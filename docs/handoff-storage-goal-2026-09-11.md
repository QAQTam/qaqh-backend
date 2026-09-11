# Handoff：存储架构演进 goal 模式（Phase 1-5）

**日期**：2026-09-11 01:27（UTC+8）
**交接原因**：owner 休息，实施暂停于 Phase 4 中段
**配套文档**：`docs/storage-architecture-plan-2026-09-10.md`（§3.5 为实施进展总览）
**Todo 系统**：T1-T8 已建（T1-T4 完成，T5 进行中，T6-T8 待做）

---

## 0. Owner 决策记录（已拍板，不得再问）

1. **依赖**：不引入 SQLite；**zstd 已批准**（Phase 5 用）；messages.jsonl 侧优化放开。
2. **破坏性变更**：磁盘格式自由变更，旧缓存可重建（BUG-006 路径兜底）；**messages.jsonl / compact-context.json 权威格式保持不变**。
3. **Phase 4**：温和方案（原意为 delta 合并落盘；实施中已证明"内存合并"会破坏 seq 连续性，等效落地为 **seal 裁剪 + 硬上限驱逐**，见 §3）。
4. **基准**：~100MB 轻量合成数据，快速出报告。
5. **提交**：全部改动攒**一个大 commit** 收官（工作区含 Phase 0 未提交改动 +389/-820，一并入库）。

---

## 1. 已完成（全部测试绿，可直接依赖）

### Phase 0（前序会话完成）
- timeline-journal 移除、快照唯一权威、审计迁移、1.01 GB 死数据物理删除（含备份，磁盘实测 `.qaqh` ~1 MB）。
- 工作区未提交改动：`timeline_store.rs`（重写）/`timeline.rs`（-363 净删）/`timeline_hub.rs`/`hub.rs`/`lib.rs`，**+389/-820**。

### Phase 1：per-seed 懒加载锁 ✅（T2）
- `hub.rs`：`lazy_load: Mutex<()>` → `lazy_loads: Mutex<HashMap<String, Arc<Mutex<()>>>>`；`pub(super) fn lazy_load_lock(&self, seed)` helper（entry+clone，锁表条目只增不减，~80B/条，驱逐留给 Phase 5）。
- `timeline_hub.rs` `ensure_timeline_loaded` 与 `hub.rs` `ensure_seed_loaded` **两条路径都**改走 per-seed 锁（原 plan 只标 timeline_hub.rs，实测锁被双路径共用）。
- 测试：`per_seed_lazy_loads_do_not_block_each_other`、`concurrent_first_access_of_same_seed_loads_exactly_once`（hub.rs tests 模块尾部）。
- qaqh-runtime **168 测试全绿**。

### Phase 2：有界恢复 ✅（T3）
- **新文件** `crates/qaqh-session/src/store/bounded_read.rs`：
  - `read_last_lines(path, max)`：64KB 反向块读（对标 Codex `READ_CHUNK_SIZE`），跨块 pending 拼接，返回 `(正向序行, truncated)`；
  - `read_messages_tail(path, max)`：尾部最近 N 条可解析消息（正向序），损坏行跳过 + 扩窗重试（64× 上限）；
  - 5 个单测（含 20MB 文件只触尾部 <500ms 的界验证）。
- `manager.rs`：`load_recent_for_projection(seed, recent)`——WAL fold 后，compact context 完好则优先（与 resume 同构），**损坏则降级归档尾部（fail-open）**，区别于 `load_for_resume` 的 fail-closed；会话不存在返回 None。
- `timeline_rebuild.rs`：`rebuild_timeline_snapshot` 改走有界路径（200 条消息 → `project_recent_turns_from_messages` 40 turns 窗口），不再 `read_to_string` 全量。
- **明确不做**：持久化字节偏移索引——反向扫描无需持久化状态、无 compact 失效问题。
- qaqh-session **24 测试全绿**。

### Phase 3：append-only 增量索引 ✅（T4，无 SQLite 方案）
- `store/mod.rs`：
  - `IndexOp`（Upsert/Remove，serde tag=op）；`index.jsonl` 每行一操作；
  - `upsert_index`/`remove_from_index` → `append_index_op` **O(1) append**（无全量重写/fsync/IndexLock 忙等；跨进程依赖 daemon.lock 单实例假设，已注释）；
  - `read_merged_index`：内存归并（同 seed 后行胜）+ 超 1024 行自动 compact（`rewrite_index_log` 原子替换）；
  - 旧 `index.json` 首读一次性迁移后删除（`migrate_legacy_index_if_present`）；
  - `write_index` 与 `IndexLock` 已删除（外部零调用，grep 确认）；
  - 4 个专项测试（迁移/归并/tombstone/损坏行）。
- qaqh-session 24 绿；`cargo check -p qaqh-runtime -p qaqh-daemon` 无错。

---

## 2. 当前暂停点（Phase 4 精确状态）

**已完成**：
- `crates/qaqh-runtime/src/ringing/persistence_policy.rs`（新文件，已注册 `pub mod`）：
  - `MAX_TIMELINE_JOURNAL_ENTRIES = 8192`；
  - `is_snapshot_persisted(event)`（结构事件 ✅ / TextDelta+ToolProgress ❌）；
  - `occupies_replay_tail(event)`（当前恒 true，占位供未来细分）；
  - 2 个回归测试锁定分类。
- ⚠️ 刚修完测试 import（`TimelineToolState`），**测试还没跑**——接手第一步：
  ```
  cargo test -p qaqh-runtime --lib persistence_policy
  ```

**未做**（timeline.rs 零改动）：
- TimelineAppender 内存 journal 有界化，两个动作：
  1. **seal 即时裁剪**：`seal_turn_with_state` 成功后，删除 `timeline.journal` 中该 `turn_id` 的全部条目（与 persist 侧 `prune_sealed_timeline_journal` 语义一致：sealed turn 条目在快照内已物化；restore 的 `next_fragment` 只从活跃 turn 条目重建，sealed block 拒绝新 delta 所以无影响——该语义已有既有测试覆盖）；
  2. **硬上限驱逐**：`next_entry` push 后若 `journal.len() > MAX_TIMELINE_JOURNAL_ENTRIES`，`drain(..超限数)` 头部驱逐。被驱逐区间的重连：客户端 `dispatch` 发现 seq 断裂 → `TimelineGap` → `recover_gap` 快照重基线（Phase 0 已接受的代价，且 hub 的 timeline 快照 1s 合并窗口保证水位很新）。
- 建议测试：`sealed_turn_journal_is_pruned_immediately`（seal 后 journal 不含该 turn 条目、replay 仍正确）、`journal_enforcement_bound`（>8192 后长度回到界内、活跃 turn 条目仍可回放）。

**设计定稿（重要，不要走回头路）**：
- ❌ 内存 TextDelta 合并：合并条目导致回放 seq 跳号，客户端 `qaqh-client/src/timeline.rs::dispatch` 要求 `timeline_seq == cursor + 1`，跳号被误判 gap → 放弃。
- ✅ 三频道侧无需改：RoundDelta/BlockCheckpoint 已是 Replaceable（64 次一 checkpoint + RoundCompleted compact + 4MB rewrite_if_oversized），写盘受控。

---

## 3. 后续路线（T6-T8）

### Phase 5（T6）
1. **冷 seed 驱逐**（`timeline.rs` TimelineAppender + `timeline_hub.rs`）：驱逐前**必须** `persist_timeline_sync`（快照落盘后才能丢内存态）；候选条件建议：无活跃 running turn + 最后访问超过阈值（如 10 分钟）。入口建议挂在 `publish_timeline`/`ensure_timeline_loaded` 的定期检查，避免新线程。
2. **zstd 冷快照**（zstd 依赖已批准，加进 `qaqh-runtime/Cargo.toml`）：`ringing-timeline/{seed}.json` 7 天未更新 → 压缩为 `.json.zst`（daemon 启动 best-effort 扫描一次，不建后台线程）；`TimelineStore::load_seed` 按扩展名透明解压。注意 `persist` 仍写未压缩 json（活跃会话读写频繁，压缩只做冷归档）。
3. 持久化度量（可选）：审计行已带 ts/seq；如做，写放大指标挂 audit 侧即可。

### G6 基准（T7）
- 脚本生成 ~100MB 合成 `messages.jsonl`（每行 ~2KB × 5 万行，多 seed 目录）+ 对应 timeline 快照；
- 三个对比（临时目录，勿碰 `C:\Users\QAQTam\.qaqh`）：
  1. 尾部读（bounded_read 200 条）vs 全量 `read_to_string`：耗时/峰值内存；
  2. 多 seed（≥8）并发首次加载 vs 旧全局锁语义（可用 git stash 前 hub.rs 对比，或直接断言 per-seed 不互斥测试的吞吐差）；
  3. journal 有界化前后：长会话（10 万 delta）的 TimelineAppender 内存估算（条目数×~200B）。
- 产出：`docs/storage-baseline-2026-09-11.md`，对标 plan §5 表格。

### G7 收尾（T8）
```
cargo test --workspace --exclude qaqh-mcp   # mcp 的 orphan_reap.rs Windows 既有编译失败
cargo clippy --workspace --exclude qaqh-mcp --all-targets
cargo fmt --all
```
- 既有失败（与本次无关，勿修勿慌）：qaqh-lsp `projection::tests::display_is_one_based` / `goto_scalar_renders_count_header`（Windows file uri）。
- 更新 plan §3.5 状态 → 全 ✅。
- **单 commit**（owner 决策 ⑤），message 建议范围：`feat(storage)!: timeline journal 下线 + per-seed 锁 + 有界恢复 + 增量索引 + 回放尾预算（Phase 0-5 收官）`。

---

## 4. 踩坑记录（接手必读）

1. **`store/mod.rs` 的 read 工具行号异常**（内容含某段导致 line 解析错位）：用 `edit` 内容匹配仍可靠；要看行可用 pwsh `[System.IO.File]::ReadAllLines` dump 到临时文件再 read。
2. **反向拼接是正序**：bounded_read 的 `chunk[relative+1..cursor] + pending` 直接就是完整行正序，**不要 reverse**（曾出 bug）；`read_messages_tail` 从最新往回收集后**必须 reverse 回正序**再返回（曾出 bug）。
3. **per-seed 锁不可重入**：`lazy_load_lock` 返回的 `Arc<Mutex<()>>` 内部任何路径不得再 ensure 同 seed（hub.rs:415 注释；测试曾因锁内调 `replay_since` 死锁 300s 被杀）。`load_for_resume`/`bounded_read` 纯读已验证安全。
4. **借用细节**：`self.lazy_load_lock(seed).lock()` 会因临时 Arc 立即释放而编译失败——先 `let lock = ...` 再 `.lock()`。
5. **domain 字段速查**：`TimelineEvent::RoundSealed { is_final }`（无 round_num）；`TimelineTool` 字段 = tool_call_id/name/state/summary/args_json/output/diff/progress/failure/permission；`SessionMeta` 用 `Default` + 字段赋值（无 `new(seed)`）。
6. **测试基建**：qaqh-session manager 测试用 `manager()` helper（temp root + 直构 SessionManager）；qaqh-runtime hub 测试用 `temp_root(label)` + `with_persistence("epoch-N", &root)`（drop 后重启验证懒加载）。
7. **qaqh-mcp warning**（unused `socket_path`）是既有问题，顺手修不修都可，勿扩大改动面。
8. Windows 上跑测试：`rg` 直接可用但部分管道（`ForEach-Object` 截断输出）会吞 stdout；长测试用 `timeout_secs ≥ 300`，test 编译一次约 1-8 分钟。

## 5. 工作区文件清单（相对 git HEAD 的全部改动）

```
M  crates/qaqh-client/src/discovery.rs          （Phase 0 期间既有，非本次）
M  crates/qaqh-config/src/registry.rs           （同上）
M  crates/qaqh-runtime/src/agent/state/*        （同上）
M  crates/qaqh-runtime/src/lib.rs               （Phase 0）
M  crates/qaqh-runtime/src/ringing/hub.rs       （Phase 0 + P1 per-seed 锁）
M  crates/qaqh-runtime/src/ringing/timeline_hub.rs（Phase 0 + P1）
M  crates/qaqh-runtime/src/ringing/mod.rs       （P4 注册 persistence_policy）
M  crates/qaqh-runtime/src/ringing/persistence_policy.rs（P4 新文件，测试未跑）
M  crates/qaqh-runtime/src/timeline.rs          （Phase 0；P4 有界化待做）
M  crates/qaqh-runtime/src/timeline_store.rs    （Phase 0 重写）
M  crates/qaqh-session/src/manager.rs           （P2 load_recent_for_projection + 测试）
M  crates/qaqh-session/src/store/mod.rs         （P3 append-only 索引 + 4 测试）
M  crates/qaqh-session/src/store/bounded_read.rs（P2 新文件）
?? docs/storage-architecture-plan-2026-09-10.md （原文档 + 本次 §3.5 更新）
?? docs/handoff-storage-goal-2026-09-11.md      （本文档）
?? docs/electron-render-plan-2026-09-10.md      （owner 的另一文档，勿动）
（qaqh-types/session.rs、qaqh-workspace/* 等 M 项为 Phase 0 期间既有改动，未验证归属，commit 时一并入库——owner 已批准大 commit）
```

**当前测试基线**：qaqh-runtime 168 绿 / qaqh-session 24 绿 / workspace 其余绿（除既知 lsp 2 失败 + mcp 编译失败）。
