# 从本地 Codex 源码借鉴 QAQH 下一阶段架构

日期：2026-10-09。参考仓：`E:/myXCode`，HEAD `406eb53`（提交时间 2026-10-07）。QAQH 参考 HEAD `a32276d`，并读取了当时工作树中的变更。

性质：**源码对照与设计建议，未实施，不替代已批准的架构/协议 ADR**。没有编译、运行或修改 Codex，也未对它的全部架构做审计。结论以该本地源码快照为准，不声称上游最新版本具有完全相同行为。

## 1. 核心判断

QAQH 已经借鉴了协议、事件与工具的部分形状。接下来更应该学习的是：

1. 连接状态与执行状态分别有所有者。
2. 对外提供小的运行时门面，内部状态私有。
3. 进程、会话、turn、sampling step、工具调用各自承载不同寿命的状态与能力。
4. 将快照响应与订阅接续放入同一会话顺序边界。
5. 宿主显式装配有类型的功能贡献者，核心循环不承担所有产品功能。
6. 存储、provider、工具在实际行为边界抽象，不按文件大小机械拆包。
7. 用生成物一致性、lint 和真实接缝测试落实规范。

不是把 QAQH 改名成 Codex，也不是复制 156 个 workspace 成员。建议保留单 daemon 与现有 Axum/Tokio，将架构核心表述成：

> **执行内核 + 能力模块 + 应用服务 + 传输适配 + 单一装配入口。**

## 2. 源码证据与适配建议

### 2.1 app-server 与 core 分别持有连接和会话执行

Codex 中：

- `core/src/thread_manager.rs:248` 的 ThreadManager 维护运行时 thread。
- `app-server/src/thread_state.rs:345` 维护 thread/connection 双向订阅索引。
- `app-server/src/request_processors/thread_processor.rs:3605` 的 connection_closed 清连接登记，仅在 core 已移除 thread 时收尾陈旧 bookkeeping，不在这里无条件取消 thread。
- `app-server/src/request_processors/thread_lifecycle.rs:56` 的卸载条件要求“无订阅者且不活跃”，再叠加延迟；不是连接断开即停止任务。

QAQH 的适配：

| 对象 | 所有者 | 生命周期终止条件 |
|---|---|---|
| Connection | 传输/应用连接层 | socket/IPC 关闭、吊销、明确协议失败 |
| Subscription | 会话发布层 | 退订/连接关闭/权限撤销 |
| SessionRuntime | 执行内核 manager | 显式停止；或空闲、无外部/内部持有且允许卸载 |
| ActiveTurn | 会话 actor | 完成、失败、显式取消、服务关闭 |
| ToolExecution | 执行器 | 返回/失败/调用 deadline/取消 |

驱动座只管理写控制权。普通读取不依赖短租约；资源回收基于明确的运行态与持有关系。子代理、待处理审批、后台进程等内部持有者必须计入回收判据，不能只看前端订阅计数。

### 2.2 快照与订阅在一个会话顺序边界中处理

`app-server/src/thread_state.rs:59` 定义 ThreadListenerCommand，SendThreadResumeResponse 用于发送运行中历史并原子接续订阅。

实际分支在 `request_processors/thread_lifecycle.rs:507`，处理器在同一 listener 上读取 active-turn 展示态、检测 pending unload、调用 `try_add_connection_to_thread`（`:729`）、构造并发送 resume 响应。

借鉴点是 **顺序边界**，不是必须复制它的 resume RPC、锁或 JSON-RPC：

```text
同一 session 的发布顺序
  → 建立订阅缓冲/登记
  → 获取覆盖到 W 的一致展示快照
  → 响应快照 W
  → 交付 W 之后的增量
```

QAQH 可以保留 HTTP + SSE，通过 snapshot cursor、订阅前缓冲、过滤和确认实现同等不变量。传输不同不影响目标：不能出现“响应已经推进到新基线，旧连接却继续按旧身份/旧基线读”的窗口。

建议提供 `SessionSubscriptionService::open(session_id, principal, resume_cursor)` 的完整用例，而不是让前端协调 bootstrap、attach、timeline 和 canonical 四个松散步骤。接口名为设计示例，不是当前存在的 API。

单一逻辑发布器可以仍输出多个视图；它必须保证各视图基线对应同一提交位置。是否合并物理 SSE 可后做，但 snapshot 与增量的交付语义先定。

### 2.3 运行时门面与纯契约包不是同一件事

Codex 的 CodexThread 提供 thread 级运行接口，core 内部 session/turn 状态多用私有或 pub(crate)。`core-api/src/lib.rs:3` 启用 private_bounds/private_interfaces/unreachable_pub 的 deny，收窄外部可见性。

但 **codex-core-api 仍依赖 codex-core 并大量 re-export**。它是运行时 facade，不是一个把存储/运行实现完全隔离的纯 DTO/port 包。

QAQH 应区分：

- `SessionHandle`：可克隆的活会话操作门面，不允许客户端直接修改内部 AgentState/Registry。
- `SessionCommands/SessionQueries`：应用用例接口，按操作具名，不提供万能 `handle(method, JSON)` 作为业务核心。
- `protocol/contracts`：供 Rust 客户端、TS 生成、TUI、移动端共享，不引入 writer/runtime 实现。
- facade 可以保留实现依赖，但不能拿“新增一个 api crate”当作减负成功的证据。

逐步删除 session/workspace/global 的逃生入口。先使业务依赖可见，再以 Cargo 依赖图和真实消费者构建证明边界。

### 2.4 Session / Turn / Step 的显式上下文

Codex 的三个重要锚点：

- `core/src/state/service.rs:52`：SessionServices 明确装入 MCP runtime、执行 manager、审批、provider/model、runtime handle、extensions、store 等句柄。
- `core/src/session/turn_context.rs:321`：TurnContext 持有 admitted turn 的身份、初始设置、环境/权限与 turn 生命周期状态。
- `core/src/session/step_context.rs:24`：StepContext 抓取一次 sampling request 的 settings、environment、MCP binding、tool router、instructions 等确切版本。

StepContext 的 settings 是该请求捕获的一份版本；临时授权仍通过原 turn 的 grants 解释。这不是“所有状态永不更新”，而是“版本与适用范围明确”。

QAQH 需要在已有 RingContext / ToolCallContext 上补充寿命边界，而不是全量换类名：

```text
DaemonServices       进程资源：runtime、客户端池、配置服务、存储工厂
SessionServices      会话资源：正文/事实句柄、工具环境、审批/子代理服务
TurnContext          turn id、设置基线、取消树、授权来源、因果归属
StepSnapshot         本次模型请求的配置/工具目录/MCP binding/环境版本
ToolCallContext      单次执行的参数、权限、deadline、取消与进度出口
ConnectionContext    身份与传输能力；不进入执行器变成业务所有者
```

必要不变量：

- 模型看到的工具目录与执行实际使用的目录同版本。
- 授权时绑定的资源、cwd、sandbox 规则与执行复核一致。
- 后台工具/子代理保留原 turn/调用上下文，不重新读“当前会话”。
- 更新明确生效于当前调用、下一 step 还是下一 turn；撤权不得因旧 snapshot 永久被忽略。
- 不把上述 context 合并成另一个包含全系统可变状态的超级 Context；只传调用所需的能力。

### 2.5 显式、有类型、宿主装配的功能贡献者

`ext/extension-api/src/registry.rs:21` 的 Builder 在构造期注册 typed contributors，build 后得到 registry。

`app-server/src/extensions.rs:50` 的 thread_extensions 是明确的宿主装配点，安装 queue、goal、history-notes、memories、MCP、web-search、skills 等贡献者。

QAQH 最适合借鉴这一点，将目标/待办/记忆/技能/任务板等可选功能从核心 engine 分支中逐步外移。保留的内核职责应是命令接收、turn/step 调度、模型/工具执行编排、取消/终态、事实提交。

先设少量有真实消费者的扩展点，例如：

- ContextContributor：提供具来源标识的上下文片段；
- ToolContributor：提供可执行的 typed tools；
- TurnLifecycleContributor：明确启动/终止阶段；
- WorkspaceChangeContributor：提供具归属的工作区变化；
- BudgetContributor：提供预算观测/决策输入。

**这与万能事件总线不同。** 核心主动调用已知契约，输入/输出具名，宿主显式安装。不能偷偷监听一个字符串事件来追加事实或改变 driver。

在引入前必须补齐：调用顺序、deadline、取消、fatal/可忽略失败、重复调用、状态所有者、允许副作用与重放策略。计费、授权、提交、取消等关键裁决不要变成任意第三方 hook。

不要一次复制 Codex 全部 Contributor、泛型 Config 或 ExtensionData 类型映射；它们也可能成为间接服务定位器。QAQH 应从真实重复改动中提取最小接口，默认编译期/显式构造，不引入热加载插件平台。

### 2.6 存储与模型 provider 以行为接口隔离

Codex 的 `thread-store/src/lib.rs:1` 明确应用用 ThreadId，而实现负责映射本地 rollout/RPC/backing store；`thread-store/src/store.rs:97` 提供 ThreadStore。

但该 crate 也包含 local/in_memory 等实现，trait 还有 as_any 与 legacy 默认值。可学习接口语义，不应复制逃生口或大而全存储服务。

QAQH 的更窄目标接口：

- 追加/读取 committed facts；
- 持久正文写入与读取；
- 构建/读取当前展示 projection；
- 读取 committed tail 或带 checkpoint 的重放；
- 对外返回真实 commit boundary 与具名恢复错误。

实现内可有 WAL/缓存/索引，但不能让应用任意改某个 .jsonl 文件。client 不读 SessionManager、writer 或数据目录。

Codex 的 `model-provider/src/provider.rs:108` 以 ModelProvider 描述运行时 provider 行为。QAQH 已有 gate/EndpointSpec，应继续收口能力与请求策略，而不是增加按 URL/model 字符串猜测的分支。

保持 QAQH 自己的 durable-before-publish 与 canonical/blob 不变量。Codex ThreadStore 的 PersistContext 明确允许某些背景持久化（`store.rs:63`）；不能未经比较就把这套持久化时序替换进 QAQH。

### 2.7 工具描述与执行保持同一注册对象

Codex `tools/src/tool_executor.rs:106` 的 ToolExecutor 同时提供 tool_name、spec、exposure、handle，描述与可执行 runtime 不分家。

QAQH **已经有** `qaqh-tool-core/src/tool_api/typed.rs:26` 的 TypedTool：Args/Output、meta、run；adapter 从类型生成 schema。这里应保留而不是另造 CodexTool。

下一步是消除周边重复：

- descriptor 默认 timeout 与调用 deadline 的优先级；
- 各种 ToolResult/ToolOutcome 的多次转换；
- model/display/canonical 三种投影各自消费哪份已归一结果；
- permissions/resources/effects 使用同一执行描述，不在 runtime 和工具内分别猜；
- 登记接口不要求工具依赖整个 workspace manager。

目标是新增一个工具只实现能力与一个显式装配点，而不是同时改 engine、domain event、wire mapper、前端 reducer。

### 2.8 工程规则变成可执行机制

此本地快照中可见的具体机制：

- workspace Clippy deny 包括 await_holding_lock、await_holding_invalid_type、disallowed_methods、expect_used、unwrap_used（Cargo.toml:563）。
- clippy.toml 限制直接 SQLite 构造入口，要求通过 state shim；这类规则比一句“状态要统一”更可执行。
- core/lib.rs:6 禁止库直接 stdout/stderr；消息经指定展示/日志通道。
- `app-server-protocol/src/schema_fixtures_tests.rs:18` 比较 TS 与 JSON schema 的生成物/固定夹具，包括预计算导出。
- `.github/workflows/rust-ci.yml:85` 对未使用依赖执行 cargo shear --deny-warnings。
- 快速 PR 检查、完整平台测试分层；justfile 提供一致命令。

这些是源码中的机制，不是本次已运行通过的结果。

QAQH 首先实现：依赖矩阵、公开面约束、TS/协议 golden、禁止越过 store/runtime 装配入口、调用取消/超时接缝验证。不要第一步复制 Bazel、remote cache 或所有 lint。

## 3. 目标调用结构

```text
Desktop / TUI / Mobile
        ↓ client + protocol
HTTP/SSE/IPC transport adapter
        ↓ authenticated request
Application services
  ├─ SessionCommandService
  ├─ SessionQueryService
  ├─ SessionSubscriptionService
  └─ Driver / Identity service
        ↓ explicit handles and capabilities
Execution kernel
  SessionRuntimeManager → SessionHandle → turn/step scheduler
        ├─ SessionStore / BlobStore
        ├─ ModelProvider
        ├─ ToolExecution
        └─ explicitly installed feature contributors

daemon composition：构造并注入以上实现，不承担所有业务规则
```

这是调用责任图，不是“所有箭头都必须对应新增 Cargo 包”。跨包依赖应保持 DAG；消费者拥有所需 port 或引用中立契约，具体实现由上层 composition 注入。不能让 feature→engine 与 engine→feature 形成循环；不能用 Any/global callback 隐藏反向依赖。

## 4. 实施顺序

| 顺序 | 任务 | 验收 |
|---|---|---|
| 0 | 确认执行/存储/传输/扩展职责、标准规范入口、最小 CI；同时修已复现连接缺陷 | 规则对应真实路径；旧流明确终止；恢复有 deadline |
| 1 | app/transport 与 runtime 分离，建立订阅协调器、明确 ConnectionId/SessionId | 两前端并连；一端断开不影响任务与另一端；快照/增量无缺口 |
| 2 | Session/Turn/Step/Tool 作用域与 settings/catalog snapshot | 两会话并发不串；后台工具沿用原权限；模型工具目录与执行一致 |
| 3 | 建 SessionHandle 与窄 store/provider/tool 接口，消除 global 服务查找 | 消费者不能任意修改 runtime/store；client 不拖 writer；mock 可以驱动真实用例 |
| 4 | 从一个可选功能提取 typed contributor，完成显式安装 | 功能改动局部；失效/取消策略明确；去掉对应 engine 旧分支 |
| 5 | 完成事实源 cutover、其他功能迁移、旧接口与转换删除 | 历史迁移/崩溃/恢复/正文完整、所有客户端与发布验证 |

不要把每个过程强制串成半年大重写：step 1/2/3 的已确定子边界可以增量交付；共享契约与存储迁移仍单一所有者。拆层后旧入口必须有明确删除任务，不能长期保留第二条调用链。

## 5. 不应照搬的 Codex 部分

本地测量：156 个 workspace member；core 的 src 下 586 个 Rust 文件、250,698 物理行，含内联测试/注释/空行，不是生产行数。这说明它不是“每 crate 都小且完全解耦”的范本。

另外：

- core-api 不是纯契约；thread-store、app-server-protocol、extension-api 本身也带较宽依赖。
- session/turn 中仍有 legacy 字段与迁移 TODO；不能把这些当作 QAQH 新架构必须保留的结构。
- 背景持久化、文件/DB fallback、RPC/WS 的策略不自动满足 QAQH 的 canonical/恢复约束。
- `docs/contributing.md:5` 在这个快照中明确不接收外部代码 PR。QAQH 若希望开放贡献，应学习它的检查机制，另设计自己的贡献流程，而不是复制该政策。
- 不把“有接缝测试源码”说成“此版本全部验证通过”；本次没有运行 Codex 测试。

## 6. 建议的开发不变量

可以把上一份 v2 提案的规范进一步落成以下可检查条款：

1. 一个进程/会话/turn/调用资源有且只有一个所有者与关闭路径。
2. ConnectionContext 不决定持久会话身份、任务存续或命令回执归属。
3. 普通订阅不启动、resume、取消 actor；控制权单独裁决。
4. 模型请求绑定一份工具/环境/配置 snapshot；后台执行保留原作用域。
5. public facade 与纯契约分开，禁止用 re-export 壳宣称实现依赖已消除。
6. features 通过显式 typed installation 接入；核心提交/授权/取消不能被隐式 hook 接管。
7. snapshot/replay/live 使用明确边界；各流健康与前端 applied 水位可观测。
8. 数据目录/IO/数据库构造仅在批准的存储/平台边界；所有 reader 只读取 committed 前缀。
9. 库不直接写 UI 输出；协议生成物与实际类型必须一致。
10. 每个迁移 PR 列旧入口/依赖/转换的删除项、消费者基线和行为证据。

完成判断：不要看是否“像 Codex”；看一个前端刷新、一项功能新增、一次配置变更、一条工具取消、一次崩溃恢复是否具有清楚的唯一执行路径。
