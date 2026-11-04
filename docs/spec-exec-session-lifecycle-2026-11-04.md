# Exec 执行会话与工具状态统一修改 Spec

日期：2026-11-04。状态：**待实施，跨仓破坏性变更规范**。本文不是实现完成报告。

涉及仓库：`qaqh-backend`、`qaqh-sdk`、`qaqh-electron`。Rust 客户端 `qaqh-client` 属于后端仓，也必须同步更新。

规范优先级：RingingV2 公共类型、命令面与事件语义先定型，运行时、模型工具、SDK、前端直接实现同一契约。不得增加旧 exec/process 到新协议的长期转换层。

本文以当前工作区及本地 Codex `db3c444` 为分析基线；工作区中已有上一轮流式修复，实施时保留并整合，不覆盖这些改动。本文只规定后续改造。

## 1. 目标与范围

本次解决六件事：

1. 正常交还控制权不再被记为 timeout、error 或失败审计。
2. 工具调用返回与操作系统进程退出有独立身份和生命周期。
3. exec 与续读/输入共用一个执行会话管理器，模型不再学习 `process(action=...)` 的另一套状态语义。
4. stdout/stderr 在采集端统一定序；实时展示、模型读取、最终记录来自同一捕获源。
5. 审批等待、取消、后台退出、重启和模型失败均能经公共契约得到真实状态。
6. 实时事件、bootstrap、历史模型重放、统计与审计对同一事实得出相同结论。

本次不建设完整终端模拟器，不新增远程执行平台，不移植 Codex 的插件或权限框架，不为每次内部调度增加公共状态。执行会话的持久化不表示支持 daemon 重启后重新接管旧进程。

## 2. 已确认问题及证据

| 编号 | 当前问题 | 代码定位 | 必须改变的行为 |
| --- | --- | --- | --- |
| E01 | 普通管道的等待窗口结束和主动后台移交都置 `timed_out=true` | `crates/qaqh-process-tools/src/exec/direct.rs:350` | 交还控制权与终止超时分开 |
| E02 | exec 投影据此产生 `Error/timeout`；展示投影先判断 timeout，遮蔽 Backgrounded | `exec/display.rs:37,93` | 成功移交不带 error、不增加失败统计 |
| E03 | runtime 解析模型正文里的 `status/process_id`，再特殊纠正后台状态 | `crates/qaqh-agent-runtime/src/tool_job.rs:697` | 调度只读取契约类型，正文只供阅读 |
| E04 | `ToolFinished` 携带 `Running`；数据库同时写结束时间 | `tool_job.rs:709`、`crates/qaqh-session/src/owner.rs:534`、`db/store.rs:653` | 调用终态与执行会话运行态独立 |
| E05 | 前端无条件在 tool_finished 写 endedAt、清空尾窗；running 状态又不显示存下来的最终正文 | `qaqh-electron/webui/src/session/reducer.ts:497`、`tools/presentation.ts:68` | 调用完成不得清空仍在运行的执行会话输出 |
| E06 | 历史重放只将 `ToolCallStatus::Finished` 当成功，后台返回下回合可能变错误 | `session_loop.rs:1091` | 重放不可变的调用 outcome，不查进程当前状态推断调用成功 |
| E07 | 等待授权仅更新 DB 工具状态，实时 reducer 不更新关联工具 | `owner.rs:390`、前端 reducer 的 interaction_requested 分支 | 等待迁移必须实时可见且快照一致 |
| E08 | cancel_turn 只发信号；审批/Ask 的 oneshot 等待不监听取消 | `session_loop.rs:237,721,766` | 取消必须唤醒等待并清理交互 |
| E09 | 前端 10 秒后自行制造 aborted 终态 | `qaqh-electron/webui/src/session/store.ts:1099` | 超时只表明状态尚未确认，不能证明已经停止 |
| E10 | 后台进程回收主要释放 quota，没有同步发布原始执行的最终公共状态 | `tool_job.rs:311`、`process_registry.rs:766` | 实际退出独立通知 owner，无需下一次工具调用触发 |
| E11 | provider 故障可能被写成取消；前端将所有 Interrupted 都显示为 aborted | `session_loop.rs:454`、前端 `statusFromOutcome` | 区分用户取消、执行失败、崩溃与重启 |
| E12 | 实时回合结束发 Idle；快照因后台工具未完成又计算成 Running | `owner.rs:168`、`db/view.rs:46` | agent 活动与后台执行数量分开 |
| E13 | exec-sessions kill 仍绕过统一命令面；SDK 返回 unknown[] 和整数进程 ID | daemon `axum_impl/v2.rs:973`、SDK `core/index.ts:437` | 控制走 RingingCommand，读面强类型化 |

已通过前端源码的轻量运行复现 E05、E07、E11。没有取得用户那一次报错的原始 SSE；不得声称所有历史红色卡片或右下弹窗已经归因。`conn://error` 是宿主/协议错误，必须与工具业务失败分开。

## 3. 参考 Codex 的裁定

参考本地源码，而非只参考字段名称：

- `codex-rs/core/src/tools/handlers/unified_exec/exec_command.rs` 与 `write_stdin.rs` 共用 `session.services.unified_exec_manager`。
- `unified_exec/process.rs:43` 同锁追加 pending 与 transcript；`:657` 接收两条管道后统一追加。
- `unified_exec/process_manager.rs:1615` 消费 pending，轮询不会反复返回整个历史。
- `unified_exec/async_watcher.rs:155` 独立等待退出与输出收尾，发布原始 exec 的完成事件。
- 同一执行会话的写入与模型续读用 interaction lock 串行化。

采用上述结构。QAQH 保留捕获块的来源信息；不照搬 Codex 在 unified exec 中丢弃 `_stream` 并将合流增量统一标记为 Stdout 的做法。合流顺序定义为**采集器接收顺序**，不承诺还原独立管道的绝对写入时间。

## 4. 核心裁定：三个独立生命周期

### 4.0 类型依赖方向

现有依赖为 `qaqh-ringing-contract -> qaqh-types`，且 Message/ToolResult 定义在 qaqh-types。不能反过来让 qaqh-types 依赖 ringing-contract，否则本 spec 的统一 outcome 会形成 Cargo 环。

新增 `qaqh-types/src/execution_contract.rs`，只依赖基础 serde/JSON，单一定义 ExecSessionId、ToolCallOutcome、取消/中断原因及其所需的规范错误/内容引用值对象。将现有 Ringing 的 ContentHash、SessionContentRef、SessionToolError 对应定义下沉到该基础模块，删除旧重复定义，由 ringing-contract 导出**相同的 Rust 类型**；保持其 wire 含义与 TS 公共命名。ToolResult 直接持有这个 outcome，不能再持有另一个可转换的“内部 outcome”。

View、SessionEvent、命令仍定义于 ringing-contract。tool-core、process-tools、session 可以按需依赖该契约；依赖链只向 types 下沉。规范权威仍是 RingingV2，基础类型位置不是一套新的内部协议。实施时同步 Cargo.toml、feature 和 exporter，禁止通过双类型/From 转换绕过依赖环。

### 4.1 身份

| 身份 | 含义 | 生命周期 |
| --- | --- | --- |
| `SessionId` | QAQH 对话会话 | 跨多个回合 |
| `TurnId` / `ToolCallId` | 一次模型回合 / 一次工具调用 | 一次调用完成后不可重开 |
| 既有 `ExecutionId` | 工具执行证明/授权追踪身份 | 不得拿来代替长期进程会话 ID |
| 新增 `ExecSessionId` | 可续读、输入和停止的执行会话，格式 `execs_<ULID>` | 跨 exec_command 返回、后续轮询及回合 |

系统 PID、旧 registry 整数 ID 均是内部信息，不再作为客户端或模型的操作凭据。每个 ExecSession 固定记录 owner_session_id、origin_turn_id、origin_call_id；续读有自己的 ToolCallId，但引用同一 ExecSessionId。

### 4.2 工具调用

公共 `ToolCallState` 改为带标签的联合类型，替代当前独立的 ToolCallStatus/error 组合：

```ts
type ToolCallState =
  | { kind: "declared" }
  | { kind: "awaiting_interaction"; interaction_id: string }
  | { kind: "running" }
  | { kind: "cancelling" }
  | { kind: "finished"; outcome: ToolCallOutcome };

type ToolCallOutcome =
  | { kind: "completed" }
  | { kind: "yielded"; exec_session_id: string }
  | { kind: "failed"; error: SessionToolError }
  | { kind: "cancelled"; reason: ToolCancellationReason }
  | { kind: "interrupted"; reason: ToolInterruptionReason };

type ToolCancellationReason = "turn_cancel" | "interaction_dismissed" | "session_shutdown";
type ToolInterruptionReason = "daemon_restart" | "lost_execution" | "legacy_state_unknown" | "execution_unconfirmed";
```

上述 TS 是 wire 形状说明；实现以 Rust 契约生成对应 ID 和 reason 枚举，不保留任意字符串原因。错误详情放规范错误对象，不通过增加一个随意 reason 字符串绕过枚举。

- `completed/yielded` 都是一次调用的成功返回，不带 error。
- `failed` 是已确认失败；`cancelled` 是已确认取消；`interrupted` 是事实无法继续确认或执行中断，必须保留原因。
- `awaiting_interaction` 统一覆盖 permission、Ask、plan 等，具体种类由关联 InteractionView 表达；删除只覆盖权限的 `awaiting_permission`。
- `running` 的首次迁移就是执行开始，不再另外增加 tool_running 心跳事件。
- finished 是调用已返回，不等于关联进程已退出。结束时间只描述本次调用耗时。
- finished 的 outcome 不因未来进程退出、进程失败、重新连接而改写。
- ToolCallView 保留现有参数、输出、metrics、code_delta、effects，改用 `state`；关联执行会话使用显式 `exec_session_id`。取消重复且可矛盾的 status/error 权威字段。
- 工具核心结果也直接持有契约 ToolCallOutcome。`ToolProjection::status/error` 的分离判定改为一次生成 outcome；既有 ToolStatus、success bool 只能作为消费端派生值，不能继续独立写入或驱动 runtime。最终删掉重复权威状态的存储与分支。
- yielded 中的 ID、ToolCallView.exec_session_id、后续工具结果的关联 ID 必须相同。非 exec 工具不能生成 yielded。失败的启动不能生成一个看似 running 的执行会话。

允许的主要迁移：

| 当前 | 下一状态 |
| --- | --- |
| declared | awaiting_interaction、running、cancelling，或未执行即确认拒绝/失败的 finished |
| awaiting_interaction | 权限允许后回到 declared 待实际执行；Ask 答复后 finished(completed)；拒绝后 finished(failed/permission_denied)；关闭或取消后 finished(cancelled) |
| running | cancelling，或 finished(任一合法 outcome) |
| cancelling | 真实确认后的 finished；取消到达前已完成的事实不能被迟到取消覆盖 |
| finished | 无后续业务迁移，仅允许同一事实的幂等重放 |

交互创建/解决与关联工具、turn phase 的迁移在同一 owner 提交中协调；引用的 interaction 必须已经存在。普通权限通过不提前宣称 running，必须等真正进入执行。Ask 主动关闭只取消该次 Ask，conversation.cancel 则取消整个回合；二者不能混用。

### 4.3 执行会话

```ts
type ExecSessionState =
  | { kind: "running" }
  | { kind: "stopping"; reason: "interrupt" | "terminate" | "turn_cancel" | "timeout" | "shutdown" }
  | { kind: "finished"; outcome: ExecSessionOutcome };

type ExecSessionOutcome =
  | { kind: "exited"; exit_code: number }
  | { kind: "cancelled"; reason: "interrupt" | "terminate" | "turn_cancel" | "shutdown"; exit_code: number | null }
  | { kind: "timed_out"; exit_code: number | null }
  | { kind: "interrupted"; reason: "daemon_restart" | "lost_execution" | "upgrade_state_unknown" };
```

- 后台不是执行失败状态，也不是进程的一种新生命状态。模型调用已 yielded、执行会话仍 running，即构成后台继续执行。
- 正常退出码非零用 `exited { exit_code != 0 }` 记录事实，UI/模型结果据此展示执行失败，不伪造 transport error。
- 发送信号只进入 stopping；实际退出与有界输出收尾完成后才能进入 finished。
- OS 退出但输出尚未收尾使用内部 `ExitedAwaitingDrain` 阶段，不增加公共 tool_draining；输出收尾超限标记不完整，不能因此把 exit 0 改成执行失败。
- 终态不可回退；对已终态会话的停止操作返回 already_finished，不改写原始 outcome。
- `output_complete` 单独描述捕获是否完整，不与进程 outcome 混为一谈。
- 已知的 OS 退出码在 cancelled/timed_out outcome 中也可作为附加事实保存，不为取消/超时伪造退出码。停止原因与发起者分开记录，不能把模型发送的中断标为用户主动取消。

### 4.4 回合与会话活动

TurnView 增加 `phase: running | waiting_user | cancelling | null`；有 outcome 时 phase 必须为 null。等待关联交互、请求取消时分别迁移，不靠前端定时器造终态。

ActivityState 统一为 `idle | running | waiting_user | cancelling`：无活动回合为 idle，有活动回合则由其 phase 直接派生。bootstrap、session.activity/get_activity、session 列表及运行状态查询采用同一词表和计算入口；删除 working/running 两套业务枚举的转换与 Interrupted 活动状态。失败/中断记录在回合 outcome，不能用活动状态替代历史结果。

后台进程数量单独读取 exec_sessions，不影响 activity。连接中的 connecting/reconnecting/disconnected 属于客户端连接状态，不进入业务 activity。若存在取消请求，cancelling 优先；否则有未决交互时为 waiting_user，再考虑普通 running，避免等待状态被 active turn 遮蔽。未开始过回合由 turns 是否为空表达，不再增加另一套生命周期词表。

已确认用户取消写 `TurnOutcome::Finished(terminal=cancelled)`；provider/上下文等确定失败写 finished/failed 并保留 TurnError；崩溃、重启或事实未知用 Interrupted(reason)。删除将普通 provider 错误映射成 CancelBeforeSeal 的路径。前端不再把全部 Interrupted 显示为用户取消。

新写入不再使用 InterruptReason::CancelBeforeSeal。升级旧历史时，若没有独立的用户取消证据，不把该旧值强行解释成用户取消，按 UnknownFact 保留不确定性与原始原因记录。

## 5. 模型工具入口

仅保留两个执行类模型入口，直接删除旧 `exec` 和 `process(action=...)` 注册、别名、提示与分发代码。

### 5.1 exec_command

```json
{
  "cmd": "cargo check",
  "workdir": "/workspace/project",
  "shell": "bash",
  "login": false,
  "tty": false,
  "yield_time_ms": 10000,
  "timeout_ms": null,
  "max_output_tokens": 10000
}
```

- `yield_time_ms` 默认 10000，范围 250..30000；提前完成就提前返回。
- 等待窗口到期且执行仍在进行：调用 outcome=yielded，携带 ExecSessionId、本次新增输出与当前进程状态。不设置 timeout/error。
- `timeout_ms` 默认 null；若指定，是从实际启动起跨轮询有效的硬期限。到期停止进程树、确认退出并结算 timed_out；不能改成后台移交。
- 进程已经退出且输出收尾结束：返回本次输出和退出结果；exit 0 对应调用 completed，非零或执行失败对应 failed。
- 删除 background_after_secs、把 timeout 当 yield 的逻辑及旧参数名兼容。PTY 与普通管道具有同样的 yield/timeout/续读契约。

### 5.2 write_stdin

```json
{
  "exec_session_id": "execs_01...",
  "chars": "",
  "yield_time_ms": 5000,
  "max_output_tokens": 10000
}
```

- 空 chars 只等待并读取自上次模型消费后新增的输出；默认 5000 ms，范围 5000..30000。
- 非空 chars 默认等待 250 ms，范围 250..30000；写入后返回对应观察窗口的新输出，不只返回“写了几个字节”。
- 初版仅 tty=true 支持一般交互输入；普通管道拒绝非空输入，明确的中断操作走同一控制逻辑。不能因不支持输入退回旧 process 工具。
- 输入写入失败、部分写入、执行已结束分别返回明确的契约结果；不能自动重发可能已生效的输入。
- 同一执行会话的模型读取与输入共用 interaction lock，避免两个 poll 重复消费或互相抢输出。
- 没有新输出且窗口到期仍是成功 yielded。轮询已结束会话时，消费剩余 pending 并返回已记录的最终 outcome；已消费部分不重复返回。
- 前端观察者不消费模型 pending；多个 UI 也不共享一个破坏性读取游标。
- 一次 poll 的输出消费与不可变工具结果提交绑定：结果提交前被取消，不得把保留输出静默丢掉；可回滚未提交的读取范围或用提交游标实现。已经写入的 stdin 不能回滚，部分写入必须如实返回。原 ToolCallId 的结果回放不再次消费 pending 或再次写入。
- `chars` 恰为 `\u0003` 时调用统一 interrupt 操作；其他普通管道输入仍拒绝。初版单次输入最多 64 KiB UTF-8，超限在写入前拒绝。

模型结果中的 outcome、执行会话 ID、退出结果和省略信息使用固定字段；正文放 output。模型序列化只是契约事实的展示，不再反向喂给 runtime 判断状态。

## 6. RingingV2 事件与读写面

### 6.1 事件词表

全部仍进入同一 SessionEventFrame 流，保留 seq/exclusive watermark 和 epoch:seq 语义，不创建第二条进程 SSE。

| 事件 | 载荷与约束 |
| --- | --- |
| `tool_declared` | 保留最终参数和工具身份；建立 declared 调用 |
| `tool_state_changed` | call_id、turn_id、非终态 ToolCallState；替代 tool_started，并表达等待与取消 |
| `tool_finished` | call_id、turn_id、ToolCallOutcome、可空 exec_session_id、输出及现有 metrics/effects/code_delta；类型层面不能携带 running |
| `exec_session_started` | 完整 ExecSessionView，先于该会话任何输出 |
| `exec_session_state_changed` | 完整非终态执行会话视图，表达 stopping 等迁移 |
| `exec_session_output_delta` | exec_session_id、start_offset、end_offset、source、text；追加语义 |
| `exec_session_finished` | 完整终态 ExecSessionView，含权威输出引用/正文与完整性信息；每个会话一次 |
| `turn_phase_changed` | turn_id、非终态 phase；waiting_user/cancelling/running |

保留 tool_output_delta 供没有执行会话的普通工具使用；exec 输出只发 exec_session_output_delta，不能两类事件重复发布同一字节。

ExecSessionView 至少包括：id、owner_session_id、origin_turn_id、origin_call_id、state、tty、command、cwd、started_at_ms、finished_at_ms、next_offset、output_preview、preview_start_offset、output_complete、capture_omitted_bytes、stream_dropped_bytes、最终 output(ContentValue)。输入/权限身份不由正文携带。客户端不能以 command/cwd 的相似性配对会话。

一次典型 yield 顺序：

```text
tool_declared(A)
tool_state_changed(A, running)
exec_session_started(X, origin_call_id=A)
exec_session_output_delta(X, ...)
tool_finished(A, yielded(X))
...执行会话继续产生输出...
tool_declared(B=write_stdin)
tool_state_changed(B, running)
tool_finished(B, yielded(X) 或实际退出结果)
exec_session_finished(X, 实际最终结果)
```

以上 B 返回与 X 最终事件可能因调度先后交换，客户端不得靠二者顺序推断关联；二者必须引用同一 X、消费同一最终事实。X 的 finished 之前必须完成已接受输出的发布；之后不再发布该 X 的输出。A 的 outcome 永远保持 yielded。

### 6.2 统一控制命令

接通已有 `CommandTarget::ExecSession { id }`，id 类型改为 ExecSessionId，新增：

```text
exec_session.interrupt    target={kind:exec_session,id:...}
exec_session.terminate    target={kind:exec_session,id:...}
```

- interrupt 是中断请求；terminate 使用现有平台进程树终止能力。两者共用执行管理器与 owner，不另建 kill 注册表。
- 外部写操作只能经 `/ringing/v2/commands`。移除 `/ringing/v2/exec-sessions/{id}/kill` 路由和 SDK killExecSession。
- command accepted 仅代表受理；receipt 的成功结果明确为 stop_requested 或 already_finished，不宣称进程已经退出。最终事实由 exec_session_finished 或读取相同 ExecSessionView 确认。
- 重试保留原 command_id 和指纹，沿用既有 receipt。禁止 SDK 自动用新 ID 重发输入或控制操作。
- 通过执行会话查出所属对话，再复用该对话 lease/权限检查；越属会话拒绝。内部模型工具与 UI 命令使用同一业务操作，不做命令协议转换层。

读面增加强类型 service `exec_session.list { session_id } -> { sessions: ExecSessionView[] }`；详细状态也由会话 bootstrap 提供。删除旧全局 exec-sessions REST 列表及 SDK unknown[] 返回类型。初版不新增终端输入 UI、resize 命令或跨对话全局进程管理界面。

### 6.3 bootstrap、定序与断线

- SessionState 新增 `exec_sessions`：包含全部非终态执行会话及最近 32 个终态摘要；终态正文通过 ContentValue 引用按需读取。
- 非终态执行会话必须提供最多 16 KiB 的合流尾窗及 offset，重连后能恢复“仍在运行、正在停止、已有近期输出”。这是对旧 D6“允许丢失全部暂态”在 exec 输出上的明确替代；未封存 assistant 文本与工具参数仍沿用旧裁定。
- owner 统一串行处理状态迁移、DB 提交和事件分配；publish 也进入同一队列。不能多线程 fetch_add seq 后分别广播，导致帧顺序倒置。
- snapshot 在同一 owner 序列化边界取得业务读模型、执行视图和 exclusive watermark。状态变更先提交，再发布；不得读到比 watermark 更新的终态后，又被旧事件回退。
- snapshot 的执行 state/outcome 从 owner 的已提交视图读取，不直接询问 OS/manager 后自行推断。捕获尾窗与 next_offset 可从 manager 同锁取得；即使包含稍后才发布的输出，也由 offset 去重，不借此提前推进业务状态或全局 watermark。取得快照时不持 manager 锁等待 owner，避免锁顺序反转。
- 全局 seq 控制 SSE 去重，exec offset 控制输出块去重，不能互换。bootstrap 覆盖 `< state.seq`，等于边界的首帧必须处理。
- lag/resync 恢复权威 exec 视图与有界尾窗；不反拼丢失 SSE 作为最终输出，也不假装恢复了全部日志。

## 7. 捕获与资源边界

### 7.1 单一采集源

ExecSessionManager 的 collector 统一接收 stdout/stderr，分别完成增量解码后，在同一个序列化入口追加有序块。块保留 `stdout | stderr | terminal` 来源；PTY 使用 terminal，不能假称还可区分两条流。

规范 offset 按解码后、未做展示清洗的 UTF-8 字节计数；每块满足 `end_offset - start_offset = UTF8(text).length`。ANSI、CR 的渲染归一化不能改写 offset；底层无效字节的替换需单独计数，不能用模型文本长度冒充原始 I/O 字节量。

offset 对所有观察到的规范文本递增，包括之后因容量省略的部分；省略提示文案不占原始 offset。output_preview 必须是 UTF-8 边界上的连续尾窗，省略量用字段表达；head/tail 正文的省略标记只在最终渲染时插入，不能作为一个真实输出块再次发布。

捕获与实时事件共用同一次追加操作，先更新捕获与 offset，再通知 UI。模型与最终展示不得在结束时重新按 stderr→stdout 或 stdout→stderr 分组拼接。

### 7.2 两个有界缓冲

- transcript：保存整次执行的头尾，默认总 UTF-8 正文预算 1 MiB，记录中间省略量。
- model_pending：保存尚未被模型读取的新输出，默认预算 256 KiB；模型读取后清理，省略量随该次结果返回。
- 来源块元数据也有上限，默认每个缓冲最多 4096 条；相邻同源块可合并，超过预算产生明确省略记录，不能只限制正文而让块对象无限增长。
- 超过模型 token 预算消费掉但未返回的文本计入该次 model_omitted_bytes；不在下次 poll 重复播放，完整可用头尾仍由 transcript 提供。
- 单个输出事件最大 8 KiB，保持 UTF-8 边界。实时队列沿用非阻塞有界策略，丢弃计入 stream_dropped_bytes。
- 事件 offset 跳跃是可说明的输出缺口；UI 标记省略并必要时重取尾窗，不把它归为 Invalid SessionEventFrame。
- 默认同一对话最多 8 个非终态执行会话、daemon 总计 32 个。达到上限拒绝新启动并返回明确错误；不能为腾位置静默杀活进程。
- 终态条目在内存保留 10 分钟或直到更早的容量淘汰；持久化摘要和最终输出引用仍可读。过期会话不能被写入或重新启动。

本次不保留四份“完整 stdout、完整 stderr、完整合流、完整 display”。原始大日志如需完整保存应输出为文件，通过既有内容读取机制访问。

## 8. 取消、退出与重启规则

### 8.1 conversation.cancel

1. 匹配当前 turn 后将 phase 置 cancelling，发出 turn_phase_changed；未匹配不能取消别的回合。
2. 审批与 Ask 通过 select 同时等待应答和取消；取消者原子移除应答槽、取消 Interaction、结算关联工具。迟到应答返回已取消，不再次执行。
3. 不再开始本回合剩余工具或下一轮 provider 请求。
4. 尚未向模型 yielded 的本回合 exec 由该回合持有取消责任，停止进程树并等待真实结算。
5. 已 yielded 的既存执行会话不因之后的普通 conversation.cancel 自动终止；取消一个 poll 只取消该次等待。用户停止后台会话使用 exec_session 控制命令。
6. 回合收尾必须等其仍持有的工具/交互结算完成，然后发布真实 TurnEnded。不能 accepted 即宣布 cancelled。

非 exec 工具仍遵守原执行器取消能力；若尚不能确认外部效果停止，保持 cancelling 或记录 interrupted/unknown，不把发出取消信号当已取消。

### 8.2 停止、输出收尾与配额

- OS 退出监听始终存在，不依赖下一次 process.check、下一次工具调用或 UI 是否展开。
- 退出后本地输出收尾默认上限 500 ms；届满保留已捕获内容并标记 output_complete=false，说明未 EOF。远端执行不在本次新增范围。
- 更新执行终态、发布最终事件、释放该执行持有的资源/quota各执行一次；回调重复不重复计数或释放。
- 工具调用 quota 在调用返回时释放；存活进程的执行会话 quota 继续保留到真实终态，不靠下次工具调用顺便回收。
- 退出与停止请求竞争时由 manager 线性化：已观察到退出则请求返回 already_finished；尚未退出才进入 stopping。只有观察到结束才能发布终态，迟到的 watcher、poll、命令回调不能再次提交另一种结论。
- session.close/shutdown/delete 停止该对话拥有的执行会话并结算；UI detach、切换对话和窗口断线不等于上述命令，不终止后台工作。

### 8.3 daemon/actor 恢复

- 正常 daemon 关停先停止受管进程并提交最终事实；无法完成的明确记 interrupted。
- 异常重启后，对旧 epoch 非终态调用、执行会话、回合、未决交互进行一次恢复结算，原因 restart/lost_execution；不能继续展示 running。
- 不根据数据库中的旧 PID 重新接管或杀进程，也不伪造旧进程已经成功退出。旧执行会话不可继续写入。
- actor 不得在其仍负责的前台执行/等待未完成时直接丢弃责任；已 yielded 会话由会话级执行管理器持有，不随 agent 临时卸载销毁。

## 9. 持久化与模型重放

### 9.1 数据结构

数据库 schema 5 升至 6，新增一次性升级步骤；不是在运行路径同时支持两套 schema。

- tool_calls 保存契约 state/outcome、exec_session_id、interaction_id、真实调用起止时间，移除背景运行状态与结束时间共存的旧表达。
- 新增 exec_sessions 表：身份与归属、origin IDs、epoch、启动信息、state/outcome、真实执行起止时间、输出统计及最终 ContentValue。
- turns 保存 phase；Interactions 保存取消/应答结果，三者迁移由 owner 协调提交。
- 高频输出块只在有界内存捕获，bootstrap 从 manager 读取有界尾窗；启动/停止/终态与最终内容引用持久化，不为每个输出块做 SQLite 写事务。
- 无消费者也必须完成终态落库。不得以广播成功与否决定真实生命周期。

### 9.2 不可变调用结果

- ToolResult timeline 条目关联不可变 ToolCallOutcome；模型可见结果由该 outcome 与当时的正文生成。
- 当前回合和下一回合重放使用同一个序列化入口。yielded 一直是成功返回，不能因进程还在 Running 或后来 exit 1 改为历史调用 error。
- 后续进程结果通过 write_stdin 的新工具结果被模型观察；不把异步退出无条件注入成一条新用户消息或触发新回合。
- 统计分别记录调用成功/失败和执行会话退出/取消/超时。启动成功后进程失败，可同时计为一次成功 yield、一次失败执行，不再用同一个 success bool 覆盖两者。
- 审计读取同一契约 outcome，禁止扫描正文中的 error、stderr、timeout 字样决定结果。

### 9.3 一次性旧数据升级

升级在新 daemon 对外服务前完成，事务化、有明确版本记录。现有 initialize 对旧版本直接报错，需要补实际的 5→6 步骤。

- 历史已确认完成/失败/取消结果迁移为对应不可变调用 outcome，不改变原始正文。
- 原 backgrounded 调用迁移为成功 yielded；分配仅用于历史关联的 ExecSessionId，执行会话明确标为 interrupted/upgrade_state_unknown，不能冒充可操作的新活进程。
- 旧 exec 被记为 timeout/error，但存下的完整合法 ExecOutput JSON（或确实存在的历史 canonical data）明确记录 backgrounded 时，仅在一次性迁移中按这项证据修正；记录修正原因。解析不完整、缺少归属或只是在文本里出现某个单词，不构成修正证据。
- 旧非终态但无法确认结果的调用/回合迁移为 interrupted；事实不足不补成成功，也不伪造退出码。
- 历史整数 process_id 不升级为可操作凭据；新旧身份不复用。缺少归属证据的数据保留未知原因。
- 旧工具白名单的一次性配置升级将 exec 替换为 exec_command、process 替换为 write_stdin，更新生成的工具目录；不在运行期注册别名。旧 process 的行政停止能力转移至统一命令面，不通过白名单映射保留旧动作协议。

## 10. 三仓文件级改动

### 10.1 qaqh-backend

| 模块/文件 | 修改 |
| --- | --- |
| 新增 `crates/qaqh-types/src/execution_contract.rs`；相关 `lib.rs`、Cargo.toml/feature | 基础身份、outcome、错误/内容引用只有一个定义；消除反向依赖和重复类型 |
| `crates/qaqh-ringing-contract/src/state.rs`、`events.rs`、`fact/types.rs` | 新身份、调用/执行状态、outcome、ExecSessionView、Turn phase、事件及校验 |
| `crates/qaqh-ringing/src/command.rs`、`v2/types.rs`、路由目录 | 统一控制命令、target 校验、契约代际；删除旧 exec-sessions 路由 |
| 新增 `crates/qaqh-process-tools/src/exec_session/{mod,manager,capture}.rs`；整理 `exec/*`、`process_registry.rs` | 一个 ExecSessionManager；复用平台 spawn/kill 能力，统一采集/输入/等待/退出；删除旧独立 registry 生命周期和按流拼接 |
| `process_inspect.rs`、`exec/register.rs`、`lib.rs` | 删除旧 process/exec 注册；注册 exec_command/write_stdin；不保留 wrapper |
| `crates/qaqh-tool-core/src/tool_api/{output,typed,result,context}.rs` | 返回/注入契约类型；明确执行会话宿主入口；移除分离 status/error 的重复权威映射 |
| `crates/qaqh-types/src/tool_result.rs` | 结果存储与模型投影采用不可变 outcome；模型和展示均派生，不再混写成功状态和 error |
| `crates/qaqh-workspace/src/{execution,manager,tool_side_fold}.rs` | 类型化结果贯通；截断只影响正文；统计分开；更新执行工具名与预算 |
| `crates/qaqh-agent-runtime/src/{tool_job,session_loop}.rs` | 去掉 JSON 状态推断和背景终态补丁；挂接 manager；取消打通等待；重放与 provider 错误归因修正 |
| `crates/qaqh-session/src/owner.rs`、`db/{schema,store,view}.rs` | owner 串行迁移/定序/快照；新表、迁移、恢复结算与真实时间 |
| `crates/qaqh-runtime/src/registry/*`、service/handlers | 注入会话级 manager；统一停止/关闭责任；新增 typed list service；activity 统一 |
| daemon `axum_impl/{v2,commands,mod}.rs` | 新命令 dispatch、所属会话 lease 校验、bootstrap 代际与执行视图 |
| `crates/qaqh-client/src/{v2,v2_stream,client,session}.rs` | Rust SDK 类型与握手、事件覆盖、控制 API 同步 |
| `tools/qaqh-contract-export/src/{main,client_fixtures,coverage,wire_fixtures}.rs` | 导出新类型/常量/service/事件清单，更新 fixtures 和真实路由覆盖 |
| 工具 capabilities、permission、模式白名单、PTC SDK/计划、prompt | 全部更新工具目录；消除旧 process/exec 名与旧取消语义 |

manager 管 OS 资源与捕获，owner 管公共业务状态与事件。这是两个明确职责，不允许双方各存一份可独立迁移的执行终态；manager 的观察只能经 owner 提交为公共事实。

依赖方向不引入环：`qaqh-tool-core` 声明强类型 ExecSessionHost 注入点，process-tools 实现执行能力，runtime 装配 manager 与 owner；宿主通知只携带本规范的身份和观察事实，不翻译旧 JSON 状态。现有同步工具边界可继续运行在 spawn_blocking 中；等待使用通知/条件变量，不复制每 50 ms 轮询循环，也不在工具里 block_on 重入 Tokio。

### 10.2 qaqh-sdk

- generated 协议、constants、manifest、coverage、fixtures、service-map 全部由后端 exporter 生成，不手改镜像类型。
- core 的事件运行时校验从完整契约事件清单生成；编译期穷尽检查保留，未知代际不能静默忽略。
- open 与 bootstrap 校验命令/会话契约修订；不兼容在消费流之前失败。
- 新增类型化 execSessionTarget、interruptExecSession、terminateExecSession 和按 sessionId 查询的 list 方法；删除旧整数 ID API、killExecSession 和 unknown[] 响应。
- watch 只负责传输、排序边界、重连与类型化派发，不解释后台为错误，不替 UI 修改业务状态。
- 区分 protocol/transport/command-rejected 与业务 outcome；输出缺口不停止 watch。
- 本次不向 SDK 暴露“启动任意模型工具”的旁路；模型入口由后端工具目录控制，UI 控制经统一命令。

### 10.3 qaqh-electron

| 区域 | 修改 |
| --- | --- |
| `src/main/ipc-handlers.ts`、`ringing-client.ts` | 消费新生成命令清单/类型；接通 exec_session target；转发新事件；区分宿主错误和命令拒绝 |
| `webui/src/lib/transport/{backend,electron}.ts` | 强类型执行会话视图与控制方法，删除旧 kill REST 调用形状 |
| `webui/src/api/qaqh/*` | 使用 exporter 镜像，保持与 SDK 一致 |
| `webui/src/session/{types,reducer,store}.ts` | 调用 state 与 execSessions 分开；按 ID 关联；处理 phase/等待/取消；终态只能来自契约事实 |
| `webui/src/tools/presentation.ts`、`StepRow.tsx`、`registry.ts` | 统一有序输出区；终态展示权威 transcript；yielded 成功返回不标红、不清会话尾窗 |
| `webui/src/turn/{segments,TurnView}.ts/tsx` | 已结束回合仍可更新其后台执行卡；后续 poll 聚合到原执行会话，保留各次工具调用历史 |
| Composer/状态条/错误展示 | 运行、等待、正在取消、退出码、失败、中断分别表达；不把 stderr 或断线直接当工具失败 |
| watchdog 与 activity 派生 | 删除 forceSealTurn 式假终态；超时只显示“停止状态尚未确认”并重新同步；后台数量不点亮 agent 工作状态 |

前端视图规则：原 exec 调用成功 yielded 时显示“后台运行”；后续进程非零退出可显示“退出码 N”，但不重写原调用 outcome。ToolCall 的 endedAt 和 ExecSession 的 finishedAt 分开，耗时从对应生命周期取值。

最终 output 为内容引用时，加载期间保留已有尾窗并显示加载状态；正文成功载入后原子替换，不能先清空再显示占位。没有会话的普通工具失败按 outcome 展示；存在 stderr 不单独产生红色失败标签。

关联只按 call_id/turn_id/exec_session_id。原回合尚未加载或已经被分页裁剪时，将执行会话保存在会话级后台列表并按需加载原回合；不能套用当前 getActiveTurn 回退，把迟到的完成或输出挂到另一个正在运行的回合。

保留上一轮 assistant 首帧建块、思考展示、终态全文覆盖与中途发言保留修复；不要在此次状态改造中退回 pending-only 展示。

## 11. 契约版本与切换

- `/ringing/v2`、schema `qaqh.Ringing`、外层 version=2 保留，不另起一个“RingingV3”转换协议。
- `COMMAND_CONTRACT_REVISION` 从 ringing-commands-6 升至 ringing-commands-7。
- 新增 `SESSION_CONTRACT_REVISION = ringing-session-2`，由唯一 Rust 契约定义，open 与 bootstrap 均携带；SDK/Rust client 必须严格校验。此字段管理事件/读模型破坏性变化，不用模型工具目录版本冒充。
- 后端、SDK、Electron 同批生成、构建和发布。首个显式 session 修订号取 ringing-session-2，当前无该字段的读模型视为隐式第 1 代。旧客户端明确拒绝连接；不接受“能连上，但遇到第一条新事件才报错”。为此新 open 请求也必须携带客户端预期的 command/session revision，服务端在分配 lease 前校验；只在响应加字段不足以挡住旧客户端。
- 旧客户端并存不是验收目标；禁止长期双字段、双路由、双事件和旧工具别名。
- 新规范替代 2026-11-03 decisions 的 D4 工具状态事件、D6 中 exec 暂态恢复和 D8 握手部分；D1 exclusive watermark 继续有效。原文标注被本 spec 替代，不能留两套同时生效的裁定。

## 12. 实施批次与完成条件

各批次是实施顺序，不是可以独立对外发布的兼容阶段。

| 批次 | 交付 | 退出条件 |
| --- | --- | --- |
| A：契约 | 类型、事件、命令、读模型、版本、fixtures | 三仓能基于生成物编译新接口；没有空壳事件或假能力声明 |
| B：执行内核 | manager、统一捕获、yield/poll/stdin、退出 watcher、资源预算 | 普通管道和 PTY 都能 yield/续读/停止，输出事实完整 |
| C：owner/runtime | 真实状态迁移、取消、重放、DB 升级与恢复、quota | 新老数据切换完成；原始调用、执行会话、回合均不悬空 |
| D：SDK/UI | 握手、事件消费、有序展示、停止确认、后台更新 | 实时、重连、回放结果一致；无假取消和后台误报 |
| E：清理/发布 | 删除旧入口、刷新 prompts/目录、三仓构建产物 | 删除清单核查通过；同批部署并用隔离数据目录冒烟 |

实施中不新增第三套“先修一下后台 error”的文本转换方案。已有临时修复能证明路径时保留到对应批次替换，完成后删除，不作为最终架构组成。

## 13. 最小验收集

优先复用现有 owner、session_loop、exec、SDK watch、前端 reducer 的测试夹具。以下是场景矩阵，可以合并为少量参数化/贯通用例，不要求每行新增一个测试文件。

| 场景 | 必须验证 |
| --- | --- |
| 短命令 exit 0 / exit 非零 | outcome 与退出码一致；普通 stderr 日志不独立判失败 |
| 普通管道与 PTY 的长命令 | 窗口到期 yielded；error=null；失败统计不增加；进程继续运行 |
| 硬 timeout | 停止进程树并确认；结果 timed_out，不转后台 |
| 交错 stdout/stderr、拆分 UTF-8 | 采集顺序、模型结果和 UI 合流一致；字符完整，offset 正确 |
| 多次空 poll / stdin 后 poll | 只返回新输出；输入响应可见；同会话并发操作不重复消费 |
| 后台自行退出且不再调用任何工具 | 最终公共事件、DB、配额、原始卡片全部收尾一次 |
| 授权/Ask 等待期间取消 | 等待被唤醒、交互取消、工具/回合真实收尾；迟到应答不执行 |
| 运行中取消、取消一个后台 poll | 前者停止自己仍持有的执行；后者不误杀已 yielded 会话 |
| 错误/重启/连接断开 | 用户取消、provider 失败、interrupted、transport error 不混用 |
| yield 后新回合重放 | 原调用仍成功 yielded；不因进程 Running/exit 1 翻成历史 error |
| bootstrap、重复边界帧、丢进度帧 | 状态不回退；exec 尾窗恢复；省略可见；watch 不因缺口退出 |
| 配额/输出洪峰 | 缓冲与元数据受限；无静默杀进程；最终捕获不依赖 UI 队列 |
| schema 5→6 | 历史结果保持可解释；未知活进程不伪装成功或继续运行 |
| 新旧修订不匹配 | open 阶段明确失败；不进入半兼容消费 |
| 原 assistant 流式场景 | 中途发言、思考、最终尾部不回退 |

验证顺序：变更模块类型检查与定向测试 → exporter check/生成物一致性 → 三仓 SDK/前端类型检查 → 隔离数据目录的真实 daemon SSE 冒烟。Rust 限制 `CARGO_BUILD_JOBS=2`，按批次验证，不每改一个文件跑全仓测试或重复全量打包。

## 14. 最终删除清单

- 旧 exec/process 模型入口、旧参数、别名及提示文案。
- 从模型正文 JSON 推导 backgrounded/process_id/success 的 runtime 分支。
- `ToolFinished(Running)`、后台 DB 状态再映射 Running、调用结束时间充当进程结束时间。
- 结束后按流分组拼接的 combined output、前端 stdout/stderr 两区作为唯一默认展示。
- 旧 exec-sessions list/kill REST、SDK 整数进程凭据和 unknown[]。
- 轮询/下一次工具执行顺便修复公共状态的回收逻辑。
- 前端定时器制造 aborted/failed 业务事实，以及将所有 Interrupted 归为取消的归约。
- 分散维护且非穷尽的命令/事件白名单；改为 exporter 生成与穷尽消费。

历史记录原文、迁移器及迁移测试中用于解释旧工具的名字允许保留；它们不能被注册成运行期别名，也不能被当作删除检查要求抹去历史证据。

只有“契约事实 → 后端提交与事件 → SDK 类型化消费 → 前端投影”和同源模型结果两条消费路径；不允许其中任何一层重新从展示正文猜状态。
