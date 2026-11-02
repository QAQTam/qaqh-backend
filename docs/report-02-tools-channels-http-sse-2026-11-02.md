# 第二份架构简化报告：工具生命周期与 HTTP/SSE 频道收敛

- 日期：2026-11-02。
- 分析基线：当前工作区代码，包括已有未提交修改；历史对照为 2026-10-28 的 `18a89dc7e64a9b77c1d1ca3e53b3944eb924d72d`。
- 已确定的方向：SQLite + WAL 作为唯一业务存储，继续 HTTP 状态快照 + SSE，timeline 作为会话读模型。
- 本报告性质：源码分析与实施建议。下文的频道调整、事件名称和接口形态尚未写入运行代码，也不冒充已经确定的完整新协议。
- 前置报告：[第一份架构简化报告](report-01-jsonl-simplification-2026-11-02.md)。

## 1. 结论

需要调整频道设计。建议每个 session 保留一份状态、一条 SSE 流和一个同步游标，使用明确的事件类型区分消息、工具和交互；前端不再选择 control、conversation、tool 频道。

当前代码已经采用 HTTP + SSE 和会话单流。恢复旧设计的重点是快照与增量同步语义，不是重新切换传输方式，也不是恢复三个独立频道。

必须严格规定工具参数、执行、输出、结果和权限的语义，但没有必要把它们分别变成传输频道。工具调用本来就是一个回合中的业务对象，可以在同一条 timeline 流中更新。

## 2. 当前协议到底如何分配工具行为

| 行为 | 当前组织方式 | 问题 |
|---|---|---|
| 模型生成工具参数 | conversation：`ToolInputProgress` | 名字容易被误认为工具执行输出 |
| 参数完整，声明工具调用 | conversation：`ToolCallDeclared` | 与随后执行意图分属不同频道 |
| 工具执行意图 | tool 频道，payload 却是 `ControlDelta::ToolIntent` | 频道名与 payload 家族不一致 |
| 工具完成 | 线上发布 conversation；内部 control 也维护完成状态 | 在线增量与快照内部组织不直观 |
| 权限请求 | control：`InteractionRequested` | 与权限回答分属不同频道 |
| 权限回答 | tool：`ToolPermissionRespond` | 与 ask、plan 的交互入口不统一 |
| 工作区资源变化 | tool 频道，payload 为 `ResourceDelta` | tool 频道承担了资源失效通知职责 |

这套划分要求开发者记住实现历史，不能帮助理解业务。一个工具调用从参数、权限到结果横跨三个频道，频道名、payload 家族、状态归属又互不一致。

需要精确区分“内部维护”和“线上发布”：`ControlProjection` 与 `ConversationProjection` 都处理 `ToolFinished`，但 `projection_slots()` 为这个 fact 只允许 Conversation 槽位出网。不能据此声称当前每次完成都会在线发送两份完成事件。

## 3. 源码证据

| 编号 | 发现 | 源码 |
|---|---|---|
| E01 | 频道注释仍声称三个频道拥有独立可靠流、snapshot、cursor 和消费预算 | [channel.rs](../crates/qaqh-domain/src/channel.rs#L5) |
| E02 | 实际订阅已经是每会话单流，不再按频道过滤 | [v2.rs](../crates/qaqh-ringing-runtime/src/v2.rs#L876) |
| E03 | Control 槽位的 `ToolIntent` 映射到 tool；资源变化也映射到 tool | [replay.rs](../crates/qaqh-session/src/projection/replay.rs#L153) |
| E04 | 工具参数进度经 conversation 发布 | [v2.rs](../crates/qaqh-ringing-runtime/src/v2.rs#L679) |
| E05 | 两个 reducer 都维护工具完成状态，但发布白名单只允许 Conversation | [control.rs](../crates/qaqh-session/src/projection/control.rs#L164)、[conversation.rs](../crates/qaqh-session/src/projection/conversation.rs#L198)、[projection.rs](../crates/qaqh-session/src/session_fact_v2/projection.rs#L57)、[set.rs](../crates/qaqh-session/src/projection/set.rs#L84) |
| E06 | bootstrap 的 `control.tools` 与 `tool.tools` 来自同一份工具状态，工具快照 revision 也来自 control | [v2.rs](../crates/qaqh-daemon/src/axum_server/axum_impl/v2.rs#L200) |
| E07 | 命令同时在 envelope 与命令枚举上表达频道，入口还校验 URL 频道 | [types.rs](../crates/qaqh-ringing/src/v2/types.rs#L733)、[command.rs](../crates/qaqh-domain/src/command.rs#L332)、[v2.rs](../crates/qaqh-daemon/src/axum_server/axum_impl/v2.rs#L659) |
| E08 | permission 通过 tool 命令、call ID 回答；ask、plan 则通过 control 命令、interaction ID 回答 | [command.rs](../crates/qaqh-domain/src/command.rs#L112)、[command.rs](../crates/qaqh-domain/src/command.rs#L311)、[v2.rs](../crates/qaqh-daemon/src/axum_server/axum_impl/v2.rs#L750) |
| E09 | 当前工具执行上下文的 `progress`、`exec_progress` 都是 `None`，实际执行调用也传 `None` | [tool_job.rs](../crates/qaqh-agent-runtime/src/tool_job.rs#L437)、[tool_job.rs](../crates/qaqh-agent-runtime/src/tool_job.rs#L590) |
| E10 | workspace 执行函数支持将输出 sender 注入工具；exec 的备用桥接也需要 context.progress | [execution.rs](../crates/qaqh-workspace/src/execution.rs#L267)、[handler.rs](../crates/qaqh-process-tools/src/exec/handler.rs#L84) |
| E11 | workspace 已返回结构化结果和 effects，tool job 的成功返回只保留正文 bytes 与 backgrounded | [execution.rs](../crates/qaqh-workspace/src/execution.rs#L14)、[tool_job.rs](../crates/qaqh-agent-runtime/src/tool_job.rs#L615) |
| E12 | `ToolInvoke` 存在于契约，但当前 thin agent 路由没有对应执行分支，最终进入不支持命令分支 | [command.rs](../crates/qaqh-domain/src/command.rs#L311)、[messaging.rs](../crates/qaqh-runtime/src/registry/messaging.rs#L75)、[messaging.rs](../crates/qaqh-runtime/src/registry/messaging.rs#L184) |

以上均为静态代码追踪结果。本轮没有启动 daemon 或复现工具流。

## 4. 应当严厉批评的设计

### 4.1 频道隔离的声明已经失真

E01 声称物理隔离，E02 却是单流。保留这些概念会让维护者以为工具输出具有独立的流量预算、同步和故障边界。实际代码并不提供这样的隔离，必须删除错误承诺。

### 4.2 复制状态没有产生独立能力

bootstrap 把同一份工具状态同时包装成 `control.tools` 和 `tool.tools`，让前端面对两份看似独立、实际同源的数据。应只暴露一份工具调用状态。

后端根据用途读取工具摘要与详细结果是合理的；为满足频道形状而维护两套完整工具状态，没有必要。

### 4.3 工具参数流与执行输出混在讨论中，掩盖了真实断线

`ToolInputProgress` 表示模型正在生成参数。这时工具可能还没有执行，甚至还没有形成合法参数。

stdout/stderr 则来自已经运行的工具。当前 tool job 在两条进度出口上都传入 `None`，该执行路径没有把实时输出接给会话展示流。workspace 虽支持 sender，调用者没有使用它；exec 的备用桥接也因 `progress: None` 而无法生效。

改频道名称不能修复这个问题。恢复工具实时输出时，应接通执行器到会话 owner 的输出路径，再发布工具输出事件。

### 4.4 内部执行过程挤进了前端公共协议

`ToolIntent` 携带 execution ID、policy decision、replay capability、side-effect class 等执行框架概念。前端通常只需要知道：参数是什么、是否等待授权、是否正在执行、输出是什么、结果如何。

公共协议应直接表达这些业务状态。内部调度、授权检查和诊断数据按实际需要保留在后端，不要求普通 UI 理解整套执行证明体系。

## 5. 建议的 HTTP + SSE 形态

接口形态示意，路径前缀、完整命令词汇和版本字段在统一契约中确定：

```text
GET  /sessions/{session_id}/state
GET  /sessions/{session_id}/events
POST /sessions/{session_id}/commands
```

HTTP 获取一份 `SessionState`，包含当前回合、timeline、工具调用和待处理交互；SSE 推送它们的变化；命令按类型进入后端处理逻辑。

同一份工具调用记录可被 timeline 卡片引用。不能因为引入 `SessionState`，又在 timeline、control 和 tool 下各存一份可独立修改的工具对象。

事件形态建议：

```text
id: epoch123:108
event: tool.output.delta
data: {"turn_id":"t1","call_id":"c1","stream":"stderr","text":"..."}
```

一次典型调用涉及的事件如下；这些名字是建议，不是要求每次调用机械发送全部事件：

| 事件 | 语义 |
|---|---|
| `tool.arguments.delta` | 参数生成进度，只用于展示 |
| `interaction.requested` | 产生权限或其他用户交互请求 |
| `interaction.resolved` | 请求已有明确回答或终结 |
| `tool.started` | 后端进入工具实际执行阶段，不代表执行成功 |
| `tool.output.delta` | 工具执行输出，按需要区分 stdout/stderr 或普通输出 |
| `tool.finished` | 调用最终状态、结构化结果、错误和必要指标 |

工具参数可能直接完整到达，工具也可能没有实时输出；不为凑齐生命周期而制造空事件。参数完整性与授权是执行条件，不能根据参数展示事件触发执行。

## 6. 必须明确的业务边界

### 6.1 一次调用，一个稳定身份

同一次工具调用从参数生成到最终结果使用稳定 `call_id`，并关联所属 `turn_id`。timeline 的工具 block 引用该调用。前端不需要另外理解执行票据、canonical fact ID 和 projection index。

后台进程的 `process_id` 有实际用途，可以保留；调用返回与后台进程退出是不同的业务时点。不能为了减少字段，把仍在运行的进程统一描述为执行成功并结束。

### 6.2 一份结构化结果

最终状态、错误、指标和 effects 直接使用执行器返回的结构化数据。展示正文不承担机器协议职责，不能重新解析正文中的 JSON 来判断 backgrounded、process ID 或业务副作用。

effects 由会话 owner 消费，更新对应业务状态并产生必要事件。文件变化、todo 变化、子代理创建等实际效果不能在 tool job 中压成正文后丢弃。

工具输出可能很大，运行进度只保留有界尾窗；最终保存的结果按明确的产品规则决定范围。进度、最终工具结果、后台进程输出之间不能隐含“全部内容一定完整保存”的承诺。

### 6.3 交互统一寻址

permission、ask、plan 都按 `interaction_id` 回答；关联工具时保留 `call_id`。不同交互仍使用类型化答案，例如批准/拒绝、表单答案或计划审阅结论。

统一入口不等于取消权限校验。后端仍校验交互归属、当前是否待回答及回答类型；不再让前端先判断应该走 tool 还是 control。

### 6.4 SQLite 保存业务，内存承接进度

SQLite WAL 保存调用参数、必要执行状态和最终结果。参数片段与工具输出进度在内存更新并通过 SSE 推送，不逐片提交数据库事务，不保存另一套永久 SSE 日志。

对外声称已经持久化的结果，应在数据库提交成功后发布。进程异常退出时，尚未保存的进度允许丢失；未完成调用显示为中断或结果未知，不自动重跑外部工具。

## 7. 同步与流量只保留一套规则

- 每会话同一条 SSE 流使用一个 epoch 范围内的序号；`Last-Event-ID` 表达传输位置，不暴露 SQLite WAL 位置或 canonical 投影游标。
- owner 在同一串行边界安排快照水位与之后的变化，避免取快照和订阅之间漏事件。
- 有界回放尾能够覆盖时补发；不能覆盖、进程重启或序号出现缺口时，重新获取状态快照。
- 快照包含当前运行工具、待处理交互和有界进度尾窗，不能只恢复最终历史而让运行中卡片消失。
- 工具输出先按时间或大小合并，再分配传输序号；不能分配后随意丢弃某些帧，却仍声称序号连续。
- 慢客户端无法继续消费时结束连接并重新同步，不无限积压输出，也不因网络发送阻塞会话 owner 或工具进程的输出读取。
- 取消和交互回答在后端保留及时处理的调度路径；这不要求对外再开 control 频道。

这是一套同步规则，覆盖消息、工具和交互。无需为每个业务类别再引入独立 cursor、revision 比较和重放协议。必要的对象身份与业务状态仍要保留。

## 8. 可以删除、合并与必须保留的内容

| 对象 | 建议 |
|---|---|
| URL、外层 envelope、内层 command 的重复 channel 声明与校验 | 删除，由命令类型决定处理逻辑 |
| control、conversation、tool 三份频道快照包装 | 合并为一份会话状态，按业务组织字段 |
| `control.tools` 与 `tool.tools` 的重复数据 | 合并为一份工具调用状态 |
| `stream_key` 与多层 payload 对同一事件的重复分类 | 收敛为一个类型化事件，保留必要业务标识 |
| 前端可见的执行证明、投影槽位、内部 replay capability | 从普通业务协议移除；后端职责另行按需要保留 |
| permission、ask、plan 分散的交互寻址方式 | 按 interaction ID 统一，保留类型化答案 |
| `ToolInvoke` 契约 | 先核实产品调用者；没有需求就删除，有需求就直接实现统一命令路径 |
| 工具输出与结构化结果 | 保留并接通，不能作为防御性设计删除 |
| 权限检查、沙箱、取消和真实后台进程管理 | 保留实际功能，收敛调用路径 |
| 没有对应实现的隔离与恢复承诺 | 从类型、注释和文档一并删除 |

`ToolInvoke` 当前在 thin agent 路径缺少处理分支，不应因为它出现在生成类型里，就把它当成已经可用的 UI 能力。本报告没有穷尽所有外部调用者，因此删除前仍需查清实际使用范围。

## 9. 10 月 28 日的设计可恢复哪些语义

在历史提交 `18a89dc7e64a9b77c1d1ca3e53b3944eb924d72d` 中：

- `crates/qaqh-domain/src/timeline.rs` 的 `TimelineEvent::ToolProgress` 携带 block ID、输出片段、stdout/stderr 标识、截断标志和累计字节数。
- `crates/qaqh-runtime/src/timeline.rs` 的 `append_tool_progress()` 更新工具进度尾窗，并产生 timeline 条目。
- 工具进度通过同一条 timeline 流进入 transcript reducer，而不要求 UI 重组多个工具频道的生命周期。

可以恢复“工具是回合中的 block，同一条流更新，同一份快照重建”的语义。旧有文件存储、checkpoint、归档兼容与重建框架不随之整套恢复；持久化按第一份报告采用 SQLite WAL。

## 10. 后续实施顺序与完成标准

1. 直接修订 RingingV2 公共契约：统一命令与事件类型，移除前端频道选择和重复快照；同步生成 Rust/TypeScript 类型，不加旧频道转换层。
2. 让工具参数、执行进度和最终结果进入同一会话 owner；接通当前缺失的实时输出 sender，保留结构化结果和 effects。
3. 将工具调用与交互状态接入 SQLite 业务存储及 SessionState，取消平行状态权威。
4. 前端以稳定调用身份更新工具卡片，按同一快照/游标规则重连，删除频道 demux 和特殊补状态逻辑。
5. 删除无人使用的入口、旧频道类型、注释和专属测试，不保留长期兼容适配。

实施后的必要验证围绕真实使用路径：有实时输出的调用、需要权限的调用、失败与取消、后台进程，以及流中断后快照能否恢复当前卡片。静态检查生成契约与实际处理分支是否一致；不为此次收敛重新搭建通用故障框架，也不要求每改一处运行全仓测试。

完成标准是开发者可以只看工具调用身份和事件类型理解整个生命周期，前端不再拼接三个频道的真相，工具输出和 effects 也确实到达消费者。

本轮只新增本报告，没有修改运行代码、实现数据库迁移或运行业务测试。

## 11. 最终复核追加：统一命令体系与公共契约

本节补齐前文尚未展开的命令入口问题。频道收敛必须同时处理三套命令机制，否则前端即使只有一条 SSE，发送操作时仍要判断应该使用哪一套封装。

### 11.1 当前同时存在的三套机制

| 入口 | 当前请求组织 | 源码 |
|---|---|---|
| `POST /ringing/v2/service/{method}` | URL 指定业务方法，body 作为参数；方法表同时包含读取和写入 | [路由](../crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs#L155)、[service_api.rs](../crates/qaqh-daemon/src/axum_server/axum_impl/service_api.rs#L5) |
| `POST /ringing/v2/commands/{channel}` | 路由占位符在代码中名为 `id`，POST 实际要求它匹配 envelope 的 channel；使用 `RingingV2CommandEnvelope` | [路由](../crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs#L137)、[入口校验](../crates/qaqh-daemon/src/axum_server/axum_impl/v2.rs#L659)、[信封](../crates/qaqh-ringing/src/v2/types.rs#L733) |
| `POST /ringing/v2/commands` | 使用另一套 `UnifiedCommandEnvelope`，含 target、contract_revision 和 precondition | [路由](../crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs#L141)、[unified.rs](../crates/qaqh-daemon/src/axum_server/axum_impl/unified.rs#L43)、[信封](../crates/qaqh-ringing/src/v2/types.rs#L49) |

这三个入口属于不同操作机制，有些业务还存在重叠。它们的操作数量不能相加后当成独立产品功能数量。业务方法多并不自动构成架构问题；反复维护封装、路由名单、类型和回执规则才是需要删除的重复职责。

### 11.2 收敛目标

- 以一份类型化业务操作目录定义请求、结果和明确错误，生成 Rust/TypeScript 契约；daemon、SDK 与 UI 不各自手写一份路由分类名单。
- 保留真实的读取与写入差异。读取可以直接返回查询结果；长期执行命令的接收、执行和终态需分别表达，不能把 HTTP 接收成功当成业务完成。
- 公共命令只表达一次目标及必要身份，按类型交给明确的处理函数。删除 URL、外层 envelope 和内层 command 反复声明同一 channel 的设计。
- 一套契约可以包含全局操作和会话操作，不要求所有方法硬塞入 session，也不要求所有读取都改为 POST 或走 SSE。
- 队列输入、运行中消息注入、系统输入与子代理结果等已有业务语义完整传递；不能在内部转换时用 `..` 忽略字段，再宣称完成统一。
- 设备授权、会话归属、路径权限和执行沙箱保留实际校验。身份、回执、幂等与断线后结果未知的最终字段仍须明确，不在本次补充中新增持久化对账框架。
- 直接修订 RingingV2 契约及实际消费者，删除旧入口和旧类型。不能只在 SDK 前面新加一个统一函数，再让它继续分流三套旧实现。

本报告第 5 节的三个路径描述会话状态、事件与命令的基本形态，不代表已经覆盖所有全局业务操作。最终 URL 和完整操作目录实施时统一确定。

### 11.3 依赖边界与完成判断

客户端与公共契约不再依赖 `qaqh-session` 的持久化、投影或执行状态机实现。当前依赖的源码证据和调整方向见[第一份报告第 12 节](report-01-jsonl-simplification-2026-11-02.md#12-最终复核追加八类架构删减与缺口补齐)。

完成判断：前端发送同类操作不再选择三套封装；同一业务状态有一份明确来源；接收与最终完成的区别清楚；工具输出与 effects 实际到达消费者；旧频道和旧命令机制已退出运行路径。公共契约可以在不引入 session 存储与 agent-loop 实现的前提下被客户端使用和导出。

本次最终复核只在现有两份报告末尾追加内容，原有正文未改写。没有修改运行代码、恢复此前删除的文档或运行构建与业务测试。
