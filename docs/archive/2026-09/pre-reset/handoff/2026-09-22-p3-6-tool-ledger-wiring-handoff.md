# P3-6 ToolLedger 生产接线 Handoff

> 日期：2026-09-22
> 状态：实现完成并通过本机全量测试；已提交到本地分支 `feat/p3-tool-ledger-production-wiring`（`b286d96`），**尚未 push / PR / merge**
> Issue：`QAQ-Harness/qaqh-backend#287`
> Base：`betav2 @ 5ec1900`（P3-5 merge commit）
> Branch：`feat/p3-tool-ledger-production-wiring`
> Worktree：`/home/qaqtamsy/项目/qaqh-backend`

## 1. 本次目标

把 P3-5 的 `ToolLedger` 从 core 接到生产唯一执行边界：

- legacy seed session directory 获得稳定的 canonical UUIDv7 identity。
- `AgentState` 持有本 session 唯一 `ToolLedger`。
- handler spawn 前 durable append `ToolIntent`。
- handler join 后 append 唯一 `ToolFinished`。
- 已有终态、开放 intent fail-closed，不二次执行非幂等工具。

本切片**不包含**完整 SessionActor resume/terminal/cancel CAS；那部分留 P3-7。

## 2. 当前代码状态

P3-6 已提交到本地分支 `feat/p3-tool-ledger-production-wiring`：

- 基线格式修复：`7cf7833 style: apply rustfmt across workspace`
- TypedTool 错误边界 Clippy 修复：`1bf017e fix(workspace): keep typed todo error boundary clippy-clean`
- P3-6 功能提交：`b286d96 feat(session): wire ToolLedger into production execution`

当前 worktree 干净；`betav2 @ 5ec1900` 仍是远端基线。

## 3. 已实现内容

### 3.1 Canonical identity sidecar

新增：`crates/qaqh-session/src/canonical/identity.rs`

- `CanonicalSessionIdentity`：
  - `session_id: SessionId`（UUIDv7）
  - `log_id: LogId`（UUIDv7）
  - schema：`qaqh.canonical-identity/v1`
- sidecar 文件：`canonical-identity.json`
- `open_or_create()`：
  - 首次创建 UUIDv7 identity。
  - temp file + `sync_all` + no-clobber `hard_link`，避免并发创建覆盖。
  - reopen 后返回同一 identity。
  - 非法 schema / 非 UUIDv7 / 两 ID 相同均 fail-closed。
- `generate_ulid()`：
  - UUIDv7 bytes → Crockford base32 ULID shape。
- `ulid_from_text()`：
  - 把 v1 任意 wire call id（如 `call_1`、`tc_...`）稳定映射为 canonical ULID alias。
- 新增依赖：`qaqh-session` 的 `uuid = { version = "1", features = ["v7"] }`。

### 3.2 AgentState ownership

修改：`crates/qaqh-runtime/src/agent/state/agent.rs`

新增字段：

- `tool_ledger: Option<ToolLedger>`
- `tool_ledger_seed: Option<String>`

新增 `AgentState::tool_ledger_mut()`：

- ephemeral session 或空 seed：返回 `None`，不写 canonical facts。
- 首次 durable tool execution 时：
  - 打开/创建 canonical identity。
  - `ToolLedger::open()` 并取得 writer lease。
- ledger 按 seed 绑定；seed 变化时丢弃旧 ledger，避免 session 切换写穿。
- writer id：`agent-{pid}-{seed}`。
- lease 默认 30s，可用 `QAQH_TOOL_LEDGER_LEASE_MS` 覆盖（测试/运维）。

### 3.3 ToolRuntime durable barrier

修改：`crates/qaqh-runtime/src/agent/tool_runtime.rs`

执行路径现在是：

```text
admit
  -> prepare_admitted（只分类/检查 ledger）
  -> 每个 worker spawn 前 prepare_one：
       ensure_lease
       append ToolIntent（fsync barrier）
  -> spawn handler
  -> join
  -> append ToolFinished（唯一终态，fsync barrier）
```

关键行为：

- **不预先给整批写 intent**：serial tail 在执行前取消时不会留下 orphan intent。
- 新 call：
  - `ToolIntent` append 成功后才 spawn。
  - append 失败则 spawn 一个 synthetic blocked result，handler 不执行。
- 已有 `ToolFinished`：
  - 不 spawn handler。
  - 回填 `LEDGER_BLOCKED` error result。
- 开放 intent：
  - `NoReplay` / `Reconcile`：补 `ToolFinished::Indeterminate`，拒绝再次执行。
  - `IdempotentReplay`：允许复用原 `execution_id` 重放（read-only capability 映射）。
- handler panic：
  - join 后写 `ToolFinished::Indeterminate`。
- handler 完成但 terminal 写失败：
  - memory 侧回填 `LEDGER_WRITE_FAILED`。
  - ledger 仍保留 open intent；后续 reopen 走 Indeterminate fail-closed。
- writer lease idle 过期：
  - `ToolLedger::ensure_lease()` 会用同一 writer id 重新 acquire，而不是永久卡在 stale lease。

Canonical mapping：

- wire call id → `call_{ulid_from_text(wire_call_id)}`
- `ToolIntent` / `ToolFinished` / `SessionFact.call_id` 使用同一个 canonical alias。
- model/message 侧仍保留原 wire call id，不改旧 UI/消息协议。

### 3.4 ToolLedger lease 增强

修改：`crates/qaqh-session/src/canonical/tool_ledger.rs`

新增 `ToolLedger::ensure_lease()`：

- active lease → renew。
- expired lease → 同 writer id 重新 acquire。
- 解决长生命周期 actor 两次工具调用间超过 30s 后 `renew_writer` 永久 stale 的问题。

### 3.5 Engine 适配

修改：`crates/qaqh-runtime/src/agent/engine_tool.rs`

- UI 直调路径增加 `ToolRunOutcome::LedgerFailed` 分支：
  - 回填 `LEDGER_WRITE_FAILED`。

## 4. 测试与验证

已执行并通过：

```bash
cargo check -p qaqh-runtime -p qaqh-session
cargo test -p qaqh-session
cargo test -p qaqh-runtime
cargo clippy -p qaqh-session -p qaqh-runtime --all-targets -- -D warnings
```

结果：

- `qaqh-session`：全量 unit + integration tests passed。
- `qaqh-runtime`：全量 unit + integration tests passed。
- 新 runtime 契约测试：
  - `crates/qaqh-runtime/tests/tool_ledger_wiring.rs`
  - 证明 handler 开始时可读到 committed `ToolIntent`。
  - 证明结束后恰好一条 `ToolFinished::Succeeded`。
  - 证明同 call replay 不再进入 handler，并回填 `LEDGER_BLOCKED`。
- `tool_ordering_contract`：6/6 passed，既有 ordering 行为未变。
- `tool_ledger`：7/7 passed，新增 idle lease expiry reacquire 测试。

最终在基线格式与 Clippy 修复合并后，已重新执行并通过：

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

沙箱内端口绑定测试会返回 `PermissionDenied`，属于执行环境限制；非沙箱复跑全量测试通过。

## 5. 尚未完成

1. push `feat/p3-tool-ledger-production-wiring`。
2. 提 PR，base 固定 `betav2`，标题建议：
   `P3-6：接通 canonical session identity 与 ToolLedger 生产执行边界`
3. CNB `npc-auto-review` 仍可能因组织 CPU quota 在 `Prepare` 阶段失败；这不是代码失败。代码健康以本机 fmt/check/test/clippy 为准。
4. 合并 PR 后回主工作区：
   ```bash
   git fetch origin
   git merge --ff-only origin/betav2
   git worktree remove --force ../qaqh-backend-p3-6
   ```
5. 关闭 issue #287。

## 6. P3-7 建议续点

本切片只完成“正常执行路径接线 + 已终态/开放 intent fail-closed”。P3-7 应继续：

- SessionActor 内 resume/terminal/cancel 原子 CAS。
- `after_intent_before_handler` 故障注入。
- `after_handler_before_finished` 故障注入。
- crash 后 recovery batch 补 `ToolFinished::Indeterminate` 的 canonical `recovery_ref`。
- Reconcile probe 真正执行并写 evidence。
- permission/ask/plan cancel 与 `InteractionExpired/Resolved` 纳入同一 actor transition。
- serial cancel + durable ledger 的专门测试：确认未执行 tail 不产生 `ToolIntent`。
- `tool_outbox` 与 canonical ledger 对账/退场策略。

## 7. 已知风险 / 留待后续

- `canonical_call_id` 是 SHA256 派生的 ULID alias，不等同于 wire call id。若外部消费者需要反查，需要显式 mapping 或后续把 wire call id 规范化为 ULID。
- `sandbox_spec_hash` 当前是 context JSON 的 sha256 占位，不是最终 `SandboxSpec` hash；P4 必须替换。
- `side_effect_class` 当前按工具名启发式映射；后续应由 capabilities/registration 作为单一事实源。
- 开放非幂等 intent 在当前 live 路径直接写 `Indeterminate`，`recovery_ref=None`；P3-7 若要求 recovery batch provenance，需要迁移到 recovery step。
- `IdempotentReplay` 目前只对 `ToolCapabilities.idempotent == true` 的内置工具开放；动态工具默认保守 `NoReplay`。
- writer lease 是 session actor 单 owner 模型；同一 session 不得并发持有多个 `ToolLedger`。
- 本切片没有修改 canonical `ToolTerminalStatus` / `ToolStatus` 的公开类型；`Indeterminate` 在消息面仍表现为 error。

## 8. 关键文件

- `crates/qaqh-session/src/canonical/identity.rs`
- `crates/qaqh-session/src/canonical/tool_ledger.rs`
- `crates/qaqh-runtime/src/agent/state/agent.rs`
- `crates/qaqh-runtime/src/agent/tool_runtime.rs`
- `crates/qaqh-runtime/tests/tool_ledger_wiring.rs`
- `crates/qaqh-session/tests/tool_ledger.rs`

参考文档：

- `docs/plan/2026-09-20-qaqh-v2.0-总架构设计-plan.md` §P3 / §P4
- `docs/spec/2026-09-20-session-fact-v2-schema-spec.md` §ToolIntent / §ToolFinished
- `docs/report/2026-09-20-v2-p0-验收矩阵与反证-report.md` I5/I6/I7
