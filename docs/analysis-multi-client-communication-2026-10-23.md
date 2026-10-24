# 前后端通信中的多客户端限制审查

日期：2026-10-23。

目标：在已有授权和有限资源范围内，尽量让所有客户端都能读取、提交写入并恢复连接。不设计客户端优先级，也不要求无限队列或无限并行执行。

本次检查了 backend 的 daemon / runtime / client，以及同级 TUI、桌面客户端的相关消费者。以下区分当前实现与调整建议；没有修改协议或运行代码。

先澄清：**30 秒是租约 TTL，并非每 30 秒强制更换身份**。服务端建议每 10 秒续租，Rust SDK 实际每 5 秒尝试；漏续、过期或连续续租失败后，才重新 open 并更换 `client_session_id`。不合理之处是在线状态决定了读写资格及其恢复成本，单纯延长 TTL 不能解决这些耦合。

## 1. 全会话 driver 席位把多个可写客户端变成一个可写客户端

**当前实现**：`driver_gated` 包含整个 Conversation 命令族，以及 close/archive/delete、模式和技能等操作。有存活 holder 时，其他客户端被返回 `not_driver`。部分 service 写操作也有同样门控。没有 holder 时，服务端反而允许这些客户端写入。

证据：[命令门控](../crates/qaqh-daemon/src/axum_server/axum_impl/v2.rs#L964)、[service 门控](../crates/qaqh-daemon/src/axum_server/axum_impl/service_api.rs#L95)。

客户端也实现了同样的限制：TUI 在 bootstrap 后自动申请 driver，发送等写操作先经过本地只读检查。因此仅打开一个会话就可能申请独占权，其他端不能正常发送；仅删除服务端门控还不够。

证据：[TUI 自动申请](../../qaqh-tui-app/src/app/mod.rs#L2353)、[TUI 本地拦截](../../qaqh-tui-app/src/app/mod.rs#L2767)。

还有一个相关的实际错误：bootstrap 的 `can_claim` 用 `effective_holder != caller` 计算，其他活跃 holder 存在时仍为 true；claim 端点随后却返回 `driver_busy`。TUI 对 claim 响应又直接 `.map(|_| ())`，丢弃 `accepted: false`，使提示和重试判断更难一致。

证据：[can_claim](../crates/qaqh-daemon/src/axum_server/axum_impl/v2.rs#L229)、[busy](../crates/qaqh-daemon/src/axum_server/axum_impl/v2.rs#L862)、[TUI 丢弃响应](../../qaqh-tui-app/src/app/mod.rs#L395)。

**调整方向**：取消全会话 driver 及其读写门控，普通写请求由服务端按会话串行受理。必须互斥的具体资源，例如同一 PTY 的 stdin/resize，才单独设置控制权和 fence。

## 2. 短租约把短暂离线升级成会话读写失效

**当前实现**：租约默认 30 秒；只有显式 renew 延长 TTL。读请求、写请求、SSE 数据正常往来都不会自动续租。过期后不能直接 renew 恢复，必须重新 open。`owns_session` 同时要求租约存活，SSE 也会在发送后续业务事件前检查它。

证据：[TTL、owns、renew](../crates/qaqh-runtime/src/ringing/lease_store.rs#L10)、[事件发送前检查](../crates/qaqh-daemon/src/axum_server/axum_impl/v2.rs#L640)、[SDK 续租及重新 open](../crates/qaqh-client/src/session.rs#L206)。

**影响**：移动端后台挂起、设备休眠、网络切换、短时间服务端拥塞，都可能令仍有合法设备凭证的客户端同时失去会话读取和写入能力。正常消费 SSE 也不能避免。后台续租任务成为每一种客户端必须正确实现的业务前置条件。

**调整方向**：稳定设备身份与授权独立于连接寿命。心跳只用于在线提示、连接回收和具体执行路由；断线后凭原身份恢复读取及命令提交。显式设备吊销仍立即生效。

## 3. 同一设备只能有一个逻辑连接，多窗口会互相失效

**当前实现**：设备 open 时忽略客户端的 instance id，直接以 `device_id` 作为 lease map 的键。同一 key 再次 open 会删除旧 `client_session_id` 的索引及 attach 集合。

证据：[设备键映射](../crates/qaqh-daemon/src/axum_server/axum_impl/v2.rs#L100)、[替换旧连接](../crates/qaqh-runtime/src/ringing/lease_store.rs#L45)。

**复现场景**：同一个设备 token 的 A、B 两个独立客户端实例依次 open。B 建立时 A 失效；A 的自愈逻辑重新 open 后又令 B 失效。客户端自报不同 instance id 也无法解决设备路径上的问题。

**调整方向**：一个设备身份允许多个连接或客户端实例；连接以独立 id 登记，并关联设备。设备吊销统一关闭其下全部连接。

## 4. attach 被当成临时授权，恢复必须经过额外握手且存在重试缺口

**当前实现**：读一个会话前，客户端必须先 attach；服务端用 lease 内存中的 attach 集合判断 owns。换租约时该集合被清空，新连接不继承它。SDK 监听换租约，逐会话重新 attach，但失败只记日志，并写明等“下一次重新协商”再试。

证据：[attach 与 owns](../crates/qaqh-runtime/src/ringing/lease_store.rs#L68)、[新旧集合清除](../crates/qaqh-runtime/src/ringing/lease_store.rs#L48)、[读请求 guard](../crates/qaqh-daemon/src/axum_server/axum_impl/v2.rs#L1390)、[恢复任务](../crates/qaqh-client/src/client.rs#L310)。

**影响**：换身份后重连流和重新 attach 并发，读取可能先撞上未 attach 窗口。某次 attach 恰好失败，而后续续租一直成功，该会话不会被这个恢复任务再次修复。bootstrap 和 timeline/service 对未 attach 的处理还存在 403 与 401 两种语义。

**调整方向**：授权由稳定身份和资源策略决定；attach/subscribe 只表达关注哪些会话。如果保留 attach，至少让恢复成为可独立重试的过程，并在恢复完成后开放依赖读取，避免依靠下一次身份失效触发修复。

## 5. 命令幂等与回执绑定短租约，重连后失去对账能力

**当前实现**：命令 receipt 的所有者是 `client_session_id`。换身份后用同一 `command_id` 查状态，匹配不到；重发相同 id/payload 则因 owner 改变而产生 `duplicate_command_mismatch`。

证据：[记录时比较 owner](../crates/qaqh-runtime/src/ringing/pending_store.rs#L212)、[status 与 replay 按 lease 过滤](../crates/qaqh-runtime/src/ringing/pending_store.rs#L485)、[命令入口](../crates/qaqh-daemon/src/axum_server/axum_impl/command.rs#L216)。

另有两项资源策略放大问题：receipt 固定 300 秒 TTL；全局上限 4096，超限淘汰最早记录，没有排除 Accepted/Running。同一客户端或多个客户端的请求量都可能挤掉尚未完成的命令。记录消失后，这层去重不再保护重试，是否重复产生效果取决于具体下游操作。

证据：[容量与淘汰](../crates/qaqh-runtime/src/ringing/pending_store.rs#L243)、[TTL](../crates/qaqh-runtime/src/ringing/pending_store.rs#L552)。

**调整方向**：以稳定主体、目标资源和 command id 识别同一次操作；授权检查通过后，换连接仍能查询原回执。保留进行中命令的对账记录；容量紧张时限制新受理、压缩已完成记录，不静默移除进行中命令的身份。

## 6. 普通发送隐含“取代挂起回合”，开放多写后会互相打断

**当前实现**：普通用户消息进入时，如果现有 turn 因审批/提问等挂起，会调用 `abort_suspended`，移除旧的不完整 step，再处理新消息。命令没有要求客户端明确声明“替换这个 turn”。另外，手动 compact 期间普通发送被拒绝，而注入路径可以进入延迟队列。

证据：[compact 拒绝及挂起回合替换](../crates/qaqh-runtime/src/agent/loop_dispatch_conversation.rs#L347)、[注入延迟处理](../crates/qaqh-runtime/src/agent/loop_dispatch_conversation.rs#L327)。

**影响**：A 正在处理审批，B 发送普通补充文字，就可能使 A 正在处理的回合失效。取消 driver 后，这会直接暴露为多端互相覆盖的行为，不能仅靠“所有命令都放行”完成改造。

**调整方向**：普通输入明确排队；steer、interject、替换、取消分别表达，绑定具体 turn。计算资源忙或 compact 时尽量延迟受理，队列满才返回明确的 QueueFull/TurnBusy。

## 7. 操作目标和冲突校验不足，取消甚至先产生副作用再检查目标

**当前实现**：ConversationCancel 的 `turn_id` 可省略，语义是取消当前回合。registry 在入队前，只判断命令类型就先设置会话 cancel flag；actor 的 ConversationCancel 分支也先设置 flag，之后才进入带目标的账本取消流程。目标冲突报错后仍继续执行清理和 reset。

证据：[可省略 turn_id](../crates/qaqh-domain/src/command.rs#L281)、[入队前取消](../crates/qaqh-runtime/src/registry.rs#L1580)、[actor 先取消](../crates/qaqh-runtime/src/agent/loop_dispatch_conversation.rs#L417)、[错误后仍清理](../crates/qaqh-runtime/src/agent/loop_dispatch_conversation.rs#L492)。

**影响**：一个客户端滞留的取消请求，即使携带旧 turn id，也可能在目标校验前影响另一个客户端刚开始的回合；不带 turn id 的请求则直接指向到达时的当前工作。

信封虽然有 `expected_revision`，但不是所有写操作的通用保护：路由只把它交给 Control，当前消费点是 SkillsOperation。service 写路径没有统一 command id、receipt、资源 revision；driver 门控也只枚举少数方法，`session.set_profile`、`session.rename`、`todo.set` 等并不使用相同门控。

证据：[revision 路由](../crates/qaqh-runtime/src/agent/loop_core.rs#L637)、[skills 消费点](../crates/qaqh-runtime/src/agent/loop_dispatch_control.rs#L195)、[service 执行](../crates/qaqh-daemon/src/axum_server/axum_impl/service_api.rs#L75)、[service 方法表](../crates/qaqh-runtime/src/ringing/service_methods.rs#L92)。

**调整方向**：取消等控制操作精确绑定 turn/interaction/execution，在产生副作用前校验目标；同一资源的覆盖型写使用资源 revision CAS。命令和 service 写操作共享必要的幂等、结果和冲突语义。追加消息不必因为无关资源 revision 变化而被拒绝。

## 8. 单会话队列满时，阻塞会扩散到全局 registry

**当前实现**：worker 命令队列是容量 4096 的 `sync_channel`。`QaqhService::send_ringing_command` 持有全局 registry mutex 调用 `send_ringing`，里面执行同步 `.send()`；队列满时等待消费者腾位置，没有超时或 QueueFull 返回。

证据：[有界同步队列](../crates/qaqh-runtime/src/agent/loop_core.rs#L127)、[全局持锁](../crates/qaqh-runtime/src/service.rs#L210)、[阻塞发送](../crates/qaqh-runtime/src/registry.rs#L1590)。

**影响**：一个繁忙会话可阻塞其他会话需要 registry 的提交、管理和活动查询；该调用还直接处于 HTTP 命令处理路径中。有限资源压力没有被限制在具体会话。

**调整方向**：短暂取出会话 sender 后释放全局锁，再做有界受理。满队列明确返回可重试结果或进行有限异步等待。读、恢复和命令状态查询应保有独立处理能力。不需要先引入客户端等级才能做到这些隔离。

## 9. 慢客户端 reset 后，SDK 推进了 cursor 却没有交付对应快照

**当前实现**：canonical live broadcast 容量 1024，HTTP 转发队列容量 64。消费者落后触发 ReplayOverflow reset 并关闭连接。SDK 收到 reset 后把 cursor 换成 reset 的 snapshot cursor；没有 cursor 时，下次连接会取 bootstrap，但只保存其中的 cursor，不向上层交付完整快照。

证据：[live 容量](../crates/qaqh-runtime/src/ringing/v2.rs#L32)、[HTTP 队列](../crates/qaqh-daemon/src/axum_server/axum_impl/v2.rs#L587)、[overflow reset](../crates/qaqh-runtime/src/ringing/v2.rs#L840)、[只取 bootstrap cursor](../crates/qaqh-client/src/v2_stream.rs#L157)、[reset 直接改 cursor](../crates/qaqh-client/src/v2_stream.rs#L219)。

**影响**：SDK 可能从新水位继续收事件，上层却没有应用覆盖缺口的状态。恢复责任落到每种客户端：桌面 reset handler 补拉 approvals/todo/activity，TUI 另外实现 bootstrap；SDK 自身并不保证这些过程完成后再推进消费状态。

证据：[桌面补拉子集](../../qaqh-desktop-app/src/session/store.ts#L366)、[TUI bootstrap 消费](../../qaqh-tui-app/src/app/mod.rs#L2290)。

**调整方向**：保留有界缓冲和慢消费者隔离，但优先从最后成功应用的可靠 cursor 补读。必须 reset 时，读取并交付完整 snapshot，确认应用后再推进对应水位。可替换的高频状态可以合并为最新值，避免挤占可靠事件的处理预算。

## 10. 重连成本随完整历史和客户端数量放大，还在会话锁内执行

**当前实现**：每次 subscribe 都调用 `replay_after`；它 `read_all()` 全部 canonical facts，从头 apply 投影，然后才按 cursor 过滤。即使 cursor 已在最新位置，也要扫描重建历史。调用期间仍持有该会话 state mutex，直到 replay 构造后才 drop。

证据：[subscribe 持锁及调用](../crates/qaqh-runtime/src/ringing/v2.rs#L636)、[read_all 与从头投影](../crates/qaqh-runtime/src/ringing/v2.rs#L933)。

SDK 的 `activate_timeline` 还同时启动 timeline SSE 与 canonical v2 SSE；这是两种投影需求，但目前增加了每客户端、每会话的连接及恢复工作量。

证据：[两个流同时启动](../crates/qaqh-client/src/client.rs#L651)。

**影响判断**：多端同时重连长会话，会重复扫描历史、分配 replay 队列，并争用会话锁。在 CPU/内存/磁盘紧张时，可能延长其他端的读写和恢复等待。本次未做并发负载测量，因此不提供具体延迟或客户端数阈值。

**调整方向**：使用可定位的日志游标/检查点和有界分页补读，移出同步 I/O 与历史重建的持锁区。评估统一流或多会话订阅复用；保留双流时也应分别限定恢复的字节数和工作量。

## 11. 共享配置变化主要依靠当前前端内部通知，其他客户端不能及时获知

**当前实现**：配置保存成功返回 null，服务端通知运行中的 worker reload；桌面前端通过本模块内 `Set<Listener>` 通知其他组件重读。这个通知不能到另一个进程、窗口或 TUI；worker reload 完成也不等价于对所有客户端广播带 revision 的配置变化。

证据：[config.save 返回值](../crates/qaqh-runtime/src/service.rs#L730)、[worker reload](../crates/qaqh-runtime/src/service.rs#L1307)、[前端内存通知](../../qaqh-desktop-app/src/lib/config-events.ts#L1)、[保存后本地广播](../../qaqh-desktop-app/src/settings/store.ts#L178)。

**影响**：A 修改模型、上下文参数或 profile 后，B 可能继续显示旧值，直到自己的刷新触发点到来。服务端本地配置 mutex 和 Merge Patch 已经缓解并发保存覆盖，不能把它误报成“完全没有写入保护”；剩余问题是跨端失效通知及同字段编辑冲突。

**调整方向**：共享资源变更提供服务端 revision 和可恢复的变更通知；客户端按版本刷新。独立展示偏好留在客户端。本仓 ConfigPatch 已排除 theme/lang/fontFamily/notificationsEnabled，不应再把这些字段当成本轮仍可全局写入的问题。

证据：[配置事务](../crates/qaqh-config/src/config.rs#L824)、[Merge Patch](../crates/qaqh-runtime/src/service.rs#L1337)、[本地偏好已排除写模型](../crates/qaqh-config-api/src/lib.rs#L273)。

## 建议遵守的边界

- 有权限的客户端都能读取和提交写入；连接活跃度、是否抢到席位不作为通用授权条件。
- 接受写入与并行执行分开：按会话有序处理、有界队列、明确的满队列响应，不要求所有请求立即执行。
- 仅对同一资源的实际冲突做 CAS 或局部互斥；普通发消息不会隐式替换别人挂起的工作。
- 断线不丢身份、命令对账和资源授权；重连流程可重复执行并独立重试。
- 可靠 cursor 表示上层确实应用的状态，reset 提示不能替代 snapshot。
- 资源压力限制在慢连接或繁忙会话，避免持全局锁等待它们恢复。

`new-tech/docs/v2-reset/protocol.md` 的目标设计已经表达了其中部分原则：无全会话 driver、命令绑定具体 target、reset 后应用完整 snapshot、只有 PTY 输入局部互斥。这里列出的差距来自当前执行路径，不能把目标文档视为已完成实现。

## 验证范围

- 静态追踪上述服务端、SDK、TUI 和桌面消费路径。
- 直接编译并运行现有 `lease_store.rs` 的两项单元测试，2/2 通过：过期不能续租；同 instance 换身份清除旧归属、新身份必须重新 attach。
- 未启动真实多端会话，未进行拥塞压测，也未验证所有界面的最终呈现。性能影响及跨层故障后果按代码路径分析给出。
- 本轮仅新增这份审查文档。保留工作区原有的 `crates/qaqh-config/src/config.rs` 修改。
