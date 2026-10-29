# RLC：Ringing v2 唯一路径的 Agent Loop 硬切重构规范

- 编号：RLC-2026-10-30。
- 状态：**提案，供所有者审阅；不是实现完成报告。** 本轮只研究并制定规范，不执行生产代码清除或用户数据迁移。
- 裁决方向：替换执行体系，不维护旧 loop；保留已有产品能力、成熟算法和可验证的数据，不保留旧内部 API、旧线程模型、在线兼容桥。
- 适用范围：backend 的会话准入、模型/工具/交互/功能任务、上下文、事实提交、订阅、主/子 agent 生命周期，以及受影响的 Rust client/支持客户端接缝。
- MUST / MUST NOT 为验收义务；“建议”不构成交付前置。

> **一句话标准：一个 session 一个决策所有者；先提交意图，再派发任务；结果回投原所有者；事实同时驱动恢复、上下文和 Ringing v2 展示；切换发布中删除全部被替代运行链。**

## 0. 效力、范围与不做的事

### 0.1 唯一施工入口

本提案获批后，作为本次 loop 重构的唯一施工入口，取代下列文件中冲突的实施路线，而不是与它们并行建设第四套系统：

1. [ThinAgentLoop engineering](spec-thin-agent-loop-engineering.md)。
2. [ThinLoop / Timeline sunset](spec-thin-loop-and-timeline-sunset.md)。
3. [agentloop-spec](../new-tech/docs/v2-reset/agentloop-spec.md) 的分版本施工、临时桥期限及本次范围外的前置条件。

同时覆盖 [UAR](spec-unified-agent-runtime.md) 中本次主/子 agent 执行路径的重叠责任。**不采用其“必须等待 Ringing major 3”及另换 ExecutionFinished 词汇才能重构的前提**；其结构化结果、唯一 owner、可靠停止等性质继续保留。本次不定义第二个 execution_id：现有工具 execution_id 仍只表示一次工具执行；agent 的一次任务用 `(session_id, input_id)` 关联。

获批时须把上述旧文件的状态头改为被 RLC 取代/历史参考，并指向本文；提案阶段不伪称它们已失效。实施中发现合同需要改变，修改本文及对应合同测试，不新开一个矛盾 spec 绕行。

### 0.2 硬切的含义

- 允许在开发分支按依赖分多个提交构建新模块；**不发布新旧 loop 并存的产品版本**。
- 新模块不得回调旧 Loop/TurnEngine、持有 AgentState/RingContext，或以旧 worker 充当实际 executor。
- 没有 `use_legacy_loop`、旧 store 在线 fallback、shadow 双执行、同会话双 writer、长期 v2 facade。
- 最终发布只有一个构造入口和运行模型；新旧客户端不同步时拒绝不支持的组合，而不是降级执行。
- “不维护旧版”不等于删除用户数据。旧格式只允许离线导入；不允许新 daemon 在线打开旧写链。

### 0.3 收紧范围，避免另一轮架构膨胀

本次必须迁移现有的模型、工具、ask/permission/plan、手动 compact、skills、goal/todo、标题、主/子 agent、task/board 接线和必要观测。自动 compact 不顺便重启。

**不以** ACP 双向新功能、插件市场、动态 ABI/WASM、全量 new-tech crate DAG、数据库/分布式队列、Ringing v3、完整 UI 重做为前置。现有 MCP/LSP 能力须保留并走工具端口；尚不存在的协议能力不为“证明扩展性”临时实现一遍。

先在现有 crate 内形成真实边界；不造空 crate、全量 re-export facade、万能 service locator 或通用 workflow 框架。删除的是职责和旧路径，不以行数、文件数、命名现代化替代完成证明。

## 1. 研究基线：当前代码实际上是什么

### 1.1 方法与可信边界

2026-10-30 核对工作树 HEAD 为 `18a89dc7e64a9b77c1d1ca3e53b3944eb924d72d`，且已有大量未提交修改。本文以本轮读取的当前工作树为依据，不覆盖这些修改，也不把 HEAD 当成工作树完整快照。

已使用 CodeGraph CLI 的 `status / explore / callers / impact` 探索，而非仅凭文档猜架构。索引报告 611 files、16,488 nodes、53,166 edges，同时报告 5 added / 28 modified 未同步；**调用图只作导航，关键事实均用当前源码复核，不能宣称图完整或索引已刷新**。没有使用子代理。

CodeGraph 查询覆盖 Loop、actor.rs、engine_turn.rs、RingContext、RingingHub、CanonicalSessionStore、append_and_publish；其中 run_actor callers 显示主/子入口已共用 actor body。现状不是“完全没改造”，而是**协议/事实侧先行，执行所有权尚未收敛**。

### 1.2 源码证据与拆除责任

| 编号 | 已核对事实 | 源码定位 | 本次裁决 |
|---|---|---|---|
| B01 | Loop 持 SessionBundle、多 engine、phase、pending、ContextFlow、InjectionBus 和 CompactionPort | [loop_core.rs:154](../crates/qaqh-runtime/src/agent/loop_core.rs#L154) | 整套编排退出；不能把字段集合搬进新对象 |
| B02 | RingContext 暴露跨域可变 agent/phase/pending/stats/flow | [types.rs:476](../crates/qaqh-runtime/src/agent/types.rs#L476) | 删除共享功能 API，换受限 view 和 typed proposal |
| B03 | TurnActor 明确是观察旧 run_lap outcome 的 adapter，另有交互/input 集合 | [turn_actor.rs:1](../crates/qaqh-runtime/src/agent/turn_actor.rs#L1)、[159](../crates/qaqh-runtime/src/agent/turn_actor.rs#L159) | 删除追认镜像；有效 TurnCore 规则进入唯一 reducer |
| B04 | 命令容量实际是 4096，事件容量实际是 16384 | [loop_core.rs:132](../crates/qaqh-runtime/src/agent/loop_core.rs#L132) | 退出大同步通道，不延用旧文档倒置的参数 |
| B05 | registry 创建 reader 线程和 actor 线程；run_actor 还安装线程局部 workspace | [registry.rs:1198](../crates/qaqh-runtime/src/registry.rs#L1198)、[1220](../crates/qaqh-runtime/src/registry.rs#L1220)、[actor.rs:135](../crates/qaqh-runtime/src/actor.rs#L135) | 根治 owner/TLS 后改 async task，不只换 spawn |
| B06 | Worker event reader 既处理 Timeline，又做交互正文及业务副作用、compact/错误镜像 | [actor.rs:58](../crates/qaqh-runtime/src/actor.rs#L58) | 先给这些责任新 owner，再整条删除 |
| B07 | assistant 仍 ingest 到 MessageStore，并另 emit TimelineIntent | [parse.rs:86](../crates/qaqh-runtime/src/agent/turn_lap/parse.rs#L86) | 模型上下文与展示改从同一 committed response 投影 |
| B08 | canonical store 是同步单 fact append；提交后更新投影；注释明言未接管旧消息 writer | [store.rs:24](../crates/qaqh-session/src/canonical/store.rs#L24)、[92](../crates/qaqh-session/src/canonical/store.rs#L92) | 复用原语，不冒充已有异步批提交接口 |
| B09 | CanonicalLog 有 writer fence、fsync 和 events.commit.json 水位 | [log.rs:370](../crates/qaqh-session/src/canonical/log.rs#L370) | 保留耐久/单 writer 性质，扩成实际的批次屏障与对账 |
| B10 | ToolLedger 已有 intent/finish 去重、恢复、投影发布，但累计多类历史 map | [tool_ledger.rs:224](../crates/qaqh-session/src/canonical/tool_ledger.rs#L224)、[334](../crates/qaqh-session/src/canonical/tool_ledger.rs#L334) | 迁移规则到唯一 commit/recovery，不复制为第二个全量账本 |
| B11 | 存在 canonical 单流，也仍注册旧 timeline snapshot/SSE | [router:168](../crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs#L168)、[184](../crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs#L184) | URL 含 v2 不代表其内容不是 legacy；按 producer/cursor 判断 |
| B12 | 旧 hub 同时承载 Timeline、content、交互 pin 和 worker 活表 | [hub.rs:104](../crates/qaqh-runtime/src/ringing/hub.rs#L104)、[245](../crates/qaqh-runtime/src/ringing/hub.rs#L245) | 不允许未经迁移就整文件删除 |
| B13 | V2ProjectionHub 仍有每会话 ProjectionSet/replaceables；ConversationSnapshot 有增长的 turns/context Vec | [v2.rs:93](../crates/qaqh-runtime/src/ringing/v2.rs#L93)、[conversation.rs:112](../crates/qaqh-session/src/projection/conversation.rs#L112) | 新 v2 hub 也须预算与驱逐，不能只清理旧 Timeline |
| B14 | 工具收割超时会 drop JoinHandle，合成 CancelledOrphan | [tool_runtime.rs:1030](../crates/qaqh-runtime/src/agent/tool_runtime.rs#L1030) | 删除伪停止，保留残留任务 owner/配额/证据 |
| B15 | 默认 idle unload 为 0；旧 session manager 仍调用 LegacyWriterFacade | [config.rs:821](../crates/qaqh-config/src/config.rs#L821)、[manager.rs:250](../crates/qaqh-session/src/manager.rs#L250) | 生命周期与唯一 writer 同批替换 |

## 2. 对三份指定规范的分别裁决

### 2.1 spec-thin-agent-loop-engineering.md

**保留**：不保留旧 API/Facade、移除 AgentState/RingContext、异步化、工具/上下文独立、canonical 单一事实方向。

**不得直接照做**：

1. [§4.2](spec-thin-agent-loop-engineering.md#42-极薄-loop-驱动函数核心逻辑骨架) 在 `select!` 分支内 `step(state).await`，且只在 Idle 收邮箱。选中 step 后，模型/工具等待期间不再轮询该 select，取消和审批不因外层写了 select 就变得可响应。
2. [§3.1](spec-thin-agent-loop-engineering.md#31-事实账本抽象canonicalledger) 的 async append_payload/current_fact_seq 不是当前 store API；`Arc<Mutex<Store>>` 也不能证明唯一写权、幂等批次或不确定提交安全。
3. [§3.3](spec-thin-agent-loop-engineering.md#33-透明网关客户端gateclient) 同时返回最终结果与未消费 stream，没定义谁驱动网络和归集结果；全部网络故障“对 loop 透明”掩盖 partial/计费/重复请求风险。
4. 示例遗漏 TurnStarted、ToolCallDeclared、交互持久状态、response 完整性和可靠回投；unwrap/expect 磁盘写入不是失败规约。
5. [§1.1](spec-thin-agent-loop-engineering.md#11-现有-22011-行代码定性分布) 的“约 60% 原样搬运、400～600 行、半天”不是施工依据。ToolRuntime 同时提交账本、回填消息、产生子代理效果，不能原样包成 runner（见 B10/B14）。
6. O(1) 内存、每协程 2～4KB、<20KB 通道和固定 RSS 不是仅改 async/channel 就能保证的性质；每 lap 全日志重放可能把内存问题变成 O(history) IO。

**结论：保留目标，撤回该骨架、API 和工期/资源承诺；用本文真实邮箱、批提交、受监督 runner 代替。**

### 2.2 spec-thin-loop-and-timeline-sunset.md

**保留**：旧 Timeline 在线链应退出；大通道和每会话线程应消失；默认启用安全 idle eviction。

**修正**：

- 文中把两类通道容量方向写反，元素尺寸和 15.8MB/700MB 因果量化未在本轮测量；不能作为已复现结论（B04）。
- “Timeline 必须删”必须区分 **runtime 旧 TimelineAppender/Store/独立 SSE** 与 **session 的 canonical TimelineProjection**。后者是纯事实投影，名字相同不是旧权威，不能误删（[projection/timeline.rs:1](../crates/qaqh-session/src/projection/timeline.rs#L1)）。
- 没有 sessions.remove 不等于全部大正文永久驻留：当前已有 turn offload/journal budget（[timeline.rs:1261](../crates/qaqh-runtime/src/timeline.rs#L1261)）；但 hub.forget_session 确实没释放 timeline map（[hub.rs:203](../crates/qaqh-runtime/src/ringing/hub.rs#L203)）。要验实际释放，而非按一个 grep 下“纯泄漏”结论。
- 删除 hub.rs/sse.rs 前必须迁移内容读取、交互、权限复核和必要测试；冻结“任何触碰均违规”会阻止安全拆除。
- “注销路由”与“路由必返 410”不是同一动作。本方案删除路由，回框架 404；不维护永久墓碑 handler。
- 天数倒计时、35～50MB RSS、线程总增量恒 0、秒级无损唤醒均撤回，改为可运行的容量/恢复/性能闸门。

**结论：采用一次切换后的物理删除，不采用按日历拔线或机械执行删文件脚本。**

### 2.3 new-tech/docs/v2-reset/agentloop-spec.md

**优先保留**：纯决策→提交→派发→completion；Unknown 对账；完整 provider bundle；精确身份/generation；取消不等于停止；交互 CAS；离线迁移；观测不裁决业务。

**本次收紧**：

- 取消 S1～S4/v3.0 多版本过渡、旧写链适配和临时 facade 路线；拆分开发工作，不拆成长期生产双轨。
- 不要求本轮建立整套目标 crate、可持久插件平台或真实 ACP client/server；用现有功能证明端口。
- Ringing v2 是本次唯一公开执行/订阅入口协议，不再作为“以后随 major 3 退出的临时桥”；core 仍不得依赖 wire DTO。
- 不把所有 feature 设计成带 schema registry 的通用平台；只提取实际存在的功能，并为影响上下文/恢复的状态定义显式事实。
- 保留 G01～G13/G16～G20 中本次能力对应的反例；ACP 新功能不挡住旧 loop 删除。

**结论：它是三份中最适合作为安全合同的底稿，但需要删去多版本拖延和本轮无关工程。**

## 3. 目标架构：只有这一条执行路径

```text
Ringing v2 adapter / in-process caller / collaboration tools
                    │ validated ApplicationCommand
                    ▼
           AgentHost → AgentHandle
                    │ bounded mailbox lanes
                    ▼
           SessionAgent（唯一决策 owner）
              reduce(view, input)
                    │ proposal
                    ▼
        SessionCommit（本 session 唯一 writer）
        blobs → events batch → commit marker
                    │ Committed
             ┌──────┴────────────────────┐
             ▼                           ▼
      fold committed state       TaskSupervisor.dispatch
             │                   Provider / Tool / FeatureJob / ChildCommand
             ▼                           │ typed completion
 read/context projections                └────────► owner mailbox
             │
   Ringing v2 snapshot + 单条 /events + content
```

### 3.1 组件边界

| 组件 | 拥有 | 严禁拥有 |
|---|---|---|
| SessionAgent | 一个 session 的 committed 状态、有界 pending、reducer、提交协调 | HTTP/SDK、工具实现、全量 MessageStore、全局 current workspace、旧 engine |
| SessionCommit | writer fence、事实批次、blob 引用校验、命令回执和恢复屏障 | provider/tool 执行、UI dispatch、按订阅者回调业务 |
| TaskSupervisor | task/进程句柄、执行许可、取消树、completion 归集与收割 | 修改上下文、直接写会话事实、裁决 turn 成功 |
| ProviderRunner | 一次请求的完整归集、SDK/账号、受限重试、usage/attempt | 写 turn/tool 终态、调用新模型 lap、拼接不同 attempt 冒充一个响应 |
| ToolExecutor / ToolRunner | 准入策略、权限复核、沙箱/工作区/进程执行、结果准备 | writer、AgentState、另一份交互裁决表 |
| ContextProjector | 固定 committed head + bundle/config/skills 的可重建模型视图 | 从旧 messages 或 UI transcript 兜底、运行中按名字取最新配置 |
| 功能模块 | skills/compact/plan/goal/child 的受限 view→proposal | 任意 IO、Emitter、writer、共享可变状态 |
| Ringing v2 adapter/hub | DTO、纯投影、分页、订阅、live overlay、认证/可见性过滤 | 业务 writer、工具触发、通过 SSE 判断成功 |
| AgentHost/registry | handle/incarnation/准入索引及装配 | 持锁 send/join/文件 IO、第二套主/子执行模型 |

根/子 agent 共用同一个 SessionAgent 构造、恢复与任务监督入口，区别仅是 parent/policy/budget。actor unload、业务完成、显式 close、delete 是不同操作。task/board 保留其现有业务 owner，不升级为 loop 调度器；跨 owner 交付经显式幂等命令，不伪造跨 session 原子事务。

### 3.2 实际落点与依赖方向

- `qaqh-runtime/src/agent/`：重建为 input/state/reducer/mailbox/session_agent；**不长期同时存在 thin_agent 和 legacy agent**。
- `qaqh-runtime/src/execution/`：supervisor、provider/tool/job 的受限端口与调度；具体工具算法留原业务 crate。
- `qaqh-runtime/src/features/`：现有功能的纯决策；core 通过构造注入，不按工具名特判功能。
- `qaqh-session`：fact/commit/blob/context/projection。中立存储与 context 模块不得依赖 ringing/runtime；现有 client_v2 的 wire 组合类型迁到 adapter/client 所属边界，不能仅改注释宣布无 wire 依赖（[Cargo.toml:18](../crates/qaqh-session/Cargo.toml#L18)）。
- `qaqh-runtime/src/ringing/` 与 daemon：仅 v2 映射及装配。`qaqh-client` 同批删旧 timeline reader。

是否最终拆新 crate 不影响本次验收；拆包必须带实际消费者迁移。也不要求删除整个 qaqh-runtime 才算完成。

## 4. 决策、状态与邮箱标准

### 4.1 唯一状态机，而不是没有内存状态

运行态允许保存从 facts fold 得到的有限状态和待执行索引；“单一事实源”不禁止 reducer 状态或缓存，禁止的是另一套可独立写入/恢复的业务权威。

最小状态包含：session/log/head/incarnation、准入状态、当前 turn/round/context revision、固定配置/能力引用、有限队列与 open effects/interactions、必要功能状态。已完成历史按 ID/索引从 store 查询，不永久累计到 map。

```text
Recovering → Ready
Ready + 已提交 Trigger → Active
Active 子阶段：PreparingContext / AwaitingProvider / AwaitingTools / AwaitingUser
Active → Finalizing → Ready
任意状态 → Closing → Unloaded
提交/副作用证据不足 → Blocked/Reconciling（不能 reset 成 Ready）
```

phase 是当前视图，必须由同一个 reducer 决定；runner 不写 phase。一个 session 同时最多一个主动推进的 turn，独立 background job 受资源/上下文 revision fence。

### 4.2 Typed input 与纯决策

输入只分 ApplicationCommand、ContextPrepared、ProviderCompleted、ToolCompleted、InteractionRequested、FeatureCompleted、CommitCompleted、Lifecycle/Deadline；progress/live 走低可靠等级路径，不为每个 token 调 reducer+fsync。

`reduce(committed_view, validated_input, explicit_decision_context) -> Proposal` 不做 IO、不读全局环境、不生成随机数/读时钟。Proposal 只包含 facts/context changes/blob drafts/typed effects；ID、时间、预算作为输入。

实施只需要几个明确的 effect 变体，不建任意 JSON 的 `execute(method, payload)`、可脚本化状态图或通用事务引擎。正式类型由真实消费者锁定，不发布没有实现的“示意 Rust ABI”。

### 4.3 真实邮箱与背压

- actor 发起 provider/tool/context/job 后立刻回收控制权；不能在 select 分支内跑完整 lap 或等待 execute_batch。
- 正常命令 FIFO、completion、控制分 lane，最终由一个 owner 串行裁决，不是三个并行状态机。
- dispatch 前联合预留执行许可、可靠回投 slot 和 byte budget；终态/交互请求不能被 token progress 挤掉。
- 请求队列满或字节超额时在 accepted 前拒绝；accepted 后的排队是持久状态，不能靠内存 TTL 消失。
- cancel/revoke/shutdown 有独立保留容量与目标去重；限额和速率受控，不能形成另一条无限队列。
- progress 只能累计覆盖/限速/丢弃；最终 bundle、工具输出、签名、参数和 usage 必须可靠归集。大正文回传受校验的 durable ref，不塞满 mailbox。
- AnswerInteraction 在 AwaitingUser 时照常处理；普通新文字排队，不暗中替换或取消审批中的 turn。
- 公平轮询确保普通 lane 不被控制风暴饿死；subscription/diagnostics 永不反向阻塞执行。

## 5. 唯一提交、事实与上下文

### 5.1 提交合同

每个 session 只有 SessionCommit 持有 writer capability；runner/feature/hub/后台标题任务不得取得 writer。允许 store 内会话级短临界区和 OS 文件锁；禁止的是全局 LegacyWriterFacade 与锁内执行，不承诺“全程零锁”。

```text
proposal → 校验身份/head/refs/预算 → 准备并持久 blob
         → commit(batch_id, expected_head, digest, facts)
Committed    → fold → ACK/只读通知；另按已提交 effect 派发
NotCommitted → 丢弃 proposal，不发布成功、不派发
Unknown      → fence 该 session 新派发，用原 batch_id 对账
```

- batch 包括本次命令回执、必要上下文变化和执行意图；同 batch ID 同 digest 幂等，异内容冲突。
- events.commit.json 只推进到完整 batch 尾；日志写完但水位未确认不能当 Committed。batch 身份和 digest 必须耐久可查，不能只在内存保存。
- 保留文件锁、writer fencing、fsync、坏尾/poison、索引可重建规则；**不增加一套执行 WAL**。现有单 fact append 不能直接签发 CommittedBatch 证明。
- store 的阻塞 IO 在有界、可监督的 blocking executor；每 session 最多一个 in-flight commit，actor 以 CommitCompleted 输入推进，不能让慢盘占满 Tokio worker。
- commit 超时属于可能 Unknown，不等于 future drop 后写入停止。writer task/锁/许可仍有 owner；对账前不发第二个新 batch。
- 提交通知丢失由 canonical tailer 从 committed 水位补读，不重做 effect。observer 回调失败不能把已落盘事实伪装成未提交。
- supervisor 只接受提交端口构造的 CommittedEffect capability，不接受裸字符串自报“已提交”。同一 effect/generation 的在进程派发幂等；崩溃后只有 intent 而没有可信派发/结果证据时，按 §6.2 的恢复类别处理，不能把“已提交未见结果”自动当作“从未执行”。不承诺 OS/网络 exactly-once。
- blob 准备必须检查完整性、hash、scope 与平台耐久边界；不把当前 best-effort directory sync 当成已证明的跨平台强保证。未被 committed prefix 引用的准备 blob 可按安全水位回收，不能误删恢复/未决结算引用。
- 存储不健康时拒绝新业务成功 ACK；安全停止允许独立继续，恢复后补真实停止证据，不谎称已有持久终态。

### 5.2 最小事实集合与版本裁决

**内部存储升级与公开 Ringing 版本分开。** 本次采用一个显式新内部格式 `qaqh.session-fact/v3` 及其版本化 commit metadata，以容纳批次、response bundle、回执/功能恢复信息；不在旧 v2 enum 上无限堆 Option。该内部 v3 **不代表 Ringing major 3**。在线只支持新格式；旧 session-fact/v2 通过离线 importer 转换。

保留 InputAccepted / TurnStarted / ModelRoundStarted / AssistantBlockSealed / ToolCallDeclared / ToolIntent / ToolFinished / InteractionRequested-Resolved-Expired / TurnFinished 等清楚的业务词汇，不另造一套并行 turn engine 终态。新增内容只服务下面缺口：

| 记录 | 必须包含的语义 |
|---|---|
| 命令回执事实 | principal/session/command ID、body digest、接纳/业务结算、关联输入/终态事件；包括无 turn 的 queued 取消/拒绝结算 |
| InputAccepted / TurnStarted | 输入正文与来源、FIFO 序号、purpose、固定 config/capability/feature 引用；排队不冒充已经开始 |
| ModelRoundStarted / attempt evidence | turn/round、request/attempt、context head/hash、purpose 与预算；attempt 分离，partial usage 可保留 |
| ProviderResponseCommitted | 完整中立 message/parts + opaque replay bundle refs/digests、关联工具声明、finish/incomplete 判定、usage；是进入下一请求的屏障 |
| Tool / interaction facts | call/execution/generation、实际参数与授权/route 引用、结果/停止/副作用证据；交互 revision CAS |
| Context/feature facts | frozen prefix/skills digest/激活、compact 边界、必要 plan/goal 状态及 context revision；仅为实际现有功能定义 |
| TurnFinished / recovery evidence | 终态、输出 refs、usage/error/停止与不确定原因；一条不可改写的业务终态；迟到证据追加 reconciliation |
| Child delivery facts | parent/child/input 稳定关联、投递意图、源终态和本地接收 refs；父回执不能重新裁决 child 成败 |

每个新增 fact 都须定义字段校验、blob 引用枚举、fold、投影 slot 或明确无公开投影、幂等键和恢复动作；这些测试属于 RLC-1，不留给“以后完善 schema”。持久 queued input 的结算不能硬造空 turn。

### 5.3 ContextProjector 的底线

- 模型输入只来自固定 committed head 的上下文投影，绝不从 Timeline UI 树、live overlay、meta 或旧 messages/WAL 猜消息。
- 保存中立完整模型消息及 provider 必需的签名/opaque bundle、多模态、reasoning 类型/effort、正文+工具同时出现、call 顺序和结果映射。**展示块封口不等于 response 已完整可复用**。
- 只在 ProviderResponseCommitted 验证成功后允许该 response 的工具派发和下一 lap；partial/未知 finish 不当成功，不自动执行半个 JSON 参数。
- 前缀、系统环境、skills 和配置以 digest/revision 固定；恢复后可重建同样的下一请求。普通后续 turn 不偷偷改历史前缀。
- 准备上下文使用可重建的 checkpoint+tail、有界有效窗口和分页 blob，不能每 lap 读完整 events.jsonl，也不为 token 估算再序列化一份全量 JSON。
- 缓存按 session/log/context/config digest 命中；可删除后重建。需要保真的正文缺失时明确 blocked/unavailable，不回退旧存储或静默截断成成功。
- compact 只提交新的上下文边界/摘要，不删除 UI 历史；手动 compact 结果用 revision CAS，过期结果不应用。

## 6. 任务、重试、交互与取消

### 6.1 任务的唯一所有者

所有 provider、tool、compact/title、child command 和后台进程都有 owner：`session + input/turn + effect + generation`，并由 supervisor 保存 join/kill/wait 句柄与配额。

completion 校验原 owner、effect digest、generation、actor incarnation 与未结算状态；外部服务自报的 session/turn 不构成可信路由。重复同结果幂等；冲突结果对账；旧 incarnation 结果只作为原任务证据，不更新新上下文。

工具依模型声明顺序回填；实际运行允许有界并发。文件写入继续遵守同文件顺序、stale/LF/CRLF、preview/confirm、沙箱/授权和 workspace journal 规则。结果已观察而 terminal commit 失败时保留原结果重试结算，不能重新运行工具。

### 6.2 透明退避的边界

退避算法、SDK/账号刷新和 transport 错误分类归 ProviderRunner/qaqh-gate；loop 不写 sleep/backoff/回滚消息逻辑。但“技术细节透明”不等于“请求身份、费用和不确定效果透明”。

- 有限 attempts、总 deadline/预算、cancel-aware backoff；每次实际调用有 attempt ID。
- 尚未获得有效输出的可重试 transport 错误按明确分类处理；请求可能到达上游时不能承诺没有计费。
- 已有 partial 或可能触发上游原生副作用时，禁止把重试结果拼成原响应，禁止无条件重发。允许 continuation 必须是新 intent/context，旧 partial 可查且不能派发工具。
- 工具/委派恢复按 PureReplay / Reconcile / NonReplayable 分类；read-only hint 不是可安全重放证明。不能证明没执行则对账或 Indeterminate，绝不默认再跑一次。

### 6.3 交互

ask/permission/plan 的正文必须先耐久、请求 fact 提交后才发布。executor 只持 durable interaction handle 等答案；**唯一 pending/结论在 session fold**，无需再养 worker/daemon/hub 三套业务表。

答案经 actor 对 interaction ID + revision CAS；答案、超时、取消竞态只提交一个结论。原批准不能跨 execution generation 复用；吊销/策略变紧在派发前复核。挂起不占 mailbox 栈，也不占模型/工具运行许可；必要残留进程资源仍计入真实配额。

### 6.4 取消和停止

顺序固定：验证 caller/精确 target/generation → 封闭目标新派发 → 触发该节点取消 → 收割/kill+wait/远端对账 → 提交业务结论 → 释放可释放资源。

取消树 host→session→turn/effect→execution 单向触发；新 turn 新 token，不 clear 旧 token 复活任务。无 turn_id 的 wire cancel 必须在受理时绑定当时的精确目标，随后串行复核；不能对“处理时碰巧当前 turn”生效。

commit 进行中，独立安全控制端口仅可对已验证的现存执行目标停止/禁发，不能越过提交 owner 改 committed 状态；其持久记录待当前提交对账后排序写入。未知/未授权目标不能先设全局 flag。

取消 ACK、drop future、关 SSE、进程发 kill 信号、清 map 均不是停止证明。无法停止的 blocking task 保留 owner/handle/配额，标 StopIncomplete；外部效果不明为 Indeterminate。**业务结算与资源收割分开**，不能因报了失败就释放冲突资源。panic 也不得 reset-all→Ready 继续猜状态。

## 7. Ringing v2 对齐与 Timeline 终结

### 7.1 固定不变的协议骨架

继续使用 `/ringing/v2`、现有 envelope、per-session 单条 `/sessions/{id}/events`、bootstrap 和 content。command 转 application input，core 不认识 HTTP/lease/driver/wire enum。

- Reliable 来自已提交事实的确定性投影，cursor 为 `(log_id, fact_seq, projection_index)`；projection_index 沿固定 slot，不按 reducer 注册顺序生成。
- snapshot 使用同一 committed 水位，`END_OF_FACT=65535`；先 snapshot 后从其 cursor 续订，不能出现丢事实窗口。
- 只有 reliable 推进 canonical cursor；replaceable 有独立有界 revision/slot，ephemeral 不参加历史回放。
- 坏游标、log/epoch mismatch、溢出发具名 ResetRequired 并重建，不静默退回 0 伪装连续。
- SSE replay/read/renderer 重建不能触发模型、工具、followUp 或补业务事实。
- 订阅权限每帧/吊销信号复核，content 以 session scope 校验；哈希不是访问凭据。

依据：[wire envelope:199](../crates/qaqh-ringing/src/v2/types.rs#L199)、[cursor:15](../crates/qaqh-ringing/src/v2/cursor.rs#L15)、[fixed slots:49](../crates/qaqh-session/src/session_fact_v2/projection.rs#L49)、[client bootstrap→subscribe:157](../crates/qaqh-client/src/v2_stream.rs#L157)。

### 7.2 必须正视的合同缺口，不挂着 v2 名字偷换语义

1. **ACK**：当前 accepted 只保证进入正确 worker（[envelope.rs:11](../crates/qaqh-ringing/src/envelope.rs#L11)、[command.rs:654](../crates/qaqh-daemon/src/axum_server/axum_impl/command.rs#L654)）。新实现加强为命令回执/业务接纳已提交后才 accepted；accepted 仍不等于业务成功。取消命令成功仅表示请求已落实，不代替被取消任务的真实停止结果。
2. **命令状态**：保持现有 accepted/running/succeeded/failed/rejected 词表；目标业务 cancelled/indeterminate 等通过其业务事实/typed error 读取，不能凭空给旧 enum 填新值或把 Indeterminate 映成 Succeeded。必须有每命令的明确结果映射表。
3. **live 文本**：当前 ConversationDelta 有 sealed 等词汇，但没有通用普通回答 live 文本合同（[projection_event.rs:640](../crates/qaqh-session/src/session_fact_v2/projection_event.rs#L640)）。不能以删旧 Timeline 为代价把所有回答变成等终稿才显示。
4. **历史分页**：当前旧 timeline handler 从旧 hub/messages archive 读（[timeline_api.rs:99](../crates/qaqh-daemon/src/axum_server/axum_impl/timeline_api.rs#L99)）。bootstrap 全量 Vec 不等于有界历史加载。

因此本切换必须同批交付一个明确命名的 v2 能力修订 `agent_loop_cutover_v1`，由 open/能力合同及 Rust/TS 类型显式声明；支持客户端必须验证该能力。open 请求须显式声明支持该修订，服务端不能只在响应里加一个旧客户端会忽略的 bit；未声明/不支持者在建立可写会话前拒绝。它不是旧 loop 适配器，而是本次 coordinated cutover 的协议门槛；对不支持的组合具名 UpgradeRequired，不建立旧 payload fallback。

其合同必须在 RLC-1 锁定并包含：

- 现有命令逐项的 ACK/terminal/取消结果映射和可恢复回执查询。
- 普通 assistant/reasoning 与工具进度的 bounded live current-value 载荷：session/turn/request/attempt/block/kind、anchor fact、monotonic revision、累计有界文本或 chunk refs、显式 truncation/gap；不以可丢增量拼接作为唯一正文来源。
- live 锚定已提交 ModelRoundStarted/ToolCallDeclared 等身份；seal/response commit 后用耐久内容替换 overlay，不把每 token 写成 AssistantBlockSealed。断线允许丢 live，最终正文不能丢。
- canonical 历史页合同：同一 snapshot head、opaque page cursor、数量和字节上限、has_more/正文 ref；bootstrap 给有限首屏，不能另养历史权威。新增 canonical history 方法和 DTO 由 adapter 定义，旧 `/timeline` 不保留同形转换层。
- 新增载荷的 delivery/slot/版本校验、能力协商、支持客户端 reducers 和重连行为。能力 bit 不是允许任意破坏 envelope 或重解释旧字段的借口；无法在 v2 的具名能力修订内定义清楚的变化，先收敛设计或另行审批公开协议，不能实现后才解释。

本提案即对上述**有范围的 v2 协调修订**提出审批，不宣称未经审阅已获协议授权；不以“必须先造 Ringing v3”代替明确合同。

### 7.3 删除与保留必须精确

必须删除旧 runtime TimelineAppender/TimelineStore、旧 TimelineIntent producer、独立 timeline SSE/旧 cursor、消息归档重建 timeline、快照/offload/checkpoint 专属 worker 及旧 client TimelineStream。

旧 `/ringing/v2/sessions/{id}/timeline` 和 `/timeline/events` 注销，框架 404；支持客户端只用 canonical history + 单流。没有 410 兼容期，没有后台继续写但“不再显示”的 Timeline。

**保留或重写优化的是** `qaqh-session::projection::TimelineProjection` 所表达的纯 transcript read model；它与 conversation/control/resource/team 可以来自同一 fact，不是“双事实源”。保留 UI 时间顺序和历史能力，不保留旧独立 journal。

hub.rs 中 content/交互职责迁到 session-scoped durable content reader 和提交后的 pin/retention owner；worker 活表迁 host。新 content 路径不再先查临时 transport cache 找不到才猜正文，不能删 hub 时一起删权限。V2ProjectionHub 只持可驱逐投影/live cache，不变成新的巨石。

## 8. 生命周期、预算与性能

- SessionAgent 是共享 daemon runtime 上受监督的 async task，不为每 session 建 actor/reader 物理线程或独立 Tokio runtime。同步算法可走全局有界 blocking executor，不能在 task 跨 await 留线程 TLS。
- workspace、authorization、tool catalog、provider account 和 process host 用实例 scope 显式传递；不拿 Tokio task 恰好落在哪个线程作为会话身份。
- idle unload 默认 300s，检查周期默认 60s；有 queued/active input、pending interaction、in-flight commit、未交付 child result 或残留 runner 时不得当 idle 卸载。只读订阅不能永远 pin actor，投影 residency 独立管理。
- 卸载必须关闭准入竞争、确认无活任务/待提交、释放 fence/缓存/句柄，再从 registry 摘除；不调用业务 close 伪装 unload。并发新命令通过 host 的 incarnation/CAS 唤醒单个 owner，不能一边卸载一边旧 actor 接单。
- shutdown：停新准入→停新派发→取消/收割→结算或记录未决→关闭 writer；未收割任务不能提前报告 clean shutdown。

首版默认预算如下；是**拟实施的限制，不是当前测量结果**。调整须带压测证据，不许只调大掩盖死锁。

| 域 | 首版默认预算/规则 |
|---|---|
| 普通邮箱 | 32 items、合计 256KiB；单帧 64KiB，超大输入先走受限上传/ref |
| 控制 lane | 8 个不同目标待办、合计 64KiB；重复同目标合并，并保留 host shutdown 路径 |
| 可靠回投 | 32 slots、合计 256KiB；派发前预留，terminal frame 用有界 refs |
| 单 session in-flight | provider 1、tools 最多 8、feature jobs 最多 2；全 daemon 另设总量与每 owner 配额 |
| 已接纳未启动 input | 最多 32 个，正文耐久化；达到限额在接纳前拒绝 |
| live overlay | 每 active session 256KiB、固定 active slots；超额用 refs/gap，不扩大成全历史 |
| 历史页 | 默认 30、最多 200 条，编码后最多 1MiB；大正文只发 ref；空页不能 has_more=true |
| projection/read cache | daemon 全局默认 64MiB，按字节 LRU；无订阅/访问后 TTL 300s；active 临时 pin 仍计预算 |
| replay | 分页/流式 committed reader；同时限制帧/字节，超窗口 ResetRequired，不读全史到 Vec 后再截断 |

内存模型是 `O(active bounded state + effective prompt window + bounded IO buffers + capped caches)`，不是总内存 O(1)。模型上下文本身受窗口上限；上下文无法在预算内构造时明确失败，不偷偷压缩/截断。普通邮箱“32”不代表总排队内存只有 32 个 Rust enum 的大小。

性能验收用相同平台/allocator/构建/profile/config/fixture，记录 idle actor 增量、10/100 会话斜率、长历史峰值、线程/任务、queue wait/commit/control/stop/recovery 分位和残留。**不得承诺未测的 35～50MB RSS 或所有场景总线程增量恒 0**；必须证明 idle 会话没有专属线程、缓存总量受控且重复加载/卸载不单调保留 session payload。RLC-0 锁定实际性能阈值；没有预登记阈值/实测结果不得签性能通过。

## 9. 删除清单：按职责验收，不按文件名误杀

| 旧责任/入口 | 终态去向 | 删除证明 |
|---|---|---|
| Loop / SessionBundle / RingContext / AgentState | SessionAgent + 显式 ports/view | 编译图与生产构造无旧符号、无同等万能替身 |
| TurnEngine::run_lap / apply_outcome / TurnActor 镜像 | 唯一 reducer + effects | 普通输入到真实 provider/tool 走新链；旧递归/追认整条删除 |
| loop_dispatch_* / loop_outcome / reset_all_engines | typed application input + typed failure | 无 panic reset/clear-token 继续运行分支 |
| WorkerCommand / WriterEvent / PacedEmitter 回转 | application mailbox + 纯投影/live publisher | actor reader 不再存在；无 event subscriber 触发业务 |
| LoopChannels 的大 sync_channel、actor/reader 专属线程 | async lanes + shared runtime | 主/子所有构造和恢复接线无漏网旧 spawn |
| ToolRuntime 的混合 spawn/collect/ledger/backfill | executor/runner + supervisor + actor commit/context fold | 不再 detach 假报 Cancelled；文件/权限反例迁走 |
| MessageStore/WAL/PersistOp/SaveFull/MetaOp 的在线权威 | session facts/blobs + context projector | 新启动/恢复不读写旧消息权威；需要旧格式仅 importer 使用 |
| InjectionBus / loop_injection / 文本 SUBAGENT 标签解析 | 显式排队/目标化命令/child delivery facts | 原有消息/协作能力保留，无隐式拼 system 文本 |
| CompactionPort 轮询、title 后台直写、core token calibration | FeatureJob completion / 标题命令 / 可关闭观测 | stale compact、手动 title revision、usage 保真；遥测不裁决 |
| TimelineAppender/Store/旧 hub Timeline worker/旧 SSE/client | canonical read model + v2 单流/history/content | 旧路由不注册，旧 writer/reader/fixture 从在线依赖退出 |
| LegacyWriterFacade 全局锁、会话 TLS、全局 current manager | 每 session writer + 显式 execution scope | registry/状态锁内无 send/join/IO/回调；canonical 文件提交锁限于本 session 的存储屏障；不同 session 可独立推进 |
| child SSE collector / 以观察断流猜成功 / 多份 outcome | 同一 agent + committed child outcome/幂等交付 | 断流/blob 缺失不返回空成功，不按 child ID 混淆多次 input |
| PendingCommandStore 内存/TTL 作为业务权威 | facts 回执 fold + bounded 查询缓存 | 重启仍能对账，TTL 只淘汰缓存不淘汰 accepted 义务 |

旧目录中成熟 parser/沙箱/参数容错/文件 journal/顺序算法须迁出后再删；不要求保留旧文件名。**迁移了纯算法不算兼容旧 loop；保留一个调用旧 Loop 的 wrapper 才算。**

静态禁止清单同时覆盖直接依赖与别名包装；按生产模块检查，importer/历史文档中出现字符串不算在线残留。仅 grep 零命中不能代替依赖图和运行接缝证明。

## 10. 施工顺序：分步开发，一次发布

| 任务 | 前置 | 必须交付与删除 | 出口 |
|---|---|---|---|
| RLC-0 基线与合同冻结 | 提案获批 | 当前能力/脏文件登记、唯一 owner 表、请求金标、故障注入和性能阈值；锁定支持客户端组合 | 每项迁移有实际消费者和反例；无臆测的前端已切流声明 |
| RLC-1 存储与 v2 能力修订 | RLC-0 | 新 fact/commit 格式、批幂等/Unknown、完整 response/context、离线 importer；v2 ACK/live/history DTO/能力/错误映射及合同测试 | commit/replay/context 金标通过；未接完整执行链的数据根不公开可写 |
| RLC-2 唯一 actor 与监督 | RLC-1 | typed mailbox/reducer、实际 provider/tool/interaction、task handles、真实取消/结果回投 | 长调用中可处理控制；无旧 run_lap fallback；主/子共用入口 |
| RLC-3 功能与读侧收敛 | RLC-2 | skills/compact/plan/goal/title/child/task-board 接线迁移；canonical history/content/v2 live；显式 scopes/缓存驱逐 | 现有能力走新 owner；临时镜像、旧副作用 reader 可整条删除 |
| RLC-4 冷切与清除 | RLC-1～3 | 停写/导入/支持客户端切换；物理删除 §9 所有旧路径及专属测试/开关；旧规范状态同步 | §11 全部闸门有证据；唯一二进制路径、唯一 writer，无“下版再删” |

阶段出口是工程依赖，不是允许发布兼容版本的许可证。不能先切 store 再让旧引擎补模型/工具能力；也不能先删 guard 再等待新 owner。开发中任一未完成组合只在隔离 fixture 上验证。

冷切顺序：停接单/停派发→收割或记录未决效果→完整备份→staging 离线导入→校验 refs/因果链/请求金标→原子选择新数据根与格式身份→新 writer 启动→真实请求/恢复探针→仅发布新路径。

导入不复制活 task、审批 grant、PTY writer、连接票据或账户凭据。历史 intent 不进入 scheduler；缺 provider bundle/必要结果的历史只读展示并注明不能 resume。旧版本读取新根须明确拒绝；回退只用停写备份或已验证反向导出，不能让旧二进制直接打开新写入数据，也不能自动重跑外部副作用。备份保留策略显式制定，不以“不维护旧版”为由删除历史。

## 11. 验收矩阵与完成定义

所有测试记录关联具体提交/工作树差异、fixture、平台、命令/退出码和证据；区分已运行/未运行/平台不可用。旧机制专属断言删除，有效安全反例迁到新所有者，不靠删测试得绿。

| 闸门 | 必须注入的场景 | 通过标准 |
|---|---|---|
| A01 唯一路径 | 普通输入→模型正文+工具→下一 lap→终态，根/子分别执行 | 真实 SessionAgent/commit/runner；没有旧路径调用或镜像追认 |
| A02 邮箱 | 模型/工具不返回，普通/progress lane 满，提交取消/答案/查询 | 控制先于长任务结束得到处理；已接纳 completion 不丢、普通输入不无限饿死 |
| A03 提交 | blob/fact/batch marker 各点失败、kill、marker 成功但响应丢失 | 只有完整 committed prefix 可见/可派发；同 batch 对账不重做副作用 |
| A04 副作用 | 文件已写、tool finish 提交失败；重启、重复/冲突 completion | 结算原结果/证据，不重执行；未知明确 Indeterminate |
| A05 交互/取消 | 取消与答案/超时/完成并发；旧 turn/旧 incarnation 回投 | 一个持久结论，目标准确，旧答案不复活新派发 |
| A06 真实停止 | Rust blocking 不理取消、远端仅 ACK、自有进程 kill 失败 | 保留 owner/配额/残留；无 drop-handle 后成功 Cancelled |
| A07 模型保真 | 四类现有 provider 路径、opaque bundle/签名、多模态、前缀、partial | 重启后的下一请求金标一致；不完整响应不执行半工具、不冒充成功 |
| A08 工具能力 | 并行顺序、同文件写、LF/CRLF/stale、沙箱、preview/confirm、后台进程 | 原有效规则保留；backgrounded 与进程 exit 分开；拒绝不伪造执行 |
| A09 功能 | 手动 compact 迟到、skills digest、plan/goal、标题竞态 | revision/配置固定；功能状态可恢复，core 无工具名特判 |
| A10 协作 | parent 取消/child 完成、断流/blob 缺失、同 child 多次输入、task/board | 同一 runtime；结果结构化/可对账；不靠文本标签或空成功 |
| A11 v2 消费 | bootstrap 与订阅间提交、重连、同 fact 多 slot、reset、慢端 | 无缺口/重复业务；cursor 正确，overlay 不推进 reliable，不触发副作用 |
| A12 内容与历史 | 删除 transport cache、跨页/字节上限、大正文、跨 session 请求、吊销 | durable 正文仍可读；分页稳定；越权拒绝；无空页 has_more=true |
| A13 恢复/卸载 | 重复加载卸载、unload 与新命令竞争、pending interaction/child/残留 | 不卸载有义务的 owner；只启动一个新 incarnation；缓存真正释放 |
| A14 资源 | 10/100 idle session、长历史、慢 observer、关闭/破坏诊断 | 数量+字节预算成立；无每会话专属线程；符合预登记阈值，业务结论不变 |
| A15 删除 | 静态依赖图、构造入口、路由表、生产二进制、离线导入/回退 | §9 在线残留为零；旧 root 明确拒绝；无双 writer/旧开关/fallback |
| A16 支持组合 | backend + Rust client + 实际支持客户端的 live/history/审批/GenUI | 验能力修订；不兼容显式拒绝；代码生成通过不冒充 renderer 通过 |

最低验证命令（实施后执行，不表示本次已执行全量）：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

另跑模型请求金标、storage 故障探针、真实工具/审批/MCP 与支持平台的进程/沙箱测试、Rust/TS 合同及真实客户端消费测试。并发接缝使用隔离根/实例依赖和确定性信号，不用全局 HOME/cwd、OnceLock 或全部串行化掩盖共享状态。

**整体完成 = A01～A16 全部有对应实现与有效证据，§9 旧责任已经删除，且没有本 spec 的必需项被标成“未来兼容修复”。** 单个 mock、抽出 trait、减行数、前端连上 v2、编译通过，均不等于完成。

## 12. 本轮验证记录与下一步

本轮仅落规范。已实际运行以下现有基线测试：

```bash
TMPDIR=/home/qaqtamsy/Projects/qaqh-backend/target \
  cargo test -p qaqh-session \
  --test canonical_log_contract --test canonical_session_store \
  --test tool_ledger --test canonical_takeover_mock --offline
```

结果：**42 passed / 0 failed**（15 + 4 + 22 + 1）。首次使用默认 `/tmp` 因本执行环境只读而失败，改用工作区 target 作为临时目录后通过；这是环境修正，不是修复代码。构建仍报告既有 unused/dead-code warnings，本轮未修。

证明范围仅限现有 log/store/tool ledger 和手工事实投影/重开。特别是 [takeover mock](../crates/qaqh-session/tests/canonical_takeover_mock.rs#L47) 使用构造 ContentRef，手动追加事实且未走真实 provider/ToolIntent 执行/正文端点，不证明 response bundle、真实邮箱或新 loop 接管成功。

未运行本规范目标实现的 A01～A16、全 workspace fmt/clippy/test、真实模型/平台/前端消费、内存与延迟压测；本轮没有这些实现，不能宣告通过。未审阅外部客户端仓库，不能据此断言 Electron/TUI/WinUI 已完全脱离旧 timeline。

建议审批先锁定三件事：**单发布硬切、Ringing v2 具名能力修订、新内部事实格式+离线导入**。获批后只从 RLC-0 开始实施，完成一条全生命周期垂直链并删除旧 owner；不再对旧 loop 加一个“现代化适配层”。
