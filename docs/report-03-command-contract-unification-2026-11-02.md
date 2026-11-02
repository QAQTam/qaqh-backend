# 第三份架构简化报告：命令真源与 qaqh-domain 收敛

- 日期：2026-11-02。
- 分析基线：`d88b90cf` 加当前未提交工作区，包含新增的 `unified.rs` 与 provider handlers；源码现状优先于历史分层注释。
- 性质：源码审查与实施建议；本轮只新增报告，没有修改运行代码。
- 前置报告：[第一份：存储与执行组织](report-01-jsonl-simplification-2026-11-02.md)、[第二份：工具、频道与 HTTP/SSE](report-02-tools-channels-http-sse-2026-11-02.md)。
- 资源：磁盘剩余约 97 GiB，内存可用约 3.0 GiB，CPU 8 核；没有运行构建或全局测试。
- 定位工具：已先尝试 CodeGraph MCP 和本地 `codegraph explore`，二者均报告无可用索引。本地 `.codegraph/` 实际只有 `.gitignore`，没有索引文件；后续证据来自 `rg` 与逐段源码检查，没有重建索引，也没有取得 CodeGraph 调用图。

## 1. 结论与选型

维护者反馈成立。应当选定 **`qaqh-ringing` 中的 RingingV2 契约作为唯一业务命令真源**，合并重复命令定义和入口；`qaqh-domain` 不再拥有另一套业务命令体系。

具体分两步理解：

1. `DomainCommand` 是与 `RingingCommand` 同构、且仓库内没有实际生产者和消费者的根枚举，可以删除它、导出及对应 `From` 实现。
2. 实际业务字段目前定义在 domain 的三个子枚举中，ringing 只是起别名。应把这份字段定义迁入 RingingV2，并与 `UnifiedCommand`、service 写操作目录收敛；只删根枚举不能完成命令统一。

最终保留一份业务操作目录、一套公共提交信封和明确的处理函数。HTTP、SDK、子代理及会话 owner 使用同一份命令语义；agentloop 的调度实现为它服务。删除旧频道包装和旧提交机制，不增加 `DomainCommand → RingingCommand → AgentCommand` 的长期转换链。

`qaqh-domain` 中仍有事件、回合、状态和交互数据被其他模块使用。本报告裁决其命令职责退出；其余类型按真实业务用途保留或归并到已有模块。迁空后删除 crate，不为了保留四层架构名称留下空壳，也不在本次只查命令的基础上直接删除全部领域类型。

## 2. 到底重复了什么

| 位置 | 当前内容 | 判断 |
|---|---|---|
| `qaqh-domain::DomainCommand` | Control / Conversation / Tool 三个根变体与 `channel()` | 与 Ringing 根枚举实质重复；仓库内没有实际使用链 |
| domain 的 `ControlCommand` / `ConversationCommand` / `ToolCommand` | 分别 19 / 6 / 2 个业务变体，共 27 个 | 当前旧频道命令的字段真源，需要迁移职责 |
| `qaqh-ringing::RingingCommand` | 同样三个根变体与 `channel()` | 实际被 daemon、runtime、SDK 和子代理使用 |
| `RingingControlCommand` / `RingingConversationCommand` / `RingingToolCommand` | 指向 domain 子枚举的三个 `type` 别名 | 没有复制字段；问题是归属与命名绕路，不是三份 payload 内存 |
| `qaqh-ringing::UnifiedCommand` | 9 个变体，包括创建会话、设置和 provider 操作 | 是另一份真实目录，尚未覆盖会话输入、取消和交互；其中 discover 明确拒绝执行 |
| `qaqh-runtime::service::command` | `Command` 及 System / Workspace / Session / Task 分类、参数与 decoder | 混合查询和写操作，与公共命令重叠；部分 Args 已复用 Params，不能笼统称为全部复制 |
| `qaqh-runtime::ringing::service_methods` | `METHODS`、`TYPED_METHODS`、Params/Result、校验及分发 | 公共操作目录落在实现 crate 中，另行维护方法、权限和结果清单 |
| `qaqh-client::ActionRequest` | 再列 skills、工具模式等操作及字段，并手写 `into_parts()` | SDK 又维护一份业务操作词表与编码规则 |
| `qaqh-agent-runtime::AgentCommand` / `AgentControl` | 输入、取消、交互、metadata 和 reload 等 mailbox 消息 | 部分是同一业务命令的重述，部分是执行所需的内部消息；必须逐项拆开 |
| `qaqh-session::actor::SessionCommand` / `TurnCommand` | 旧 actor 的回合与订阅状态机消息 | 属于另一套内部状态机；当前 registry 仍借用它处理订阅，不能按同名认定与公共命令完全重复 |

27 个旧业务变体加 9 个 Unified 变体，不等于 36 项独立产品能力。相同操作有重叠，有些字段语义不同，有些只有声明没有执行分支。

## 3. 快速定位清单

行号对应本次工作树，以下链接用于直接进入定义和分发入口。

| 编号 | 证据 | 位置 |
|---|---|---|
| E01 | 两个根枚举完全对应；转换只把相同子类型换一个外壳 | [domain 根枚举](../crates/qaqh-domain/src/command.rs#L331)、[ringing 根枚举与转换](../crates/qaqh-ringing/src/command.rs#L17) |
| E02 | ringing 三个子命令名只是 domain 类型别名 | [command.rs](../crates/qaqh-ringing/src/command.rs#L3) |
| E03 | domain 注释声称 agent core 消费 DomainCommand；实际入口持有 RingingCommand | [旧分层声明](../crates/qaqh-domain/src/command.rs#L1)、[worker 信封](../crates/qaqh-ringing/src/worker.rs#L15)、[实际分派](../crates/qaqh-runtime/src/registry/messaging.rs#L72) |
| E04 | Unified 与频道命令同时存在两套提交信封 | [Unified 信封](../crates/qaqh-ringing/src/v2/types.rs#L49)、[Unified 目录](../crates/qaqh-ringing/src/v2/types.rs#L699)、[频道信封](../crates/qaqh-ringing/src/v2/types.rs#L733) |
| E05 | daemon 同时挂载频道 POST、Unified POST 与 service POST | [路由](../crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs#L137)、[service 入口](../crates/qaqh-daemon/src/axum_server/axum_impl/service_api.rs#L8) |
| E06 | service 表和 decoder 再维护操作名单 | [METHODS](../crates/qaqh-runtime/src/ringing/service_methods.rs#L717)、[decoder](../crates/qaqh-runtime/src/service/command.rs#L31)、[SessionCommand](../crates/qaqh-runtime/src/service/command.rs#L260) |
| E07 | SDK 再声明相同业务操作，并把 skills revision 编成另一个 JSON 字段 | [ActionRequest](../crates/qaqh-client/src/endpoint.rs#L106)、[into_parts](../crates/qaqh-client/src/endpoint.rs#L193) |
| E08 | service 的 skills 操作再构造 RingingCommand，生成新的 worker 命令 ID | [skills handlers](../crates/qaqh-runtime/src/service/handlers/session.rs#L142)、[send_ringing_cmd](../crates/qaqh-runtime/src/service.rs#L557) |
| E09 | registry 把完整会话消息缩减成 text、attachments、command ID | [丢字段的分支](../crates/qaqh-runtime/src/registry/messaging.rs#L75)、[原始消息字段](../crates/qaqh-domain/src/command.rs#L251)、[mailbox 输入](../crates/qaqh-agent-runtime/src/session_agent/host.rs#L70) |
| E10 | agent 再声明业务输入和控制命令，执行器按它们处理 | [AgentCommand](../crates/qaqh-agent-runtime/src/session_agent/types.rs#L97)、[AgentControl](../crates/qaqh-agent-runtime/src/session_agent/types.rs#L170)、[输入处理](../crates/qaqh-agent-runtime/src/session_agent/agent.rs#L1408) |
| E11 | SDK 提交仍使用频道命令，契约导出同时列出新旧命令 | [SDK 提交](../crates/qaqh-client/src/v2.rs#L355)、[旧类型导出](../tools/qaqh-contract-export/src/main.rs#L665)、[Unified 导出](../tools/qaqh-contract-export/src/main.rs#L750) |
| E12 | 所谓独立 application contract 仍依赖 session 的投影实现 | [Cargo.toml](../crates/qaqh-ringing-contract/Cargo.toml#L11)、[快照字段](../crates/qaqh-ringing-contract/src/lib.rs#L8) |

全仓 Rust 源码检索 `DomainCommand`，命中仅限其定义、`channel()`、文档、lib 导出与 ringing 中的 `From` 实现。没有找到构造、函数参数、mailbox 或 handler 使用它。这个结论限于当前仓库；不能据此声称已盘点仓库之外的所有使用者。

## 4. 实际链路没有经过所谓中立领域命令

旧频道路径：

```text
SDK / 子代理构造 RingingCommand（payload 类型实际来自 domain）
  → RingingV2CommandEnvelope / RingingWorkerCommandEnvelope
  → daemon 生命周期处理，或 AgentRegistry::send_ringing
  → SessionAgentHandle 的另一组方法
  → AgentCommand / AgentControl
  → SessionAgent
```

service 写操作还可能绕一圈：

```text
ActionRequest::SkillsOperation
  → method + JSON
  → service_methods::dispatch → Command::decode
  → SessionCommand::SkillsOperation
  → RingingCommand::Control(SkillsOperation)
  → worker 信封 → registry 再分发
```

Unified 路径对部分操作直调 typed handler，方向更直接，但尚未替代上述入口。创建会话仍通过 `create_session_unified()` 重装为旧 `SessionNewArgs`，调用 `create_new()`；并且另外一份 `SessionCommand::SessionCreate` 的实现继续存在。见 [Unified 创建入口](../crates/qaqh-daemon/src/axum_server/axum_impl/unified.rs#L198)、[参数重装](../crates/qaqh-runtime/src/service/handlers/session.rs#L391)。

这说明应删除重复分发职责。仅用一个 `From` 或统一函数包住旧实现，会把多套真源继续保留下来。

## 5. 重复体系已经产生的具体问题

### 5.1 创建会话至少四种表示，结果形状也不同

| 表示 | 参数侧差异 | 当前执行结果 |
|---|---|---|
| `ControlCommand::SessionCreate` | `close_current`、cwd、tool mode、自定义工具 | daemon 改走 `session.new`，然后处理 lease attach |
| 内部 `session.new` / `SessionNewArgs` | cwd、工具、title、model selection | `create_new()` 返回 JSON 字符串形式的 session ID |
| service `session.create` / `SessionCreateParams` | cwd、profile、effort、title、mode | 独立创建实现，返回 `{ "session_id": ... }` |
| `UnifiedCommand::SessionCreate` | cwd、title、model selection、工具 | 调 `create_new()`，ACK.result 仍是 session ID 字符串 |

依据：[旧创建命令](../crates/qaqh-domain/src/command.rs#L49)、[service DTO](../crates/qaqh-runtime/src/ringing/service_methods.rs#L195)、[service 创建实现](../crates/qaqh-runtime/src/service/handlers/session.rs#L56)、[create_new 返回值](../crates/qaqh-runtime/src/service/handlers/session.rs#L378)、[频道创建分支](../crates/qaqh-daemon/src/axum_server/axum_impl/command.rs#L410)。

`session.new` 不在公共 service 白名单，但 daemon 仍通过内部 decoder 调用它。不能把它再算作一个公开 HTTP 入口。旧命令的 `close_current` 没有在创建分支被消费；当前注释承诺与执行路径不一致。

统一时必须确定创建后的状态、返回 DTO、模型和工具初始化以及 lease 归属流程。Unified dispatcher 当前只调用 service 创建并返回结果，没有频道创建分支的 attach 步骤；需要明确创建响应后的归属行为，不能只统一操作名称。

### 5.2 完整消息在 agent 边界被降格为普通用户输入

`ConversationSendMessage` 声明了图片、`message_id`、`input_purpose`、`as_system`、`inter_agent`、`subagent_terminal`。registry 的消息分支却只提取 `text` 和 `attachments`，其余被 `..` 忽略，随后总是调用 `submit_user_input()`。

子代理终态注入实际填写了 QueueOnly、代理间元数据和终态通知，见 [lib.rs](../crates/qaqh-subagent/src/lib.rs#L1464)。这些字段不是没有调用者的预留设计。当前链路没有把它们交给 session agent；应完整保留并执行其语义，或在修订契约时明确删除不再提供的能力，不能接受后静默丢弃。

agent 已有独立 `QueueInput` 变体，但该 registry 分支不会依据 `input_purpose` 选择它。Steer / Interject、系统输入与终态通知也不能用统一的 UserInput 代替。

### 5.3 skills 操作的 revision 在 service 链路消失

SDK 的 `ActionRequest::SkillsOperation` 有 `expected_revision`，编码为 `expectedRevision`；runtime 的 [SkillsOperationArgs](../crates/qaqh-runtime/src/service/command.rs#L334) 没有该字段。handler 再通过 `send_ringing_cmd()` 新建 worker 信封，默认 `expected_revision = None`。

因此这条路径没有把调用者提交的 revision 传到 worker。删减目录时应一起删掉这种“SDK 声称提供、执行链路却不消费”的保护承诺；真正需要的 revision 在统一契约中定义一次并在 owner 写边界执行。

### 5.4 名单、类型和实际实现已经不一致

- `METHODS` 包含 `provider.upsert` / `provider.delete`，但普通 `Command::decode` 不接受 `provider` 前缀。[accounts::dispatch](../crates/qaqh-runtime/src/ringing/service_methods/accounts.rs#L226) 也只接账号方法；按当前源码，该 service 路径最终报 UnknownMethod，而 Unified 路径有对应 handler。
- `UnifiedCommand::ProviderModelsDiscover` 已导出，但 [dispatch](../crates/qaqh-daemon/src/axum_server/axum_impl/unified.rs#L291) 明确返回 `unsupported_operation`。
- `ToolInvoke` 已在公共命令定义，daemon 把它归为 Engine，但 registry 没有执行分支，落入 [不支持命令](../crates/qaqh-runtime/src/registry/messaging.rs#L184)。
- `UnifiedPrecondition::SettingsRevision` 出现在契约中，但 settings patch 分支只传 `patch`；当前 dispatcher 没有把该条件交给设置写入。见 [前置条件](../crates/qaqh-ringing/src/v2/types.rs#L37)、[settings 分发](../crates/qaqh-daemon/src/axum_server/axum_impl/unified.rs#L214)。

以上是静态调用链确认，未启动 daemon 验证 HTTP 行为。不能将这些声明计为已经实现的功能，也不能靠额外转换层掩盖缺口。

## 6. 合并后的职责边界

### 6.1 qaqh-ringing：一份公共契约

命令名称、请求参数、结果 DTO、错误以及必要目标和前置条件放在同一公共契约中。操作是创建、输入、取消、回答交互、修改设置等业务行为；control / conversation / tool 不再决定命令归属。

吸收当前 `UnifiedCommand` 已实现的业务能力和旧命令中仍需提供的能力，最终只保留一个业务命令根类型和一套提交信封。最终命名实施时确定；不是继续长期并存 `RingingCommand` 与 `UnifiedCommand`。

查询与写入共享契约来源，但保留实际语义：读取可直接返回 typed 结果，写入由明确 owner 执行；异步操作区分接收与终态，不要求所有查询塞进命令队列或 SSE。与[第二份报告第 11 节](report-02-tools-channels-http-sse-2026-11-02.md#11-最终复核追加统一命令体系与公共契约)一致。

权限、目标类别、调用方式和是否已实现应从同一目录维护，SDK 与导出器复用或生成。真实业务 handler 仍需实现；不为省几个 match 再造通用反射框架。

### 6.2 qaqh-runtime / daemon：边界校验与业务执行

daemon 解码一次、鉴权、确认会话归属，然后把同一命令及可信调用上下文送至对应 handler。runtime 不再通过 `method + Value` 重编码并解码另一个业务枚举。

命令 ID、目标、必要前置条件与因果信息贯穿调用，不在 service 转发中无意生成第二个业务请求。身份由认证结果产生；系统与代理来源由受信任调用方填写，不因删除转换层而采信外部自报身份。

生命周期、provider 配置等全局操作由各自 handler 处理，不为了“统一”强制送入会话 actor。会话内状态变化则由会话 owner 在同一串行边界处理。

### 6.3 agentloop：执行同一命令，保留真实调度能力

会话 owner 直接消费公共契约中的会话业务 payload；mailbox 可以区分普通输入和高优先级取消/交互，但应传递原命令，不再发明另一组字段较少的同义业务命令。

provider/tool 完成消息、任务句柄、取消信号、解析后的 provider spec、提交回执 sender 等属于内部执行数据，可以保留私有消息类型。HTTP 身份信封也无需整包塞给模型循环。这些真实执行对象不构成第二套公开业务命令。

`PublishResourceChanged` 这类后端提交通知退出 UI 命令目录，保留确有消费者的内部通知。模型输入中的 queue、steer、系统来源和代理终态按业务语义处理，不为适应旧 UserInput 形状丢字段。

### 6.4 domain 与其他 contract crate

- domain 中的命令根、频道子命令及 wire 专属参数迁出；共用值对象如答案、附件、模式和代理消息信息按实际消费者确定单一归属，避免为了搬文件制造循环依赖。
- 事件、回合与状态类型还有消费者，按前两份报告的 SessionState / timeline 方向继续归并。`DomainEvent` 与 `RingingEvent` 也存在同形包装，但本报告没有完成事件消费者审计，不将其冒充已确认可全量删除。
- `qaqh-ringing-contract` 当前是依赖 session 投影的快照包装，不是命令重复的解决方案。公共 DTO 收敛到不依赖执行/存储实现的契约中；不得再新建第三个命令 crate 包装前两套。
- `qaqh-session::actor::SessionCommand` 随旧订阅/回合状态机审查处理。registry 当前还使用其 Subscription 分支；先迁移真实订阅职责，再移除旧 actor，不能仅按名字和公共 Session 命令合并。

## 7. 可以删除、需要合并与需要保留

| 对象 | 处理 |
|---|---|
| `DomainCommand`、导出、`From<DomainCommand>`、重复 `channel()` | 删除；当前仓库没有实际调用链需要迁移 |
| domain 的三类业务子命令与 ringing 别名 | 字段定义迁至唯一契约，取消别名绕路和频道根包装 |
| `RingingCommand` 与 `UnifiedCommand` | 合并能力；完成消费者切换后删除旧的平行类型 |
| 频道 URL、信封 channel、命令内 channel | 随第二份报告收敛，目标及操作只表达一次 |
| service `SessionNew` / `SessionCreate` 与多个创建 DTO | 合并为一个创建实现与返回 DTO，明确初始化及归属行为 |
| runtime 的 service 写操作 decoder 与 SDK ActionRequest 同义变体 | 消费者迁至契约类型；删除重复业务目录与手写 JSON 编码 |
| service 查询、账号能力、workspace/fs 操作 | 逐项迁入统一契约和明确 handler，保留实际能力，不能随旧入口整批丢弃 |
| AgentCommand / AgentControl 中同义业务 payload | 复用契约业务类型，保留调度 lane，消除字段缩减转换 |
| 任务完成、运行资源、内部通知和可信执行上下文 | 保留实际执行职责，不暴露为 UI 命令 |
| 只有声明、没有实现的操作和前置条件 | 有需求则实现并接通；没有需求则从公开目录和导出中删除 |
| domain 剩余事件/状态、旧 SessionActor | 按真实消费者继续收敛，迁空再删，不保留架构占位壳 |

## 8. 实施顺序与完成标准

1. 删除闲置 `DomainCommand` 根包装及过时分层声明。这是局部清理，不能单独标记整个统一工作完成。
2. 在 RingingV2 中定下唯一操作目录、请求/结果、目标、前置条件及接收/终态语义。逐项盘点旧命令、Unified、service、SDK 操作，保留真实能力，删除已决定退出的声明。
3. 合并创建与设置实现，迁移 service 写操作。会话 owner 完整消费会话 payload，去掉 registry 的字段缩减；查询与账号面同步迁移实际调用者。
4. SDK、子代理、daemon 和导出器在同次切换中改用契约类型。删除频道提交、旧 service 提交、旧 decoder 和重复枚举；不留长期旧协议转换层。
5. 处理 domain 余留类型及快照契约依赖，再删除迁空的模块/crate。不要让命令统一扩张为未经审查的全仓类型移动。

完成标准：

- 同一业务操作只有一份字段真源和一个实际业务实现；SDK、daemon、子代理与 owner 不手写同义 payload。
- 输入用途、来源、代理通信与终态通知完整到达消费者；不能用 `..` 省略后宣称支持。
- 创建返回值和归属行为确定；需要的 revision 在实际写边界检查；声明的操作均能找到实现。
- 公共契约和 SDK 使用类型时不需要引入 session 持久化或 agentloop 实现；生成导出不同时保留两代命令目录。
- 同步完成、接收成功、运行中与最终失败的含义明确，统一调用函数不再在后台分流三套旧机制。

实施验证应集中在创建、一次普通输入、queue/steer 或代理终态输入、取消/交互、带 revision 的设置/skills 写入以及代表性 service 查询。复用已有检查并在切换完成后运行受影响 crate 的必要检查，不每改一处就做全仓构建或另搭一套测试框架。

本轮完成源码定位和本报告；尚未执行上述合并、修复丢字段、重建 CodeGraph 索引或运行业务验证。
