# QAQH v2 P0 Gate 与 I1-I18 测试追踪报告

> 基线：`origin/betav2 @ 66539a0`
> 日期：2026-09-20
> Issue：[#118](https://cnb.cool/QAQ-Harness/qaqh-backend/-/issues/118)
> 范围：只做证据追踪与缺口报告，不修改生产代码、fixture、测试或既有文档。

## 0. 方法与状态定义

本报告以当前仓库源码和测试 target 为依据，把冻结契约中的 I1-I18 与 P0-P6 Gate 映射到：

- 当前测试 target 或源码证据；
- 合法精确命令；
- 当前状态；
- 首次必须通过的阶段；
- 缺口与建议 issue。

状态定义：

| 状态 | 含义 |
|---|---|
| `existing` | 目标测试已存在，且直接覆盖当前 v1 行为 |
| `partial` | 有相关测试，但覆盖的是 v1 语义或只覆盖不变量的一部分 |
| `missing` | 当前没有对应测试 target 或 fixture |
| `blocked` | 依赖尚未实现的 canonical store/SessionActor/ToolRuntime，当前无法形成 E1 证据 |

E1 = 当前实现上的直接测试证据；E2 = 现有源码/测试基线；E3 = 设计目标或待实现 fixture。本报告不把 E2/E3 当作 v2 完成证据。

## 1. 当前可复现测试抽查

以下命令均在本 worktree、commit `66539a0` 上执行，使用 `--test-threads=1` 避免全局状态竞争；未使用墙钟阈值作为通过条件。

| 命令 | 结果 | 关联不变量 |
|---|---|---|
| `cargo test -p qaqh-session --test save_append_watermark -- --test-threads=1` | 6 passed | I2（v1 消息水位部分） |
| `cargo test -p qaqh-runtime --test subagent_inprocess -- --test-threads=1` | 4 passed | I16（v1 子代理生命周期部分） |
| `cargo test -p qaqh-runtime --test tool_outbox_locking -- --test-threads=1` | 3 passed | I1/I2（tool outbox 锁与 flush 部分） |
| `cargo test -p qaqh-workspace --test permission_level_fail_closed -- --test-threads=1` | 5 passed | I10（权限档位 fail-closed 部分） |
| `cargo test -p qaqh-runtime --test timeline_stale_restore -- --test-threads=1` | 2 passed | I3（timeline 从 messages 重建部分） |
| `cargo test -p qaqh-runtime --test ringing_architecture -- --test-threads=1` | 2 passed | I12（领域/传输依赖边界部分） |
| `cargo test -p qaqh-runtime --test ask_user_lifecycle -- --test-threads=1` | 15 passed | I4/I9（v1 ask 生命周期部分） |
| `cargo test -p qaqh-runtime --test permission_lifecycle -- --test-threads=1` | 10 passed | I5/I9/I10（v1 permission 生命周期部分） |
| `cargo test -p qaqh-runtime --test replay_equivalence -- --test-threads=1` | 7 passed | I8（v1 Ringing replay 部分） |

抽查总计 **54 passed / 0 failed**。这些结果证明 v1 基线仍稳定，不证明 I1-I18 的 v2 语义已经实现。

## 2. I1-I18 追踪矩阵

### I1 单 session 单 writer

- Gate：P1。
- 状态：`partial`。
- 现有证据：
  - `crates/qaqh-runtime/tests/tool_outbox_locking.rs` 验证 outbox 锁分片与 flush 行为；
  - 命令：`cargo test -p qaqh-runtime --test tool_outbox_locking -- --test-threads=1`；
  - 结果：3 passed。
- 缺口：
  - 当前不存在 canonical `events.jsonl` writer facade；
  - 不存在跨进程 writer conflict；
  - 不存在 epoch/token stale writer 拒绝；
  - 不存在 canonical JSONL 无交叉写断言。
- 建议 issue：`[P1] canonical writer facade、writer fence 与并发 append 反证`。

### I2 `fact_seq` 连续、snapshot 不领先

- Gate：P1。
- 状态：`partial`。
- 现有证据：
  - `crates/qaqh-session/tests/save_append_watermark.rs` 验证 messages append 幂等、水位与 torn tail；
  - `tool_outbox_locking` 验证显式 flush/后台 flusher 的锁与 fsync 行为。
- 缺口：
  - 没有 canonical `fact_seq`；
  - 没有 group commit/crash 注入后的 seq 连续性；
  - 没有 snapshot cursor <= log cursor 的 v2 断言。
- 建议 issue：`[P1] events.jsonl fact_seq 连续性、group commit 与 snapshot high-water`。

### I3 projection 可重建

- Gate：P1。
- 状态：`partial`。
- 现有证据：
  - `crates/qaqh-runtime/tests/timeline_stale_restore.rs`；
  - 命令：`cargo test -p qaqh-runtime --test timeline_stale_restore -- --test-threads=1`；
  - 结果：2 passed；
  - 当前重建输入是 `messages.jsonl`，不是 canonical facts。
- 缺口：
  - 删除 derived 后从 `events.jsonl` 重建等价；
  - `ContentUnavailable` 与 content GC marker 保留；
  - projection revision 单调。
- 建议 issue：`[P1] ProjectionSet rebuild 与 ContentUnavailable 等价性`。

### I4 单 active turn

- Gate：P2。
- 状态：`partial`。
- 现有证据：
  - `crates/qaqh-runtime/tests/ask_user_lifecycle.rs` 15 passed；
  - 现有 cancel/resume/ask 生命周期覆盖 v1 turn 行为。
- 缺口：
  - canonical `TurnStarted/TurnFinished/TurnInterrupted` 单 active turn；
  - `input_purpose=trigger_turn` 的恢复补 turn；
  - concurrent input/cancel/resume 的 v2 反例。
- 建议 issue：`[P2] SessionActor 单 active turn 与 TurnCore 迁移`。

### I5 工具单终态

- Gate：P3。
- 状态：`partial`。
- 现有证据：
  - `crates/qaqh-runtime/tests/permission_lifecycle.rs` 10 passed；
  - `tool_outbox_locking` 覆盖 outbox flush/lock；
  - 现有 v1 `ToolStatus` 与 cancel 路径。
- 缺口：
  - 每 call 恰好一个 canonical `ToolFinished`；
  - 无 `ToolIntent` 的 deny/ask/cancel 路径；
  - `Indeterminate` 后 resume 拒绝；
  - interaction terminal 后不再重放 pending modal。
- 建议 issue：`[P3] ToolIntent/ToolFinished ledger 与 terminal 唯一性`。

### I6 非幂等工具不重跑

- Gate：P3。
- 状态：`missing` / `blocked`。
- 现有证据：无 v2 durable intent/replay 证据。
- 缺口：
  - `no_replay` crash fixture；
  - `idempotent_replay` 重复 load 不重复副作用；
  - `reconcile` probe conclusive/inconclusive 两分支。
- 建议 issue：`[P3] replay capability 与 side-effect crash matrix`。

### I7 typed output 四投影同源

- Gate：P3。
- 状态：`partial`。
- 现有证据：当前工具结果展示契约与 workspace typed output 代码已有骨架，但没有 v2 canonical `ToolFinished` 接线。
- 缺口：
  - model/display/resource/service 从同一 typed output 派生；
  - service/TUI 不再 JSON 考古；
  - `ToolFinished.output_ref` 与 projection 一致。
- 建议 issue：`[P3] typed tool output 四投影同源验收`。

### I8 重连无 gap/dup

- Gate：P5。
- 状态：`partial`。
- 现有证据：
  - `crates/qaqh-runtime/tests/replay_equivalence.rs` 7 passed；
  - 覆盖 v1 reliable replay、replaceable、epoch/cursor reset。
- 缺口：
  - canonical `(fact_seq, projection_index)` 严格递增；
  - v1 `Last-Event-ID` 1:N mapping；
  - replay window floor/expiry；
  - snapshot/replay 原子订阅。
- 建议 issue：`[P5] canonical cursor 与 v1 Last-Event-ID 映射验收`。

### I9 interaction 幂等

- Gate：P5。
- 状态：`partial`。
- 现有证据：
  - `ask_user_lifecycle` 15 passed；
  - `permission_lifecycle` 10 passed。
- 缺口：
  - `InteractionResolved/Expired` canonical first-answer-wins；
  - duplicate resolution rejected input；
  - call terminal 与 interaction terminal 的顺序闭合。
- 建议 issue：`[P5] InteractionResolved/Expired canonical 幂等与竞态`。

### I10 权限 fail-closed

- Gate：P4。
- 状态：`partial`。
- 现有证据：
  - `crates/qaqh-workspace/tests/permission_level_fail_closed.rs` 5 passed；
  - 覆盖档位解析与写权限 fail-closed 方向。
- 缺口：
  - sandbox/policy/audit barrier；
  - `AuditWriter` durable `sync_data` ack；
  - 失败进入 quarantine/Indeterminate；
  - 配置解析与执行期授权分开验收。
- 建议 issue：`[P4] Policy/Sandbox/Audit fail-closed barrier`。

### I11 TUI reducer 纯

- Gate：P5。
- 状态：`blocked`。
- 现有证据：跨仓 TUI 有既有 reducer 成果，但当前 backend 没有固定 TUI rev 的联调证据。
- 缺口：
  - 同一事件序列任意重放得到同一 `SessionModel`；
  - snapshot rebaseline/epoch 防护；
  - TUI pin bump 与固定 backend rev。
- 建议 issue：`[P5] TUI SessionModel reducer 跨仓固定 rev 验收`。

### I12 wire 不泄漏存储

- Gate：P5/P6。
- 状态：`partial`。
- 现有证据：
  - `crates/qaqh-runtime/tests/ringing_architecture.rs` 2 passed；
  - 当前已验证 domain crate 不依赖 legacy/wire、无 Agent2UI → Ringing bridge。
- 缺口：
  - client/TUI 不引用 journal/checkpoint/offload/messages 路径；
  - 静态路径扫描 `legacy_paths_absent`；
  - `serde_json::Value` 兼容面收口。
- 建议 issue：`[P5/P6] wire 存储路径泄漏静态门禁`。

### I13 背压有界

- Gate：P1/P2/P3/P4/P5。
- 状态：`missing`。
- 现有证据：当前有 v1 Ringing outbox/背压实现，但没有 v2 容量基线。
- 缺口：
  - actor/client/replay/progress/content 队列 high-water；
  - disconnect/reset/拒绝策略；
  - 不丢 reliable revision；
  - 不 OOM。
- 建议 issue：`[P1/P5] canonical/replay/content 背压高水位矩阵`。

### I14 未知 fact fail-closed

- Gate：P0/P1/P6。
- 状态：`partial`。
- 现有证据：PR #115 分支已有 unknown kind fail-closed 类型测试，但尚未合并；当前 `betav2` 没有 canonical fact。
- 缺口：
  - 同 schema unknown kind → read-only/upgrade-required；
  - 不跳过 unknown fact 继续解释后缀；
  - unknown payload version；
  - upgrade writer 新 batch。
- 建议 issue：`[P1] unknown fact/version fail-closed fixture`。

### I15 路径/TOCTOU 安全

- Gate：P4。
- 状态：`partial`。
- 现有证据：workspace 有 `file_shared`、`file_query`、`apply_patch`、`edit` 相关测试。
- 缺口：
  - symlink/device/FIFO/TOCTOU 完整矩阵；
  - SandboxSpec hash 与 object refs；
  - rename/open 竞态。
- 建议 issue：`[P4] sandbox path/TOCTOU 完整矩阵`。

### I16 子代理无孤儿与 message purpose

- Gate：P2。
- 状态：`partial`。
- 现有证据：
  - `crates/qaqh-runtime/tests/subagent_inprocess.rs` 4 passed；
  - `#114` 已登记 parent close/unload 的 child cancel/join 最小切片。
- 缺口：
  - parent unload/delete/shutdown/panic 前 child 全部 terminal+join；
  - spawn 双向孤儿恢复扫描；
  - trigger-turn/queue-only 恢复不产生第二个 turn；
  - canonical `SubagentSpawned/Finished` edge。
- 建议 issue：`[P2] SubagentSupervisor canonical edge 与孤儿恢复`；最小切片沿用 `#114`，不重复建 issue。

### I17 commit/clock 单解释

- Gate：P1。
- 状态：`missing` / `blocked`。
- 现有证据：无 `events.commit.json`、`CommitRecoveryRequired` 或 clock 实现。
- 缺口：
  - marker missing/repair durable 分支；
  - fsync EIO、crash-before-rename；
  - `commit_repaired`/`projection_rebuilt`；
  - clock 不越过 committed high-water。
- 建议 issue：`[P1] events.commit/poison/upgrade sidecar 与 recovery clock`。

### I18 root quota 跨 session 串行

- Gate：P2。
- 状态：`missing` / `blocked`。
- 现有证据：无 root `QuotaLedger` 或 `quota.lock` 实现。
- 缺口：
  - root 与全部 child 经同一 owner/lock 串行 reservation；
  - child close/delete/cutover 不删账本；
  - reconciliation 无超卖；
  - quota 与 I16 顺序分开判定。
- 建议 issue：`[P2] root QuotaLedger/quota.lock 并发验收`。

## 3. P0-P6 Gate 追踪

| Gate | 冻结要求 | 当前证据 | 状态 | 缺口 / 首次必须通过 |
|---|---|---|---|---|
| P0 | 契约冻结、基线可重复、无未裁决 P0 | `#103/#105/#106` 已合并；本报告 9 个 target、54 tests 抽查通过 | `existing` | `#116` 回填文档状态；`#117` 消费者盘点；本报告 `#118` 补齐 traceability |
| P1 | canonical log + ProjectionSet | `#111` 类型/PR #115；`#113` outbox 目标 | `partial` | writer facade、`events.jsonl`、sidecar、projection rebuild、cursor、unknown fact |
| P2 | SessionActor + TurnCore | v1 ask/cancel 基线；`#114` 子代理最小切片 | `partial` | active turn、mailbox、cancel token tree、SubagentSupervisor、quota |
| P3 | ToolRuntime + Typed Tool SDK | v1 permission/tool ordering 基线；outbox 已存在 | `partial` | ToolIntent/ToolFinished ledger、typed output、resume CAS、Indeterminate |
| P4 | Policy + Sandbox | `permission_level_fail_closed` 5 passed | `partial` | policy/sandbox/audit barrier、AuditWriter durable ack、TOCTOU 矩阵 |
| P5 | Ringing v2 + Client/TUI | `replay_equivalence` 7 passed；TUI 外部工作已有 | `partial` | canonical cursor、ResetRequired、interaction replay、固定 TUI rev |
| P6 | 清理与可选 composition root | 当前无 canonical 单源 | `blocked` | 删除旧 journal/checkpoint/offload 前必须先有 canonical 重建与迁移证据 |

## 4. 现有测试 target 与精确命令

以下命令均使用合法形式 `cargo test -p <pkg> --test <target> -- --exact <full::test::path>`；未执行 `--exact` 的 target 只作为 gate 级抽查。

### `save_append_watermark`

```bash
cargo test -p qaqh-session --test save_append_watermark -- --test-threads=1
```

精确测试：

```bash
cargo test -p qaqh-session --test save_append_watermark -- --exact save_append_watermark::max_msg_id_matches_full_scan
cargo test -p qaqh-session --test save_append_watermark -- --exact save_append_watermark::repeated_appends_do_not_rescan_the_archive
cargo test -p qaqh-session --test save_append_watermark -- --exact save_append_watermark::append_idempotency_survives_watermark_cache
cargo test -p qaqh-session --test save_append_watermark -- --exact save_append_watermark::external_tail_append_invalidates_watermark
cargo test -p qaqh-session --test save_append_watermark -- --exact save_append_watermark::delete_then_recreate_starts_from_scratch
cargo test -p qaqh-session --test save_append_watermark -- --exact save_append_watermark::torn_tail_line_does_not_lower_watermark
```

### `tool_outbox_locking`

```bash
cargo test -p qaqh-runtime --test tool_outbox_locking -- --test-threads=1
```

精确测试：

```bash
cargo test -p qaqh-runtime --test tool_outbox_locking -- --exact tool_outbox_locking::flush_does_not_serialize_on_a_slow_session
cargo test -p qaqh-runtime --test tool_outbox_locking -- --exact tool_outbox_locking::flush_is_joined_before_returning
cargo test -p qaqh-runtime --test tool_outbox_locking -- --exact tool_outbox_locking::concurrent_sessions_scale_end_to_end
```

### `timeline_stale_restore`

```bash
cargo test -p qaqh-runtime --test timeline_stale_restore -- --test-threads=1
```

精确测试：

```bash
cargo test -p qaqh-runtime --test timeline_stale_restore -- --exact timeline_stale_restore::stale_timeline_snapshot_is_rebuilt_from_messages_instead_of_restored
cargo test -p qaqh-runtime --test timeline_stale_restore -- --exact timeline_stale_restore::windowed_but_tail_consistent_snapshot_is_restored_without_rebuild
```

### `subagent_inprocess`

```bash
cargo test -p qaqh-runtime --test subagent_inprocess -- --test-threads=1
```

精确测试：

```bash
cargo test -p qaqh-runtime --test subagent_inprocess -- --exact subagent_inprocess::spawn_subagent_runs_inprocess_loops_and_shutdown_signals_all
cargo test -p qaqh-runtime --test subagent_inprocess -- --exact subagent_inprocess::spawn_subagent_does_not_go_through_process_spawn
cargo test -p qaqh-runtime --test subagent_inprocess -- --exact subagent_inprocess::spawn_subagent_registers_liveness
cargo test -p qaqh-runtime --test subagent_inprocess -- --exact subagent_inprocess::parent_cancel_propagates_to_children
```

### `permission_level_fail_closed`

```bash
cargo test -p qaqh-workspace --test permission_level_fail_closed -- --test-threads=1
```

精确测试：

```bash
cargo test -p qaqh-workspace --test permission_level_fail_closed -- --exact permission_level_fail_closed::invalid_levels_never_resolve_to_unrestricted
cargo test -p qaqh-workspace --test permission_level_fail_closed -- --exact permission_level_fail_closed::valid_levels_keep_their_meaning
cargo test -p qaqh-workspace --test permission_level_fail_closed -- --exact permission_level_fail_closed::invalid_level_requires_approval_for_workspace_write
cargo test -p qaqh-workspace --test permission_level_fail_closed -- --exact permission_level_fail_closed::invalid_level_admits_write_as_approval_required_not_authorized
cargo test -p qaqh-workspace --test permission_level_fail_closed -- --exact permission_level_fail_closed::max_lockdown_asks_even_for_reads_proving_fail_closed_direction
```

### `ask_user_lifecycle` / `permission_lifecycle`

```bash
cargo test -p qaqh-runtime --test ask_user_lifecycle -- --test-threads=1
cargo test -p qaqh-runtime --test permission_lifecycle -- --test-threads=1
```

精确入口示例：

```bash
cargo test -p qaqh-runtime --test permission_lifecycle -- --exact permission_lifecycle::llm_approval_resumes_original_turn_once
cargo test -p qaqh-runtime --test ask_user_lifecycle -- --exact ask_user_lifecycle::cancel_during_gate_emits_one_complete_terminal_transaction
```

### `replay_equivalence`

```bash
cargo test -p qaqh-runtime --test replay_equivalence -- --test-threads=1
```

精确测试：

```bash
cargo test -p qaqh-runtime --test replay_equivalence -- --exact replay_equivalence::live_and_replay_agree_on_reliable_events
cargo test -p qaqh-runtime --test replay_equivalence -- --exact replay_equivalence::replaceable_replay_returns_current_value_unfiltered_by_cursor
cargo test -p qaqh-runtime --test replay_equivalence -- --exact replay_equivalence::replay_since_cursor_is_strictly_greater_for_reliable
cargo test -p qaqh-runtime --test replay_equivalence -- --exact replay_equivalence::restart_preserves_reliable_sequence
cargo test -p qaqh-runtime --test replay_equivalence -- --exact replay_equivalence::cursor_expired_signals_reset_path
cargo test -p qaqh-runtime --test replay_equivalence -- --exact replay_equivalence::channel_replay_merges_seeds_in_stream_order
cargo test -p qaqh-runtime --test replay_equivalence -- --exact replay_equivalence::fresh_connection_skips_reliable_history
```

### `ringing_architecture`

```bash
cargo test -p qaqh-runtime --test ringing_architecture -- --test-threads=1
```

精确测试：

```bash
cargo test -p qaqh-runtime --test ringing_architecture -- --exact ringing_architecture::domain_crate_does_not_depend_on_legacy_or_wire
cargo test -p qaqh-runtime --test ringing_architecture -- --exact ringing_architecture::no_agent2ui_to_ringing_bridge_functions
```

## 5. 缺失或尚未落地的测试 target

以下 target 在报告中作为目标名称出现，但当前仓库没有对应文件，不能作为 gate 证据：

- `reconnect_contract`
- `ringing_v2_contract`
- `permission_matrix`
- `audit_chain_faults`
- `legacy_paths_absent`
- `sandbox_path_toctou`
- `tool_sdk_parity`
- `canonical_log_contract`
- `legacy_migration_v2`

建议按阶段建立：

| 目标 target | 首次 Gate | 建议 issue |
|---|---|---|
| `canonical_log_contract` | P1 | `[P1] events.jsonl writer/seq/commit contract` |
| `projection` | P1 | `[P1] ProjectionSet rebuild 与 cursor` |
| `tool_recovery` | P3 | `[P3] ToolIntent/ToolFinished crash matrix` |
| `policy_lifecycle` | P3/P4 | `[P4] Policy/Sandbox/Audit barrier` |
| `permission_matrix` | P4 | `[P4] permission matrix + TOCTOU` |
| `ringing_v2_contract` | P5 | `[P5] Ringing v2 since_cursor contract` |
| `reconnect_contract` | P5 | `[P5] gap/dup/reset replay contract` |
| `legacy_migration_v2` | P6 | `[P6] legacy cutover/rollback contract` |

## 6. 与现有 issue 的去重关系

| 现有 issue | 覆盖内容 | 本报告不重复建议 |
|---|---|---|
| `#111` | canonical types + golden fixtures | 只引用，不新建类型 issue |
| `#113` | tool_outbox 显式 flush / FS 顺序 | I1/I2 的 outbox 子项沿用 #113 |
| `#114` | parent close/unload child cancel/join | I16 最小切片沿用 #114 |
| `#116` | 文档状态与 schema 字段 errata | 不在本报告重复修文档 |
| `#117` | canonical fact consumer/migration inventory | I3/I8/I12 的消费者盘点引用 #117 |
| `#118` | 本追踪报告 | 只登记缺口，不创建实现 PR |

## 7. 未确认项

1. `I1-I18` 中哪些在 P1/P2 需要拆成独立 issue，哪些可以合并为一个实现 PR。
2. TUI 侧固定 backend rev 和 reducer 测试入口尚未确定。
3. Windows 路径/TOCTOU 和 Linux OS sandbox 的平台覆盖边界尚未固定。
4. `replay_equivalence` 当前覆盖 v1 `stream_seq`，canonical `(fact_seq, projection_index)` 的等价关系尚未建立。
5. `permission_lifecycle` / `ask_user_lifecycle` 是 v1 行为基线，不能作为 v2 canonical interaction 幂等证据。

## 8. 结论

- P0 文档与基线可复现性已达到当前阶段的 `existing` 证据。
- P1-P5 的绝大多数不变量目前只有 `partial` 或 `blocked`，不能把现有 v1 测试通过当作 v2 完成。
- 最优先需要建立的是 P1 的 canonical log/commit/projection 测试，以及 P3 的 ToolIntent/ToolFinished crash matrix。
- 本报告只建立追踪，不改变任何生产代码或既有测试。
