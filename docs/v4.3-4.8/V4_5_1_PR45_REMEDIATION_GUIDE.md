# PR #45 返修与 Fast-Follow 修复指南（`v4.5.1`）

> **基线时间**：2026-11-26  
> **评审对象**：`PR #45`（分支 `fix/v4.5.1-storage-hardening`，基线 `bdf29170`，历经首轮 `7c7d1a71` → 二修 `6f71212e` → 终修 `cb1afa3d`）  
> **Worktree**：`../qaqh-wt-v4.5.1-storage-hardening`（已于合入后清理）  
> **验收结论**：**`已通过并合入 (df89e6b0)`**  
> **Companion 文档**：[V4_5_1_EXECUTION_PLAN.md](./V4_5_1_EXECUTION_PLAN.md) · [AGENTS.md](../../AGENTS.md)（《项目宪法 v2 · 高质量发展修宪版》）

---

## 一、PR #45 返修必做清单（5 项，全部在原 worktree 修补并推回 PR #45）

> 以下 5 项均为本轮代码评审直接抓出的真实高/中危缺陷或宪法 #8 硬门禁不达标项，**必须全部修复并通过反证验证后，方可合入 PR #45**。

### 1. Blocker 1（高危）：消除 `finish_failed` 双重触发 `notify_terminal`
- **锚点**：
  - `crates/qaqh-agent-runtime/src/session_loop.rs:2424-2434`（`finish_turn_or_fail`）
  - `crates/qaqh-agent-runtime/src/session_loop.rs:2518-2552`（`finish_failed`）
- **病灶**：当真 0 字节盘满导致 `seal_response` 失败进入 `finish_persist_failed -> finish_failed`，随后 `owner.finish_turn` 同样因 `SQLITE_FULL` 返回 `Err` 时：
  1. `finish_turn_or_fail` 内部先调用了第 1 次 `self.notify_terminal(turn, TurnTerminal::Failed, Some(persist_failure_message(&error)))` 并返回 `false`；
  2. `finish_failed` 丢弃了该 `bool` 返回值，继续执行 `observe_stop_hook`，随后在函数末尾又调用了**第 2 次 `self.notify_terminal(...)`**。
  - **后果**：`terminal_port` 被重复触发两次，导致子代理回执（ADR-0014）、Goal 预算记账（ADR-0015）、Handoff 自动续推调度（ADR-0016）全部收到双份终态通知。
- **修法**：
  - 持久化辅助函数**严禁越权调用 `notify_terminal`**，统一返回 `Result<(), DbError>`（或明确终态结论）；
  - 由调用方 / 统一收口函数保证每个回合**恰好调用一次** `notify_terminal`；
  - 若 `finish_failed` 自身写库也失败，合并原业务失败原因与持久化失败提示（含「磁盘空间不足，上下文未保存」）后单次上报。

### 2. Blocker 2（高危）：修复 `owner.finish_turn` 失败时前端事件流永不收到 `TurnEnded` 导致 UI 永久挂死
- **锚点**：
  - `crates/qaqh-session/src/owner.rs:522-552`（`SessionOwner::finish_turn`）
  - `crates/qaqh-agent-runtime/src/session_loop.rs:2424-2434`（`finish_turn_or_fail`）
  - `crates/qaqh-agent-runtime/src/session_loop_tests.rs:527-539`（`disk_full_seal_fails_closed_instead_of_lying_completed`）
- **病灶**：
  - 原测试把 `PRAGMA max_page_count` 设成 `pages + 4`，仅让 2 MiB 的 `append_block` 撞墙失败；随后 `finish_persist_failed` 写几十字节的 `turns.outcome_json` 侥幸塞进了剩余 4 页写成功，**从未覆盖 `finish_turn` 自身失败**的场景；
  - 一旦 `pump.db.write(finish_turn).await?` 返回 `Err`，`owner.finish_turn` 直接提前返回，**既不发 `SessionEvent::TurnEnded`，也不发 `SessionEvent::ActivityChanged(Idle)`**；
  - `session_loop.rs` 调用的 `notify_terminal` 只通知内部 `terminal_port`，不会广播到前端 SSE 事件流。结果就是真 0 字节盘满或 `finish_completed` 自身写库失败时，前端永久停在 Running 死等，且用户看不到「磁盘空间不足，上下文未保存」。
- **修法**：
  - 在 `SessionOwner` 新增显式内存广播入口（如 `pub async fn emit_turn_persist_failed(&self, turn_id: &str, usage: Option<UsageInfo>, code: &str, message: String)`），走泵内 `emit` 顺序发布：
    1. `SessionEvent::TurnEnded { turn_id, outcome: TurnOutcome::Finished { terminal: TurnTerminal::Failed, usage, error: Some(TurnError { code, message, retryable: false, details_ref: None }) } }`
    2. `SessionEvent::ActivityChanged { activity: ActivityState::Idle, current_turn_id: None }`
  - 当 `owner.finish_turn` 落库失败时，强制调用该口向前端事件流广播失败终态与空闲状态，确保 UI 立即解锁并展示错误提示。
- **红→绿反证要求**：
  - 新增一条 `finish_turn` 自身写库失败的端到端反证测试（例如对 `turns` 表创建 `BEFORE UPDATE` `RAISE(ABORT, ...)` 触发器，或精确收紧 `max_page_count` 卡死 `finish_turn`），断言：
    1. 事件流在时限内必收到 `TurnEnded(Failed)`（携带 `response_persist_failed` 与盘满提示）以及 `ActivityChanged(Idle)`（旧实现超时卡死，必红）；
    2. `terminal_port` 回调计数精确等于 `1`（同步反证 Blocker 1，旧实现在 `finish_failed` 路径会触发 `2` 次）。

### 3. Blocker 3（宪法 #8 硬门禁）：将持久化收口整体迁入 `session_persist.rs`，使 `session_loop.rs` 净行数 `≤ 0`
- **锚点**：
  - `crates/qaqh-agent-runtime/src/session_loop.rs`（当前 `+157 / -116`，**净 `+41` 行**，违反宪法 #8「> 1,000 行生产文件净行数只减不增」）
  - `crates/qaqh-agent-runtime/src/session_persist.rs`（当前仅 46 行，生产代码仅 12 行）
- **修法**：
  - 经核查，以下函数**零访问 `SessionLoop` 私有字段**，仅消费 `&SessionOwner` / `&TurnId` / `&AttemptBlocks` / `&ProviderOutcome` / `TurnOutcome` 等参数，可直接整块迁入 `crates/qaqh-agent-runtime/src/session_persist.rs`：
    1. `seal_response`（含 reasoning/answer 提取）+ `seal_partial` + `seal_assistant_blocks`；
    2. `finished_outcome` + 统一的持久化与前端失败广播收口函数（融合 Blocker 1 与 Blocker 2 的修复逻辑）。
  - `session_loop.rs` 内仅保留每个调用点 **≤ 3 行**的薄调用接线。
- **硬性验收指标**：
  - `git diff --numstat bdf29170..HEAD -- crates/qaqh-agent-runtime/src/session_loop.rs` 的**净行数变化必须 `≤ 0`**（预计可达净减 `-60 ~ -100` 行）。

### 4. 中危 4：修复 `read_pool.rs` 假测试，真实覆盖 `Condvar::wait` 池满排队
- **锚点**：`crates/qaqh-session/src/db/read_pool.rs:215-241`（`leases_queue_when_pool_is_exhausted_then_drain`）
- **病灶**：池容量 `READ_POOL_CAPACITY = 4`，但该测试只启动了 `for _ in 0..4`（恰好 4 个线程）且无持锁睡眠或同步屏障，4 线程配 4 连接导致池从未打满，`Condvar::wait` 一行代码都未跑到。
- **修法**：
  - 改为启动 **8 个并发线程**（明确 `> READ_POOL_CAPACITY`），每个线程借用连接后持有 `40ms`；
  - 使用 `AtomicUsize` 记录实时活跃连接数与峰值（`max_active`），断言 `max_active == 4`（证明真正打满且严格不超容），且 8 个线程全部排队完成后连接正常回流无死锁。

### 5. 中危 5：`delete_orphan_session_contents` 防误删保留回合用户附件 BLOB
- **锚点**：
  - `crates/qaqh-session/src/db/maintenance.rs:174-205`（`delete_orphan_session_contents`）
  - 关联写入方：`crates/qaqh-ringing-runtime/src/hub.rs:50`（用户上传附件写入 `session_contents`，由 `blocks.payload_json` 引用）
- **病灶**：`session_contents` 按内容 SHA-256（`content_id`）寻址。用户上传附件也落在该表并由 `blocks.payload_json` 引用；目前 `delete_orphan_session_contents` 只检查 `tool_calls` 与 `exec_sessions` 的剩余引用，若被撤回合的工具输出与保留回合的用户附件发生内容碰撞，撤回时会误删保留回合的附件 BLOB。
- **修法**：
  - 在 `DELETE FROM session_contents` 的 `NOT EXISTS` 守卫中追加对 `blocks` 表的检查：
    ```sql
    AND NOT EXISTS (
        SELECT 1 FROM blocks b
        WHERE b.session_id = ?1 AND instr(b.payload_json, ?2) > 0
    )
    ```
  - 补充一条回归测试：保留回合的 `blocks.payload_json` 引用了与撤回回合相同的 `content_id` 时，`truncate_from_turn` 绝不误删该 `session_contents` 行。

---

## 二、Fast-Follow 独立修复单范围（`v4.6 P3-0` 前置清债，已按 `fc1ed287` 刷新精确行号）

> 本节所列病灶于 PR #45 合入后单独立项为 **`v4.6 P3-0`**，先于 `P3-1` 执行（避免与 `P3-1` 争抢 `session_loop.rs` 并同步清零已知基线红灯）。已按 **`main @ fc1ed287`（`session_loop.rs` 现 2,887 行）** 刷新全部锚点：共 **16 处 `.await\n.ok()` 吞错**、**2 处错误回滚内存态泄漏**，外加 **1 项基线存量红灯修复**。

### 优先级 1（高 / 中高危，防终态悬挂、历史结构损坏与已知红灯）
1. **A 类（高危 ×1）**：
   - `session_loop.rs:2396`：`interrupt()` 内 `self.owner.finish_turn(turn.as_ref(), "interrupted", TurnOutcome::Interrupted { reason }).await.ok()`。
   - **Fail-Closed 策略**：`finish_turn` 失败严禁吞错，必须响亮 `log::error!`，经 `owner.emit_turn_persist_failed` 向前端广播 `TurnEnded(Failed)` + `ActivityChanged(Idle)`，并单点上报 `notify_terminal(TurnTerminal::Failed, ...)`（与 PR #45 Blocker 1/2 完全同律，防库内永久无终态或 UI 挂死）。
2. **D 类（中高危 ×3）**：
   - `session_loop.rs:1331`（`inject_failed_tool_results` 写截断 assistant prefix 块）
   - `session_loop.rs:1361`（`inject_failed_tool_results` 写截断 `ToolResult` 块）
   - `session_loop.rs:1400`（`declare()` 写 `ToolCall` 块）
   - **Fail-Closed 策略**：三处 `append_block` 均改为向上返回 `Result<..., DbError>`，任一写库失败立即中止工具派发/重试注入，**严禁将未落库消息塞入内存 `messages`**，交由主循环统一收口为 `finish_persist_failed`（守住 `L770`「落库成功才并入 `messages`」与 `assemble_context()` 配对不变量）。
3. **基线存量红灯修绿（并入 `P3-0`）**：
   - 锚点：[crates/qaqh-runtime/tests/tool_surface_schema_shape.rs](../../crates/qaqh-runtime/tests/tool_surface_schema_shape.rs)（`full_surface_schemas_are_object_shaped_without_composition_keys` 报告 `exec_command` schema 存在 `oneOf` 泄漏）。
   - **策略**：定位并消除 `exec_command` 入参 schema 中的 `oneOf`/`anyOf`/`allOf` 聚合关键字生成，使 `P3-1` 开工前主线测试零已知红灯（宪法 #12）。

### 优先级 2（中危，防幽灵审批卡与内存态泄漏）
1. **B 类（`interaction_resolved` 吞错 ×5）**：
   - `session_loop.rs:1794`（权限取消）、`1815`（权限批准）、`1904`（ask 取消/关闭）、`1926`（ask 应答完成）、`2382`（`resolve_pending_interactions` 批量取消）。
   - **Fail-Closed 策略**：取消 `.ok()` 吞错，写库失败必须显式 `log::error!` 并向调用方/回合终态传播持久化错误（不再「开口守卫、收口吞错」制造幽灵审批卡）。
2. **错误回滚内存态泄漏（×2）**：
   - `session_loop.rs:1767`（`request_permission` @ `L1713`）：此前压入了 `active.pending_interactions`，但 `interaction_requested` 失败回滚时仅清理了 `inner.permissions`，必须同步清理 `active.pending_interactions`；
   - `session_loop.rs:1867`（`run_ask` @ `L1826`）：此前压入了 `inner.asks` 与 `active.pending_interactions`，`interaction_requested(view).await?` 提前返回时必须清理这两处内存槽。

### 优先级 3（中低危，防阶段与状态机投影漂移）
- **C 类（×7）**：
  - `session_loop.rs:1726`（`tool_awaiting_interaction`——参与 owner 泵定序与前端等待态展示，写库失败按交互路径回滚/报错不跳过）
  - `session_loop.rs:1609`（`tool_back_to_declared`——显式 `log::error!` 并按路径返回错误/记故障，不静默吞错）
  - `session_loop.rs:1775, 1820, 1871, 1909, 1931`（`set_turn_phase` ×5——取消 `.ok()`，写库失败显式 `log::error!` 并返回/记故障避免 `WaitingUser` 与 `Running` 状态漂移）

### Fast-Follow 硬性施工纪律与派工 Prompt
- 严禁靠“口头登记豁免”继续给 `session_loop.rs` 堆行：必须把交互落库 / 阶段流转 / 截断注入辅助逻辑同步拆出至独立子模块（如 `session_persist.rs` 或新增交互辅助子模块），**确保 `session_loop.rs` 净行数变更 `≤ 0`**（宪法 #8）。
- 本 PR 不引入新契约变化，铁闸 1（`qaqh-contract-export check --against docs/api-surface/v4.2`）必须仍为 `EXIT=0`。

```markdown
请在 `/home/qaqtam/项目/qaqh-backend` 推进 `v4.6 P3-0`（Fast-Follow：清剿 `session_loop.rs` 16 处 `.await.ok()`、2 处回滚泄漏与 `tool_surface_schema_shape` 基线红灯），依据 `docs/v4.3-4.8/V4_5_1_PR45_REMEDIATION_GUIDE.md` §二。

1. 在 `../qaqh-wt-v4.6-p3-0` 从 `origin/main` 开分支 `fix/v4.6-p3-0-fail-closed-and-schema`（禁 `git checkout`，共享 `CARGO_TARGET_DIR="/home/qaqtam/项目/qaqh-backend/target"`）。
2. 按 §二 优先级 1–3 与对应 Fail-Closed 策略清零 16 处 `.await.ok()` 与 2 处回滚泄漏，辅助函数拆入独立子模块保证 `session_loop.rs` 净行数 `≤ 0`；并修复 `tool_surface_schema_shape`（`exec_command` `oneOf` 泄漏）。
3. 提供红→绿反证（覆盖 `interrupt()` / `declare()` / 回滚清理及 `tool_surface_schema_shape`），并跑 `/home/qaqtam/.cargo/bin/cargo test -j2 -p qaqh-session -p qaqh-agent-runtime -p qaqh-runtime`、clippy 及铁闸 1 `EXIT=0`。
4. 提交（带 `Co-Authored-By:`）并 `gh pr create`（不自行 merge）。
```

---

## 三、给执行模型的返修派工 Prompt 模板

```markdown
请在原 worktree `/home/qaqtam/项目/qaqh-wt-v4.5.1-storage-hardening`（分支 `fix/v4.5.1-storage-hardening`）上修复 PR #45 的评审阻塞项（见 `docs/v4.3-4.8/V4_5_1_PR45_REMEDIATION_GUIDE.md` §一）。

### 开工前核查（宪法 #14、#15、#16）
1. 阅读 `AGENTS.md`（宪法 v2）、`docs/v4.3-4.8/V4_5_1_EXECUTION_PLAN.md` 与 `docs/v4.3-4.8/V4_5_1_PR45_REMEDIATION_GUIDE.md` §一。
2. 先检查工作树与远端状态（禁 `git checkout`）：
   - `git -C /home/qaqtam/项目/qaqh-wt-v4.5.1-storage-hardening fetch origin`
   - `git -C /home/qaqtam/项目/qaqh-wt-v4.5.1-storage-hardening status -sb`
   - 核查本机内存与磁盘（8 逻辑核 / 7.7 GiB RAM，禁止本地全仓 `--release`）。
3. 使用任务列表工具同步你的返修步骤后再改代码。

### 强制返修清单（5 项全部必做，不得跳过）
1. **Blocker 1（消除双重 `notify_terminal`）**：修改 `finish_turn_or_fail` / `finish_failed`（连同迁入 `session_persist.rs`），落库 helper 返回 `Result<(), DbError>` 不越权调 `notify_terminal`，保证任何失败路径每回合恰好调用一次 `notify_terminal`。
2. **Blocker 2（修复 `finish_turn` 失败前端挂死）**：在 `SessionOwner` 增加显式内存广播入口（发布 `TurnEnded(Failed)` + `ActivityChanged(Idle)`），`finish_turn` 落库失败时强制广播解锁前端；补充 `finish_turn` 自身失败（触发器或精确满页）且验证 `terminal_port` 计数 `== 1` 的红→绿反证测试。
3. **Blocker 3（宪法 #8 达标：`session_loop.rs <= 0`）**：将 `seal_response`、`seal_partial`、`seal_assistant_blocks`、`finished_outcome` 及统一收口持久化逻辑整块迁入 `crates/qaqh-agent-runtime/src/session_persist.rs`，`session_loop.rs` 仅留薄接线，使 `git diff --numstat origin/main..HEAD -- crates/qaqh-agent-runtime/src/session_loop.rs` **净行数 `≤ 0`**。
4. **中危 4（修 `read_pool` 假测试）**：将 `leases_queue_when_pool_is_exhausted_then_drain` 改为 8 线程并发持连接 `40ms`，用 `AtomicUsize` 断言并发峰值 `== 4` 且全部排队完成后正常归还。
5. **中危 5（防误删保留回合附件 BLOB）**：在 `maintenance.rs::delete_orphan_session_contents` 增加 `NOT EXISTS (SELECT 1 FROM blocks b WHERE b.session_id = ?1 AND instr(b.payload_json, ?2) > 0)` 守卫，并补一条回归测试验证保留回合 `blocks.payload_json` 引用的 `content_id` 不会被 `truncate_from_turn` 误删。

### 环境与验证纪律
- `cargo` 绝对路径：`/home/qaqtam/.cargo/bin/cargo`；共享 `CARGO_TARGET_DIR="/home/qaqtam/项目/qaqh-backend/target"`。如遇跨 worktree stale artifact 假报错，执行 `touch crates/qaqh-session/src/db/mod.rs` 后重跑。
- 不要把 §二 Fast-Follow 范围的 16 处顺手塞进本 PR（留待独立 fast-follow PR）。
- 验证命令（须全部通过，基线存量失败 `tool_surface_schema_shape` 如实注明作用域）：
  - `/home/qaqtam/.cargo/bin/cargo test --no-fail-fast -p qaqh-session -p qaqh-agent-runtime -p qaqh-runtime`
  - `/home/qaqtam/.cargo/bin/cargo clippy --no-deps --lib -p qaqh-session -p qaqh-agent-runtime -p qaqh-runtime`
  - `/home/qaqtam/.cargo/bin/cargo run -p qaqh-contract-export -- check --against docs/api-surface/v4.2`（必须 `EXIT=0`）
  - `git -C /home/qaqtam/项目/qaqh-wt-v4.5.1-storage-hardening diff --numstat origin/main..HEAD -- crates/qaqh-agent-runtime/src/session_loop.rs`（确认净行数 `≤ 0`）
- 追加提交（含 `Co-Authored-By:` trailer）并 `git push` 回 `fix/v4.5.1-storage-hardening` 更新 PR #45，同步更新 PR 描述中的 `numstat` 与新增反证结果。**不要 merge，停下等待验收官复审。**
```

---

## 四、PR #45 复审通过门禁清单

- [x] **Blocker 1**：`finish_failed` 在 `owner.finish_turn` 失败路径下 `terminal_port` 触发次数实测 `== 1`（`6f71212e` 补齐 `finish_turn_persist_failure_unlocks_ui_and_notifies_terminal_once` 与 `finish_completed_persist_failure_emits_failed_and_idle` 并实测通过）。
- [x] **Blocker 2**：`owner.finish_turn` 自身失败时前端事件流确定收到 `TurnEnded(Failed)`（含 `response_persist_failed` +「磁盘空间不足，上下文未保存」）与 `ActivityChanged(Idle)`（实测通过）。
- [x] **Blocker 3（宪法 #8）**：`session_loop.rs` 净 `-137` 行（`+49 / -186`），持久化收口完整沉淀在 `session_persist.rs`；同时在 `cb1afa3d` 同步拆出 `replay_ring.rs`，使 `owner.rs` 净 `-100` 行（`+107 / -207`）、`store.rs` 仅 `+4` 行，全面达成宪法 #8。
- [x] **中危 4**：`leases_queue_when_pool_is_exhausted_then_drain` 改为 8 线程 `Barrier` 并发持连接 `40ms`，实测断言并发峰值 `== 4` 且真实走过 `Condvar::wait` 排队路径。
- [x] **中危 5**：`delete_orphan_session_contents` 包含 `blocks.payload_json` 防误删守卫，且新增 `truncate_keeps_blob_referenced_only_by_retained_block_payload_json` 回归测试全绿。
- [x] **全量门禁**：`cb1afa3d` 终审实测 `qaqh-session --lib`（55 passed / 1 ignored）、`disk_full_fail_closed_tests` + `session_persist::tests`（5 passed / 0 failed）、三 crate `clippy --no-deps --lib` 零错误且新增模块零警告、`SCHEMA_VERSION == 13`、铁闸 1 `EXIT=0`（注：终审未在本地重跑 `qaqh-runtime` 全量测试，该项在 `6f71212e` 已报告 `qaqh-runtime --lib` 65 passed，基线存量失败为 `tool_surface_schema_shape`）。
