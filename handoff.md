# Ringing v2 thin-loop 硬切交接

更新时间：2026-10-30（P1–P8 批次完成）
上一版交接：见文件后半部分（保留历史上下文）。

## 0. 一句话现状

**生产切入前置证据已闭环，22 步端到端隔离探针全部通过（PROBE OK (22 checks)）。**
新链（`agentloop-core` + `SessionCommit` + `provider_job` + `tool_job`）不仅跑通基本流，更完整实现了：
1. **P5 故障注入与重启恢复**：覆盖 pre-write、post-write-pre-flush、post-flush-pre-receipt 三类崩溃点，证明 A03 截断与幂等性、A04 工具重复提交拒绝与未定状态结算、A04/A05 会话恢复（`SessionRecovered` 与 `TurnInterrupted`）。
2. **P6 四大 Provider 金标与 Opaque Bundle**：覆盖 OpenAI Chat、OpenAI Responses（含 `encrypted_content` 等 `ResponseOutputItem` 原样回存与回放、`function_call` 参数与 ID 精确还原）、Anthropic Messages（带 thinking）、Gemini（带 thought），证明跨重开后请求上下文和摘要逐字节一致。
3. **P7 真实工具执行端口（`WorkspaceToolPort`）**：对接真实 `ToolManager`、执行目录作用域；在派发前强制复核审批有效性（撤回即安全拒绝）；长任务后台移交（`background_after_secs`）保持 `QuotaLedger` 配额持有；并确保挂起输入（`QueuedInputSettled`）在单轮内结算，不产生空 turn。
4. **P8 全量验证与全仓自洽**：22 步集成探针全过；修复 `responses_sdk` 终端 items 映射、`exec` 工具后台秒数过滤、`sbx-win` 在非 Windows 环境的条件编译，以及 `qaqh-contract-export` 和 `qaqh-config-api` 中的 schema 对齐与测试。

---

## 1. 核心架构与决策（P5–P8 新增）

- **故障注入与恢复（A02/A03/A04）**：
  - 在 `canonical/batch.rs` 引入 `BatchFaultPoint`（`PreWrite`, `PostWritePreFlush`, `PostFlushPreReceipt`）和 `fail_next_commit()`，支持确定性进程内故障模拟。
  - 发生故障或崩溃重开时，`commit_state` 与 `context_projection` 自动检查 `SessionObligations`：
    - 未决的 `ToolIntent` 标记为 `Indeterminate`，禁止盲目重跑工具，必须先重试结算。
    - 挂起的交互自动过期，受损未完成的 turn 被密封为 `TurnInterrupted` 并记入 `SessionRecovered` 事实。
- **Opaque Blob 与多协议金标（A07）**：
  - OpenAI Responses 协议下的原始未解析项（如 `encrypted_content`、加密推理块等）统一落盘为 blob 并在 `ProviderResponseCommitted.opaque_refs` 建立不可变引用。
  - 重开恢复时，`context_projection` 重新将 blob 装配为 `ContentBlock::ResponseOutputItem`，与通用 `ToolUse` 一并提供给 `project_responses_messages`，确保 wire 上的请求逐字节精确还原。
- **真实工具端口与配额生命周期（A05/A08）**：
  - `WorkspaceToolPort` 桥接 `qaqh_workspace::execution`。
  - 工具派发前执行 Challenge/Approval 校验；若用户在调度后、执行前撤销授权（`reject_authorization`），工具立即以 `NotStarted` 拒绝，绝不触碰文件或执行进程。
  - 针对后台进程（如 `exec` 带 `background_after_secs`），在 `retained_owners` 中保留对应 `ExecutionId` 与进程句柄，同时通过 `QuotaLedger` 持续持有配额，直到被显式 `kill`、进程退出或通过 `settle_retained_owner` 结算释放。

---

## 2. 探针验证结果（22 项全绿）

执行命令：
```bash
TMPDIR=$PWD/target cargo run -q -p qaqh-runtime --example thin_loop_seam_probe --offline
```

输出记录：
```text
ok: [1] synthetic supervisor semantics
ok: [2a] capability carried 1 frozen messages
ok: [2] authorize -> start -> completion -> acknowledge
ok: [3] committed response becomes the next request prefix
ok: [5] generation follows the committed attempt
ok: [4] open round blocks the prefix until its response commits
ok: [6] supervised tool result lands as a committed fact
ok: [7 & 14] cancelling an unstarted authorization withdraws its ticket
ok: [8] interaction answers are revision- and target-checked
ok: [9] real gate round completed against a local provider
ok: [10] live response is committed and replayable
ok: [11] dropped commit owner fences the capability
ok: [12 & 13] replay of a committed batch issues no capability and preserves prefix
ok: [15] A03 fault before marker truncates torn tail and reconciles cleanly
ok: [16] A03 fault after marker reconciles idempotently without double fold
ok: [17] A04 terminal commit failure retries settlement without re-running tool; conflicting completions rejected
ok: [18] A04/A05 mid-turn crash recovery settles Indeterminate tool, expires interaction, interrupts turn, and seals SessionRecovered
ok: [19] A02/A06 control lane serviced under full command lane; blocking task ignoring cancel keeps slot until exit and reports StartedUnknown
ok: [20] A07 RequestIdentity mismatch refused before wire; multimodal validated; incomplete response blocks tools/rounds until TurnInterrupted
ok: [21] A07 Chat, Responses (with opaque_refs replay), Anthropic Messages, and Gemini rounds committed and verified across restart
ok: [22] A05/A08 WorkspaceToolPort runs real tools, enforces pre-dispatch approval revocation, releases quota, and settles queued input without empty turn
PROBE OK (22 checks)
```

---

## 3. 工作区修复与自洽性

1. **`crates/qaqh-gate/src/responses_sdk.rs`**：
   - 在流终态 `SdkEvent::Done` 收口时，自动提取并保留非语义展示的响应原始 output items（如 `encrypted_content` 等），写入 `ContentBlock::ResponseOutputItem`，同时避免对普通 tool calls 进行无序重复追加。
2. **`crates/qaqh-process-tools/src/exec/handler.rs`**：
   - 修正 `background_after_secs` 参数过滤：允许 `0` 秒立即移交后台（`n <= 3600` 代替原 `n > 0 && n <= 3600`）。
3. **`crates/sbx-win/tests/*.rs`**：
   - 为该 crate 下所有集成测试文件补充 `#![cfg(windows)]`，保证在 Linux/macOS 平台执行 `cargo check/test --workspace` 时语法与导入解析正常。
4. **`crates/qaqh-config-api/src/lib.rs`**：
   - 测试用例补齐新字段 `toolAllowlistEnabled` 与 `toolAllowlist`，通过测试。
5. **`tools/qaqh-contract-export/src/main.rs`**：
   - 补充 `service.session.create` 与 `service.session.set_effort` 的契约 fixture 及类型导出，解决契约导出测试断言失败。
6. **`crates/qaqh-runtime/tests/ask_user_lifecycle.rs`**：
   - 修复等待 InteractionRequested 期间消费并丢弃先前工具执行事件的断言问题，测试全绿。

---

## 4. 下一步施工建议（RLC-4 冷切准备）

前置逻辑（P1–P8）已在隔离沙盒内证明了完备性与鲁棒性。下一步应聚焦于**生产切入装配点**：
1. **替换 daemon / service 中的会话执行器**：以 `SessionCommit` 和受监督 runner 替代旧的在线执行线程。
2. **连接 Ringing v2 控制与命令通道**：将真实客户端命令（交互答复、审批响应、取消）直接路由至新 runner 的 control lane。
3. **物理清理退役逻辑**：下线并移除旧版 `AgentState`、`RingContext` 及旧 loop 残留代码。
