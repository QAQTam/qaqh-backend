# QAQH v2.0 P2 SessionActor 运行时接线计划（2026-09-20）

> 基线：`betav2 @ 1d062d9`
> 上位计划：[`2026-09-20-qaqh-v2.0-总架构设计-plan.md`](./2026-09-20-qaqh-v2.0-总架构设计-plan.md) §P2
> P2-3 交接：[`2026-09-21-p2-3-handoff.md`](./2026-09-21-p2-3-handoff.md)
> 状态：已完成（2026-09-22）；本文件是 P2 运行时接线的执行清单与状态回写入口。
> 原则：每个 PR 只完成一个可验收切片；未合入前不得把计划项描述成运行时已完成。

## 1. 当前事实

已完成：

- `SessionActor` FIFO mailbox、容量和 shutdown 纯状态机。
- `TurnCore` 的 Start/Round/Suspend/Resume/Cancel/Finish 状态转换。
- 单 active turn、cancel 幂等、冲突终态 fail-closed 的单元契约测试。
- P2-2 的用户/系统输入、permission、ask/plan、subscription ingress 已接入
  SessionActor mailbox；P2-3 取消树与单一 terminal 已收口。
- P2-4 显式 runtime/turn context、lifecycle port、compaction port 与生产
  ToolCallContext/sandbox 接线已完成；thread-local 仅作为 legacy handler
  兼容视图保留。
- P2-5 SubagentSupervisor、P2-6 spawn recovery/消息去重、P2-7 root
  QuotaLedger 已完成。
- 代码位置：`crates/qaqh-session/src/actor.rs`。

当前边界：

- `run_lap` 仍是实际 I/O 执行器；`TurnActor`/`SessionActor` 是 turn
  lifecycle、interaction、subscription 状态的唯一提交边界。
- compaction/title/liveness/session lifecycle 已收口到 port，但异步 task
  ownership 仍可在 P3 继续演进。
- `ActorToolScope` 作为兼容类型保留；生产工具 worker 已改走显式
  `ToolExecutionContext`。
- P2-6 的恢复扫描已落地；daemon startup 的 recovery executor 需在
  canonical log production writer 接线后接入。
- P2-7 的 quota ledger 已接 subagent spawn；content/tool reservation 应随
  P3 ToolRuntime 接入。

结论：P2 计划内切片已收口；P3 的前置显式 ToolCallContext 已具备。

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

状态：已完成；按入口类型拆分为 P2-2a/2b/2c/2d-a/2d-b。

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

状态：已完成，拆成 token tree、legacy 状态清理与单一终态三个可验收切片。

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

非目标：本切片未删除 `Loop::user_cancelled`，也不把 `run_lap` 改成 SessionActor 唯一执行 owner。

#### P2-3b 删除重复 user_cancelled，统一取消门

状态：已完成，PR #227，merge `0d096c6`。

交付：

- 删除 `Loop::user_cancelled` 字段及赋值/读取，系统注入与 compact 后注入统一读取 `CancelToken::is_set()`。
- 用户输入、ToolInvoke、session switch 继续通过 `cancel.clear()` 复位取消门。

Gate：

- ConversationCancel 后系统注入只入队、不发布 `TurnStarted`。
- 用户输入与 session switch 后取消门解除，系统注入恢复。
- cancel-before-start、cancel-in-round、重复 cancel、完成与取消竞争全绿。
- 任何 terminal 后不得再发布 round、tool start 或 pending interaction。

#### P2-3c 单一 InterruptReason producer

状态：已完成，PR #231，merge `e471f11`。

交付：

- `TurnActor::cancel` / `cancel_active` 返回 `Interrupted { reason }`、`Idle` 或 `AlreadyTerminal`，`InterruptReason` 只由该转换产生。
- `ConversationCancel` 在 actor 已 terminal 时不再补发 `ConversationCancelled`；运行中 token 取消只保留先到的 `TurnCompleted(cancelled)`。

Gate：

- cancel 只产生一个 `InterruptReason` 和一次 terminal。
- cancel-during-gate 与重复 cancel 的同一 turn 只出现一个 terminal 事件。

### P2-4 loop 外移与 thread-local 清理

状态：进行中；P2-4a/P2-4b/P2-4c-a/P2-4c-b/P2-4d 已完成；后续仅剩
P2-5/P2-6/P2-7。

交付：

- compaction、title、liveness、session lifecycle 从 loop 主路径移出。
- 通过显式上下文传递 workspace、sandbox、session 和 cancellation。
- 清理运行路径对 thread-local workspace/sandbox 的依赖。

Gate：

- turn lifecycle、compaction、suspend/resume 行为契约全绿。
- 同一输入在显式上下文和旧入口下产生等价事实/投影。
- P3 开工前 `ToolCallContext` 所需字段已具备明确来源。

#### P2-4a 显式 runtime/turn context adapter

状态：已完成，issue #236 / PR #237，merge `ad01dea`。

交付：

- `qaqh-runtime` 新增 `agent::context`：`RuntimeContext` 显式承载 session、
  workspace、sandbox、cancellation；`TurnContext` 在其上补 turn identity
  与 round。
- legacy adapter 在 `run_lap` 回合入口一次性快照现有 `RingContext` 与
  actor thread-local，不改变状态所有权。
- `run_lap` 的本轮取消门读取显式 context 中与 legacy 共享的
  `CancelToken`。

Gate：

- legacy ambient -> explicit context 的 session/workspace/sandbox/cancel
  等价性测试通过。
- `cargo test -p qaqh-runtime`、`cargo test --workspace`、
  `cargo clippy --workspace --all-targets -- -D warnings` 通过。

非目标：未迁移 compaction/title/liveness/session lifecycle，未删除现有
thread-local 或 workspace/session cancel bridge。

#### P2-4b lifecycle port

状态：已完成，issue #240 / PR #241，merge `7f03c70`。

交付：

- `qaqh-runtime` 新增 `LifecyclePort` 与 `RuntimeLifecyclePort`，统一承接
  dispatch liveness 记账、session create/resume/reload 和 turn title 触发。
- `Loop` 不再直接持有 `SessionEngine` 或操作 `WorkerLiveness`。
- title 从 `seal_timeline_terminal_round` 的 Completed 分支迁到
  `Outcome::TurnComplete` 边界；fallback 写入保持一次性和冻结语义。

Gate：

- lifecycle port 的 busy/touch/suspend 记账与旧实现等价。
- session create 契约和 title fallback 幂等回归测试通过。
- `cargo test -p qaqh-runtime`、`cargo test --workspace`、
  `cargo clippy --workspace --all-targets -- -D warnings` 通过。

非目标：未迁移 compaction，未删除 thread-local，未改消息/timeline/持久化顺序。

#### P2-4c-a compaction task port

状态：已完成，issue #244 / PR #245，merge `fe7d617`。

交付：

- `qaqh-runtime` 新增 `CompactionPort`，持有 background compaction 的
  receiver、compact_id 与 causation。
- `Loop` 删除 `pending_compact_rx/id/causation` 字段，统一通过 port
  query/poll/take。
- manual compact 的 start/check/finish 行为、apply_result 与 injection
  派发顺序保持 1:1。

Gate：

- compaction port 的 empty/running/ready/disconnected/take 契约测试通过。
- `cargo test -p qaqh-runtime`、`cargo test --workspace`、
  `cargo clippy --workspace --all-targets -- -D warnings` 通过。

非目标：该切片未迁移 `ToolCallContext` 字段来源，未改 compact prompt/LLM/
事件顺序。

#### P2-4c-b ToolCallContext 显式字段来源

状态：已完成，issue #248 / PR #249，merge `3b0babb`。

交付：

- `RuntimeContext` 增加 permission_level 与 agent mode 快照。
- `RuntimeContext::tool_call_context` 从显式 context 构造
  session/workspace/mode/permission/cancellation，call_id、timeout、
  progress、source 由调用方显式传入。
- `CancellationToken::from_shared_flag` 连接 runtime cancellation tree 与
  显式工具上下文。

Gate：

- ToolCallContext 字段来源与取消共享测试通过。
- `cargo test -p qaqh-runtime`、`cargo test --workspace`、
  `cargo clippy --workspace --all-targets -- -D warnings` 通过。

非目标：本切片不接线生产工具执行路径，不删除 thread-local；P3 前仍须完成
生产接线和 thread-local 清理。

#### P2-4d 生产工具路径接入显式 ToolCallContext

状态：已完成，issue #252 / PR #253，merge `b04f4ef`。

交付：

- `ToolCallContext` 增加 `SandboxMode`，由 `RuntimeContext` 显式注入。
- model/UI/permission-resume 路径在调用边界构造上下文，并随
  `AdmittedTool`/worker 传递。
- 新增 `execute_authorized_with_context`；旧 `execute_authorized` 保留等价
  legacy adapter。
- 新增 `ToolExecutionScope`，只搬运仍需 thread-local 的 ToolManager /
  fold policy；explicit context 是 workspace/session/mode/cancel/sandbox
  的权威来源。
- `LegacyToolAdapter` 执行期间安装显式上下文；生产 worker 不再直接捕获
  `ActorToolScope`。

Gate：

- 显式上下文驱动 admission/execution 并恢复 ambient 的测试通过。
- 显式 sandbox 控制 admission、显式 cancellation 在 dispatch 前生效、
  legacy adapter ambient 恢复测试通过。
- `cargo test -p qaqh-workspace`、`cargo test -p qaqh-runtime`、
  `cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`
  通过。

非目标：本切片保留 `ActorToolScope` 兼容类型；P2-5/P2-6/P2-7 仍待实现。

### P2-5 `SubagentSupervisor`

状态：已完成，issue #256 / PR #257，merge `69029ef`。

交付：

- `qaqh-runtime` 新增 daemon 级 `SubagentSupervisor`，统一负责
  parent/child link/unlink、children 查询、postorder unload 与生命周期 trace。
- `registry.close`、`shutdown_all`、dead-parent respawn 统一经 supervisor
  取消、观察 child terminal、记录 parent edge finish、join child，再完成
  parent unload ack。
- parent 取消/关闭/异常退出不再以移除 instance + 裸 unlink 代表 child
  已终止。
- link 冲突/成环 fail-closed，spawn 后 link 失败会立即关闭 child。

Gate：

- `parent_unload_waits_child_terminal_join` 通过。
- 固定顺序：`child terminal -> parent SubagentFinished -> child join -> parent unload ack/tombstone` 通过。
- `shutdown_all`、dead parent respawn、parent close/unload、childless close
  回归测试通过。
- `cargo test -p qaqh-runtime`、`cargo test --workspace`、
  `cargo clippy --workspace --all-targets -- -D warnings` 通过。

非目标：canonical edge fact 持久化与双向恢复扫描留给 P2-6。

### P2-6 两阶段 spawn 恢复与消息去重

状态：已完成，issue #260 / PR #261，merge `aa4e7ef`。

交付：

- `qaqh-runtime` 新增 canonical facts 驱动的 `subagent_recovery` 扫描器：
  edge 无 child log、child log 无 edge、parent mismatch、重复 spawn、
  child terminal 未闭合 edge、trigger_turn 缺 `TurnStarted`、queue_only
  不补 turn。
- 恢复计划只产生 append-only action，不伪造或重写 `SubagentSpawned`。
- `ConversationSendMessage` 增加稳定 `message_id/input_purpose`；
  subagent task/result 注入使用稳定 ID。
- `InjectionBus` 与 `TurnActor` 按 `input_id` 去重，重复输入在 terminal
  后仍不会开启第二个 turn。

Gate：

- edge/child 双向孤儿、重复 edge、parent mismatch、terminal 补 finished、
  trigger_turn 补一次 `TurnStarted`、queue_only 不补 turn 测试通过。
- `cargo test -p qaqh-runtime`、`cargo test --workspace`、
  `cargo clippy --workspace --all-targets -- -D warnings` 通过。

### P2-7 root `QuotaLedger`

状态：已完成，issue #262 / PR #263，merge `986eb8f`。

交付：

- `qaqh-runtime` 新增 `quota_ledger`：路径固定为
  `{data_dir}/quota/{root_session_id}/ledger.jsonl`，独立 `quota.lock`。
- append-only `reserved -> committed/released`；soft/hard watermark；
  replay 后按 canonical child edge reconciliation。
- `AgentRegistry` 按 root session 持有 ledger；subagent spawn 先 durable
  reserve，成功 commit，spawn/link 失败 release。
- reservation durable ack 先于 child actor 启动。

Gate：

- ledger durable replay、idempotent commit/release、hard/soft limit、
  unbacked spawn reconciliation、spawn commit/reject 测试通过。
- quota 测试与 subagent terminal/join 顺序独立。
- `cargo test -p qaqh-runtime`、`cargo clippy --workspace --all-targets -- -D warnings`
  通过。

## 4. P2 总体 Gate

P2 只有同时满足以下条件才可标记完成（本机证据已逐项满足）：

- turn lifecycle、cancel、suspend/resume、compaction 行为契约测试全绿。
- 同一 session 最多一个 active turn。
- terminal 只发布一次。
- 输入、取消、审批和订阅不存在绕过 `SessionActor` 的状态写入。
- child 终止、parent unload、join 的固定顺序通过故障注入。
- quota reservation/reconciliation 与 child join 顺序独立验收。
- 总计划、handoff 和本文件的实施状态已回写，且每项状态有对应 commit/PR
  证据；P2-6/P2-7 证据见本文件及
  [`2026-09-22-p2-final-handoff.md`](./2026-09-22-p2-final-handoff.md)。

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
