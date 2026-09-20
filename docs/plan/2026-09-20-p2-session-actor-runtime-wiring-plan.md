# QAQH v2.0 P2 SessionActor 运行时接线计划（2026-09-20）

> 基线：`betav2 @ 1d062d9`
> 上位计划：[`2026-09-20-qaqh-v2.0-总架构设计-plan.md`](./2026-09-20-qaqh-v2.0-总架构设计-plan.md) §P2
> 状态：进行中；本文件是 P2 运行时接线的执行清单与状态回写入口。
> 原则：每个 PR 只完成一个可验收切片；未合入前不得把计划项描述成运行时已完成。

## 1. 当前事实

已完成：

- `SessionActor` FIFO mailbox、容量和 shutdown 纯状态机。
- `TurnCore` 的 Start/Round/Suspend/Resume/Cancel/Finish 状态转换。
- 单 active turn、cancel 幂等、冲突终态 fail-closed 的单元契约测试。
- 代码位置：`crates/qaqh-session/src/actor.rs`。

尚未完成：

- `SessionActor` 尚未被 daemon/runtime 生产路径持有；当前主要是 `qaqh-session` 内的独立模块。
- 真实 turn 仍由 `crates/qaqh-runtime/src/agent/engine_turn.rs` 的 `run_lap` 直接控制。
- 输入、取消、审批和订阅尚未统一经过 mailbox。
- 取消状态仍存在 token、thread-local 与 `user_cancelled` 等多处语义。
- compaction/title/liveness/session lifecycle 仍在 loop 路径内。
- `SubagentSupervisor`、两阶段 spawn 恢复和 root `QuotaLedger` 尚未实现。

结论：P2 只有纯状态机原型完成，不能描述成 SessionActor 已接管运行时。

## 2. 不变量

1. 每个 session 的 turn 状态只有一个 owner：`SessionActor`。
2. `TurnCore` 保持纯状态机：不 await，不访问文件、网络、线程和锁。
3. runtime I/O、工具和持久化只能存在于 adapter 或后续 `ToolRuntime` 边界。
4. 同一 session 同一时刻最多一个 active turn。
5. 一个 turn 只能发布一次终态；冲突终态 fail-closed。
6. 迁移过程中先保持旧行为 1:1，再做所有权转移；不得用“顺手重构”扩大单个 PR 的语义面。
7. P2 不以移除旧路径数量作为完成标准，以行为契约和唯一所有权作为完成标准。

## 3. 执行顺序

### P2-1 `run_lap` 1:1 adapter 与统一生命周期入口

状态：已完成，PR #199，merge `b7cd6b3`。

已落地：

- `qaqh-runtime` 新增 `TurnActor` adapter，将现有 `Outcome` 映射为 `SessionActor` / `TurnCore` 转换。
- Start/RoundStarted/Suspend/Resume/Cancel/Finish 在统一 `apply_outcome` 边界校验。
- 重复 terminal 幂等，冲突终态 fail-closed；cancel、stale suspension 与 superseded turn 接入 actor。
- `run_lap` 内部逻辑、messages、timeline 和持久化顺序未改。

交付：

- 在 `qaqh-runtime` 增加显式 adapter，把现有 turn 生命周期映射为 `TurnCommand` / `TurnEffect`。
- 保留 `run_lap` 作为内部执行器，首刀不改其对话、工具和持久化行为。
- turn 的 Start、RoundStarted、Cancel、Finish 至少统一经过 `SessionActor`。
- 将现有 `Outcome` 映射为统一终态，确保 terminal 只产生一次。
- 增加契约测试：单 active turn、重复 cancel、重复 finish、正常完成与取消竞争。

Gate：

- 现有 turn/run_lap 行为测试保持通过。
- runtime 不再存在多个 turn 生命周期入口。
- terminal 幂等，冲突终态 fail-closed。
- messages、timeline 和持久化顺序不变。
- 本切片不清理 thread-local，不改工具执行路径。

### P2-2 mailbox 全入口

状态：进行中；按入口类型拆分，避免一次迁移全部 wire 语义。

#### P2-2a 用户与系统输入准入

状态：已完成，PR #204，merge `c199e64`。

交付：

- 用户输入、goal 自动推进和 system injection 在写入消息存储前调用 `TurnActor::begin_input`。
- 输入准入失败时发布 `input_rejected`，不得继续落盘或开新 turn。
- `InputId` 使用上游 command identity，而不是从 `turn_id` 临时拼接。

Gate：

- 第二 active turn 在消息落盘前被拒绝。
- 用户与系统输入路径没有绕过 `TurnActor` 的 turn start。
- 原有输入、注入和 session lifecycle 测试保持通过。

#### P2-2b permission resolution

状态：已完成，PR #207，merge `a061a0d`。

交付：

- `TurnActor` 在 `YieldToUser` 时登记 pending permission/ask/plan 身份。
- permission resolution 经 ToolEngine 校验后提交 actor，重复 resolution 返回 `interaction_not_found`。
- actor 保留 pending/remaining/resolved 集合，first-answer-wins；未知/恢复态继续走 legacy 校验。

Gate：

- 重复 permission resolution 和 partial resolution 有契约测试。
- 原有 permission lifecycle 回归测试保持通过。

#### P2-2c ask/plan resolution

状态：已完成，PR #211，merge `2283cf2`。

交付：

- ask/plan resolution 在 legacy 校验成功后提交 actor。
- rejected/expired resolution 直接闭合 interaction，不启动后续执行。
- 重复 resolution 在 actor 已解决态 fail-closed，不重新调用 legacy handler。

Gate：

- 重复 resolution、迟到 resolution 和 cancel 竞争有契约测试。
- terminal 后不得重放 pending modal。

#### P2-2d-a SessionActor subscription registry

状态：已完成，PR #215，merge `1afe17e`。

交付：

- `SessionActor` mailbox 增加 connection-scoped subscription command/effect。
- `SubscriberRegistry` 成为逻辑订阅事实的唯一容器；transport 后续只保留 socket/connection 映射。
- subscribe/unsubscribe/connection closed 与 turn 命令共享 FIFO 顺序，重复命令幂等。

Gate：

- 订阅命令与 turn 命令的顺序、mailbox full、shutdown 拒绝有契约测试。
- connection close 清除该连接全部频道且不影响其它连接。
- 本切片不把 daemon SSE receiver 映射迁入 worker actor。

#### P2-2d-b daemon/SSE transport 与 lifecycle ingress

状态：已完成，PR #219，merge `37323dd`。

交付：

- daemon/SSE transport 接入 `P2-2d-a` subscription registry，socket/receiver 只保留 connection 映射。
- transport 只持有 socket/connection 映射，不维护第二份 session 订阅事实。
- 明确 mailbox 满、shutdown 和迟到 command 的错误语义。
- AgentInstance shutdown 先关闭 subscription ingress；stream 结束或会话失活时按 connection 收口逻辑订阅。

Gate：

- 所有 ingress 路径有顺序和幂等测试。
- 没有旁路直接修改 active turn / terminal / subscription。
- 订阅事件顺序与 canonical fact 提交顺序一致。

### P2-3 取消 token 树与单一终态

状态：进行中；拆成 token tree 与 legacy 状态清理两个可验收切片。

交付：

- root turn 与 child work 使用统一 token tree。
- cancel 只产生一个 `InterruptReason` 和一次 terminal。
- 删除运行路径中的重复 `user_cancelled` 判定。

Gate：

- cancel-before-start、cancel-in-round、重复 cancel、完成与取消竞争全绿。
- 任何 terminal 后不得再发布 round、tool start 或 pending interaction。

#### P2-3a 取消 token tree 与子代理派生

状态：已完成，PR #223，merge `dc649f3`。

交付：

- `CancelToken` 从单点 flag 升级为 parent/child tree；父取消同步并 latch 到所有后代，子取消不污染父或兄弟。
- `arc()` 轮询面读取每个节点的 effective cancellation，覆盖 Gate SSE 与工具线程。
- 子代理 spawn/respawn 从父会话 token 派生；registry 仍负责投递子代理取消命令以收口 terminal。

Gate：

- parent cancel -> child/grandchild 的 `is_set()` 与 `arc()` 立即可见。
- child cancel 不影响 parent/sibling；child 不能清除从 live parent 继承的取消，parent clear 也不复活已取消的后代。
- 父取消仍向已登记 child 投递命令，且不重新拉起已退出实例。

非目标：本切片不删除 `Loop::user_cancelled`，也不把 `run_lap` 改成 SessionActor 唯一执行 owner。

#### P2-3b 单一 InterruptReason 与删除重复 user_cancelled

状态：待开始。

交付：

- cancel 的 producer 只登记一次原因，runtime 不再从 token/thread-local/boolean 多路推导。
- 删除运行路径中的重复 `user_cancelled` 判定，保留行为契约测试。

Gate：

- cancel-before-start、cancel-in-round、重复 cancel、完成与取消竞争全绿。
- 任何 terminal 后不得再发布 round、tool start 或 pending interaction。

### P2-4 loop 外移与 thread-local 清理

状态：待开始。

交付：

- compaction、title、liveness、session lifecycle 从 loop 主路径移出。
- 通过显式上下文传递 workspace、sandbox、session 和 cancellation。
- 清理运行路径对 thread-local workspace/sandbox 的依赖。

Gate：

- turn lifecycle、compaction、suspend/resume 行为契约全绿。
- 同一输入在显式上下文和旧入口下产生等价事实/投影。
- P3 开工前 `ToolCallContext` 所需字段已具备明确来源。

### P2-5 `SubagentSupervisor`

状态：待开始。

交付：

- daemon 级 supervisor 统一负责 parent/child cancel、join 和 edge 生命周期。
- `registry.close` 不再以裸 unlink 代表 child 已终止。
- 覆盖 parent panic、SessionDeleted、shutdown 和 spawn 竞态。

Gate：

- `parent_unload_waits_child_terminal_join` 必须通过。
- 固定顺序：`child terminal -> parent SubagentFinished -> child join -> parent unload ack/tombstone`。
- child 无 parent edge 时不得继续启动。

### P2-6 两阶段 spawn 恢复与消息去重

状态：待开始。

交付：

- edge/child log 双向孤儿恢复扫描。
- inter-agent message 使用 `message_id/input_id` 去重。
- 恢复不得伪造或重写 spawn edge。

Gate：

- spawn 前后崩溃、重复投递、孤儿 child 和重复 edge 场景全绿。
- 恢复结果可从 canonical facts 重放。

### P2-7 root `QuotaLedger`

状态：待开始。

交付：

- 路径固定为 `{data_dir}/quota/{root_session_id}/ledger.jsonl`。
- 由 root owner 持有 `quota.lock`，实现 reservation/reconciliation。
- reservation durable ack 必须先于 spawn/content/tool 副作用。

Gate：

- quota 测试与 subagent terminal/join 顺序解耦判定。
- crash 后 reservation 可恢复，不重复扣减或漏释放。
- child writer fence 不错误覆盖 root quota lock 语义。

## 4. P2 总体 Gate

P2 只有同时满足以下条件才可标记完成：

- turn lifecycle、cancel、suspend/resume、compaction 行为契约测试全绿。
- 同一 session 最多一个 active turn。
- terminal 只发布一次。
- 输入、取消、审批和订阅不存在绕过 `SessionActor` 的状态写入。
- child 终止、parent unload、join 的固定顺序通过故障注入。
- quota reservation/reconciliation 与 child join 顺序独立验收。
- 总计划、handoff 和本文件的实施状态已回写，且每项状态有对应 commit/PR 证据。

## 5. 验证与回写

每个 P2 PR 至少执行：

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

说明：

- 全局 `cargo fmt --all -- --check` 若仍有历史基线漂移，只允许对新改文件执行 `rustfmt --check`，不得顺手格式化无关文件。
- 云端 NPC 若因组织 CPU core-hours 配额失败，必须如实记录为外部阻断，不能描述为代码 CI 通过。
- 每次合入后更新本文件对应条目的状态、PR 和 merge commit；复选框不得无证据勾选。
- 任何范围变化先更新本文件，再开下一实现 issue，避免把 P2 剩余项遗忘在对话上下文里。
