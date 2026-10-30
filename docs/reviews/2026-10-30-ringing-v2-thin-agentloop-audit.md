# Ringing v2 / thin-agentloop 硬切换问题审查报告

审查日期：2026-10-30。

审查基线：提交 42e62f4「埋葬timeline,移除庞大agentloop状态机」，以及审查时工作区尚未提交的会话初始化修复。工作区原有 responses_sdk.rs 修改保留。

本报告合并两轮审查：一轮关注删除后的可用性、残留实现和集中模块；另一轮关注既定 Ringing v2 与 thin-agentloop 架构下的职责、所有权、类型和异步执行问题。依据是代码、提交差异及本地已观测行为，项目设计文档不作为实现正确性的依据。报告只记录问题、影响和代码位置，不包含测试实施方法或重构施工方案。

## 1. 总体判断与证据标记

旧 Loop、turn_lap 及 timeline 主体、相关路由的删除已经发生。当前问题并非旧主循环仍完整运行，而是新执行链尚未承接完整生产能力，同时旧状态、读写路径和适配层仍大量存在。

当前代码可以通过 workspace 编译检查，新 actor 的简单模型与工具往返可以运行；这不足以说明桌面工作闭环成立。会话展示、生产配置与工具装配、连续工具对话、未完成回合恢复等链路存在确定缺口。目前不能认定可以正常开工。

证据标记：

- **已观察**：本地直接观察到对应行为，确认所述场景存在；不表示所有配置都必然出现。
- **代码确认**：调用路径或数据结构直接显示问题，尚未对全部用户场景做端到端观察。
- **条件性风险**：代码允许有问题的执行顺序或状态，影响取决于调度、输入或异常条件；不表述为已复现事故。

优先级含义：P0 为直接阻断关键工作闭环；P1 为正常持续工作、恢复或执行语义中的严重问题；P2 为维护、规模、边界和残留问题。架构问题与可用性问题可能有共同根因，不应按条目数量理解为相互独立的缺陷数。

## 2. 已定位并在工作区修正的初始化问题

### U00：创建会话使用旧存储，启动 actor 使用新存储

**状态：工作区已有修复；不代表已部署，也不代表旧目录已经迁移。**

原 session.new / session.create 会先通过 CanonicalSessionStore 写入 SessionCreated，生成旧 events 文件；随后 registry 又用 SessionCommit / BatchJournal 在同一目录创建新存储，触发「batch journal requires an isolated data root」。原 spawn_new 还会强制创建已有 journal。

工作区修复已将初始化首条事实统一到 SessionCommit，并让已有 storage-format 的目录走 open。该修复只解除初始化格式冲突，不能解决下面的读链、展示、生产装配或恢复问题。此前已经落成旧格式的会话目录不会因此自动恢复可用。

位置：[service.rs](../../crates/qaqh-runtime/src/service.rs#L1446)、[registry.rs](../../crates/qaqh-runtime/src/registry.rs#L1306)。

## 3. 影响实际使用的问题

### U01：新 journal 的写入与 bootstrap / SSE 的读取没有接通

**P0；已观察。**

新 actor 写 committed-batches，但 V2ProjectionHub.resolve_identity 仍要求 events.commit.json；load_session_state 和 replay_after 仍用旧 CommittedFactReader。已提交首条事实、head 为 1 的 batch 会话，bootstrap 仍返回 SnapshotMissing。Electron 的 attach / watchSession 使用这条 bootstrap 与重放链路。

影响：初始化 502 解除后，新会话仍可能无法建立展示流，重连、重放和状态读取也无法以新 journal 为依据。

位置：[v2.rs](../../crates/qaqh-runtime/src/ringing/v2.rs#L918)、[load_session_state](../../crates/qaqh-runtime/src/ringing/v2.rs#L938)、[replay_after](../../crates/qaqh-runtime/src/ringing/v2.rs#L975)、[Electron ringing-client.ts](../../../qaqh-electron/src/main/ringing-client.ts#L73)。

### U02：模型响应没有生成展示正文所需事实，流式进度被丢弃

**P0；代码确认。**

prepare_response_commit_facts 主要提交 ProviderResponseCommitted 及工具声明；新 actor / provider 路径没有生成 AssistantBlockSealed。投影注册表明确不给 ProviderResponseCommitted 和 ContextMessageCommitted 分配公开展示 slot，ConversationProjection 的助手正文依赖 AssistantBlockSealed。

provider runner 同时忽略 ContentDelta、ReasoningDelta、ToolCallProgress、NativeToolProgress 等流式事件。

影响：即使修好 journal 读取，仍缺少正常答案和推理正文的展示来源；流式文本及工具进度也没有接入前端。

位置：[provider_job.rs](../../crates/qaqh-runtime/src/provider_job.rs#L201)、[run_spec](../../crates/qaqh-runtime/src/provider_job.rs#L315)、[projection_slots](../../crates/qaqh-session/src/session_fact_v2/projection.rs#L48)、[ConversationProjection](../../crates/qaqh-session/src/projection/conversation.rs#L167)。

### U03：生产 provider、工具和上下文装配不完整

**P0；代码确认。**

registry 的主会话与子代理启动路径固定构造 Chat provider。ProviderRequestSpec.new 默认 tools=None、effort=None，生产入口没有补齐工具定义和 effort。主会话构建 ToolManager 后也没有像子代理路径一样执行 apply_init。

新会话上下文从空 ContextProjector 开始，start_turn 只提交用户文本；旧 AgentState.build_context 中的 system prompt、环境、技能、AGENTS、MCP 环境等装配没有接入新 actor。这里的 AGENTS 指产品生成模型请求时的项目指令注入，不是本次审查工具自身读取仓库规则的行为。

影响：已配置的协议、兼容参数、工具模式和推理参数不能完整进入生产请求；执行器注册了工具，模型请求却未必声明这些工具。已有旧上下文算法的存在不能证明新会话会使用它们。

位置：[spawn_session_inprocess](../../crates/qaqh-runtime/src/registry.rs#L1283)、[spawn_subagent_inprocess](../../crates/qaqh-runtime/src/registry.rs#L1151)、[ProviderRequestSpec](../../crates/qaqh-runtime/src/provider_job.rs#L29)、[start_turn](../../crates/qaqh-runtime/src/session_agent/agent.rs#L171)、[旧 AgentState](../../crates/qaqh-runtime/src/agent/state/agent.rs#L154)。

### U04：同一会话再次工具续轮时复用 batch ID

**P1；已观察。**

工具结束与 ask 回答后的续轮使用 batch-r{round}-next，未包含 turn 身份。同一 actor 连续完成两轮带工具的对话时，第二轮再次使用 batch-r2-next，journal 返回「batch id reused with different content」，actor 退出。

影响：单次工具往返成功不能延伸为同一会话可持续使用工具。

位置：[ask 续轮](../../crates/qaqh-runtime/src/session_agent/agent.rs#L562)、[工具续轮](../../crates/qaqh-runtime/src/session_agent/agent.rs#L1082)、[batch 重用校验](../../crates/qaqh-session/src/canonical/batch.rs#L455)。

### U05：持久化未完成义务没有接入 actor 恢复

**P1；已观察。**

SessionCommit.open 能折叠未完成 turn、round、tool 和 interaction，并提供 obligations；SessionAgent.new 却总是设置 Ready、清空 active_turn 和各执行句柄，没有按已恢复状态初始化。

已观察到：provider 请求中关闭 actor 后，重开 journal 仍有 active_turn；新 actor 收下一条输入时发生「turn already active」并退出。

影响：关闭、重启或异常中断后，会话可能无法继续工作，尚未结算的外部工作也没有得到完整接管。

位置：[SessionCommit.open](../../crates/qaqh-session/src/session_commit.rs#L209)、[obligations](../../crates/qaqh-session/src/session_commit.rs#L283)、[SessionAgent.new](../../crates/qaqh-runtime/src/session_agent/agent.rs#L90)。

### U06：持久化身份使用可重置计数器和毫秒时间

**P1；代码确认。**

event ID 从进程内固定值 100 开始生成，turn、部分 execution 和 batch 身份由它派生。进程重启后生成序列重复，可能与已有会话记录冲突。InputId 仅使用当前毫秒时间，同毫秒提交也可能产生相同身份。

影响：重启后的新工作和短时间内多次输入可能被当成重复工作，或在提交时失败。进程内唯一不能替代持久化身份唯一。

位置：[next_event_id](../../crates/qaqh-runtime/src/session_agent/agent.rs#L47)、[submit_user_input](../../crates/qaqh-runtime/src/session_agent/host.rs#L47)。

### U07：部分命令被静默忽略，命令元数据在适配中丢失

**P1；代码确认。**

registry.send_ringing 未匹配的命令直接返回 Ok；ToolPermissionRespond 进入 actor 后也直接返回 Continue。ConversationCancel 丢弃传入 turn_id，调用 cancel(None)。消息转成 AgentCommand 时主要保留文本、附件引用和 command_id，原 worker envelope 的 actor、expected_revision、inter_agent 等信息没有完整进入新 actor 的业务判断；InputAccepted 直接构造 User / UserText / TriggerTurn。

影响：入口成功与业务执行脱节；定向取消可能扩大成当前回合取消，部分交互、配置更新和代理间输入语义不能按原命令表达执行。

位置：[send_ringing](../../crates/qaqh-runtime/src/registry.rs#L1563)、[通配成功分支](../../crates/qaqh-runtime/src/registry.rs#L1655)、[权限答复分支](../../crates/qaqh-runtime/src/session_agent/agent.rs#L594)、[InputAccepted 构造](../../crates/qaqh-runtime/src/session_agent/agent.rs#L185)。

### U08：生产执行策略固定，未采用完整配置和子代理约束

**P1；代码确认。**

ToolPolicySnapshot.workspace_write 固定 Code 模式、WorkspaceWrite 权限、Main sandbox 和空 trusted_dirs。registry 主会话和子代理都使用这一构造，子代理 spec.permission_level 未实际用于该策略装配。

影响：配置中的只读、计划模式、权限档位、信任目录和子代理身份约束不能可靠传入真实执行；不同配置可能被执行成相同策略。

位置：[ToolPolicySnapshot](../../crates/qaqh-runtime/src/tool_job.rs#L95)、[主会话策略装配](../../crates/qaqh-runtime/src/registry.rs#L1354)、[子代理策略装配](../../crates/qaqh-runtime/src/registry.rs#L1234)。

### U09：actor 内复制的工具 runner 丢失执行和结算语义

**P1；代码确认。**

actor 没有使用已有 tool_job.run，而是在 async task 内直接调用同步 port.execute。它重新拼接执行时的 execution_id，与已提交 ToolIntent.execution_id 不一致；所有错误都归成 NotStarted，错误输出被丢弃；成功结果中的 backgrounded 标志没有保留，随后按成功终态结算。

ToolIntent 还先固定记录 Allow、rule_id=auto 和占位 sandbox hash，真正权限判断却在执行器中发生。

影响：执行身份、授权记录和实际行为不一致；已产生副作用但结果不确定的失败会被错误描述为未启动；后台工作可能被提前视作结束。同步执行同时占用异步执行线程。

位置：[actor 工具派发](../../crates/qaqh-runtime/src/session_agent/agent.rs#L795)、[直接执行与结果转换](../../crates/qaqh-runtime/src/session_agent/agent.rs#L860)、[已有 tool_job.run](../../crates/qaqh-runtime/src/tool_job.rs#L502)。

### U10：取消与迟到 completion 的处理不完整

**P1；代码确认及条件性风险。**

取消发送后，actor 清空活跃工具身份并尝试直接提交 TurnInterrupted；CommitState.finish_turn 要求未完成工具和交互先结算，因此带未结算工作的取消路径可能提交失败并退出。

provider completion 主要依据当前 active_turn 是否存在继续处理，没有完整核对其是否属于当前期待的 provider identity。取消后立即开始新回合时，旧 provider 的迟到结果存在作用到新回合的风险。工具身份被清空后，迟到工具 completion 也可能变成 unrecognized tool completion 错误。

影响：请求取消、确认工作停止和持久化结算被混在一起，取消期间的回合连续性与结果归属不可靠。

位置：[CancelTurn](../../crates/qaqh-runtime/src/session_agent/agent.rs#L373)、[completion](../../crates/qaqh-runtime/src/session_agent/agent.rs#L598)、[finish_turn](../../crates/qaqh-session/src/commit_state.rs#L427)、[EffectIdentity 比较能力](../../crates/qaqh-agentloop-core/src/lib.rs#L75)。

### U11：关闭和失败退出会丢掉恢复所需的运行时 owner

**P1；代码确认。**

Shutdown 直接返回 Stop，没有完成活跃回合和外部任务的结算。AgentHost 将 DriverFailure 简化成 failure.error，把原本携带的 handler、mailbox 和失败输入丢掉；正常返回的 handler 也直接丢弃。Supervisor.Drop 只 abort async handle，不能据此证明已经启动的外部 blocking 工作停止。

影响：本来保留了恢复材料的底层 driver，在生产 host 处失去这些材料；未完成工作可能继续存在，但会话 owner 已退出。

位置：[Shutdown](../../crates/qaqh-runtime/src/session_agent/agent.rs#L369)、[AgentHost](../../crates/qaqh-runtime/src/session_agent/host.rs#L145)、[DriverFailure](../../crates/qaqh-agentloop-core/src/driver.rs#L29)、[Supervisor.Drop](../../crates/qaqh-agentloop-core/src/supervisor.rs#L126)。

### U12：completion 通知有竞态，持久化结算前已经 acknowledge

**P1；条件性风险及代码确认。**

任务在返回前发送 done 通知，collector 随即调用 collect；collect 要求对应 JoinHandle.is_finished 已成立。若通知先被处理，collect 返回 NotFinished，调用方不重试，completion 可能永久不投递。该调度竞态尚未作压力复现。

actor 收到 completion 后，先 acknowledge 并释放 supervisor 中的结算材料，随后才进行持久化业务提交。若提交失败，先前的任务证据和恢复 owner 已经释放。

影响：回合可能一直等待已完成的任务；结果结算失败后缺少完整的在途材料。

位置：[dispatch_task](../../crates/qaqh-runtime/src/session_agent/agent.rs#L150)、[collect](../../crates/qaqh-agentloop-core/src/supervisor.rs#L282)、[completion 开头](../../crates/qaqh-runtime/src/session_agent/agent.rs#L598)。

### U13：上下文容量存在硬终点，压缩生产链未接入

**P1；代码确认。**

生产 ContextBudget 固定 max_messages=64、payload 上限 4MB。新 actor 没有生成 context checkpoint 或接入压缩的生产路径，超过预算会在上下文折叠时失败，并沿当前错误处理路径退出。

影响：持续会话存在固定的消息或正文容量终点；有工具和多轮响应的会话会更快耗尽消息数。

位置：[registry ContextBudget](../../crates/qaqh-runtime/src/registry.rs#L1300)、[ContextProjector](../../crates/qaqh-session/src/context_projection.rs#L164)、[旧压缩模块](../../crates/qaqh-runtime/src/agent/plugins/engine_compact.rs)。

### U14：繁忙期间输入进入无界内存队列，尚未持久化

**P1；代码确认。**

非 Ready 状态收到 UserInput 时，actor 将正文和附件放入 queued_inputs；QueueInput 也直接进入该队列。该 VecDeque 没有容量上限，排队时不提交 InputAccepted。只有开始执行下一回合时才建立持久化输入事实。

影响：mailbox 的有界限制不能约束 actor 内部排队总量；重启会丢失尚未轮到执行的输入，先前的入队成功不代表输入已持久化。取消和异常分支也没有统一推进队列的规则。

位置：[queued_inputs](../../crates/qaqh-runtime/src/session_agent/agent.rs#L86)、[command](../../crates/qaqh-runtime/src/session_agent/agent.rs#L325)。

### U15：activity 与空闲卸载维护链路断开

**P1/P2；代码确认。**

新实例启动时调用 activity.begin，但原有 observe 更新链没有接入新 actor。新实例的 liveness 固定为 None，unload_idle_sessions 又只处理具有 liveness 的实例。is_running 只检查实例表是否有键，不等同于 task 仍健康运行。

影响：状态查询可能停留在 Starting，空闲卸载不生效；实例登记和实际 task 存活可能分离。

位置：[实例装配](../../crates/qaqh-runtime/src/registry.rs#L1370)、[空闲卸载](../../crates/qaqh-runtime/src/registry.rs#L2030)、[is_running](../../crates/qaqh-runtime/src/registry.rs#L2160)、[activity.begin](../../crates/qaqh-runtime/src/activity.rs#L74)。

### U16：子代理图、父子身份与结果恢复仍依赖旧读链

**P1；代码确认。**

agent_graph.read_canonical_facts 仍通过旧 CommittedFactReader 加载会话事实；registry.canonical_parent_session_id 仍依据旧 events 文件是否存在判断是否有 canonical 基线。batch 会话没有这些旧文件，可能被当作只有 metadata 的根会话。

影响：父子关系、恢复提示和代理状态不能完整从新 journal 重建。子代理模块存在并不表示新 actor 与其恢复、结果收集链已经完整接通。

位置：[agent_graph.rs](../../crates/qaqh-runtime/src/agent_graph.rs#L51)、[canonical_parent_session_id](../../crates/qaqh-runtime/src/registry.rs#L837)、[host_impl.rs](../../crates/qaqh-runtime/src/host_impl.rs)。

### U17：入口命令回执与实际输入、业务终态缺少持久化关联

**P1；代码确认。**

HTTP 命令入口在 worker 入队后 mark_running；PendingCommandStore 是独立的 best-effort 持久化回执表，保留期为 300 秒。其业务终态依赖 projection event 的 causation_id 折叠，而新 actor 构造的主要事实将 causation_id 设为 None，只把 command_id 放进 client_request_id。

影响：入队、输入接受、业务结束由不同位置维护，commandStatus 的结果可能长期无法随真实回合结束更新。独立回执缓存不能充当已接受输入的持久化依据，也不提供崩溃安全的 exactly-once 保证；现有代码本身也将其声明为 best-effort。

位置：[HTTP 入队与回执](../../crates/qaqh-daemon/src/axum_server/axum_impl/command.rs#L629)、[PendingCommandStore](../../crates/qaqh-runtime/src/ringing/pending_store.rs#L95)、[终态折叠](../../crates/qaqh-runtime/src/ringing/pending_store.rs#L407)、[输入事实](../../crates/qaqh-runtime/src/session_agent/agent.rs#L185)。

## 4. 架构异味与 Rust 约束缺口

本节所述「不符合 Rust 设计」指关键约束未由所有权、类型或线程边界表达，不等于发现了内存不安全。Arc、Mutex、Clone、serde_json、状态机和事件日志本身都不是问题；问题在于它们在本项目中的组合和职责位置。

### A01：单 owner 内部又建立共享可变调度中心

actor 持有 Arc<Mutex<Supervisor>>，多个后台 collector 同时修改它。正常锁竞争被 try_lock 映射为致命业务错误。单 owner 的状态修改权被重新分散，随后又需要靠锁和身份 map 协调。

它直接关联 U12 的通知竞态，也增加取消、失败和资源释放的时序组合。代码量小不能证明这套抽象简单。

位置：[SessionAgent.supervisor](../../crates/qaqh-runtime/src/session_agent/agent.rs#L79)、[dispatch_task](../../crates/qaqh-runtime/src/session_agent/agent.rs#L131)。

### A02：合法状态由多个可独立修改的字段拼接

SessionAgent 同时保存 phase、active_turn、active_input、current_round、current_attempt、provider identity、tool identities 和 pending_ask_id；ActivePhase 内还重复保存 round、attempt、active_calls 或 interaction_id。CommitState 另外保存已提交业务状态。

类型允许 Ready 与 active_turn=Some 同时出现，也允许 AwaitingUser 与 pending_ask_id=None 同时出现。调用者必须人工维持这些字段的一致性。运行时句柄与持久化业务状态确实具有不同用途，但当前结构没有明确表达两者的对应约束。

位置：[SessionAgent](../../crates/qaqh-runtime/src/session_agent/agent.rs#L70)、[ActivePhase](../../crates/qaqh-runtime/src/session_agent/types.rs#L107)、[CommitState](../../crates/qaqh-session/src/commit_state.rs#L77)。

### A03：资源凭证在可失败操作前被消耗，错误不返还工作

Supervisor.start 先从 authorized 中移走 intent，然后才检查 runtime、slots 分配和 semaphore 容量。失败时只返回 SupervisorError，调用者拿不回未启动工作。

actor 也在提交意图之后才调用 try_reserve_completion。每个任务预留 60,000 字节，总 completion 预算 262,144 字节，实际最多允许四个同时保留的任务；supervisor 的名义上限却是 16。第 5 次预留被拒绝的行为已经观察到。

问题不是限制并发，而是多个容量合同不一致，容量失败又发生在已持久化意图之后，并沿失败退出路径丢失 owner。

位置：[Supervisor.start](../../crates/qaqh-agentloop-core/src/supervisor.rs#L233)、[LaneBudget](../../crates/qaqh-agentloop-core/src/mailbox.rs#L19)、[completion 预留](../../crates/qaqh-runtime/src/session_agent/agent.rs#L140)。

### A04：SessionCommit 同时承担存储、业务状态、模型上下文和展示发布

SessionCommit.commit 同时校验业务转换、复制并更新模型上下文、写 journal、更新 ProjectionSet、构造 projection events，并调用进程级全局 sink。V2ProjectionHub 又保存另一份 ProjectionSet，兼管磁盘加载、重放、overlay、状态查询和广播。

提交后的投影事件构造失败被 if let Ok 吞掉；ProjectionSink.publish 没有可返回的结果，加载失败只记日志。由此，持久化成功与展示可恢复性之间的缺口不会明确反馈给提交调用方。

位置：[SessionCommit.commit](../../crates/qaqh-session/src/session_commit.rs#L326)、[全局 ProjectionSink](../../crates/qaqh-session/src/projection/sink.rs#L12)、[V2ProjectionHub.publish](../../crates/qaqh-runtime/src/ringing/v2.rs#L792)。

### A05：回放和快照的重工作仍处于共享状态锁路径中

V2ProjectionHub.subscribe 持有 session state 锁注册 live receiver，并在持锁期间调用 replay_after；后者读取和折叠完整事实历史。bootstrap 在锁内克隆全部投影快照。首次 session_for 还会加载完整会话。

影响：订阅与恢复成本随历史增长，磁盘和大量分配工作会阻塞同一 session 的发布与查询。按实例加锁并未消除锁内重工作的延迟问题。

位置：[subscribe](../../crates/qaqh-runtime/src/ringing/v2.rs#L661)、[bootstrap](../../crates/qaqh-runtime/src/ringing/v2.rs#L238)、[session_for](../../crates/qaqh-runtime/src/ringing/v2.rs#L760)。

### A06：同步决策接口没有隔离同步 IO 和工具执行

InputHandler.command / control / completion 是同步方法；生产 handler 内直接执行 journal 提交、blob 访问、上下文序列化及哈希。工具任务内又同步执行实际 handler。

影响：接口形式上没有持锁跨 await，不代表执行器不会被阻塞。磁盘同步屏障、较大上下文计算或慢工具仍占用 Tokio 执行线程，当前主循环的类型边界没有表达这些工作的位置与时限。

位置：[InputHandler](../../crates/qaqh-agentloop-core/src/driver.rs#L19)、[start_turn](../../crates/qaqh-runtime/src/session_agent/agent.rs#L171)、[工具同步执行](../../crates/qaqh-runtime/src/session_agent/agent.rs#L876)。

### A07：工具执行存在两套 runner 和大量可矛盾的结果字段

tool_job.run 已有独立的执行与分类逻辑，生产 actor 又维护一份不同实现。ToolOutcome 用 evidence、terminal:Option、error:Option 和 output 表达结果，ToolOutput 再用 backgrounded:bool 表示仍有外部工作。类型允许「未启动但成功终态」等语义矛盾组合。

工具进入内核后仍以字符串重新构造 execution_id、按名称特判 ask，授权事实与执行期授权也分离。ask / permission / plan 没有统一接到可靠的交互与恢复边界。

位置：[ToolOutcome](../../crates/qaqh-runtime/src/tool_job.rs#L78)、[ToolOutput](../../crates/qaqh-runtime/src/tool_job.rs#L45)、[已有 runner](../../crates/qaqh-runtime/src/tool_job.rs#L502)、[actor ask 分支](../../crates/qaqh-runtime/src/session_agent/agent.rs#L719)。

### A08：显式上下文进入后又回到全局与线程本地隐式输入

ToolCallContext 被传入后，ToolExecutionScope 再将其安装到 RUNTIME_CTX、ACTOR_TOOL_MANAGER、AGENT_MODE、线程 workspace、sandbox 与 folding policy；深层函数随后从 ambient accessor 重新读取。with_manager 在缺少 actor manager 时还可回退到进程 manager。

影响：函数签名无法完整表达真实输入，调用方上下文和实际执行上下文存在分叉可能，初始化顺序与线程位置继续影响业务行为。

位置：[ToolExecutionScope](../../crates/qaqh-workspace/src/runtime.rs#L91)、[with_manager](../../crates/qaqh-workspace/src/runtime.rs#L404)、[execute_authorized_with_context](../../crates/qaqh-workspace/src/execution.rs#L90)。

### A09：恢复线程本地状态的 guard 没有线程亲和约束

**类型性质已确认；跨线程错恢复属于条件性风险。**

ToolExecutionScopeGuard 的 Drop 会恢复创建线程的 TLS 状态，但该类型实际满足 Send，可以被移动到另一线程。当前主要使用处是同步作用域，未据此认定已经发生跨线程串会话；接口本身却没有阻止错误用法。一旦跨 await 迁移或跨线程移动，析构可能恢复错误线程的上下文。

位置：[ToolExecutionScopeGuard](../../crates/qaqh-workspace/src/runtime.rs#L173)。

### A10：每次工具调用额外创建取消轮询线程，panic 路径缺少退出保证

WorkspaceToolPort.execute 使用 thread::scope 派生线程，每 5ms 轮询 done/cancel。done 在 handler 正常返回后才设置。

若 handler panic 且取消未触发，done 不会被设置，scoped thread 可能一直循环，而 scope 在异常退出时仍需等待它结束。这是代码推导出的条件性阻塞风险，未表述为已复现死锁。

影响：异常工具可能阻塞清理和关闭；正常调用也承担额外线程与轮询成本。

位置：[tool_job.rs](../../crates/qaqh-runtime/src/tool_job.rs#L379)。

### A11：手写内存计量在包装层失真

**已观察。**

ProviderOutcome 自身按正文和其他数据估算占用，但 AgentOutcome 对 provider 重新按「内容块数 × 128」计算，忽略正文长度及部分数据。包含 2,097,152 字节正文时，ProviderOutcome 估计 2,099,001 字节，包装后的 AgentOutcome 只报告 632 字节。

AgentCommand 的计量也未完整计入附件集合、各标识和可选请求字段。队列预算只覆盖 Frame，不能覆盖随后转入 actor 内部无界 queued_inputs 的数据。

影响：名义字节预算不能可靠限制实际保留内存。

位置：[AgentOutcome::retained_bytes](../../crates/qaqh-runtime/src/session_agent/types.rs#L80)、[AgentCommand::retained_bytes](../../crates/qaqh-runtime/src/session_agent/types.rs#L31)、[ProviderOutcome::retained_bytes](../../crates/qaqh-runtime/src/provider_job.rs#L179)。

### A12：每次提交复制累积历史，查重和恢复成本缺少可持续上界

SessionCommit.commit 每次克隆 ContextProjector 和 CommitState，commit_with_effect 又先克隆业务状态并复制模型请求。CommitState 的历史 input / turn 集合与 tool / interaction map 持续保留已完成记录；journal 维护全量 receipt 和 event ID 内存索引，并以 100,000 项硬上限拒绝继续增长。

已有 batch ID 的查询会通过 read_batch 从 journal 开头查找对应批次；open 和多处重放也扫描完整已提交历史。

影响：提交复制量与恢复、查重成本随会话历史增长；在累积历史上反复全量复制会产生接近平方级的总工作量，硬上限最终表现为会话停止接受新事实。当前不是只有活跃工作影响内存与提交成本。

位置：[SessionCommit.commit](../../crates/qaqh-session/src/session_commit.rs#L335)、[commit_with_effect](../../crates/qaqh-session/src/session_commit.rs#L403)、[CommitState](../../crates/qaqh-session/src/commit_state.rs#L77)、[BatchJournal 索引上限](../../crates/qaqh-session/src/canonical/batch.rs#L35)、[read_batch](../../crates/qaqh-session/src/canonical/batch.rs#L608)。

### A13：领域身份和错误在内部退化成字符串

EffectIdentity、ToolRequest、多个 registry / host 接口仍用 String 传递不同种类的 session、input、execution 和 effect 身份。业务 SessionId 与 Ringing client lease 的 ClientSessionId 在部分适配位置缺少类型区分；batch ID 则由各业务分支自行拼接。

大量已知内部错误变成 String，再到上层统一映射、记录或忽略。注册、启动、派发等位置还存在 unwrap_or_default 兜底与通配成功分支，使缺配置、缺支持或上下文缺失不一定在边界被清楚拒绝。

影响：编译器无法阻止不同身份混用，调用者也无法按错误类型做可靠的恢复判断。外部工具自由 JSON 和面向用户的最终错误文本有合理用途，问题在于内部已知领域约束也被抹平。

位置：[EffectIdentity](../../crates/qaqh-agentloop-core/src/lib.rs#L37)、[ToolRequest](../../crates/qaqh-runtime/src/tool_job.rs#L33)、[registry](../../crates/qaqh-runtime/src/registry.rs#L1283)、[命令适配](../../crates/qaqh-daemon/src/axum_server/axum_impl/command.rs#L454)。

### A14：存储层反向依赖执行框架，全局投影进一步扩大耦合

qaqh-session 为使用 ActorIdentity / EffectIdentity 依赖 qaqh-agentloop-core，而该 core 同时包含 Tokio mailbox、driver 和 supervisor。存储与上下文层因此通过身份类型绑到执行框架。SessionCommit 再通过全局 ProjectionSink 连到 runtime 的投影 hub。

影响：身份定义、持久化、执行监督和传输展示的变动相互牵连，单独理解或装配 session store 的成本上升。这里不存在已确认的 Cargo 依赖环，问题是职责层级与依赖边界不清。

位置：[qaqh-session/Cargo.toml](../../crates/qaqh-session/Cargo.toml)、[CommittedExecution](../../crates/qaqh-session/src/session_commit.rs#L83)、[ProjectionSink](../../crates/qaqh-session/src/projection/sink.rs)。

### A15：Registry 是集中编排与阻塞等待的共同入口

AgentRegistry 同时承担实例登记、生产装配、命令映射、父子图、配额、订阅、状态和卸载。QaqhService 通过全局 registry mutex 调用它；关闭过程在持有该锁的生命周期内进行等待，wait_until_stopped 最长轮询 30 秒。

将调用放进 spawn_blocking 能避免占用 async worker，但不能解除 registry 锁对其他 session 操作的串行阻塞。旧 thread / reader 字段与关闭分支仍与新 ThinAgent task 混存。

影响：一个会话的慢启动、扫描或关闭可能扩大为整个 registry 的延迟，生命周期与业务装配也难以独立判断。

位置：[AgentRegistry](../../crates/qaqh-runtime/src/registry.rs#L321)、[finish_for_unload](../../crates/qaqh-runtime/src/registry.rs#L1788)、[wait_until_stopped](../../crates/qaqh-runtime/src/registry.rs#L2225)、[QaqhService.registry](../../crates/qaqh-runtime/src/service.rs#L21)。

## 5. 未清理的旧实现与集中模块

以下行数是审查时文件或目录总行数，包含内置测试和注释，仅用于说明体量，不等同于可删除行数。

| 位置 | 体量 | 已确认的问题或边界 |
| --- | ---: | --- |
| runtime/src/agent/ | 25 个 Rust 文件，7,942 行 | 旧执行骨架仍编译；LoopPhase、Outcome、TurnState、PendingAsk、RingContext 等仍在。旧算法与失去入口的骨架混杂，不能把整个目录都视为无用代码 |
| runtime/src/registry.rs | 3,014 行 | 生产装配与运行编排集中；thread、reader、liveness 及旧关闭逻辑仍残留 |
| message/src/store.rs | 3,758 行 | 旧消息存储仍是大模块；导入、归档等用途与新执行真相之间的边界尚未完全收口 |
| session/src/manager.rs | 2,606 行 | 仍维护 messages/WAL 等旧存储；部分统计入口继续读旧消息，而新 actor 写 batch journal |
| session/src/actor.rs | 713 行 | registry 主要使用订阅功能，但类型内仍携带 TurnCore、工具准入和另一套回合状态机 |
| session/src/canonical/tool_ledger.rs | 1,592 行 | 旧工具账本与旧恢复家族继续存在；生产部分读者尚未离开旧格式 |
| runtime/src/session_agent/agent.rs | 1,116 行 | 新执行中心已集中工具执行、交互、事实构造、续轮和取消；多个续轮分支重复构造相同协议 |
| runtime/src/host_impl.rs | 1,812 行 | 宿主、子代理、任务板与消息板适配集中，部分读取仍依赖旧 canonical reader |
| subagent/src/lib.rs | 2,276 行 | 子代理工具与宿主接口仍是大模块，存在不等于已接入新执行闭环 |
| runtime/src/ringing/v2.rs | 1,673 行 | 同时承担读模型、磁盘回放、快照、overlay、状态查询和广播 |
| runtime/src/service.rs | 1,800 行 | 服务入口、会话生命周期、配置与资源管理集中；仍与 registry 和多个全局入口耦合 |
| runtime/src/ringing/compact_mirror.rs 等 | 多个旧适配模块 | CompactMirror、旧 emitter、标题和压缩执行适配仍在编译，部分已无生产调用入口 |
| Electron webui/src/api/qaqh/ | 19 个 Timeline* 生成文件 | 旧生成物仍在，ProjectionPayload / ProjectionSlot 仍声明 timeline，与后端删除状态不一致 |

旧消息/WAL写入仍通过 LegacyWriterFacade 全局锁等路径存在。它们对归档、导入或元数据可能仍有用途，因此「仍有旧写入代码」不能直接等同于「新旧主循环正在双执行」。实际问题是生产状态、统计和恢复读取尚未全部统一到新事实来源。

位置：[LegacyWriterFacade](../../crates/qaqh-message/src/legacy_writer.rs)、[SessionManager](../../crates/qaqh-session/src/manager.rs)、[统计旧读链](../../crates/qaqh-runtime/src/service/stats.rs#L48)。

旧测试和辅助构建路径也继续直接调用 AgentState.build_context 等旧实现。相关行为成立不能证明新 SessionHost 的生产配置、prompt、工具与恢复装配成立；这是验证对象与实际执行入口脱节的问题。本报告不展开测试实施方法。

## 6. 不能仅凭名称或体量认定为残留的问题

- 新 qaqh-agentloop-core 约 972 行、业务 CommitState 约 489 行、父子生命周期 supervisor 约 279 行。状态机存在本身合理；已指出的问题是职责重复、共享可变调度和恢复合同没有闭合。
- 不可 Clone、私有构造的 committed / authorized 凭证具有实际约束价值；其当前结果载荷和失败所有权仍有缺口，不代表所有凭证都无用。
- journal 原子性、writer fencing、未知结果恢复、Ringing v2 cursor / replay / reset 仍是必要的正确性边界。Rust 所有权不能替代崩溃与 IO 不确定性处理。
- Electron 的 timeline CSS 类名和普通展示命名，不等同于旧 timeline 协议仍在运行。生成 API 类型的残留与 CSS 命名应区别看待。
- Mutex、RwLock 和 Arc 本身没有违背 Rust；本报告指向的是共享修改权、锁内重工作、错误恢复以及线程亲和约束的问题。
- 当前最大缺口是生产能力和状态合同没有闭合。仅继续删除文件，无法自行补齐正文展示、请求装配、交互、恢复和外部任务结算。
