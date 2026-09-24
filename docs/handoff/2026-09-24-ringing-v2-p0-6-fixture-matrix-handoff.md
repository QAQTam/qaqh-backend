# Ringing v2 P0-6 fixture 矩阵 Handoff（2026-09-24）

状态：**已落地**。承接
`docs/handoff/2026-09-24-canonical-writer-fence-release-handoff.md` §4-4。

## 1. 本次范围

P0-6 是「fixture 与故障钩子」，不是新功能。本次把 v2 冻结 spec §13 验收矩阵里
**当前可达**的行全部补上可重复执行的 fixture，并把真实 HTTP/SSE 冒烟脚本扩到同样
的覆盖面。发现并顺手补掉一个契约缺口（见 §4）。

## 2. 新增 / 变更

### 2.1 `crates/qaqh-runtime/tests/v2_acceptance_matrix.rs`（新）

直接驱动真实 `V2ProjectionHub`，用种子 canonical fact 断言：

| 行 | 断言 |
|---|---|
| V2-C1 | 从 bootstrap cursor 订阅不回放历史；之后 commit 的 fact 以 reliable cursor 恰好到达一次 |
| V2-C2 | 从旧 cursor 重连只回放 cursor 之后的 fact，严格 `(fact_seq, projection_index)` 有序 |
| V2-C4 | ephemeral envelope 无 cursor、`cursor_value() == None`，且带 cursor 的 ephemeral 校验失败（wire 契约） |
| V2-C5 | 异 log_id cursor → `ResetRequired { reason = log_id_mismatch }` + `snapshot_cursor` |
| V2-C6 | 超前 cursor → `unknown_fact` + `snapshot_cursor`；不可解码 cursor → `V2HubError::InvalidCursor` |
| V2-C7 | 只有 identity、没有 commit marker 的会话 → `snapshot_missing` |
| — | 订阅者落后 broadcast buffer → `ResetRequired { reason = replay_overflow }` |

为让 `replay_overflow` 可在单测里廉价触发，`V2ProjectionHub` 增加
`with_live_capacity(epoch, capacity)`（`#[doc(hidden)]`，生产恒用 `LIVE_CAPACITY = 1024`）。

### 2.2 `crates/qaqh-daemon/src/axum_server.rs`（路由级 fixture）

- `v2_second_answer_returns_winning_verdict_for_permission_and_plan`：补齐 V2-R4 的
  permission / plan 变体——第二个回答稳定拒绝 `interaction_already_resolved`，并带
  typed 获胜裁决 `permission_resolved { approved = true }` /
  `plan_review_resolved { approved = false }`。
- `v2_driver_handover_emits_reliable_control_event`：V2-D3——从 handover 之前的
  cursor 重连，control 频道必须回放 `delivery = reliable` 的 `driver_changed` 事件。
- `v2_bootstrap_reports_snapshot_missing_for_uncommitted_session`：只有 identity 的
  会话 bootstrap → 409 `snapshot_missing`（不是 500）。

新增共享 helper `seed_resolved_interaction_session(kind, decision, tag)`。

### 2.3 `crates/qaqh-session/examples/e2e_seed.rs`

`--resolved` 现在一次性种下 **ask / permission / plan** 三个已裁决 interaction，并打印
`{"ask": {...}, "permission": {...}, "plan": {...}}`（各含 `interaction_id` + `call_id`），
供冒烟脚本按 kind 寻址。

### 2.4 `scripts/v2-smoke.sh`（真实 HTTP/SSE）

新增阶段：

- `reliable reconnect replays driver handover`：从 claim 之前的 cursor 订阅，断言
  `event: ringing.event` + `"delivery":"reliable"` + `"kind":"driver_changed"`。
- `permission first-answer-wins typed verdict` / `plan ...`：真实 POST 第二个回答，
  断言 code 与 typed 裁决字段。
- `snapshot_missing probe`：identity-only 会话 bootstrap → 409 `snapshot_missing`。

## 3. 验证证据

```text
cargo test --workspace -- --test-threads=1     PASS（138 suites，0 failed）
cargo clippy --workspace --all-targets -- -D warnings   PASS
cargo fmt --all -- --check                     PASS
scripts/v2-smoke.sh                            PASS（全部阶段通过）
```

冒烟输出（节选）：

```text
== reliable reconnect replays driver handover ==
== first-answer-wins typed verdict ==
== permission first-answer-wins typed verdict ==
== plan first-answer-wins typed verdict ==
== snapshot_missing probe ==
PASS: Ringing v2 real-machine smoke test
```

## 4. 顺手补掉的契约缺口

`V2HubError::SnapshotMissing` 之前**不可达**：只有 identity、没有 `events.commit`
的会话，`CommittedFactReader::open` 会先抛 `Canonical("commit recovery required:
commit marker is missing")`，被 daemon 映射成 500 `canonical_error`。而 spec §6.1/§7
要求「无法给出 snapshot_cursor → `snapshot_missing`」。

修法：`V2ProjectionHub::resolve_identity` 在 identity 存在但 `EVENTS_COMMIT_FILE`
缺失时返回 `V2HubError::SnapshotMissing(seed)`。daemon 既有映射把它变成 409
`snapshot_missing`。identity/events 都不存在时仍是 `SessionMissing` → 404。

## 5. 仍未完成（alpha 迭代清单）

### A. P0-6 剩余

1. **V2-C3 replaceable producer 已补（2026-09-24）**：
   `projection_replaceable_events_for_fact` 已冻结 control/resource 当前值映射，
   `V2ProjectionHub` 在重连时补发 cursorless 最新 revision；验收见
   `crates/qaqh-runtime/tests/v2_acceptance_matrix.rs::v2_c3_*`。
2. **V2-V1 v1 `Last-Event-ID` → v2 cursor 映射**：服务端映射表尚未实现
   （spec §10 的 `(server_epoch, channel, stream_seq)` 表）。`cross_session` /
   `v1_epoch_mismatch` / 映射缺失的 `cursor_expired` 都依赖它。这是 P1 项。
3. **Windows alpha 共用 fixture**：无 Windows 实机，fixture 与 spec 已按平台无关写，
   但 V2-W1 未在 Windows 上跑过。

### B. driver 侧（承上）

1. 回收延迟（3s 巡检）。
2. `not_eligible` / 优先级策略。
3. `driver_epoch` 未进 command fingerprint。
4. workspace 命令 gate 集合。

### C. P1 未开工

`/ringing/v2/service/{method}`、`/ringing/v2/content`、timeline v2 完整分页与重连、
v1 映射表。

## 6. 接手注意

- `v2_acceptance_matrix.rs` 的 `Fixture::metadata_fact` **只构造不 append**；要落盘
  需显式 `append` / `append_and_publish`。别再把它当 append 用（曾因此重复 append）。
- 所有等待 live 事件的测试都必须套 `tokio::time::timeout`：`V2Subscription::next()`
  在 replay 排空后会阻塞在 live channel 上。
- `snapshot_missing` 现在对「identity 存在、commit marker 缺失」的会话生效；
  不要把它改回 `Canonical`，否则 spec §7 的 reset reason 又不可达。
