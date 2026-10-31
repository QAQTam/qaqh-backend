# 工具执行侧 CPU、等待和死锁风险盘点

日期：2026-10-31；基线：HEAD `8e640b8a` 加当前工作区。保留已有未提交修改。
本轮没有修改生产执行逻辑，没有重启 daemon，没有接触用户会话数据或真实外部 MCP/LSP 服务。

## 1. 结论与范围

工具侧存在与本次 CPU 事故同类的调度成本，也有独立的无限等待、取消失效、锁阻塞和配额不归还问题。
**没有在已审查的生产工具循环中发现与 agent collector 相同的 `yield_now` 无等待自旋。**
不能把所有 `sleep/recv_timeout` 循环都判为死锁，也不能因为加了 timeout 配置就认为实际等待已经有界。

审查范围：WorkspaceToolPort → workspace execute/ToolManager → exec/process、MCP、LSP、subagent/wait_agent，
以及文件账本／journal 的相关锁。没有证明整个仓库不存在其他锁环。

| 编号 | 优先级 | 问题 | 证据等级 |
| --- | --- | --- | --- |
| T01 | P1 | process kill 持全局表锁 wait，组终止失败后可能无限占锁 | 当前源码隔离复现跨进程查询阻塞 |
| T02 | P1 | process.write 背压重试无取消、无截止时间，写锁等待也不受控 | 当前源码 + 可控 writer 复现 |
| T03 | P1 | wait_agent 捕获通知代数顺序可漏提交；取消位不能唤醒等待 | 当前通知源码复现两种条件；宿主调用顺序核实 |
| T04 | P1 | LSP shutdown 等远端回复无上限，close_timeout 未使用 | 源码确认；未搭建失联 LSP 端到端实验 |
| T05 | P1 | MCP service 锁覆盖 RPC；取消通知排队，远端未停止却按确定失败结算 | 生产代码、既有测试及其断言边界确认 |
| T06 | P1 | 后台／不确定工具的保留配额未接生产收割；panic 路径也会跳过清理 | 调用者盘点 + 源码；未运行有限配额耗尽实验 |
| T07 | P2 | 每次工具执行多建一个 5ms 取消轮询线程 | 生产源码确认；未单独测量现场 CPU 占比 |
| T08 | P2 | 每次文件 journal append 在全局 IO 锁内全量读取解析历史 | 生产源码确认复杂度；未做长历史负载实验 |

## 2. T01：kill 失败可把整个进程注册表卡住

位置：[ProcessRegistry::kill](../crates/qaqh-process-tools/src/process_registry.rs#L843)，
[全局 with 锁](../crates/qaqh-process-tools/src/process_registry.rs#L260)。

`Self::with` 持全局 Mutex，Unix 分支忽略 `killpg` 的返回值后调用 `Child::wait()`。
如果组已经不包含该存活进程、组信号失败或进程不能退出，wait 就不受调用方超时控制。
无关进程的 check、输出追加、其他 kill 等也需要同一把全局锁，都会被阻塞。
Windows 的 taskkill/status + wait 也位于锁内；本轮仅动态验证 Linux。

隔离探针先按生产方式创建独立进程组，再让自有子进程合法移回父组，使原组不存在但 child 存活。
生产 `kill` 的 `killpg` 失败后仍进入 wait；无关 ID 的 get_info 被阻塞约 **600ms**。
看门狗在 650ms 时直接终止自有 child 后，wait 和无关查询才恢复，kill 最终仍返回 Killed。
这是长时间／可能无限的锁阻塞；本轮没有把它伪称为已复现一个永不解开的互斥锁环。

修复方向：全局锁只负责取得/登记有稳定所有权的条目；终止和收割在锁外进行。
检查终止结果，失败时保留 Child 与未决状态，不无条件标 Killed；不得 drop 句柄后假称已停止。

## 3. T02：process.write 的“调用方超时”实际未接线

位置：[write_to](../crates/qaqh-process-tools/src/process_registry.rs#L523)、
[handle_write](../crates/qaqh-process-tools/src/process_inspect.rs#L153)。

write_to 对 WouldBlock 每 20ms 重试，循环没有取消或 deadline 参数，也不检查进程终态。
handle_write 虽收到 ToolCallContext，却只取 session 做归属校验，没有将 cancellation/timeout 交给写入。
writer 的 Mutex::lock 以及底层 Write::write 本身也可能阻塞，外层 effective_timeout 只是上下文字段，不能终止同步调用。

探针使用当前 ProcessRegistry 源码及一个可控、合法返回 WouldBlock 的 writer：
置取消标记后 150ms 内仍不返回；只有显式解除背压才完成。
这里的 writer 是夹具，不是实际 PTY；实际触发条件是目标不读 stdin、缓冲持续满或底层写入阻塞。
这是取消/超时失效和可能无限等待，不能仅靠“有 sleep”排除。

修复方向：把取消和绝对截止时间接入写入/获取写权的完整过程；底层使用可等待就绪的非阻塞写入。
保持同一进程写入顺序，但等待背压期间不占用全局表锁。外层套 timeout 后丢 blocking 句柄不是解决方案。

## 4. T03：wait_agent 的通知顺序有漏唤醒窗口

位置：[QaqhService::wait_agent](../crates/qaqh-runtime/src/host_impl.rs#L353)、
[wait_for_commit_change](../crates/qaqh-session/src/canonical/notify.rs#L85)。

当前顺序是：

```text
读旧 mailbox 基线
    → 新消息完成提交并通知，generation 加一
    → waiter 捕获这个已经增加后的 generation
    → wait_for_commit_change 发现 generation 相等，等待下一次提交或整个 deadline
```

消息已存在，但等待者可能最终返回 TimedOut；现有注释称“读后取代数关闭竞态”，这个判断不成立。
探针直接调用当前通知实现构造上述交错，等待 **150ms** 超时；对比读基线前捕获 generation，能够立即识别这次提交。
这是通知交错的隔离证明，没有伪称已跑通真实 RingingV2 消息投递端到端反例。

另一个等待边界：条件变量只接受 commit_generation 变化，cancel AtomicBool 不在其谓词中。
should_cancel 在进入等待前检查一次，等待内部只按整个 deadline 阻塞。
探针确认单独置取消位后 150ms 仍不醒，显式 notify_commit 才恢复。
实际取消若同时产生 canonical 提交，可能碰巧唤醒；这不是取消本身的唤醒保证。

修复方向：在读基线之前捕获通知代数，或用具备再次校验水位的注册流程；
将取消加入同一等待机制并区分 wake 原因，不能伪造 durable commit 通知来表示取消。

`wait_agent(targets=...)` 的另一路每 25ms 轮询，且锁在 sleep 前释放：未发现同类锁内等待，
但这不替无 targets 的条件变量等待路径背书。

## 5. T04：LSP 关闭可能永久等待失联 server

位置：[ServerConnection::shutdown](../crates/qaqh-lsp/src/connection.rs#L531)、
[request_with_cancel](../crates/qaqh-lsp/src/tool.rs#L538)、
[shutdown_all](../crates/qaqh-lsp/src/manager.rs#L308)。

shutdown 取得 session Mutex 后，直接 await LSP Shutdown 回复，然后才发送 Exit、停止 driver 和 sweep_group。
配置虽然有 close_timeout，生产 shutdown 并未使用；server 不回复时，后面的强制收尾无法到达。
manager 顺序关闭连接，单个失联 server 还能卡住后续连接和 daemon 的 LSP 收尾。

此外，request_with_cancel 在安装超时/cancel select 之前先 await session.lock；
与持锁的 shutdown 相遇时，请求可能卡在自己的超时范围之外。这里是确定的无界等待代码路径，
没有动态宣称所有 LSP server 都会触发或已经观察到永久死锁。

修复方向：关闭截止时间覆盖获取关闭权和协议等待；超限后明确停止驱动、终止及收割子进程。
常规请求也要把获取 socket/连接等待纳入预算。

## 6. T05：MCP 取消通知被正在取消的调用挡住

位置：[call_tool](../crates/qaqh-mcp/src/connection.rs#L814)、
[try_send_cancelled](../crates/qaqh-mcp/src/connection.rs#L921)、
[close_service](../crates/qaqh-mcp/src/connection.rs#L995)、
[bridge 等待](../crates/qaqh-mcp/src/bridge.rs#L231)。

- RPC 在 `service.lock().await.call_tool(...).await` 期间持锁；同一 server 的其他 RPC 被串行化。
- 取消通知同样先取得 service 锁，必须等原 RPC 完成／到超时释放锁后才能发送，不能及时打断它。
- 通知未绑定 request_id；代码自己的注释承认只能退化为不精确的取消提示。
- bridge 返回 mcp_cancelled/timeout 不意味着远端执行已停。WorkspaceToolPort 对这些码走默认 TerminalError，
  释放配额并报告确定的工具失败，而不是保留 AfterStart/不确定执行所有权。
- close_service 的 close_timeout 只从拿到 service 锁之后开始，锁等待不在关闭预算内。
  多数 call_tool 有 RPC 上限，所以这是有界长阻塞／队首阻塞；`probe_tools` 的 RPC 本身没有 timeout，
  特定不回复场景下关闭也可能无限等锁。

本轮运行既有 `cancel_hits_and_sends_notification`：**1 passed**，运行约 2.04 秒。
不能据此宣布及时停止已正确：测试允许通知在慢 RPC 结束后 6 秒窗口内送达，
[测试注释](../crates/qaqh-mcp/tests/call_path.rs#L348) 也明确写了等待 service 锁释放。

修复方向：生命周期持有 RunningService，调用只短暂取得可克隆 Peer，网络 await 不占生命周期互斥锁；
每次请求保留 cancellable RequestHandle，按真实 ID 取消，结果不明时保留执行证据而非归为确定失败。
已用 Context7 查当前文档，并核实本地 rmcp **3.5.1** 源码已有 Peer::send_cancellable_request、RequestHandle::cancel 和 peer()；
不需要为这件事换库或再套取消适配层。请求取消仍不能代替远端副作用停止证明。

参考：[Peer](https://docs.rs/rmcp/3.5.1/rmcp/service/struct.Peer.html)、
[RequestHandle](https://docs.rs/rmcp/3.5.1/rmcp/service/struct.RequestHandle.html)。

## 7. T06：后台／未知执行的配额回收链不完整

位置：[保留 owner](../crates/qaqh-agent-runtime/src/tool_job.rs#L548)、
[settle_retained_owner](../crates/qaqh-agent-runtime/src/tool_job.rs#L273)、
[额度预留](../crates/qaqh-agent-runtime/src/tool_job.rs#L492)。

后台 exec、partial、部分 cancelled 等分支保存 RetainedToolOwner，保留 durable quota reservation。
全仓调用盘点中，settle_retained_owner 的消费只有 `thin_loop_seam_probe`，没有生产 reaper 将进程终态接回来。
因此后台工作实际结束后，有限配额仍可能不归还，后续工具持续遭到准入拒绝；这是额度耗尽，不是 Mutex 死锁。
无限配额配置下不会以同样方式阻断，但保留记录仍可能积累。

另外，handler panic 会越过 finalize_req 以及 WorkspaceToolPort 的释放/保留分支；
外层 joined 会转成 StartedUnknown，但没有替它补齐 quota owner 记录和 manager inflight 清理。
settle_retained_owner 还在 ledger.release 成功之前移除内存 owner，release 失败会丢掉该内存恢复入口。
本轮没有运行 panic 后配额耗尽的动态实验，以上按静态控制流和调用者盘点定级。

修复方向：将进程终态／未知执行的收割与对应 execution_id、quota reservation 接通，幂等释放；
panic 也必须保存可接管的执行证据。释放成功后再移除 owner，不能用超时自动归还未知执行配额。

## 8. T07：工具取消桥每次额外创建原生轮询线程

位置：[WorkspaceToolPort::execute](../crates/qaqh-agent-runtime/src/tool_job.rs#L516)。

所有工具都会 thread::scope + spawn，一个线程每 5ms 检查外部 AtomicBool，再同步给 context cancellation。
每个正在执行的调用带来额外原生线程及最多约 200 次/秒轮询周期；短工具也承担创建、收割线程的成本，
scope 退出还可能等待该线程结束睡眠。它不是本次已定位的满核 yield_now 忙等，不能把现场 CPU 百分比归给它。

DoneGuard 已覆盖 panic 时的 done 置位；**没有把“panic 导致轮询线程永不退出”的旧问题误报成当前缺陷**。

修复方向：让 runner、ToolCallContext 和 handler 使用同一个取消句柄，取消时直接更新／唤醒；
删除 AtomicBool→另一个 AtomicBool 的逐调用线程桥，不引入新的监视线程。

## 9. T08：文件 journal 追加成本随全历史增长

位置：[append_step](../crates/qaqh-file-tools/src/journal.rs#L148)、
[load_steps](../crates/qaqh-file-tools/src/journal.rs#L124)。

每次追加都在 JOURNAL_IO 全局锁内 load_steps，读取整份 steps.jsonl、逐行反序列化，再扫描 max(seq)。
历史 N 条时单次成本 O(N)，连续追加累计可到 O(N²)；所有共享 journal 的会话写入互相等待。
这会造成长历史下 CPU、临时内存与写工具延迟升高，属于增长型热点，未证明它造成当前 daemon 的 20% 占用。

修复方向：由唯一 journal writer 维护 next_seq，启动时恢复索引，追加只做本次记录所需工作；
历史读取按需进行，不在每次 append 的锁内重新解析全历史，也不新增第二个历史权威。

## 10. 未发现同类问题的已检查路径

- workspace execution 已将 handler 执行放在 ToolManager 锁之外；prepare/finalize 短借锁，
  不能笼统称为“ToolManager 锁住整个工具执行”。
- exec direct 每 50ms 检查、process.wait 每 200ms 检查，均有实际休眠；
  pipe reader 的无睡眠路径在排空真实数据，WouldBlock 路径会休眠。
- pipe 超出捕获字节预算后仍排空，防止子进程因写满管道而无法退出；没有将此工作误判为空转。
- subagent collector 在 RecvTimeoutError::Disconnected 时退出，并在每轮开头检查总 deadline；
  没发现断开的 channel 让它无限立即重试。
- 文件 cache/ledger 的已检查调用顺序中没有确认到反向互锁环；journal 的全局锁热点按 T08 单列。

## 11. 验证方法、局限与修复顺序

复现入口：`python3 probes/tool-execution-audit/run.py`（Linux）。
runner 用已有 libc/log/serde_json rlib 直接编译 **当前 registry 和 notify 源文件**，不使用旧进程库产物。
会创建自有短命子进程、可控 Write 夹具和内存通知，不连接 daemon、不写会话目录；
临时二进制自动清理，终止信号仅指向该探针自有 child。
环境取消函数是夹具；writer 背压和“基线读”也是明确标注的夹具，不能视为完整工具端到端验证。

修复前的探针 exit 0，四个 REPRODUCED：write 取消无效、kill 跨查询占锁、wait 基线交错漏通知、取消位不唤醒 commit wait。
MCP 只运行了上述一个既有测试；没有运行全仓测试。该目标因 Cargo feature 组合触发了依赖重编译，构建约 2 分 16 秒，
不将它说成纯复用缓存，也不将其他未运行场景标为已验证。

建议先修 T01/T02/T03/T04 的实际等待边界，再统一 T05/T06 的取消／副作用归属与收割，
最后删除 T07 的轮询线程并处理 T08 的历史增长成本。
这几项都能在现有 Rust 库和 RingingV2 语义内改正，不需要引入 actor 框架或另一个 runtime。

审计当时只新增了报告和隔离探针，尚未修改生产代码。下面记录后续修复状态。

## 12. 基础设施与 journal 修复进展

- **T01 kill 全局锁等待：已修。** `ProcessRegistry::kill` 现在在全局锁内取出 Child 和标记回收中，实际发信号与 `wait` 在锁外；每进程 gate 串行化并发 kill，回收中条目不会被驱逐。`killpg` 失败时会退回直接终止 owned Child。进程注册表的无关查询不再等子进程退出。
- **T02 process.write 无限等待：已修。** PTY master 在进程工具路径设为 `O_NONBLOCK`，输出 pump 对共享 fd 的 `WouldBlock` 休眠重试；writer 锁竞争和输入 `WouldBlock` 都检查调用取消及 `ToolCallContext.timeout`，不再把工具线程卡在阻塞系统调用里。失败携带已写字节数：零字节取消按未启动处理，部分写入保留 AfterStart owner，并关联目标进程供后续回收。
- **T06 quota owner：部分修。** 背景进程 owner 会在每次工具执行前后通过 ProcessRegistry 刷新状态；进程退出/被杀/条目已驱逐后释放 reservation，之后再移除 owner。释放失败时 owner 保留以便下次重试。handler panic 会转为 tool_panicked 终态，经过 manager finalize 和审计写入，同时保留未知副作用的 quota owner；finalize 前其他 panic 会清理 in-flight 项。没有 process_id 的未知执行 owner仍需显式结算。
- **T07 取消轮询线程：已删。** runner 与 `ToolCallContext` 共享同一个 `Arc<AtomicBool>`，每个同步工具不再额外创建 5ms 原生轮询线程。
- **T08 journal 全历史重扫：已修。** append writer 按当前根路径缓存最后 seq；切换 journal 根时恢复一次历史最大 seq，后续追加只序列化当前记录、写入并 fsync，不再每次解析整个 JSONL。根切换时会修复损坏的尾记录：有效 JSON 缺少换行时补齐，无效尾记录会截断；单次写入失败则回滚到原文件长度，避免下一条记录粘连在残行后。
- **T03 wait_agent、T04 LSP、T05 MCP：本轮没有改动。** 这是用户指定的边界。

本轮 `cargo check -p qaqh-agent-runtime -p qaqh-workspace -p qaqh-process-tools -p qaqh-file-tools -p qaqh-pty --lib -j 2` 和 `cargo check -p qaqh-runtime --example thin_loop_seam_probe -j 2` 通过；未运行测试或审计探针。编译输出包含仓库已有的 dead_code/private_interfaces 警告。审计探针已经改为断言 T01/T02 的修复行为，但本轮没有运行探针，因此不把运行期行为标记为已验证。
