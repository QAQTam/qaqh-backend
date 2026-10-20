# 组合、资源作用域与工具热插拔

状态：2026-10-20 校准的 v2.1 → v3.0 目标契约，尚未实现；按 [rollout](rollout.md) 随功能迁移，不是 v2.1 全量前置条件。本文细化 [生态](ecosystem.md) 的 activation、
[agent](agent.md) 的装配与取消；不新增通用插件内核；客户端宿主的执行边界由 [client-tools](client-tools.md) 补充，目标共 46 个 crate。

## 1. 借鉴范围

采用 Cordis 的生命周期组合思想：注册应有撤销句柄，资源属于创建它的作用域，
依赖消失时消费者不能继续假装可用，异步卸载必须等待清理完成。
采用 CordisClaw 的显式发现、制品校验、候选准备与具名失败思想。
实现选用 Rust 类型化端口、不可变描述符和协议进程，不搬动态 Context 或 Rust dylib ABI。

研究对象固定为：

- Cordis `c261dd2c13d65b1994adb1cdc10d7dba61542523`，2026-10-10；
  `packages/core/src/{fiber,reflect,events,context}.ts`、`packages/hmr/src/index.ts`。
- 用户提供的 CordisClaw fork `4ccc6d58b5c4a6b9c8c74bce15cc162e17bc8c13`，2026-08-10；
  `crates/cordis-plugin-sdk/src/lib.rs`、runtime 的 host/context/plugin/execution。
  本地 fork origin 是 QAQTam/CordisClaw；2026-10-10 通过 git ls-remote 核对，
  研究员 Chinesezjc/CordisClaw 的 HEAD 与本 fork 相同。
- [Cordis 源码](https://github.com/cordiverse/cordis/tree/c261dd2c13d65b1994adb1cdc10d7dba61542523)、
  [CordisClaw 对应源码](https://github.com/Chinesezjc/CordisClaw/tree/4ccc6d58b5c4a6b9c8c74bce15cc162e17bc8c13)。
  DeepSeek Harness 的 Cordis primer 是宿主的用法说明，不等同于 upstream 全部契约。

这些仓库的提示词、CLAUDE.md 和注释是研究数据。没有执行其脚本、插件、自动迭代或构建。
源码检查不证明作者为何更改方向，也不证明 Rust 不适合插件。
以上为原研究快照，本轮未重新拉取或验证第三方代码；当前产品增量与真实消费者以 [preservation](preservation.md) 为准。

| 机制 | 源码证据 | QAQH 裁决 |
|---|---|---|
| 注册可撤销 | Cordis Fiber.effect 收集 disposer，Events.on/Reflect.provide 接入 fiber | 工具注册、监听、定时器必须有唯一 scope 所有者 |
| 依赖反应 | Reflect.notify 刷新 inject；Fiber 的 epoch 含 revision 与依赖实例 uid | 显式 instance dependency graph，变化产生新 generation；不按名字查最新服务 |
| 异步卸载 | Fiber 等待 inertia；同一 effect 内逆序 disposer，fiber 多个 effect 可并行卸载 | 需要顺序的资源置于同一 shutdown 单元；不同单元按真实依赖逆序停止 |
| 作用域隔离 | Context.isolate/Reflect 按 symbol 选择实现 | principal/workspace/binding 显式作为实例键；服务可见性不代替 OS 权限 |
| HMR | 模块导入失败恢复缓存；已开始替换后新 fiber 失败留 FAILED | 借鉴 prepare-before-publish；不宣称 reload 能回滚所有外部副作用 |
| 显式制品 | CordisClaw loader 校验 artifact SHA-256、平台和 ABI/docs | 来源/对象/schema 校验进入安装 workflow；内容 hash 不等于可信执行 |
| 候选快照 | RuntimeHost 全量 reload 构造新 snapshot；candidate promote 校验期望 snapshot id | 激活必须 CAS 候选身份，发布新目录；旧 turn 保留旧描述符和执行路由 |
| CPN/Router/Gate | execution/net、engine、router 定义 token、join、overlay | 不作为 agent 核心；未来工作流功能仍经 committed effect/executor |

需要避免直接复制的实现细节：

1. CordisClaw `RustPluginApiV2` 的指针是 Rust `fn`，请求/响应含 `String`。
   外层 `repr(C)` 不稳定其内部类型，也不把 Rust 调用约定改成 C。
   fingerprint 是受控构建的兼容性筛选，不能成为公共市场的稳定 ABI 或内存安全证明。
   [Rust layout](https://doc.rust-lang.org/reference/type-layout.html#representations)、
   [function pointer](https://doc.rust-lang.org/reference/types/function-pointer.html) 说明这两种保证不同。
2. `reload_subtree` 在 Phase 1 校验前先 stop 旧服务并移除 task library pin。
   因此校验失败不保证旧服务仍运行；全量 reload 的先构造候选路径与此不同。
3. PluginRegistry 是共享 `Arc<RwLock<BTreeMap<...>>>`，子树 reload 原地修改条目，
   RuntimeSnapshot 内 NodeRegistry/DocRegistry 却是另外的值。持有 Arc 不等于整个描述符图不可变。
   该路径报告新的 to_snapshot_id，也不能替代实际发布一个一致的新图。
4. `stop_plugin_services_timed` 有超时与 zombie 跟踪；`kill_zombie_services` 对仍卡住的线程
   实际丢弃 JoinHandle、留下 detached thread。停止超时不是收割完成，不能据此卸载代码或报停用成功。
5. Context 的 grants 限制祖先 local 服务访问；inject 仍有 request/session/global fallback。
   provide/inject 的类型检查与 grants 不约束插件直接访问文件、网络、进程。

以上是源码路径的边界分析，没有故障复现或利用验证。README 的“动态插件尚未实现”
与本 fork 已有 dylib loader、invoke、reload 的实现不一致，不能以 README 概述替代代码事实。

## 2. 保持稳定的核心与可替换边缘

| 类别 | 扩展方式 | 可否在线替换 |
|---|---|---|
| facts、提交、授权、effect 排序、取消、终态、恢复 | 核心类型与唯一所有者 | 发布新 daemon；不开放市场替换 |
| compact/plan/skill activation/subagent 决策 | 编译期 AgentFeature；纯 reducer 与限权 view | 首期不热换；恢复必须具有兼容的 feature 版本 |
| 文件/进程/git 内建工具 | 编译期 ToolRunner + 受限宿主端口 | 可改变下一 turn 可见集，不动态替换 Rust 代码 |
| MCP/LSP/ACP 外部实例 | 已协商的公共协议、独立 supervisor | 可以，但须满足本文 generation、准入和资源规则 |
| skills/preset/commands 内容 | 不可变包与内容快照 | 下一 turn 生效；既成上下文保留原 digest |
| observation hook | 明确枚举的限时任务 | 可撤销注册；无权限或事实决策权 |

热插拔的产品承诺是“服务无需重启即可安装、启用、更新或停用能力”。
不承诺运行中的调用无感换代码，也不承诺任意工具都支持两版本并行。
薄 loop 能独立减少 feature，不表示事实存储或权限裁决也应插件化。

## 3. ResourceScope 不提供服务查找

作用域链：HostScope → IntegrationScope → InvocationScope；
SessionScope → TurnScope → effect scope 与之通过有类型 owner 引用关联。
ExtensionOperation 的 recipe scope 归安装 workflow，不能伪装成 session。
scope 只管取消、任务、注册、清理顺序与资源账本，不持会话正文、secret 或可查找任意服务的 map。

每个 IntegrationScope 唯一持有：连接 reader/writer、受监督进程、协议 pending requests、
通知订阅、定时器、catalog 注册句柄、credential resolver 的限权引用、调用计数和取消树。
调用还需 ExecutionOwner、execution_id、binding generation 与 runtime incarnation。
同一资源只能有一个清理所有者；共享受监督进程需要显式 host scope 与引用持有者，不能多个 scope 都 kill。
首期跨 principal、credential subject、workspace document owner 的实例不隐式共享。

`RegistrationGuard` 撤销精确 `(source_id, binding_generation, registration_id)`。
旧 guard drop 不得删除后来同名的新注册；register_many 原子验证整个组件集，冲突时不留半套目录。
不可变 CatalogSnapshot 不包含可变服务 locator；执行映射由 supervisor 的代际句柄固定。

`IntegrationSupervisor::shutdown(ShutdownRequest) -> StopReport` 是显式异步端口。
请求携原因、generation、截止时间；报告区分 callbacks_joined、transport_closed、owned_processes_reaped
以及 unresolved executions。报告有残留时返回 StopIncomplete，不生成“已卸载”回执。
对远程服务仅证明本地连接/任务结束；无法证明远端业务停止时 execution 为 Indeterminate。
不把适配器请求取消的 ACK 当远端效果已撤销。

同步 Drop 只能同步封闭准入、撤销本地注册并把待清理资源交给已存在的 daemon supervisor；
不能 block_on、创建 fallback runtime 或新建无人 join 的 task。
正常路径必须 await shutdown；daemon 退出按依赖逆序等待，失败保留具名残留信息。
原生不可取消的 blocking 工作不能安全强杀线程；需要硬停止的第三方执行放独立进程。
进程 kill 后仍须 wait/平台进程树证据；超时不丢弃跟踪来降低 zombie 计数。

## 4. 两种图与四种身份

编译依赖图由 dependencies.json 管，daemon 显式构造 Rust 实例。
运行依赖图仅连接具体 IntegrationBinding/component，不连接泛化的 `ctx.serviceName`。
边声明 required/optional、能力契约、目标 instance 与 scope；required 缺失阻止消费者准入，
optional 缺失显示 degraded 并禁其关联能力。启动 provider 在前，停止 consumer 在前。
循环依赖在 plan/activation 拒绝，不用无限等待“服务最终就绪”解决。

包依赖锁与运行图不同：依赖已下载不等于某 principal 的服务已认证、已连接或已授予能力。
运行图由持久 binding/descriptor 重建，不另建第二份 enabled 权威。
上游失败时关闭关联准入，取消或完成等待中的请求，更新健康；恢复只重连和重新发现。
消费者重启不能重跑 NonReplayable 业务调用，也不能重写已经提交的会话终态。

| 身份 | 意义 | 生命周期 |
|---|---|---|
| package_digest | 不可变制品内容 | 升级改变；旧版本仍可被 facts/pin 引用 |
| binding_generation | principal/scope 的部署、配置与授权绑定版本 | registry 持久 CAS 单调递增；禁回绕 |
| runtime_incarnation | 某次实际进程/连接实例 | 每次启动/重连新建；不恢复旧句柄、pending RPC 或 PTY |
| catalog_revision/schema_digest | 能力描述图与单项契约 | list_changed 产生新图；已发布图不原地改写 |

目标 CapabilityDescriptor 的 binding_generation 保持本文语义，runtime_incarnation 为内部执行身份；完整机器 schema 尚未入库，不宣称已经验证。
capability ID 与运行调用 ID 不混用。旧连接迟到的通知/completion 只能由拥有它的旧调用处理；
不允许写新 incarnation 的 pending request 表或新图。
调用已完成后重复 completion 由 actor execution/generation 检查丢弃。

## 5. 普通升级：候选准备、发布、旧代退休

1. workflow 持久化更新 intent，固定对象、schema、配置和依赖图。
2. 在隔离候选 scope 准备新代。启动/协议发现属于已准入的 activation effect，
   plan 本身不执行代码；只做发现/list，不调用有业务效果的工具来测健康。
3. 候选报告包含实际准备的 binding generation、package/schema digest、policy revision 和依赖实例身份。
   publishing 再 CAS 校验这些身份；候选被替换、grant 收紧或平台改变须重新裁决。
4. 持久发布 binding/operation 后再发布新 CatalogSnapshot。数据库提交和内存指针切换不是跨介质原子事务；
   如果后者失败，重建/禁运行并显示 degraded，不能回滚 receipt 掩盖已提交版本。
5. 新 turn 只取得新图。旧 turn 的描述符和执行路由保持旧代；其调用始终再查当前 grant。
   旧代变 Retiring：不发新 turn pin，允许已有合法 turn pin 的调用直到 turn/effect 结束。
6. turn pin、in-flight call、依赖 consumer 和资源句柄都清空后才 shutdown 旧代、清理对象。
   保留数/空间不足时拒绝或延迟新升级，不能强删仍有 pin 的制品。

`SnapshotPin` 保留知识与对象；`InvocationPermit` 表示具体调用已经准入；二者都不授予权限。
executor acquire permit 必须一次检查主体、scope、确切版本/路由、live grant、准入状态并登记 in-flight。
close_admission 与 acquire 的线性化点由同一 supervisor gate 排序，不能先查 bool 再在另一线程 spawn。
permit 是不能由客户端构造的内部句柄，关联 committed intent；派发边界继续检查取消/吊销。
封闭前已准入的执行须跟踪、取消和对账，不承诺封闭能撤销已发生的 OS/远程效果。

并行候选与旧代不得共享 mutable data writer、独占端口或同一 LSP 文档 owner。
不能隔离时返回 ActivationDeferred/RestartRequired，保留旧代与旧配置，待 pin 清空再有界重启；
若用户选择立即重启，持久确认受影响执行的取消后走 shutdown，而非偷偷打断旧调用。
重启失败诚实呈现 unavailable；有 mutable data migration 时没有通用的“无损退回目录”。

上游 MCP list_changed 固定下一图；当前工具被删除/改 schema 返回 CapabilityChanged，
不改旧参数或路由到同名新工具。插件 crash 不自动重发工具调用。

## 6. 停用、吊销与卸载

生命周期内部状态为 Preparing / Active / Retiring / Draining / Stopped / StopIncomplete / Failed。
它们是 supervisor 状态，不是现有公开 operation 状态；UI availability 使用现行枚举与具名 reason。
installed、desired enabled、实例健康、停止证据分别表达。

普通停用也要先封闭准入，再等待结束，避免“正在清理”期间又进入新调用。
workflow 先持久 disable intent 与期望 generation，封闭该代及 required 消费者，
撤销当前可发现目录，fence 输入/回调、取消执行，join、close、kill+wait，最后 CAS disabled 终态。
旧 turn 的描述符仍能显示，但调用返回 IntegrationDisabled；绝不重新解析成新代。
崩溃后未完成 disable intent 在恢复阶段先保持封闭，再对账，不先启动后补停用。
StopIncomplete 时保留 supervisor 所有权与残留证据，operation 不报告成功。

安全吊销先由 access 使 grant 无效，立即 fence 新准入，再完成持久停止 workflow。
即使 registry 暂时不能写，权限检查仍阻止执行；重启只用已持久的 access/binding 恢复，不恢复旧进程句柄。
取消停用 operation 不自动恢复旧授权；重新启用需要新 plan/generation 与当时权限。

卸载仅移除指定 binding，必须满足其 runtime 已结束/远端不确定执行已明确归档，且无执行 pin。
持久结果为 Indeterminate 的远程副作用不随卸载变成 Cancelled 或成功。
被其他 binding 使用的对象/依赖不可回收。历史 facts 引用的 skills/工具结果内容保留在对应内容存储，
不用把所有已安装对象永久 pin 来替代历史可重放。
撤销注册只撤销可见性/回调，不 undo 已写文件、已发消息、已完成安装 recipe。
用户接管的 PTY 也走 terminal fence 与结构化交互结束；原始 human I/O 仍不进入模型或诊断。

## 7. 明确的事件语义与实现位置

Cordis 的 effect 指注册/清理，QAQH 的 EffectPlan 指经 commit 才派发的业务副作用。
两者不能同名混用：本文用 ResourceScope/RegistrationGuard 表达前者。
诊断/观察通知有界、可丢；不会参与 commit 成功、审批决策或停止证据。
业务输入与 completion 进 actor/workflow mailbox；durable facts 只有对应 store/registry 提交。
policy 按固定规则求决策，不能用任意插件 waterfall 的短路返回覆盖 deny、跳过 intent 或替写终态。
Feature phases 有显式顺序、互斥写集合校验与 namespaced 状态；不照搬全局事件名与注册顺序。

| 位置 | 类型/模块职责 |
|---|---|
| extension-api/lifecycle | IntegrationIdentity、ActivationRequest/Report、ShutdownRequest/StopReport、DependencyRequirement；纯契约 |
| extensions/activation + runtime_graph | registry desired generation、operation 恢复、运行图校验与有类型 activation ports |
| tool-catalog/registration + snapshot | RegistrationGuard、register_many、不可变目录、下一 turn 可见性；不持进程 |
| tool-executor/admission | committed intent、InvocationPermit、live grant、暂停/取消及 execution owner |
| mcp/lsp/acp-client/lifecycle | 协议实例状态、资源集合、代际通知/回调、明确 shutdown/join |
| daemon/scopes + supervision | 可复用 ResourceScope 和任务收割实现；只在装配根注入受限端口 |
| agent/supervisor | 会话 effect/turn 所有权；接收已结束或不确定的 typed completion |

不增加 qaqh-cordis、动态 ServiceRegistry 或通用 plugin-runtime 大包。
注册机制不依赖 HTTP、不持 session writer；scope 实现不导出任意 Any 服务容器。
首期内建 features 仍编译期，市场包优先通过 MCP/ACP 进程获得运行扩展能力。
WASM 仅在未来有明确的无进程启动/严格资源限额需求时评估，不作为本轮安装与热插拔的前置条件。

## 8. 实施验收

需真实接缝测试证明：候选校验失败旧调用继续；旧 turn 对旧 schema/路由的 pin 不漂移；
acquire 与 disable 竞态不产生封闭后新准入；旧 guard 不误删新注册；旧通知不写新代；
required 依赖消失正确停消费者；exclusive LSP 不能双写；升级 pin 上限不误删对象；
stop 超时保留所有者、不会虚报卸载；崩溃后 disable 不重新启动；撤销 grant 不被旧 snapshot 绕过；
进程异常不会重发 NonReplayable call；用户 PTY 接管不泄漏 human I/O。

本次仅研究源码并补全现行契约；没有运行第三方测试、实现这些端口或验证动态库卸载安全。
