# QAQ-Harness `v4.5.1` 存储防丢、瘦身与并发减负修复单

> **基线时间**：2026-11-26 · **`main` 基线**：`318fbfab`（已含 `v4.5` P2-0 ~ P2-2b 及 P2-closeout #44、`../qaqh-gpui` 迁移 `4387154`）
> **Companion 文档**：[MUST.md](./MUST.md)（总宪章与整改总表）、[V4_5_EXECUTION_PLAN.md](./V4_5_EXECUTION_PLAN.md)、[V4_6_EXECUTION_PLAN.md](./V4_6_EXECUTION_PLAN.md)、[AGENTS.md](../../AGENTS.md)（《项目宪法 v2 · 高质量发展修宪版》）
> **Worktree & 分支**：`../qaqh-wt-v4.5.1-storage-hardening` · `fix/v4.5.1-storage-hardening`
> **PR 标题格式**：`[kernel-refactor][v4.5.1] SQLite 盘满防丢上下文、零 schema 瘦身与并发读写减负`
> **协作分工**：由外部执行模型在独立 worktree 落地实现并提 PR（**不要自行 merge**）；验收官按 §四清单逐条硬性验收后合入 `main`，随后开启 `v4.6 P3-1`。

---

## 一、问题背景与硬边界

在 `318fbfab` 上对并发会话访问上下文与极端盘满（C 盘 / 数据盘可用空间为 `0`）场景的审计发现以下 3 类真实病灶：

1. **盘满 / 写库失败会静默丢失模型上下文**：`crates/qaqh-agent-runtime/src/session_loop.rs` 中 `seal_response`、`seal_partial`、`finish_completed`、`finish_cancelled` 等关键收口点存在 `.await.ok()` 吞错。因流式展示走纯内存 `MessageDelta` 不落库，盘满触发 `SQLITE_FULL` / `ENOSPC` 时前端仍显示模型回答完成并上报 `Completed`，但数据库并未落盘；刷新或下回合从 SQLite 重建 `assemble_context` 时整段上下文蒸发。
2. **`qaqh.db` 体积无止境膨胀（动辄上百 MB）**：
   - `owner.rs::timeline_block` 向全仓 0 生产消费的 `blocks.text` 列冗余双写与 `payload_json` 相同的正文；
   - `truncate_from_turn` 撤回回合后未清理孤儿 `session_contents` BLOB；
   - 全库从未设置 `PRAGMA journal_size_limit`，从未触发 `PRAGMA wal_checkpoint(TRUNCATE)` 或高碎片空页回收，删除旧会话后文件体积只涨不跌。
3. **重度并发会话读写负担**：
   - `Db.read_conn` 为单把 `Arc<Mutex<Connection>>`，多会话并发 `assemble_context` 与前端快照读互相串行排队；
   - 高频 `exec` 流按 `20ms` 合帧周期直打 `update_exec_session_progress` + `synchronous = FULL`，导致全局唯一写线程 `qaqh-db-writer` 被 `fsync` 队头阻塞。

### 硬边界（严禁触碰，宪法 #6 / #8）
- **严禁退回 JSONL 或更换存储引擎**；
- **严禁修改 `SCHEMA_VERSION`（必须保持 `13`）**，严禁新增磁盘 `migrate-on-read`；
- **严禁将 `session_contents` / 图片 base64 外置为新磁盘文件格式**（盘上格式重构留待 `v4.10+` / `v5.0` 单立 ADR）；
- **严禁改变任何 V2 Wire 契约**（铁闸 1：`cargo run -p qaqh-contract-export -- check --against docs/api-surface/v4.2` 必须返回 `EXIT=0`）；
- **严守宪法 #8（上帝文件止血令）**：`session_loop.rs`（3,038 行）、`store.rs`（2,460 行）、`owner.rs`（2,310 行）均处于净行数只减不增 / 极薄接线管制——新增机制必须拆入独立子模块（`db/read_pool.rs`、`db/maintenance.rs`、`exec_progress.rs`），既有大文件内仅允许薄调用接线（尽量净减或 ≤ 10 行增量）。

---

## 二、四项施工任务精确规格

### 任务 1（P0）：盘满 / 写库失败 Fail-Closed 止丢上下文
- **锚点**：`crates/qaqh-agent-runtime/src/session_loop.rs:968-984, 1022-1053, 2309-2480`（`seal_response`、`seal_partial`、`finish_completed`、`finish_cancelled`、`finish_failed`）、`crates/qaqh-session/src/db/mod.rs`（`DbError`）。
- **改造要求**：
  1. 为 `DbError` 增加辅助判别 `pub fn is_disk_full(&self) -> bool`（识别 `rusqlite::ErrorCode::DiskFull` 与 `std::io::ErrorKind::StorageFull`）。
  2. `seal_response` 改为返回 `Result<(), DbError>`（或等价明确错误），**物理删除 `.await.ok()`**；`reasoning` 与 `answer`（含 `opaque_items`）任一 `append_block` 失败立即向上返回错误。调用方（`Evidence::Terminal`）收到错误后中止回合，调用 `finish_failed(..., "response_persist_failed", &msg)`，严禁继续走 `ProviderFinish::Stop -> finish_completed` 或 `ToolUse` 续跑。
  3. `seal_partial` 取消 `.await.ok()`，写库失败必须显式 `log::error!` 并返回持久化失败标记，避免上层当成正常中断收尾。
  4. `finish_completed` 若 `self.owner.finish_turn(...).await` 返回 `Err`，**严禁**调用 `notify_terminal(turn, TurnTerminal::Completed, None)`，必须改报 `TurnTerminal::Failed` 并带出持久化失败信息（盘满时在错误文本中明确提示「磁盘空间不足，上下文未保存」）。
  5. `finish_cancelled` 与 `finish_failed` 同样消除静默 `.ok()`，写库失败必须 `log::error!`；至少确保 `finish_cancelled` 落库失败时不假报 `Cancelled`。
- **红→绿反证**：
  - 构造单元/集成反证测试，模拟 `append_block` 或 `finish_turn` 返回写库错误（如只读库或 `SQLITE_FULL` 故障注入）：旧实现静默吞错并在事件流广播 `TurnTerminal::Completed`（必红）；新实现中止回合、事件流广播 `TurnTerminal::Failed` 且携带持久化失败/磁盘满错误码（绿）。

---

### 任务 2（P1）：零 Schema 瘦身——停写 `blocks.text` 冗余列 + `truncate_from_turn` 清理孤儿 BLOB
- **锚点**：`crates/qaqh-session/src/owner.rs:1063-1145`（`timeline_block`）、`crates/qaqh-session/src/db/store.rs:272-338`（`truncate_from_turn`）。
- **改造要求**：
  1. **停写 `blocks.text` 冗余副本**：`timeline_block` 中不再将整段正文复制进 `NewBlock.text`（统一置 `None`），并在注释中注明生产读取全走 `payload_json`、`text` 列保留可空以维持 schema `13` 零迁移。核查全仓确认生产代码对 `BlockRow.text` 零读取、且所有 `view_tests`/`owner_tests` 行为不变。
  2. **`truncate_from_turn` 清理孤儿 `session_contents`**：在执行 `DELETE FROM tool_calls` / `DELETE FROM exec_sessions` **之前**，收集被删行引用的 `output_ref`、`diff_ref`、`output_content_id` 候选集合（去重并用 `bare_content_id` 归一为裸 64-hex）；删除 `tool_calls`、`exec_sessions`、`blocks`、`turns` 之后，在同一事务内删除 `session_contents` 中属于该 `session_id`、位于候选集合内、**且不再被该会话剩余 `tool_calls.output_ref` / `tool_calls.diff_ref` / `exec_sessions.output_content_id` 引用的行**（严禁误删未撤回回合仍在共用的同一 `content_id` BLOB）。
- **红→绿反证**：
  1. `append_block` 写入长文本块后直查 SQLite `SELECT text FROM blocks` 必须为 `NULL`，同时 `load_session_state` 还原的 `TimelineEntry` 内容逐字完整一致；
  2. 同会话构造两回合，各外置大输出：其中一条 `content_id` 被保留回合与撤回回合共享，另一条仅归撤回回合独占。调用 `truncate_from_turn` 后断言：独占 `session_contents` 行已被清理（旧实现残留必红→新实现绿），共享 `session_contents` 行仍完好存在。

---

### 任务 3（P1）：WAL 体积封顶与可控空间回收（独立子模块 `crates/qaqh-session/src/db/maintenance.rs`）
- **锚点**：`crates/qaqh-session/src/db/mod.rs:200-228`（`configure`）、`crates/qaqh-session/src/db/store.rs:229-256`（`delete_session_tree`）、`crates/qaqh-session/src/owner.rs`（`OwnerDirectory::delete_session_tree` / daemon 停机收口）。
- **改造要求**：
  1. 在 `db/mod.rs::configure` 中设置并读回确认 `PRAGMA journal_size_limit = 16777216`（16 MiB），限制 `-wal` 长期膨胀上限。
  2. 新增独立模块 `crates/qaqh-session/src/db/maintenance.rs`，提供两个明确维护原语：
     - `checkpoint_truncate(conn: &mut Connection) -> Result<(), DbError>`：执行 `PRAGMA wal_checkpoint(TRUNCATE)`，安全截断已同步 WAL；
     - `reclaim_if_fragmented(conn: &mut Connection) -> Result<bool, DbError>`：查询 `PRAGMA page_count` 与 `PRAGMA freelist_count`，仅当可回收空闲页达到明确阈值（如 `freelist_count >= 1024`（≥ 4 MiB）且 `freelist_count * 100 / page_count >= 30`）时执行空间回收。
       - **盘满安全取舍**：若采用 `VACUUM`，盘满时 SQLite 会因无法创建临时副本而返回 `SQLITE_FULL`（不损坏主库，但会报错）；实现时必须显式处理或上抛错误并写清注释（或先评估磁盘策略，严禁静默吞掉损坏风险，同时保证主库在任何错误下完好无损）。
  3. 在 `OwnerDirectory::delete_session_tree` 删除会话树之后（同一写执行器路径或紧随其后）调用 `checkpoint_truncate` 与 `reclaim_if_fragmented`（删除小会话不达阈值时跳过全库 `VACUUM`，避免卡顿）；在正常停机收口路径执行一次 `checkpoint_truncate`。
- **红→绿反证**：
  1. `Db::open` 后读回 `PRAGMA journal_size_limit` 等于 `16777216`；
  2. 向会话写入 ≥ 5 MiB 的 `session_contents` 大 BLOB 并触发 checkpoint，随后删除该会话树：旧实现下 `qaqh.db` 文件体积与高水位 freelist 基本不回落（红）；新实现下 `-wal` 被截断、高碎片阈值触发回收且 `qaqh.db` 文件物理大小显著回落（绿）。

---

### 任务 4（P1）：并发读写减负（独立子模块 `db/read_pool.rs` + `exec_progress.rs`）
- **锚点**：`crates/qaqh-session/src/db/mod.rs:84-198`（`Db`）、`crates/qaqh-session/src/owner.rs:69-70, 1450-1518, 1840-1940`。
- **改造要求**：
  1. **有界只读连接池（`crates/qaqh-session/src/db/read_pool.rs`）**：
     - 不引入重量级第三方连接池依赖；用轻量 `std::sync::{Mutex, Condvar}` + `Vec<Connection>` 实现固定容量 `4` 条 `SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_NO_MUTEX` 连接的有界借用池（每条连接配置 `busy_timeout(5s)`，Drop 时自动归还，连接状态异常时 fail-closed 不返回损坏句柄）。
     - 保持 `Db::read` 与 `Db::read_blocking` 的对外函数签名 100% 不变。
  2. **`exec` 进度写库节流（`crates/qaqh-session/src/exec_progress.rs`）**：
     - **保持 `ADR-0030` 前端实时性不变**：`OwnerPump` 仍按 `20ms / 256KiB` 合帧推送 `SessionEvent::ExecSessionOutputDelta`，且 `append_exec_log` 完整日志文件追加仍按原频率及时落盘。
     - **解耦 SQLite 进度写频率**：对 `store::update_exec_session_progress` 增加按 `exec_session_id` 维度的 **≥ 500ms 节流**（记录上次 DB 落库时刻与待写入的最新进度快照）。
     - **三条硬性冲刷不变量（严禁违背）**：
       1. 距上次该 `exec_session_id` 进度落库已达 `500ms` 时立即写入；
       2. 收到 `OwnerMsg::Flush` 屏障时，**必须先把所有待落库的最新 `exec` 进度同步写入 SQLite 再响应 Flush 完成**（保证 `owner.flush()` 与既有 `owner_tests` 语义零漂移）；
       3. 收到 `OwnerMsg::ExecSessionSettled` 时，确保清理该 `exec_session_id` 的挂起节流快照，由 `settle` 事务写入权威终态进度；若中间 `update_exec_session_progress` 失败，必须 `log::error!` 并保留待冲刷标记以便下次/终态重试。
- **红→绿反证**：
  1. **只读并发反证**：4 个线程同时通过 `db.read` / `db.read_blocking` 各执行一次耗时 `100ms` 的读闭包：旧单锁实现总耗时 `>= 400ms`（必红）；新 4 连接只读池总耗时 `< 250ms`（绿）。
  2. **`exec` 节流反证**：在 `150ms` 内连续向同一 `exec_session_id` 投递 5 批递进 offset 的 `exec_output`：前端事件接收端仍收到全部输出增量；SQLite 进度更新次数 `< 5`（节流生效）；随后调用 `owner.flush().await`，直查 SQLite `exec_sessions` 的 `next_offset` 与 `output_preview` 必须精确等于第 5 批最新值。

---

## 三、给执行模型的标准派工 Prompt 模板

```markdown
请在 `/home/qaqtam/项目/qaqh-backend` 推进 `v4.5.1` 存储防丢、瘦身与并发减负修复单（见 `docs/v4.3-4.8/V4_5_1_EXECUTION_PLAN.md`）。

### 开工前必读与现场核查（宪法 #14、#15、#16）
1. 阅读 `AGENTS.md`（宪法 v2）、`docs/v4.3-4.8/MUST.md` 与 `docs/v4.3-4.8/V4_5_1_EXECUTION_PLAN.md`。
2. 运行只读检查：
   - 核查磁盘可用空间、可用内存、CPU 核数（8 逻辑核 / 7.7 GiB RAM，禁止本地全仓 `--release`）；
   - `git fetch origin && git status -sb && git worktree list`；
   - 确认 `origin/main` 处于 `v4.5` 已收口基线（含 PR #44）。
3. 使用任务列表工具向用户同步你的修改步骤后再动代码。

### 强制施工纪律
1. **Worktree 隔离 & 禁用 `git checkout`（宪法 #15）**：直接在平级目录 `../qaqh-wt-v4.5.1-storage-hardening` 开分支 `fix/v4.5.1-storage-hardening`（`git worktree add ../qaqh-wt-v4.5.1-storage-hardening -b fix/v4.5.1-storage-hardening origin/main`），共享 `CARGO_TARGET_DIR="/home/qaqtam/项目/qaqh-backend/target"`。严禁使用 `git checkout`。
2. **硬边界（宪法 #6 / #8）**：
   - `SCHEMA_VERSION` 必须保持 `13`；严禁新增磁盘文件格式或 `migrate-on-read`；
   - `session_loop.rs`、`store.rs`、`owner.rs` 严守宪法 #8 上帝文件止血令，新增机制独立拆入 `db/read_pool.rs`、`db/maintenance.rs`、`exec_progress.rs`，大文件内仅做极薄接线；
   - 严禁留下 `.ok()` 吞写库错、严禁保留 `legacy_*` 或兼容转换层。
3. **反证 + 精准验证（宪法 #12）**：
   - 先证明 4 项任务的红→绿反证测试在旧实现上确实变红、修复后全绿；
   - 运行 `cargo test -p qaqh-session -p qaqh-agent-runtime -p qaqh-runtime --lib`（及 `qaqh-session` 相关集成测试）与 `cargo clippy --no-deps --lib -p qaqh-session -p qaqh-agent-runtime -p qaqh-runtime`；
   - **铁闸 1**：执行 `cargo run -p qaqh-contract-export -- check --against docs/api-surface/v4.2` 确认 `EXIT=0`。
4. **实名提交与停等验收（宪法 #17）**：
   - 提交信息必须附带 `Co-Authored-By:` trailer；
   - 推送分支并通过 `gh pr create` 创建 PR（标题 `[kernel-refactor][v4.5.1] SQLite 盘满防丢上下文、零 schema 瘦身与并发读写减负`），正文按四联单贴齐病灶/宪法依据、新旧变更清单、4 项红→绿反证日志与契约 `EXIT=0` 证明；
   - **创建 PR 后停下并报告 PR 链接，等待验收官 review，不要自行 merge PR。**
```

---

## 四、验收官硬性审查清单（Acceptance Gate）

执行模型提交 PR 后，验收官逐条核查以下门禁，**任意一条不达标即打回**：

- [ ] **Worktree 与提交规范**：分支源自 `318fbfab`+、PR 标题符合规范、commit 含合法 `Co-Authored-By:` trailer。
- [ ] **硬边界零越线**：`SCHEMA_VERSION == 13` 未变；未新增任何独立磁盘存储格式或 `migrate-on-read`；`cargo run -p qaqh-contract-export -- check --against docs/api-surface/v4.2` 返回 `EXIT=0`。
- [ ] **上帝文件止血令（宪法 #8）**：`git diff --numstat` 核实 `read_pool`、`maintenance`、`exec_progress` 已独立成模块，`session_loop.rs` / `store.rs` / `owner.rs` 净增量受控且无大块逻辑塞入。
- [ ] **Fail-Closed 吞错清零（任务 1）**：`rg '\.await\.ok\(\)' crates/qaqh-agent-runtime/src/session_loop.rs` 在 `seal_response` / `seal_partial` / `finish_completed` / `finish_cancelled` / `finish_failed` 处 **0 命中**；盘满/持久化失败反证红→绿日志真实可复现。
- [ ] **零 Schema 瘦身（任务 2）**：`blocks.text` 停写（`None`）且全量快照/回放不变；`truncate_from_turn` 独占 BLOB 删除 + 共享 BLOB 保留双断言测试全绿。
- [ ] **WAL 上限与空间回收（任务 3）**：`journal_size_limit = 16777216` 读回校验存在；`delete_session_tree` 后碎片回收实测使文件体积回落且不破坏主库安全。
- [ ] **并发读写减负（任务 4）**：4 连接只读池并发耗时反证红→绿；`exec` 进度 500ms DB 节流 + `Flush` / `Settle` 强冲刷不变量反证红→绿；前端 `20ms` 事件流无退化。
- [ ] **全量精准回归 & Clippy**：`qaqh-session`、`qaqh-agent-runtime`、`qaqh-runtime` 测试与 `--no-deps --lib` clippy 零错误；未验范围（如 Windows 真机盘满/文件锁行为）按宪法 #13 如实登记。

---

## 五、结项状态回写

| 切片 | PR | 状态 | 备注 |
|---|---|---|---|
| `v4.5.1` | PR #45 | **已合入 (`df89e6b0`)** | 4 项任务全部落地并独立模块化（`session_persist.rs`、`db/maintenance.rs`、`db/read_pool.rs`、`exec_progress.rs`、`replay_ring.rs`）；宪法 #8 净减达标（`session_loop.rs` `+49/-186` 净 `-137`、`owner.rs` `+107/-207` 净 `-100`、`store.rs` `+4`）；终审复测：`qaqh-session --lib` 55 passed（1 ignored）、`disk_full_fail_closed_tests` + `session_persist::tests` 5 passed（0 failed）、`clippy --no-deps --lib` 三 crate 零错误且新增模块零警告、铁闸 1 `EXIT=0`、`SCHEMA_VERSION == 13` 未变（`qaqh-runtime --lib` 65 passed 等 PR 复测结果见 PR #45，存量基线失败 `tool_surface_schema_shape` 未改）；未验范围：真机物理盘满与 Windows 文件锁（模拟注入：盘满 `PRAGMA max_page_count`、`finish_turn` 失败触发器、并发流失败触发器）；[V4_5_1_PR45_REMEDIATION_GUIDE.md](./V4_5_1_PR45_REMEDIATION_GUIDE.md) §二 Fast-Follow（16 处 `.await.ok()` + 2 处回滚内存态泄漏）独立留待后续立项 |
