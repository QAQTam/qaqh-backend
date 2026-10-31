# SessionOwner 独占任务集合：设计与业务影响审查

日期：2026-10-31。基线：当前工作区，包括已有流式展示、工具闭环修复及本轮 CPU 忙等修复。

**状态：设计完成，隔离原语探针通过；未实施生产迁移，未证明新 loop 的端到端业务正确性。**
本文细化 [RLC §4.3、§6.1](spec-ringing-v2-agent-loop-cutover.md) 的任务所有权和完成归集，不另建一套执行规范。
RingingV2 协议与命令语义优先；不新增命令转换协议、状态权威、运行时、actor 框架或兼容执行链。

## 1. 裁决和范围

采用一个 SessionOwner 独占 SessionAgent、Supervisor、命令接收端和 SessionCommit。
Supervisor 使用 Tokio JoinSet 集中收集任务，宿主直接处理完成；删除逐任务 collector 与共享 Supervisor 锁。

本次只精简调度和生命周期。既有 provider/tool runner、授权策略、上下文构造、事实 schema 和 RingingV2 投影继续承担原职责。
不同时更换网络 SDK、异步运行时、工具实现或 provider ID；不扩大为另一次命令目录重构。

预期收益是每个 effect 少一个 collector 任务，去掉这条路径的 Mutex/Arc 克隆和 completion channel 转发。
**不是每个 effect 从此只有一个总任务**：当前 provider/tool runner 内部还有 spawn_blocking 及等待其结束的异步包装，必须保留。
没有依据承诺网络响应变快、固定 RSS 或确定百分比的额外性能收益。

## 2. 所有权和落点

```text
RingingV2 已校验输入
       │
有界 command / control lanes
       │
SessionOwner（host.rs，唯一驱动循环）
 ├─ SessionAgent：业务决策和有界工作队列
 ├─ SessionCommit：唯一 writer、canonical 状态和投影
 └─ Supervisor：JoinSet + 在途身份/预算 + 待结算结果
       │                         ▲
       └─ 已提交 capability ─ runner 完成
```

- `host.rs` 装配及持有完整 owner；`SessionAgent` 不再存 `Arc<Mutex<Supervisor>>` 或供自回投使用的 mailbox handle。
- 命令、控制、结果 handler 在每次调用期间通过独占借用访问同一个 Supervisor；不复制会话可变状态。
- `supervisor.rs` 保留 committed capability、actor incarnation、generation、digest、重复派发和结算凭证的检查。
  改的是等待方式，不是执行授权模型。
- `JoinSet` 中只放现有 runner 的 future。业务身份由宿主保存的 Tokio task ID 索引关联，不能只依赖任务正常返回携带的 ID。
- 在途记录保存原 turn/input、完整 EffectIdentity、dispatch 时的配置/模型/工具策略引用及资源许可。
  task ID 是进程内路由索引，不是新的业务 execution_id，也不进入持久协议。
- 完成后结果进入同一 owner 的待结算记录；只有提交确认后才能移除。任务集合不是历史缓存。
- 生产路径只剩这个循环；替换现有 `driver::run` 时同步更新真实消费者，不留第二个运行路径或调度适配层。

当前源码依据：
[共享 Supervisor](../crates/qaqh-agent-runtime/src/session_agent/agent.rs#L153)、
[逐任务 collector](../crates/qaqh-agent-runtime/src/session_agent/agent.rs#L316)、
[host 装配](../crates/qaqh-agent-runtime/src/session_agent/host.rs#L164)、
[现有 driver](../crates/qaqh-agentloop-core/src/driver.rs#L39)。

## 3. 一次执行的顺序

### 3.1 派发前

1. 预留该 effect 的在途数量、结果保留字节及必要执行许可；失败则不提交执行意图。
   对已经 InputAccepted 的输入，这是有界待派发，不得改判为未接纳，也不能丢弃。
2. 按既有 SessionCommit 屏障提交意图，取得一次性 CommittedExecution。
3. 注册原 owner、配置和预算，派发 runner，将 Tokio task ID 关联到上述记录。
4. 返回事件选择，不在 handler 内 await 完整模型请求、工具或子代理完成。

现有 `start_round` 和 `dispatch_tool` 已有“结果预留→commit_with_effect→派发”的主体顺序，迁移必须保留。
`commit_with_effect` 对重复批次不再签发 capability；不能把重建事实当成重新执行许可。

### 3.2 完成及续轮

```text
join_next_with_id / JoinError::id
  → 取回原执行记录
  → 立即保留结果与旧预算
  → 校验 owner/turn/input/incarnation/generation/digest
  → 仅生成并提交这一结果的业务结算
  → 确认 receipt，移除已结算结果，释放旧许可
  → 再推进待执行工具、下一 round 或下一个 queued input
```

**结算函数不派发后继。** 将 `finish_turn`、`settle_round_progress` 中的立即续跑改成“记录可推进工作”，
由 host 在旧结果完成结算后推进。未拿到预算时保留工作位置，等预算释放或新事件唤醒；禁止自旋重试。

这不是额外的 workflow 图：只是把现有函数中“提交、清理、派发”的执行顺序拆清楚。
后继派发失败不能使一个已确认的旧结果重新结算或执行。

当前 `completion` 是 `settle_completion` 返回成功后才 acknowledge；但 `settle_tool` 会调用
`settle_round_progress → next_round`，`finish_turn` 会调用 `start_next_turn`。
因此，若仅换 JoinSet 并把容量收紧到 1，后继可能在旧许可仍占用时遭遇 Capacity，进而退出 loop。
源码依据：[completion](../crates/qaqh-agent-runtime/src/session_agent/agent.rs#L1716)、
[工具结算](../crates/qaqh-agent-runtime/src/session_agent/agent.rs#L2030)、
[finish_turn](../crates/qaqh-agent-runtime/src/session_agent/agent.rs#L690)。

### 3.3 提交失败

- NotCommitted：保存原结果、预算及确定的结算批次身份，允许重试结算；不重新调用 provider/tool。
- Unknown/Fenced：停止新业务派发，保留完整 owner 和结果，按既有 journal 对账，不自动恢复 Ready。
- handler 返回错误时，将完整 owner 的所有权交给 registry 的恢复位置；不能只保留错误字符串。
  未结束的 runner 仍需被收割，或随完整 owner 显式接管，不能因函数返回而 drop JoinSet。
- registry 的 `is_dead/is_fully_stopped` 必须区分“驱动失败”和“所有执行已结束”，防止自动 respawn 夺取旧 writer。

当前 `RetainedDriverFailure` 实际只存 session、input_kind、error、phase 等描述，没有存 handler/mailbox/结果。
删除 observer 的 Arc 后，如果失败路径不同时改造，原先可能仍存活的监督数据会更早释放。
这是生产迁移的硬阻断项。
依据：[host 错误分支](../crates/qaqh-agent-runtime/src/session_agent/host.rs#L206)、
[提交 fencing](../crates/qaqh-session/src/session_commit.rs#L366)。

## 4. 调度、容量和取消

### 4.1 三类事件公平性

command、control、task completion 保留独立接纳预算和公平性。completion lane 可以由任务集合的就绪结果代替，
但它的可靠容量义务不能删除。

最终选择器按三类事件轮转轮询其 poll 接口；都 Pending 时才挂起，并注册相应 waker。
同类 lane 内保持 FIFO。对始终就绪的三类事件，下一类最多跨过两次其他类的选择；这不是墙钟延迟保证。
关闭的 channel 与空 JoinSet 不再作为就绪事件，避免 Ready(None) 循环。
不采用固定 `biased; control → completion → command`，否则控制风暴可能使正常输入饿死。

单次提交及上下文构造当前仍有同步 IO 和复制；因此“公平选择”不能证明慢盘时取消一定低延迟。
本次不通过新增每会话线程来隐藏这一问题。实际慢盘延迟必须单独测量，结果决定是否另做唯一 writer 的有界 IO 调度。

### 4.2 结果字节不能因删 channel 而无限制

新的 reservation 是“结果保留额度”，不再依赖 mpsc OwnedPermit 才有意义。
额度覆盖 running、已完成未收取、正在结算、提交失败保留的结果；在提交成功前不释放。
保留当前预留和硬上限要求，并核查 runner 在完整构造输出前的实际限制。

现有 `deliver_completion` 在结果超过 4 MiB 硬帧限额时仅记录日志并保留结果；没有可继续消费它的成功路径。
删除 channel 后既不能保留这个永久等待，也不能默默改成无限大结果。
若输出必须变成 blob/ref，需同步修改生产者、AgentOutcome/保留计量和结算消费者；不能临时加 JSON 中转桥。
**“实际输出构造有界”尚未由本次探针证明，实施前必须完成核对。**
依据：[超限分支](../crates/qaqh-agent-runtime/src/session_agent/agent.rs#L1056)。

### 4.3 不随本次变更引入跨父子的统一小信号量

父 agent 的工具可能等待子 agent 结束。如果父工具持有全局唯一许可，子 agent 请求模型又需要同一许可，会死锁。
本次保持现有会话／工具配额规则，不在这次替换中新增这种全局 logical-effect 许可。
将来若限制整机资源，要按真实资源及依赖关系设计预算；父代理的等待不能自动等价于占用子代理执行资格。

### 4.4 取消与关闭

取消仍经 RingingV2 原有目标语义进入；旧 turn、旧 interaction 的答复不准唤醒新工作。
请求取消后继续等待现有 runner，不能用 abort 外层包装替代停止证明。
当前 provider/tool 包装在取消后继续 await 内部 blocking job，这一行为必须保留。

关闭按“停止接纳／停止后继派发→取消在途→收割和结算→确认无 writer/runner/未决资源→清常驻态”完成。
有无法停止的执行时保留 closing/recovery owner，不能报 fully stopped、删除临时会话目录或重复打开 writer。
业务 Indeterminate 结算不意味着冲突资源已释放；新输入可以有界排队，但冲突工具不得绕过未决执行。

registry 必须在锁内标记 closing 并取出关闭等待凭证，释放全局 registry 锁后等待，再核对 incarnation 后完成移除。
否则父工具/子代理需要 registry 时，关闭等待会反向阻塞它们退出。
现有 `finish_shutdown` 的 task.abort 和 `close_session` 的清理顺序需要同批审查更新，不能只换 host。
依据：[runner 取消后等待](../crates/qaqh-agent-runtime/src/tool_job.rs#L638)、
[provider 等待](../crates/qaqh-agent-runtime/src/provider_job.rs#L660)、
[registry 关闭](../crates/qaqh-runtime/src/registry/lifecycle.rs#L188)、
[强制 abort](../crates/qaqh-runtime/src/registry/mod.rs#L481)、
[常驻清理](../crates/qaqh-runtime/src/service.rs#L252)。

## 5. 业务影响矩阵

“现有风险”表示源码分支或调用顺序可以构造反例，不表示本轮已在真实用户会话复现。

| 路径 | 直接换容器可能造成的失败 | 设计约束及验证状态 |
| --- | --- | --- |
| 模型→工具→模型、排队输入 | 旧结果未释放预算就续轮，容量 1 时失败；旧提交已成功却因后继失败重复结算 | 结算/释放/推进分离；原语探针通过，生产待迁移 |
| panic／迟到结果 | Err 不含正常返回值中的业务 ID；旧任务失败中断新 turn | 由 JoinError::id 取回宿主记录；所有结果先验完整身份。现有 JoinFailed 直接 interrupt 当前 turn 是需修正的现有风险 |
| ask／permission | 只在 Ready 收控制，或答复后丢失剩余 declared_tools | 三类事件全阶段可服务；保留 pending interaction、原声明位置和后续调度。现有 ask 基线通过，多工具审批组合待验 |
| 取消与新 turn | 清业务 map 后释放旧资源，或旧 completion 应用于新输入 | 保留原任务记录；旧结果只补原执行证据，不改变新 turn、context、usage；冲突资源按真实停止处理 |
| 配置热重载 | 旧请求按新模型/新权限归因 | dispatch 记录固定配置引用；当前 settle_provider 读取可变 provider_spec.model 的分支需审查修正 |
| 流式正文和工具参数 | 取消清 slot 后，旧 runner 的最后一次 progress 又发布，旧展示复活 | 保留现有流式通道及节流；attempt 级发布资格先失效再清理，过期更新不能重建 slot。生产端到端待验 |
| 结果超限 | 少一个 channel 却引入无界保留，或继续只 log 不推进 | 保留字节预算，明确超限结算／ref 路径；尚未证明输出构造上界，阻断实施交付 |
| 提交失败／恢复 | drop JoinSet 丢句柄；重试变成再执行；旧 writer 未退就 respawn | 完整 owner 移交，原批次对账；当前崩溃恢复基线通过，新失败移交未接入 |
| 父子 agent、后台进程 | 统一许可死锁，提前归还配额，删除仍被使用的目录 | 保留原 runner／配额／父子取消链；关闭等待移出 registry 锁；父子完整路径尚未动态验证 |
| 关闭／idle unload | driver 已结束被误认为全部执行已结束，提前 forget hub/图片或释放 writer | 真正 drain 后才确认卸载；不让每会话新线程充当收割器 |
| 标题、元数据、工作目录 | 从另一路打开 SessionCommit，打破单 writer；抢跑更新在途上下文 | 留在原 owner 命令边界；在途任务使用原快照，后续任务使用新配置 |
| RingingV2、bootstrap、SSE replay | 以 task ID 替换业务 ID，重连触发执行，改变 ACK/终态定义 | 保留现有 envelope、call ID、facts、cursor 和回执语义；此次不改 wire schema；接入后需命令／流式联合验收 |

此外，任务完成顺序不是模型工具声明顺序。并发逆序完成时必须核对原 call ID、上下文回填顺序及 opaque bundle；
本设计不通过改工具 ID 或丢弃尚未完成的同轮调用来简化收割。

## 6. 删除清单与保留清单

最终生产迁移删除：逐 effect observer、共享 Supervisor Arc/Mutex、自回投 completion channel、noop-waker/poll_once、
is_finished/yield_now 轮询、结算中直接续跑、仅有错误字符串的恢复占位，以及新 host 替代的旧生产 driver 路径。

保留：业务 phase、单次 capability、writer fence、完整执行身份、in-flight/settling 预算、pending interaction/声明队列、
原 provider/tool runner、真实停止／残留配额、canonical 及 RingingV2 展示链。
保留这些是已有业务责任，不代表保留新旧两个 loop。

库选择：使用已依赖的 Tokio 1.53.3 JoinSet；本次不增加运行时或 actor 库。
已有 watch 取消接口足够承接本次设计，不为此次精简顺便全仓换 CancellationToken。
JoinMap 的同键替换会移除旧任务的可收割结果，不作为默认容器；FuturesUnordered/分配优化库留待实际热点证明。

## 7. 本轮实际验证

### 7.1 当前生产实现的基线

命令：`cargo test -p qaqh-runtime --test session_agent_test -j 2`。
结果：5 passed / 0 failed；构建约 41.9 秒，运行约 1.2 秒。沿用既有测试，没有全 workspace 检查。

- session_agent_tool_loop_and_multi_round
- session_agent_ask_interaction_and_resolution
- session_agent_turn_lifecycle_and_shutdown
- session_agent_cancel_turn_and_recover
- session_agent_recovers_an_open_turn_after_crash

这些测试运行的是当前生产 loop，**不是本文目标 loop**，也不能覆盖多工具审批、长时间不停止的执行或所有客户端。

### 7.2 隔离设计探针

文件：[session_owner_design_probe.rs](../crates/qaqh-agentloop-core/examples/session_owner_design_probe.rs)。
命令：`cargo run -p qaqh-agentloop-core --example session_owner_design_probe -j 2`。
最终运行 exit 0，三个场景均 PASS：

1. 空任务集合等待时 Pending 且不自唤醒；控制分支抢先后，JoinSet 中的执行仍可完整收取。
2. 注入旧任务 panic 后可由 task ID 找回完整原身份；真实文件写入失败期间保留结果及容量，写入重试没有重跑执行；
   释放旧许可后容量 1 可接纳后继。stderr 的 `injected old task panic` 是该场景的预期输入，被 JoinError 收取。
3. blocking 工作已开始后，请求取消不等于工作结束；保持监督记录及容量，直到显式放行后完成 drain。

探针只使用已有 Tokio/core 类型；临时文件创建在独立 /tmp 子目录，完成后删除。
文件写入是原语实验，**不是 canonical 提交/Unknown 对账测试**。
探针不实现完整 SessionOwner，不验证三 lane 公平性、内存峰值、RingingV2 wire、审批、配额及父子生命周期。

## 8. 生产实施与交付判据

开发按依赖落地，发布时只有一条执行链：

1. 先落实结算/释放/推进的分离及完整结果身份；不先收紧许可制造容量 1 回归。
2. 替换 Supervisor 等待机制和 host，去掉 observer/共享锁，更新真实消费者与探针。
3. 同批落实失败 owner 的真实保存、关闭 drain、registry 锁外等待及 idle unload 的停止判定。
4. 解决超限输出消费与预算问题，固定在途配置，检查取消后 late progress。
5. 运行有针对性的业务验收：现有 5 项、容量 1、两工具逆序完成、审批后继续、cancel→新输入→旧结果、
   terminal commit 失败及 Unknown、关闭时工具不停止、父子关闭／临时目录清理、配置在途重载、RingingV2 流式重连。
   优先扩展现有探针和真实接缝，不为接口包装机械地铺测试。
6. 在相同负载下记录 idle CPU、task/线程数、结果保留字节、RSS 和控制延迟；未测的收益不报百分比。

交付前必须回答：旧结果是否仍有 owner？结果提交失败是否只重试结算？取消后是否仍有实际执行？
容量不足是否丢失已接纳工作？关闭是否在锁外等待？是否存在新旧双路径？
其中任何一项无法从实现和定点运行证明，就不能称为业务安全迁移完成。

## 9. 审查结论

**方案可行，但不能只把 HashMap<JoinHandle> 换成 JoinSet 后就上线。**
目前已识别的硬阻断项是结算顺序、完整身份隔离、失败 owner 保存、真实关闭／registry 锁、结果容量消费，
以及在途配置／late progress 的业务边界。它们都有明确的现有代码接缝，不需要引入新框架解决。

本轮只新增本文及隔离 example；没有实施生产迁移、重启 daemon、修改业务数据或新增依赖。

文档核对：
[JoinSet 的完成身份和取消安全](https://docs.rs/tokio/1.53.3/tokio/task/struct.JoinSet.html)、
[select 公平性](https://docs.rs/tokio/1.53.3/tokio/macro.select.html)、
[spawn_blocking 停止边界](https://docs.rs/tokio/1.53.3/tokio/task/fn.spawn_blocking.html)。
