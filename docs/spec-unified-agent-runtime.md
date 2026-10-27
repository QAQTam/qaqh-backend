# UAR：统一 Agent 运行时与子代理旧设施删除规范

- 制定日期：2026-10-26。
- 状态：**设计稿，待所有者审阅；不是实现报告。** 本次仅落文档，不表示重构、协议切换或数据迁移已完成。
- 总任务：`UAR`；实施任务见 §14。
- 产品目标：子代理是“有父子关系、能力策略不同的普通 agent”，不再是另一种 worker 或一次性后台进程。
- 交付原则：**替换旧所有者，删除旧路径；不加兼容 facade，不保留在线 fallback，不用新命名包装旧状态机。**

## 0. 本文的效力与前置裁决

### 0.1 与仓库既有规范的关系

遵守 [AGENTS-x.md](../AGENTS-x.md) 的唯一事实源、显式上下文、单一取消机制、无锁内 IO、禁止静默丢数据等约束。
本文细化并采用 [agent loop 重塑规范](../new-tech/docs/v2-reset/agentloop-spec.md) 的唯一 writer、真实邮箱、提交后派发和任务监督模型，**不是与其并列的新框架**。

对子代理运行时，本方案获批后取代 [SA-FIX-01](spec-subagent-lifecycle-bugfix.md) 及其 [施工 prompt](prompt-subagent-lifecycle-bugfix.md) 的增量修补路线；原文留作历史，不再作为本任务的执行入口。批准前不同时开工两条路线。
原 spec 中有效的故障反例、权限性质、task/board 业务性质必须迁入新验收，不能以“推翻方案”为由删掉功能。
两份文档中的执行 ID、超时和 wire 保持不变等裁决，以本文及随附 ADR 审批结果为准。

当前树缺少 `docs/ARCHITECTURE.md`、`docs/spec-architecture-convergence.md`；实施 UAR-00 必须补齐真实架构登记，不伪造已读或已满足。
本规范不依赖缺失文件才能被审阅，但实际代码交付必须补架构存储表、依赖边和线程/任务所有权。

### 0.2 破坏性变更必须显式批准

本方案有意删除旧 API/类型、修改 agent 工具输出和持久化 schema，不受“保持一切旧调用方式”约束。
[ADR：统一运行时的冷切换边界](adr/2026-10-26-unified-agent-runtime-cutover.md) 是 AGENTS-x I14 要求的协议裁决入口。
**ADR 在所有者批准前保持 Proposed；用户要求起草 spec 不自动等于对全部协议/迁移细节的批准。**

禁止在现行 Ringing major 2 中暗改 accepted、cancel、close、结果和状态语义。涉及本方案的破坏性 wire 随已规划的 major 3 协调切换；具体 URL、协议信封和认证沿用 protocol 专题，不在本文另造协议栈。
不要求把 ACP、市场、多语言 SDK 等尚未实现的能力一并实现。实际支持的客户端与 Rust/TS SDK 必须同批升级；未升级版本具名拒绝，不自动降级。

### 0.3 不允许的“折中交付”

以下均判定未完成：

- 修 collector 的几个 `if`，继续让它决定 child 成功、超时和卸载。
- 新建 AgentRuntime trait，内部仍区分 session/subagent 两条构造、恢复和结果路径。
- 只给 child 加 execution_id，主 agent 仍是另一套生命周期。
- 新结果索引与 `subagent_outcomes` 双写，或 canonical 正文读取失败后查传输缓存。
- 新旧 loop 通过配置开关切换、旧 host 适配到新 host、旧服务方法别名长留。
- 把已接纳任务的丢失变成 TTL 过期、空结果、exit 0 或“unknown error”日志。

## 1. 审计基线：要解决的不是一个超时参数

以下是 2026-10-26 工作树核对结果；行号用于定位，实施时以符号与最新源码为准。

| 基线事实 | 证据 | 必须消失的前提 |
| --- | --- | --- |
| main/child 已共用 `run_actor → spawn_agent → Loop` | `crates/qaqh-runtime/src/actor.rs:112`，`agent/spawn.rs:167` | 不再把共用循环误当成完整复用 |
| collector 断流只设 Failed，仍以空答案和 exit 0 结算 | `crates/qaqh-subagent/src/lib.rs:1281`、`:1380`、`:1398` | 观察链路不能裁决任务结果 |
| 答案下载 Err 被忽略 | 同文件 `:1328` | 取不到正文不能伪装成功空答案 |
| collector 读 canonical 投影，生产 parse 仍写 MessageStore/timeline | `host_impl.rs:1703`，`agent/turn_lap/parse.rs:86`，`gate.rs:342` | 两套消息/终态词汇不能并行维持权威 |
| 正常完成才显式调用 `record_turn_finished` | `agent/turn_lap/backfill.rs:254`，`agent/loop_outcome.rs:527` | fail/cancel/panic 不得另走只供日志或旧广播的出口 |
| child 默认绝对 120s，collector 到期取消且终态后关闭 | `qaqh-config/src/config.rs:62`，`qaqh-subagent/src/lib.rs:1296`、`:1560` | 超时、任务完成、actor 卸载不能混成一件事 |
| outcome/collector/parent finish 按 child ID，而非每次执行关联 | `registry.rs:355`、`:587`，`qaqh-session/src/canonical/tool_ledger.rs:940` | 长期身份不能充当一次执行的幂等键 |
| child reload 设置 ID 但不走 resume，创建空 MessageStore | `agent/spawn.rs:126`，`agent/loop_core.rs:513`，`state/lifecycle.rs:474` | 同 ID 新建不是恢复 |
| V2 result 的 as_system=false 与 lap 消费仅认 true 不匹配 | `qaqh-subagent/src/lib.rs:1467`，`agent/loop_injection.rs:308` | 消息语义不能由布尔位和文本标签决定 |
| registry 持锁等待停止，超时后仍无条件 join | `registry.rs:1728`、`:2181`、`:2231` | 卡住一个 worker 不能冻结整个 registry |
| 独立 `subagent.spawn` 服务入口只造 worker，另有 host 入口 | `service.rs:882`，`qaqh-client/src/endpoint.rs:199` | 不再有绕开父会话业务所有者的第二条 spawn 路 |

已运行证据：原 `qaqh-subagent --lib` 14/14 通过；临时调用真实 collector 的断流、blob 读取失败两探针均得到 `[("audit-child", "", 0)]`，与期望的非成功结构化错误不符；临时测试已撤回。
这证明既有单测未覆盖关键错误语义，不代表本文的新运行时已经验证。未读取用户故障会话，不能把某一分支当成每次线上故障的唯一归因。

## 2. 范围与保留边界

### 2.1 本次必须一起替换的域

1. 主/子 agent 的身份、装配、邮箱、执行状态、唯一提交、结果读取、取消、停止与重载。
2. provider/tool 等长调用阻塞会话处理栈的机制；没有真实邮箱就无法可靠处理 wait、回传和控制输入。
3. 迁移域的模型上下文权威：facts + durable response bundles/blobs；MessageStore 只能成为可重建投影，旧 messages/WAL 不再由在线执行链写入。
4. 子代理创建、任务投递、结果回父会话的显式业务流程，以及跨会话重试/恢复。
5. agent 协作工具的实例注入；task/board 只迁装配和端口，不重写其有效业务状态机。
6. 旧服务、旧结果 DTO、旧线程/TLS 生命周期控制和本方案覆盖的双写链删除。

这是一条跨主/子 agent 的完整垂直切片，不是只改 `qaqh-subagent` crate。不能把唯一 writer、真实 mailbox、恢复和消费者升级留到“下一轮兼容修复”。

### 2.2 不扩大重写的域

保留 provider 协议解析与账号算法、文件编辑/LF 规则、stale guard、dry-run/confirm、沙箱实现、工具参数容错、工具顺序与真实副作用证据。
skills、compact、plan/todo 的现有能力通过目标 feature/runner 端口迁移，不顺便增加产品功能；禁止把旧大 loop 整体搬进“feature”冒充完成。
不建设分布式调度器、数据库、远程 agent 平台或通用插件市场；不预先建全套空 crate。

## 3. 核心裁决：agent、execution、actor 是三件事

### 3.1 唯一命名与身份

| 名称 | 定义与寿命 |
| --- | --- |
| `session_id` | agent 的持久主键；root/child 都一样。禁止另造 seed/agent UUID 别名 |
| `agent_path` | 同一 root 树内地址；不是数据主键。路径解析始终有 root scope |
| `input_id` | 已接纳输入的稳定主键，由入口 message/command ID 按一个统一函数映射 |
| `execution_id` | **Trigger 输入的 input_id 的类型别名/视图，不再生成独立随机 run ID**；全键为 `(session_id, input_id)` |
| `turn_id` | 一次实际开始的模型工作回合；InputAccepted 可在尚无 turn 时排队或终结 |
| `actor_incarnation` | 一次驻留 actor 实例的易失代际；只用于路由和晚到 completion fencing |
| provider/tool attempt | 自己的 request/attempt/工具执行身份；不得覆盖 agent execution_id |

外部 message_id 与 canonical input_id 的映射只实现一次。重复同 ID 同 digest 返回原 receipt；同 ID 异内容明确冲突。
一次 Trigger 在本方案中对应一个 execution；模型连续工具 lap/断流续写均属于这个 execution 的同一 turn。
自动 goal 下一步是显式的新 Trigger，不能在“完成”之后偷偷延长旧 execution。

### 3.2 持久与驻留分离

- 主/子身份一经提交持续存在；普通完成、failed、interrupt、timeout 不删除身份或会话数据。
- 完成后 actor 默认留在 idle，由统一 idle-unload 策略回收；**不再每轮完成自动 SessionClose**。
- unload 只释放驻留资源；后续命令使用同一 `open_or_resume`，恢复原上下文、turn 分配水位和能力配置。
- close 是显式生命周期操作，封闭整棵子树新准入并取消/收割；delete 是另外的、经授权的持久删除操作。
- 有 active/pending execution、未决交互、未交付结果或残留 runner 的 child 及其 owner 祖先不得被 idle-unload 连带取消。

## 4. 唯一运行主路径与所有权

```text
Ringing / in-process / 18 个协作工具
                 ↓ typed application commands / proposals
            AgentHandle（有界 mailbox）
                 ↓
       同一个 SessionActor / reducer
                 ↓
       SessionCommit（facts + blobs）
                 ↓ commit 屏障
       committed view + 显式 effects
          ↙                       ↘
只读投影、wait、UI           统一 TaskSupervisor
                                 ↓
                  provider / tool / child command runners
                                 ↓ typed completion
                           原 owner mailbox
```

| 对象 | 唯一所有者 | 禁止事项 |
| --- | --- | --- |
| committed session 状态与 writer lease | 本 session 的 actor / SessionCommit | runner、tool worker、collector、订阅者不得取得 writer |
| execution、消息队列、交互状态、上下文 head | actor，持久部分从 facts fold | 不另有 child 专用 pending/outcome 权威 |
| 所有 provider/tool/后台 effect 句柄、取消与 join | 统一 TaskSupervisor，按 owner execution 分区 | 不丢 JoinHandle，不用 std::thread::spawn 后撒手 |
| registry | handle/incarnation/residency 的短锁索引 | 不执行模型、工具、磁盘 IO、阻塞 send/join |
| agent graph、结果、mailbox/read views | committed facts 的可重建投影 | 禁止 list 时惰性补身份、迁移或猜结果 |
| 子代理决策 | 现有目标中的 SubagentFeature / delegation application | 不拥有第二个 worker runner、writer 或 SSE collector |
| task/board | 原有聚合业务所有者 | 不能变成 execution 调度器 |

核心和应用模块先放现有合适 crate；不为减少改动保留反向依赖。`qaqh-session` 只提供中立事实/提交/投影，不依赖 runtime/wire。
工具适配器实例持有限定端口，端口契约在应用 API 边界；不能借 `Any`、字符串 service map 或任意回调绕过依赖方向。

建议落点用于锁定职责，不要求把模块名当公共 ABI：

| 当前工作区内落点 | 负责内容 |
| --- | --- |
| `qaqh-session` 的中立 fact/commit/context/projection 模块 | schema、唯一提交、正文、纯 fold；task/board 中立 DTO 留在其业务所有者 |
| `qaqh-runtime/src/agent/` 的 mailbox/reducer/session_scope 模块 | 同一个 agent；复用已有 TurnCore 的有效算法，不再镜像旧 Outcome |
| `qaqh-runtime/src/execution/` | supervisor、runner、deadline、收割；不解析 Ringing 投影 |
| `qaqh-runtime/src/collaboration/` 的 application/ports/tools 模块 | create/delegate/result 用例、限定端口、18 个 typed tool 实例 |
| `qaqh-runtime/src/ringing/` 与 `qaqh-daemon` | 协议映射、实例装配，不保存另一套执行状态 |

协作工具在 runtime 装配时通过现有 `ToolManager::register_typed(instance)` 注册，取消依赖零捕获函数指针才能找到全局 host 的注册方式。
通用文件/进程工具 SDK 不需要因此变成 service locator。现有 task/board host 的业务方法迁成实例端口，不复制状态机到工具文件。
如后续实际拆包，工具适配器可依赖应用 API，核心不得反向依赖工具适配器；拆包与消费者迁移同批完成，不保留全量 facade。

### 4.1 真实 mailbox，不是等待长调用结束再 drain

- actor 处理输入并提交后，把长调用派发到 runner，立即恢复 mailbox 接收。
- commands、reliable completions、control、replaceable progress 分预算；completion 投递许可在派发前预留。
- 用户/Trigger 按会话有界 FIFO；忙碌 followup 进入持久待执行队列，不拒成“已在运行”，不当作 steer。
- cancel/shutdown 有保留容量；先校验精确目标再触发相应 CancelToken。不能被 wait 或普通队列挤死。
- `wait_agent` 在独立等待任务里订阅 committed view，不持 actor 栈、writer 或 registry 锁。
- provider 最终 bundle/工具结果可靠归集；只有展示进度允许丢弃或合并。

### 4.2 提交屏障与失败语义

`SessionCommit` 返回 `Committed / NotCommitted / Unknown(batch_id)`，具体复用 agentloop-spec 的提交合同。
blob fsync → 同批 facts/commit marker → fold → 对外发布/ACK → 派发 committed effects。
Unknown 时封闭该 session 新派发并按原 batch_id 对账；不得换 ID 重做模型/工具。
提交前的临时资源失败统一清理；提交后的任务失败追加真实终态，不能“rollback”掉已经发生的历史。
存储失败不能只告警继续，不能已经告诉父代理 completed 却没有耐久答案。

## 5. 事实与执行状态合同

这是需要 schema 升级的**目标事实模型**，不是往旧结构无穷加 Option。沿用可用原语，切换时删除被替代类型。

### 5.1 最小持久记录

| 事实 | 语义与必要关联 |
| --- | --- |
| SessionCreated + AgentLinked | child 自身会话与 parent 的身份边；edge 每身份只提交一次，不随 followup 再写 |
| SessionAdmissionChanged | 显式 open/closed 与原因；关闭准入的持久状态，不等于 actor unload |
| InputAccepted | input_id、purpose、来源/owner、准入序号、正文 durable ref、预算和非秘密配置引用；Trigger 即 execution 接纳 |
| TurnStarted | turn_id、input_id、固定 config/capability revision；表示实际开始，不用于补造 queued 运行证据 |
| ProviderResponseCommitted / AssistantBlockSealed | 完整 replay bundle 的屏障、block/request/round 身份、kind 和 durable ref；live delta 不落事实 |
| ToolIntent / ToolFinished / interaction facts | 保留真实执行身份、授权与副作用证据；writer 归 actor |
| ExecutionCancelRequested | 精确 input_id、原因和取消 owner；取消请求不是停止证明 |
| ExecutionFinished | input_id、可选 turn_id、terminal、结构化 error/stop reason、final answer refs、usage、证据关联 |
| DelegationRequested | parent 的源 execution、目标 child/input、固定请求 digest 和投递意图；每次任务一条 |
| DelegationResultReceived | parent 对 child 某次 ExecutionFinished 的 durable 接收凭证及本地正文 refs，不独立裁决 child 成败 |
| ContextApplied / 必要 reconciliation facts | 输入已在何 request/context revision 被模型消费，或异常提交/副作用证据的对账结论 |

`TurnFinished` 与 `SubagentFinished` 不再作为并行的两套业务终态 writer；本域终态统一为 ExecutionFinished，turn/UI/team 状态由它投影。
`TurnInterrupted` 中有效恢复证据迁入统一 execution recovery 语义，不能再拿中断记录覆盖已完成回合。
parent receipt 可携带源 terminal 摘要，但必须可校验源 session/log/event/digest；它是交付事实，不是第二个 child 状态机。
事实 schema 的精确 Rust 命名在 UAR-01 锁定，不能将上表作为允许同义概念重复存在的借口。

### 5.2 状态机

```text
accepted/queued → started → running / awaiting_provider / awaiting_tools / awaiting_user
       │                      │
       └─取消或启动失败─────────┤
                              ↓
                cancel_requested / recovering / blocked
                              ↓
completed | failed | cancelled | timed_out | interrupted | indeterminate
```

- `queued/started/terminal` 的业务证据必须持久；细分运行 phase 可是易失观测，丢失后不能推导成功。
- queued 可以没有 turn_id 而被取消/失败，禁止为此伪造 TurnStarted。
- 完成结论唯一、不可改写；重复同 digest 幂等，冲突明确拒绝并对账。
- `failed` 必有稳定错误码和非空用户可见消息；`cancelled/timed_out/interrupted` 必有原因/发起者。
- `indeterminate` 表示副作用或停止证据不足，不伪装 completed、failed 或已停止。
- 正常空答案可合法：Completed 明确包含 `answer_present=true` 和空内容引用；与正文丢失/尚未封口区分。
- `residency` 是 loaded/unloaded/stopping/stop_incomplete 等资源状态，不能替代业务 terminal。

### 5.3 删除 agent 的 exit_code 语义

agent 不是 OS 进程。删除 AgentOutcome.exit_code 及靠 exit=0/1 解析成功的展示/提示。
新 execution outcome 返回 terminal、error/stop_reason、答案、源事实和执行身份。exit code 仅保留在真正的进程/exec 工具结果内。
超时、断流、blob 损坏不得返回 `answer=null, exit_code=0` 的等价物。

## 6. 正文、模型上下文与结果投影

1. 主/子共用一条 provider completion → 完整 bundle 验证 → durable blobs → response/answer facts → execution terminal 路径。
2. 不仅持久化人类可见文本：Responses 私有项/签名、工具调用参数、多模态关联、reasoning kind、usage 与前缀均须保真。不得用拼接 transcript 替代可恢复模型上下文。
3. 使用 SessionBlobStore 的带类型 ContentRef，明确 owner session、digest 与媒体类型；跨会话先复制正文并校验摘要，再提交 parent 本地引用。
4. canonical 内容服务、list/wait 和 UI 都读同一 durable store。传输缓存只能加速传输，cache miss 不得改变业务结果，禁止回源猜测。
5. 最终答案取最终模型轮的 Answer blocks，按 block 顺序拼接；不泄漏 reasoning，不把工具前说明当最终答案。失败可返回另标的 partial refs。
6. 多 block、Unicode、超大正文不能 silently truncate。结果体有明确预算，超过 inline 阈值返回可解析 durable ref，不靠全量 clone 填 mailbox。
7. MessageStore 若保留，仅为 committed context 的易失投影；在线读取不得在 canonical 缺失时读取旧 messages/WAL 补齐。
8. 旧 timeline 的文本/状态快照在迁移域退出权威；UI timeline 从 committed facts 加可丢 live overlay 构建。不得靠 UI 展示事件触发工具或 child 结算。

## 7. 创建与跨会话投递：显式协议，不靠事件监听补业务

不要求 parent 和 child 两个日志具有跨文件原子事务；用 committed intent + 幂等命令 + durable receipt 解决崩溃窗口。
不承诺网络/OS exactly-once，只承诺同一执行身份的持久准入/结算幂等。

### 7.1 spawn 顺序

1. parent actor 校验名称、root scope、深度、权限、配额，预分配稳定 child ID/path 和首个 Trigger input_id。
2. parent 同批提交 AgentLinked、DelegationRequested 和必要 quota/dispatch intent；**这之前不启动 child actor、不调用模型**。
3. 提交后由统一 supervisor 执行 `EnsureSession(child_spec)`，以同一 child ID 幂等初始化 child SessionCreated。
4. 向 child 普通 mailbox 投递该 Trigger；child 提交 InputAccepted 后才确认 accepted。ACK 丢失重发原 input_id/digest，不创建第二次任务。
5. 对外 spawn receipt 返回 child 身份和本次 execution_id，明确 provisioning/queued，不声称已经开始模型调用。
6. 同进程 tool 的协调动作向 owning actor 提交 proposal，不在同步 handler 里 RPC 回父 actor 并阻塞等自己，避免 self-deadlock。

两级接纳必须区分：parent 的 DelegationRequested 提交只代表请求已接纳、正在 provisioning/dispatch_pending；child 的 InputAccepted 提交才代表目标 execution 已接纳/queued。
初始客户端回执可报告 provisioning 及预留 input_id；**spawn/followup 工具最终成功输出必须等 child accepted ACK 后才返回 queued receipt**，期间 actor 仍接收邮箱，不阻塞会话处理栈。
parent intent 的成功不可以被翻译成“child 已运行”。工具等待被取消/响应丢失时，该 durable delegation 仍能按原请求身份查询和对账。

创建失败：提交前释放临时 reservation；提交后保留已提交身份/边，以明确 provisioning_failed/failed 事实结算，不删边伪装没发生。
child 从未 InputAccepted 时，失败属于 parent delegation 的 `failed/not_started` receipt，不捏造不存在的 child ExecutionFinished；list 将 provisioning 状态与 target execution 分栏。
在 child 尚无已接纳 execution 时 wait 返回明确 provisioning/no_execution，不能把 delegation 失败当成 child 已运行再失败。
崩溃后有未完成 create intent 可幂等继续 materialize；已经完成 materialize 却缺 child log 属完整性故障，不能伪造历史。
配额 reservation/commit/release 必须属于这个可恢复流程，不再靠某个异常分支手动补释放。

### 7.2 followup 与结果回父会话

- followup 只是对既有 AgentHandle 提交新 Trigger；不 spawn 新身份，不另 arm collector。
- parent DelegationRequested 固定 child input_id 和任务来源；child 执行按自己的 FIFO，不跟随 parent 当前回合漂移。
- child 提交 ExecutionFinished 后，提交层派发显式 DeliverDelegationResult effect；不是订阅 SSE/ProjectionPayload 后触发副作用。
- pending delivery 从 child terminal/原 delegation route 与 parent receipt 的 committed facts 可重建；不新增 runtime WAL 或自有结果文件。
- delivery worker 只发 typed command，不写 parent 日志。parent actor 校验来源事实/执行身份、导入 blob，并在一批中提交接收凭证与结构化上下文输入。
- 幂等键为 `(child_session_id, input_id, terminal_event_id)`；message/reply_to 指回原请求，禁止只使用 `subagent-result:{session_id}`。
- 默认回传 QueueOnly：不复活取消的父 execution、不自动开用户回合。取消也交付结构化状态，正文是否呈现是策略，不得把原因压成空字符串。
- parent busy 时立即可准入持久化，安全点再消费；parent unloaded 时正常打开并准入，但不启动模型。parent closed/deleted 时返回明确 blocked/undeliverable，child terminal 仍可查询。
- 跨日志写入/ACK 任意位置崩溃后重试原身份；交付可以 at-least-once，parent context 应用与完成通知必须幂等。

### 7.3 不把通知可靠性当执行可靠性

wait/UI 的通知只是固定 committed watermark 的唤醒；lag/reset/drop 后从 durable cursor 重读。
关闭所有 UI/SSE、禁用诊断、丢弃 progress 不改变任何 agent 的运行/终态，更不能取消任务。
通知不可达不回滚已提交 child 结果，不重新执行模型。错误只发生在观察面时应明确标为 observe_error，不谎报 execution_failed。

## 8. 超时、取消与真实停止

### 8.1 删除默认 120 秒寿命

- child 不再有特殊最大寿命。全局 agent execution 总时限默认 **None（不设总 deadline）**；provider/tool 的请求时限仍独立、明确配置。
- `spawn_subagent.timeout_secs` 保留为显式 execution budget 参数：缺省取相应策略；默认不设；显式值为 1..=86400，越界拒绝，不 clamp。
- followup 与普通用户 Trigger 也通过同一 ExecutionPolicy 获得预算，不能只有初次 spawn 有预算。
- 总时限从实际 TurnStarted 开始，queued 时间不计入；运行中的 provider/tool/用户交互等待均计入墙钟总时限，文案不得称 inactivity timeout。
- provider stall、工具 deadline、execution deadline 分别产生准确错误码，不用一次性的 collector timeout 覆盖原始故障。
- wait 自己的 timeout 只结束等待，不取消目标，不改 execution 状态。

### 8.2 取消树与精确范围

唯一取消机制：daemon → session lifetime → execution → provider/tool attempts 的单向 CancelToken 树。
新 execution 创建新节点；删除 `clear()` 复活节点、SESSION_CANCELS 和全局/线程局部取消旗标在迁移链上的读写。
持久父子归属与取消树不是同一个东西：child lifetime 不挂到某个 parent turn token 上，否则取消旧回合会污染未来 followup。

- user cancel/interrupt：在准入时固定目标 execution_id。取消自身以及该 execution 直接拥有的 active/pending delegation，按持久 ownership 递归；不误伤其他 owner 的 followup。
- parent close：封闭子树准入并取消其全部 active/pending work；close 是显式树生命周期操作。
- idle-unload：没有业务取消语义，存在 owned work 时拒绝/延后卸载，不能顺手关掉孩子。
- 取消与完成竞争以 actor 提交顺序裁决；已提交 terminal 后晚到 cancel 幂等 no-op，并返回已终结原因。

若 ExecutionCancelRequested 先于 terminal 提交，后到的成功正文/已发生工具结果仅作为原执行证据保存，不恢复 Completed 或父回合；最终为 Cancelled/TimedOut，停止或副作用未知则为 Indeterminate。
若 terminal 先提交，取消是 no-op；不得追加第二个冲突 terminal。下一 queued execution 只有在前项已结算且冲突 runner 已收割后才允许开始。

### 8.3 deadline 与停止证据

到期先提交/记录 reason=deadline_exceeded 的取消请求，封闭该 execution 新派发，再信号取消、收割。
证明确已停止后提交 TimedOut；预算到点但不响应停止的 runner 在有界宽限后结算 Indeterminate，并保留 stop_incomplete 资源状态和句柄。
禁止拿“已发 cancel”“连接断开”“drop JoinHandle”当已停止证明。不可安全取消的工作若需要硬停止，必须隔离到受监督进程；不能声称 Rust 线程可强杀。
残留 runner 仍占配额并隔离冲突资源；同一 session 不启动下一项冲突工作。确定独立的其他 session 继续服务。

### 8.4 统一收割与锁边界

registry 生命周期统一三段：锁内提取 handle/incarnation 并标 stopping → 锁外 signal/IO/wait/join → 锁内核对同 incarnation 后登记。
只 join 已证明结束的线程/任务；超时保留 stopping/stop_incomplete，不无条件转无限 join，不重新拉起同一活实例。
Supervisor 收到 runner panic、actor 崩溃、writer 故障都返回 typed outcome/health；不能只日志并 reset Idle。
恢复 actor 前 fencing 旧 incarnation，旧 completion 只能结算它自己的 execution，不得关闭/覆盖新工作。

## 9. 配置、能力与显式依赖

### 9.1 构造和恢复只有一个入口

目标只有 `open_or_resume(SessionSpec, SessionScope)` 一条入口；role/parent 是数据，不允许 `ActorKind::Session/Subagent` 决定空建或恢复。
首次 session 初始化与已有 session 恢复的区别由 durable identity/commit 判断，不能通过“是否 child”判断。
`open_or_resume` 只恢复驻留，不改变 SessionAdmissionChanged；显式 closed 的 session 必须经授权的 reopen 命令重新开准入，普通 send/result delivery 不得偷偷 reopen。
reopen 不清除旧取消节点或复活旧 execution，只允许新的输入；删除需单独 tombstone，并先处理未交付结果与跨 session 引用的保留义务。

- root 的有效配置从它的会话配置解析；child 在创建时继承父的**有效**非秘密配置快照，再应用显式 profile/预算/工具覆盖。
- endpoint 覆盖以具名 profile/binding 为单位；删除 child 裸 base_url/api_key 的半覆盖入口，防止 wire/credential/compat 不自洽。
- credential 仅保存安全配置的 binding 引用，密钥不进 facts、blob、工具输出、diagnostics 或离线 transcript。
- 配置在 execution 开始时固定 revision，运行中不静默切 model/provider；显式更改只作用未来 execution。
- reload 必须恢复已持久化上下文与原 child 配置，不重新继承全局默认或当前父模型。权限按当前授权复核，不能用历史 grant 绕开吊销。

### 9.2 主/子差异只允许是策略

`SessionScope` 显式携带 workspace、tool catalog、权限/审批能力、provider binding、clock、supervisor 和业务端口；不从 TLS/global host 猜。
root 有 UserApprovalPort，child 默认 `NoApproval`；需要审批的 Exec/Net 明确 Denied，MCP host-only 规则保留。
禁止通过 SkipPermissions 掩盖没有审批通道，不把默认 child 限制误称“另一套执行器”。

工具集合用明确 `All` / `Only(names)` 策略表示，空集不承担“全量”双义；skill 补充不能把 All 变成 Only(skills)。
默认 child 为 read、exec、三项 skill 能力；exec 是否获准由同一授权层决定。未暴露、handler 缺失、授权拒绝必须分开报错。
通信/任务板/消息板/spawn 不默认给 child；需要更深树时同时满足显式工具能力、max_depth 和配额。
同 workspace 的主/子文件写入复用同一 journal/冲突规则，不新增无隔离的“子线程写文件”旁路。

## 10. 18 个协作工具与公开结果

保留产品词汇，不保留旧后端实现：spawn/list/send/followup/steer/interject/wait/interrupt，5 个 task，5 个 board。
工具只做 typed args、显式端口调用/proposal、输出投影；不读取 Config::load、host()、全局 session manager 或订阅 Ringing。

| 工具 | 新契约 |
| --- | --- |
| spawn_subagent | child accepted 后的 durable queued receipt：child ID/path + execution_id；provisioning 可观测但不是工具最终成功 |
| list_agents | graph/residency/provisioning request + 最新已接纳 execution 及其准确状态；旧结果不能附在新 running 后面 |
| followup_task | 新 Trigger 的 accepted/queued receipt，含 execution_id；重复请求幂等 |
| send_message | QueueOnly；idle 不启动模型，busy 在安全点应用 |
| steer_agent/interject_agent | 准入时固定目标 execution，下一安全点应用；idle/已终结返回 target_not_running，不能排到未来回合 |
| interrupt_agent | 准入时固定目标 execution，只取消当前捕获项；不能按晚到时的“当前任务”误伤下一项 |
| wait_agent(targets) | 调用开始捕获每 target 的 execution_id；完成时返回同组身份、状态和 outcome，不追赶最新任务 |
| wait_agent() | 等 caller 已提交且尚未消费的 mailbox 内容；已有 pending 内容立即返回；不只比较调用后的新 watermark |
| task/board | 保留业务幂等、所有者/epoch、引用可解析性、16KiB/分页限制，迁移到注入的聚合端口 |

wait 支持 ID/绝对/相对 path，通过唯一 scoped resolver 一次解析并去重；无 execution、未知目标或跨 root 明确报错。
删除 `wait_agent_snapshot(...).unwrap_or_default()` 和另一套字符串 filter_map 解析。
结果里 error 必含 code/message/retryable/source；list/wait 的读取故障不能变成成功空数组。

结构化 inter-agent 消息保留 author/recipient/delivery/reply_to；上下文投影按“外部 agent 输入”处理，不能把任务文本里的 `[SYSTEM]` 升格为 system 权限。
删除 `<qaqh_subagent_result>`、`[SUBAGENT ...]` 标签解析作为业务协议；UI 如保留人类可读标签，只由 typed 数据渲染。

安全点固定为工具 intent/result 闭合之后、下一 provider request 之前；Interject > Steer > Queue。
busy Trigger 不在安全点插入；所有 InputAccepted/ContextApplied 保留顺序和消费证据，不把准入当成模型已读。

## 11. 必删清单与替代所有者

删除操作与替代消费者在同一交付中完成。下表是验收清单，不是允许留下 adapter 的 TODO。

| 必删/退出项 | 替代位置或处理 |
| --- | --- |
| `qaqh-subagent::collect_subagent_result`、start/arm collector、SubagentTransport/HostTransport、CompletionRoute、CollectorBatch/Event | 统一 execution commit + 显式结果交付 effect |
| `SubagentHost` 的 spawn/start/subscribe/download/finish/close 生命周期接口 | typed application / AgentHandle / TaskSupervisor |
| `HOST/TASK_HOST/BOARD_HOST`、install_host/task/board_host 与全局查询 | daemon 实例装配，工具实例持限定端口 |
| `crates/qaqh-subagent` crate | 18 工具/DTO/有效 task-board 算法迁到实际所有者后，删除 crate、workspace member 与依赖；不空壳 re-export |
| host_impl 的 SSE→mpsc 桥、projection 终态翻译、缓存下载和轮询线程 | 只读 committed subscriptions 与 durable content 服务；无业务副作用 |
| `subagent_outcomes`、`armed_collectors`、prepare/mark/unmark collector、单 child finish 索引 | 统一 execution projection，历史分页读取 |
| `ActorKind::Subagent`、SubagentSpawnSpec、run_subagent_actor、spawn_subagent_inprocess/reload 专路 | 普通 SessionSpec + 唯一 open_or_resume |
| `SubagentSupervisor` 内 Running/Terminal/Joined 业务状态和装作已落盘的 trace | 统一 task/resource supervisor；graph 只读身份/ownership 投影 |
| `subagent_recovery` 的独立扫描/伪造 TurnStarted/按 child 最新终态补 parent | 统一 execution/effect recovery 与跨会话 receipt 对账 |
| `ToolEffect::SubagentSpawned` + `apply_subagent_spawn_effects` 的先造 actor 后补边流程 | parent-owned typed proposal / committed effect |
| `subagent.spawn` service method、旧 `ActionRequest::SubagentSpawn`、直接 registry spawn 公共捷径、QAQH_SUBAGENT_PERSIST | 唯一 create/delegate 应用用例；协议切换时一起删除导出、调用者和测试 |
| per-child 部分 model/base_url/key 继承及 reload 全局权限 fallback | 固定有效 binding + 显式策略/当前授权复核 |
| default 120s、silent clamp、collector SessionClose、agent exit_code | execution budget、typed terminal、独立 unload |
| 迁移链中的 as_system 注入分流、文本标签协议、只认布尔位的安全点 drain | 统一 typed mailbox + purpose/target/context 消费 |
| 迁移链上的 session TLS/global cancel、ambient tool manager/host 查询、from_legacy_ambient | 显式 SessionScope / 单向 CancelToken / 实例 ToolCatalog |
| 旧 loop outcome 镜像终态、messages/WAL/timeline 业务双写 | 唯一 SessionCommit、context/read projections；有效算法保留 |

禁止为了保住依赖方向把整个旧 lib.rs 搬到 runtime 新文件；工具适配、应用用例、状态、存储、执行监督必须按 §4 拆开。
超过 1500 行的旧文件本次只减不增，新 Rust 模块不超过 1000 行；模块体积不是靠拆同义 facade 达标。

## 12. 数据与客户端冷切换：不做在线双轨

### 12.1 旧数据不是随意删除的兼容代码

“不兼容旧运行时”不等于默默销毁用户历史。默认不清库、不重写旧 append-only 日志。
实施一个显式离线 import：旧根只读 → staging 当前 schema 根 → 验证 → 原子切换 active data root/格式标识。
旧格式 decoder 只允许在离线 import/既有迁移模块，不得链接进在线执行路径；不能 migrate-on-read。
迁移程序可作为独立发行工具保留以保护历史数据，**它不是旧在线 runtime**，也不能提供新会话运行 fallback。

### 12.2 切换步骤

1. 停止新准入，取消/收割全部 active work；无法停止则取消切换，不能在旧任务还写文件时起新 runtime。
2. 备份原数据根和格式标识，记录可回退边界；凭据独立处理，不复制入 transcript。
3. 导入 parent/child identity、原始模型 bundle、消息与工具关联、usage、任务板/消息板、答案与真实 terminal。
4. 旧缺失 execution 关联只在能唯一证明时转换。无法证明的 active work 记 interrupted/indeterminate；没有证据不补成功、TurnStarted 或模型任务。
5. 缺 blob、签名或 bundle 时输出具名导入报告；该会话留原根/仅导入可读历史，不宣告可 resume。用户另行决定，不静默截断或遗弃。
6. 校验全部引用、图无环/parent 一致、消息金标、模型请求保真和历史计数；记录源→目标 ID 映射，仅 importer 持有旧格式知识。
7. 使用新二进制与同批客户端打开 staging，完成恢复验证后切换；旧二进制不允许打开新格式写入。
8. 在线 writer 只认当前 schema，旧协议版本返回明确 upgrade_required。删除生产开关、双读/双写和旧入口。

新根一旦发生写入，不能直接切回旧备份并假装无数据丢失。回退必须停服保留新根、明确未覆盖数据，或经验证的反向导出；外部副作用不重放。
不提供自动清理原根的脚本；删除备份需用户独立授权。

## 13. 故障恢复与可观测性

启动/attach 在协作入口开放前完成根图、execution、pending input、未交付结果和配置引用恢复；list/send 不偷偷修数据。
所有 resident 初始 unloaded，不恢复虚假任务句柄或旧 incarnation；runner 是否仍活由宿主 supervision 对账。

| 崩溃位置/故障 | 必须恢复行为 |
| --- | --- |
| parent intent 已提交，child 未初始化 | 按原 create intent 幂等 materialize；绝不随机另建 child |
| child InputAccepted 已提交，ACK 丢失 | 重发同键返回原 receipt，只执行一次 |
| queued 未 started | 保留原 FIFO/预算；正常恢复可执行；若停服/close 已取消则明确终结，不伪造开始 |
| started 无 terminal | 先对账 runner/工具副作用；无安全证明时 interrupted/indeterminate，不盲重放 provider 或工具 |
| child terminal 已提交，parent 未收到 | 从原终态派生 delivery intent，补同一 receipt/context；重启两次不重复应用 |
| terminal commit Unknown | 按原 batch 对账，不能重跑模型/工具来“取个新结果” |
| actor panic/初始化失败 | supervisor 关该实例准入、取消/收割 owner work，出结构化 health 与恢复结果；不是日志后清空状态 |
| subscriber reset/disconnected | 重建观察 cursor；无任务取消、无 exit 0、无业务副作用 |
| blob 缺失/校验错误 | 精确 data_integrity_error，可查询阻塞/失败；不以空答案结算 |
| edge/parent/cycle 不一致 | 显式完整性错误并隔离该树；不临时注册成 root 掩盖 |

观测至少关联 root/session/input/turn/attempt/incarnation/parent owner；展示 queued、awaiting、commit_unknown、cancel_pending、stop_incomplete 及原因。
诊断是有界可关闭的旁路；禁用后业务结论一致。错误正文不只存在日志，必须进入持久 outcome 或明确不可提交的 health/receipt。
存储不可用时不能承诺事实已经落盘；独立安全控制通道仍可停止工作，恢复后按证据结算。

## 14. 实施任务与删除闸门

各任务一个聚焦 PR；可以在隔离开发分支逐项合并，但**不发布新旧双轨产品**。只有 UAR-09 通过才允许切换生产根与客户端。
UAR 统一上述 agentloop 任务的实际所有者，不再同时执行一套相同 AL 实现；UAR-00 建立映射，避免两队各写一个 loop。

| ID | 工作与依赖 | 完成证据 |
| --- | --- | --- |
| UAR-00 | 批准 ADR、刷新最新后端/SDK/UI 基线与脏文件、AL 任务映射、能力/格式清单，补架构登记 | 每个现行入口有去向；明确支持客户端/平台与冷切换边界 |
| UAR-01 | 统一 input/execution 身份、事实/state/receipt/reducer 合同，旧终态退出与离线导入 schema | 状态机/幂等/queued cancel/terminal conflict 反例，typed 错误 |
| UAR-02 | 唯一 SessionCommit + blobs + context fold；主代理真实 provider/tool 意图和结算接通 | 提交故障注入与原模型上下文/工具顺序金标保真，旧权威写链退出 |
| UAR-03 | 非阻塞 actor mailbox、可靠 completion、统一 supervisor 与单向取消树 | provider/tool 阻塞时 cancel/wait/delivery 可处理；无锁内 join、无遗失句柄 |
| UAR-04 | 唯一 SessionSpec/open_or_resume、实例 scope、策略和有效配置继承 | root/child 共用装配/恢复，NoApproval/权限/配置重载不扩权 |
| UAR-05 | committed create/delegate、FIFO followup、结构化结果交付与 scoped resolver/wait | 多轮、竞态、跨日志崩溃矩阵，原 collector 两红探针转绿 |
| UAR-06 | 迁移 18 工具/端口/注册，task-board 业务保真；删全局 host/crate/旧服务 | schema 与真实 handler/权限接缝，Cargo/SDK 依赖干净 |
| UAR-07 | 统一启动恢复、delivery 对账、idle/close/reap 与容量治理 | 重启两次幂等、残留资源可追踪、其他 session 继续工作 |
| UAR-08 | importer、格式/客户端版本拒绝、Rust/TS/UI 消费同步与迁移演练 | 旧根不改、staging 完整、旧版被拒、可回退边界明确 |
| UAR-09 | 删除全清单、全回归/平台/跨仓验收、切换发布 | 无在线旧入口/兼容开关/双权威；实际运行证据与完成报告 |

先将 §15 的核心行为写成能失败的红测试，再迁移所有者。编译失败不是红证据，手工发 fake completed 不是生产答案链验证。
禁止在 UAR-05 之前靠“新 API 仍调用旧 collector”对外宣告已交付。

## 15. 行为验收矩阵

确定性测试默认临时 SessionManager/实例 ports、虚拟 clock、可控 runner/barrier、本地 provider fixture；禁止真实用户历史/凭据与真实模型委派。
允许隔离的普通 actor fixture；**实施探索和审阅不得使用子代理**。新测试并行可运行，不依赖进程全局环境/OnceLock 重装或单线程执行才正确。

| ID | 必测操作/故障 | 验收结论 |
| --- | --- | --- |
| A01 | 同一测试分别配置 root/child，真实生产 provider 产出无工具答案 | 同一启动/执行/提交路径；逐字结果、answer fact 在 terminal 之前 |
| A02 | 多 blocks、Unicode、大于 64KiB、合法空答案 | 正文完整有序、空值有显式证据、reasoning 不泄漏 |
| A03 | 缺失/损坏 blob、下载失败 | 非成功结构化错误，绝不得到空答案成功；复现本次红探针 |
| A04 | 停 UI/SSE、subscriber reset/lag、关闭诊断 | 执行不受影响；wait 从 durable view 恢复，绝不因观察失败关 child |
| A05 | provider 401/断流、初始化失败、工具/actor panic | 原始 error 分类进入正确 execution，无“等到120s再说timeout” |
| A06 | provider/tool 阻塞，投递 wait/queue/cancel/结果 | mailbox 准入/控制不等长调用完成，无 self-RPC deadlock |
| A07 | 第一次 completed → followup blocked → failed → 第三次 completed | 同 ID/path、每次 execution 正确，wait 无旧结果，parent 每轮一次 |
| A08 | busy 连续 followup、队列满、发送 ACK 丢失/重复 | 有界 FIFO、超限明确拒绝、同 input 只执行一次 |
| A09 | 旧 terminal/completion/delivery/unload 与新 followup 精确交错 | 旧代只结算旧 execution，不能关闭/覆盖新任务 |
| A10 | wait 捕获后新 followup 到达；相对 path/ID/未知/跨root | wait 只等原快照，resolver 一致，错误不变空数组 |
| A11 | idle queue 后 Trigger；busy steer/interject/queue | 首个合法请求可见，优先级正确，tool_use/result 不拆散，steer 不泄到下回合 |
| A12 | 取消、完成、deadline 同时到达 | 一个 terminal、正确原因、晚到无副作用，不清旧token启动新任务 |
| A13 | 执行超过120s但未设置deadline；排队超过显式budget | 前者不中断，后者尚未开始不消耗运行预算 |
| A14 | provider/tool/execution/wait 四种超时 | 原因各自准确；wait timeout不取消目标，execution timeout无actor自动close |
| A15 | blocking工作拒绝取消、stop宽限到期 | Indeterminate/stop_incomplete可查、句柄/配额保留，其他session仍可用 |
| A16 | child运行时parent idle；parent显式cancel/close | idle不连带取消；cancel按owner、close按树，queued逐项终结 |
| A17 | complete/interrupt/idle-unload后同child followup | 历史与provider bundle/前缀恢复保真，权限/工具不扩张 |
| A18 | 父非全局profile、child具名覆盖、权限收紧/凭据失效 | 绑定自洽、无全局fallback/密钥泄漏、当前授权复核 |
| A19 | All/Only/空Only/未知工具/NoApproval/MCP | 暴露与授权区分，skill补充不改All语义，deny不挂审批 |
| A20 | spawn在parent commit/child create/input accept/ACK各点失败 | 无未提交活actor、同ID恢复、reservation可对账、不伪造rollback |
| A21 | child终态后parent接收各点崩溃，重启两次 | 结果可读、receipt/context幂等，无模型任务重跑 |
| A22 | queued/started/open工具/Unknown提交/父子图异常后重启 | 正确恢复分类，不补造started/success，不丢子归属 |
| A23 | registry close与工具回调/parent delivery并发 | 无锁内IO/send/join，停止超时不冻结registry，不误认joined |
| A24 | 18工具经真实注册、真实注入端口、真实权限执行 | schema/default/range/返回错误一致，task/board持久业务保真 |
| A25 | 旧数据离线导入、缺证据/缺blob/损坏、切换后恢复 | 原根不改；可恢复与仅历史明确区分；无在线旧格式decoder |
| A26 | 旧协议客户端、新SDK/UI、重连/历史/实时结果 | 未支持版本具名拒绝，新端结构化错误/答案/状态相同 |
| A27 | 固定并发与大结果，反复创建/重载/订阅drop | 内存/线程/句柄/队列有界，history分页、无collector线程线性残留 |

不可替代的端到端闸口：A01、A07、A09、A17、A20、A21。helper单测、源代码字符串断言或mock正文读取不能替代。
既有取消保留真实工具结果、参数容错、文件journal、restart_request_body/Responses/prefix、ask/permission/plan测试迁归属后继续通过。
可用 grep/依赖图做“旧符号删除”附加检查，但它不能证明业务性质。

### 15.1 验证命令与诚实报告

实施时按实际测试文件记录准确命令，不以本文建议命令冒充已运行：

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace -- --test-threads=1
```

另加新建的 unified runtime、delegation、recovery、migration 集成测试，以及 Rust/TS SDK 导出和所有受影响客户端测试/构建。
旧 crate 删除后不再运行 `-p qaqh-subagent` 作为新验收；相关行为测试必须有新归属。
缺依赖/平台/网络就列“未验证”和准确阻塞，不能用 cargo check 代替端到端。新并发测试还必须使用默认并行方式单独运行。

## 16. 完成定义与交付报告

全部满足才可标“完成”：

1. 主/子 agent 同一装配、恢复、execution、commit、停止和结果所有者；差异只在数据/策略。
2. §11 所列本域旧链全部退出生产；`qaqh-subagent` crate、collector、旧host/global槽、旧spawn服务和兼容开关已删除。
3. 所有已接纳执行有可恢复状态/真实终态；没有空答案成功掩盖、按会话误去重或观察流决定任务。
4. 旧数据经显式导入保留，在线只认当前格式；旧客户端按发布矩阵拒绝，支持的消费者全部升级。
5. A01～A27与有效既有回归有真实结果；全部未验证/失败显眼列出，不自行批准豁免。
6. 架构文档、wire/schema/SDK、配置与提示词同步；没有“将来删除”的无期限桥。

交付 `docs/report-unified-agent-runtime.md`：逐项列UAR任务、删除符号/调用者、事实与所有者、红→绿、并发/恢复/迁移证据、跨仓提交、平台、命令退出码、性能/容量对比与剩余阻塞。
报告不能把本文的设计句子改成完成时态来代替运行证据。
