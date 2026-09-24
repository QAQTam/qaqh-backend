# QAQH Backend P2 收口 Handoff（2026-09-22）

> 仓库：`/home/qaqtamsy/Projects/qaqh-backend`
> 集成分支：`betav2`
> 最终 HEAD：`6a08ce1`
> 执行计划：[`2026-09-20-p2-session-actor-runtime-wiring-plan.md`](./2026-09-20-p2-session-actor-runtime-wiring-plan.md)
> 总架构：[`2026-09-20-qaqh-v2.0-总架构设计-plan.md`](./2026-09-20-qaqh-v2.0-总架构设计-plan.md)

## 1. P2 本轮收口

| 切片 | 实现 | Merge | 结果 |
|---|---|---|---|
| P2-4d 显式 ToolCallContext | issue #252 / PR #253 | `b04f4ef` | model/UI/resume 生产工具路径显式传递 session/workspace/mode/cancel/sandbox；worker 不再直接捕获 ActorToolScope |
| P2-5 SubagentSupervisor | issue #256 / PR #257 | `69029ef` | parent/child edge、cancel、terminal、join、unload ack 顺序收口 |
| P2-6 spawn recovery + dedup | issue #260 / PR #261 | `aa4e7ef` | canonical facts 恢复扫描、edge/child 双向孤儿分类、稳定 `message_id/input_id` 去重 |
| P2-7 root QuotaLedger | issue #262 / PR #263 | `986eb8f` | 固定 ledger 路径、独立 `quota.lock`、reservation/reconciliation、spawn 前 durable reserve |

状态回写证据：

- P2-4d：issue #254 / PR #255 / merge `bd42fcc`
- P2-5：issue #258 / PR #259 / merge `b19dc90`
- P2-6/P2-7/P2 收口：issue #264 / PR #265 / merge `a2187d4`
- P2-4 状态修正：issue #266 / PR #267 / merge `7f4bdcd`
- 总架构状态同步：issue #268 / PR #269 / merge `6a08ce1`

## 2. 关键实现入口

- `crates/qaqh-workspace/src/tool_api/context.rs`
  - `ToolCallContext` / `SandboxMode` / `CancellationToken`
- `crates/qaqh-workspace/src/runtime.rs`
  - `ToolExecutionScope`：显式 context + ToolManager/fold policy 兼容搬运
- `crates/qaqh-workspace/src/execution.rs`
  - `execute_authorized_with_context`
- `crates/qaqh-runtime/src/subagent_supervisor.rs`
  - `SubagentSupervisor`、postorder unload、lifecycle trace
- `crates/qaqh-runtime/src/subagent_recovery.rs`
  - `scan`、`RecoveryAction`、`RecoveryIssue`
- `crates/qaqh-runtime/src/quota_ledger.rs`
  - `QuotaLedger`、`QuotaReservation`、`QuotaLimits`
- `crates/qaqh-runtime/src/registry.rs`
  - supervisor/ledger 接线；spawn 先 reserve，成功 commit，失败 release
- `crates/qaqh-runtime/src/agent/injection.rs`
  - `InjectionBus` 的 `command_id` + `input_id` 双去重
- `crates/qaqh-runtime/src/agent/turn_actor.rs`
  - accepted `input_id` 在 terminal 后仍保持去重

## 3. 验证

本轮最终验证命令均已在 `betav2 @ 6a08ce1` 通过：

```bash
cargo test -p qaqh-workspace
cargo test -p qaqh-runtime
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

新改核心文件已单独执行：

```bash
rustfmt --edition 2024 --check <changed-files>
```

说明：

- 全局 `cargo fmt --all -- --check` 仍受仓库历史基线漂移影响；不得把该基线漂移描述为本次改动失败，也不得顺手格式化无关文件。
- NPC 自动审查的 CPU quota/error 属外部失败，不作为本机代码验证结论。

## 4. 已知边界与后续

- P2-6 的 `subagent_recovery` 已是 canonical-facts 驱动的纯扫描器与确定性 plan；daemon 启动时的实际 recovery executor 仍应在 canonical log production writer 接线后接入。
- P2-7 的 ledger 已接生产 subagent spawn；content/tool 类 reservation 的调用点应随 P3 ToolRuntime 一并接入，不能重复实现第二套 quota。
- `ActorToolScope` 仍作为 legacy/test 兼容类型保留；P3 迁移 TypedTool/ErasedTool 时再删除剩余兼容路径。
- P2 总 Gate 的状态、PR 与 merge 证据已回写执行计划；后续从 P3 ToolRuntime 开始，不再重开 P2 slice。

## 5. 开发约束

- 从 `betav2` 创建独立 branch/worktree，不直接在主工作区提交。
- 每个 PR 固定 base `betav2`；合并后 ff-only 更新主工作区。
- 当前约定仍是本机后端串行开发；只读侦察可并行，不并行修改 registry/spawn/edge。
