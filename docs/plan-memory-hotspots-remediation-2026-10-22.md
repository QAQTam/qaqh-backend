# 内存热点整改执行计划

> 状态：READY，尚未实施业务修复。下一步：B1。最后记录日期：2026-10-22。
> **后续修复的唯一进度入口是本文件。** 审计报告是证据，不再重写一篇 report。

## 1. 接续执行协议（新窗口先读这里）

1. 读本文件的状态表、当前批次、§9 交接记录；只按需打开对应源码与报告章节，禁止重新全仓审计。
2. `git status --short` 核对工作树；已有用户改动不得覆盖/回滚。审计开始时 service.rs、grouping.rs 与大量 docs 删除已经存在；不能据此推断下一窗口仍完全相同。
3. 不启用 AI 子代理、不访问真实会话正文、不调用付费模型。子代理回归用模拟事件/订阅接收器，不以真实 spawn_subagent 验证。
4. 依次 B1 → B2 → B3 → B4 → B5。同一窗口预算够就接着下一批；不要每个小修改都要求用户“继续”。时间/上下文不足时在子项边界停止，更新§9。
5. 每个子项先加针对性回归，再修业务；不能仅改预算常量、写文档或输出计划就标完成。除用户明确要求，不自行提交 git commit。
6. 每批结束记录：变更文件、执行命令/退出结果、before/after 数字、未验证边界与下一子项。只有测试/探针通过才标 VERIFIED。
7. 后续不依赖聊天历史。不要把“已测”“有风险”仅留在口头总结；必须写进本文件。

### 已有证据入口

- 审计：`docs/analysis-memory-hotspots-2026-10-22.md`，§3–§11 是源码持有链，§13–§14 是预算/验收建议。
- 基线：`probes/memory-hotspots/results-2026-10-22.txt`，保留不覆盖。
- 探针：`crates/qaqh-runtime/examples/memory_hotspot_probe.rs`，生产依赖独立入口 `probes/memory-hotspots/Cargo.toml`。
- 初始源码基线：80ddb3c 上的工作树。最终基线来自当前源码编译，不使用早期 prebuilt rlib 数字。
- 已通过：runtime lib offline 单作业构建；probe all；包含生产 token_calibration 模块的 10 条测试；探针 rustfmt check。**不是全后端套件通过。**

## 2. 状态与预计窗口

| 批次 | 目标 | 状态 | 子项进度 | 验证/交接 |
|---|---|---|---|---|
| B1 | 热路径止血：队列、重复预检、预算计量、无用事件、checkpoint 分配 | TODO | 0/5 | — |
| B2 | canonical 真增量读取、replay 与等待通知 | TODO | 0/4 | — |
| B3 | session/订阅释放边界、全局缓存预算 | TODO | 0/4 | — |
| B4 | 工具正文冷热分离、exec 捕获卸载、Range 真流式 | TODO | 0/4 | — |
| B5 | 封存存储/投影 owner 收敛及集成验收 | TODO | 0/4 | — |

窗口目标：通常 3–5 个执行窗口（不是保证）。B1 尽量一窗完成；B2/B3 可在余量允许时连做；B4/B5 在明确子项边界接续。若 B5 发现必须变更持久格式，宁可停在设计/迁移契约闸门，不带病一次改完。

状态词限定：TODO / DOING / BLOCKED / VERIFIED；只在有验收证据后用 VERIFIED。子项计数允许部分完成，避免整批被迫重做。

## 3. 不可破坏的契约与已定方向

- 不恢复已禁用的自动压缩，不静默截断用户模型历史，不自动缩小用户模型上下文。
- 不用 seal 即删全部 journal 取巧：保留 gap/reconnect 终态可恢复语义；预算不足采用显式 reset/rebaseline。
- 不取消 committed-prefix、身份、序号、fence、exactly-once admission 和 recovery fail-closed；把校验做成 streaming/增量，不是假装读不到损坏。
- 不把磁盘正文缓存 TTL 当作 durable 正文生命周期；正文驱逐后 canonical ref 必须仍可解析。
- 不清掉 process pid/墓碑来节省正文内存；读泵必须继续排空管道，避免反压卡死子进程。
- 不新增第二个通用事件总线、全量镜像缓存或 parallel legacy 路径。最终是单一增量投影 owner + 模型热历史 + 冷正文引用。
- 不直接 remove 活 writer/订阅仍会写回的状态。先 pin/停止产生端/flush/取消订阅，再驱逐；锁外 join/I/O，禁止持 registry/session mutex 等待回调。
- 首轮不改公开 wire shape；如内部 snapshot 变更必须版本化，旧数据只读恢复/惰性迁移，不覆盖唯一旧副本。
- 优先直接改善常规路径。低内存档可作为显式 opt-in；并发/TTL/预算值是待验证政策，不全局硬塞新默认，也不另起一批让用户选几十个参数。

## 4. B1：低风险热路径止血

### B1.1 队列固定税
- 入口：`crates/qaqh-runtime/src/agent/loop_core.rs:127-145`；`registry.rs` 的 session/subagent 共用 LoopChannels。
- 把 4096 cmd / 16384 event 改为小型有背压容量；先以 cmd=128、event=1024 为候选，常量集中定义。
- 不改成 unbounded；验证 writer 满队列时取消/关闭不会互相等待。若 enum payload boxing 有利，可独立小改，不扩展成 actor 重写。
- 验收：channels 模式的空队列 requested heap 从 15.10MiB 降到 <2MiB；突发消费、取消和 reader 死亡仍能收口。<2MiB 是本批目标，不是已有结果。

### B1.2 每 lap metrics 只做一次
- 入口：`agent/engine_turn.rs:1161-1184`、`agent/state/agent.rs:358-395`、`agent/state/token_calibration.rs:212-262`。
- 单次 prepared_request_metrics 返回 estimate 所需信息与 key；复用已有 key，不再次 count/serialize。
- 先不改 hash 算法与 provider wire，维持 request key 与图片计费测试。重型 telemetry 是否采样留到 B4 模型投影分离后再定。
- 验收：计数探针证明同一 lap 只调用一次；校准/图片脱敏 10 条现有测试通过。

### B1.3 journal 真实计量
- 入口：`timeline.rs:1372-1402`，`ringing/persistence_policy.rs`。
- 深度覆盖 checkpoint.arg、user_text、BlockOpened、ToolUpdated.args_json/display/failure 等所有可拥有正文的变体；slot 与 payload 分开计数，避免重复累计。
- 统一准入/驱逐/诊断所用估算函数；给超大单事件定义可恢复拒绝/reset 行为，不先无限 clone 再计账。
- 验收：journal 模式 8MiB arg 不再报0；参数/嵌套 display 的预算驱逐有回归。global 限额接 B3，不能本项就宣称已解决总预算。

### B1.4 接通 persistence 分类、削减 snapshot 临时分配
- 入口：`ringing/timeline_hub.rs:446-468,79-111,478-528`、`timeline_store.rs:169-200`。
- 结构事件使用明确策略；流中 crash checkpoint 用独立周期/dirty generation，而不是每条 TextDelta 都排全量快照。
- audit 只取小记录，不 clone 含正文 journal；borrowed PersistedTimeline 序列化到 writer，不 snapshot.clone + to_vec。
- 保留 terminal/显式 flush 的恢复边界；不借 B1 偷改 sidecar 格式。消除“21B瞬态立即重写旧史”，周期仍会全史 rehydrate 的残余必须记到 B5。
- 验收：persist 探针应改为“瞬态不会立即导致全文重写”的新契约，并另测周期恢复；timeline_checkpoint_cost / stale_restore / persist_deadlock 回归不能退化。

### B1.5 删除确定无人消费的兼容正文事件
- 入口：`agent/turn_lap/gate.rs:122-155,498-581`、`actor.rs:58-103`、`activity.rs:14-60`、`ringing/compact_mirror.rs:86-166`。
- 精确追踪 Conversation RoundDelta/BlockCheckpoint 生产与消费：只删除当前后端生产路径中确实无消费者的全量文本副本；保留活动状态、交互 side effects、compact mirror 和 timeline 事件。
- 不按名称批量删 enum/wire 协议。若发现另有调用者，用计数/路由证据保留并记原因。
- 验收：输入→模型事件→timeline 文本与终态一致；移除生产复制的次数有测试/计数依据。

## 5. B2：增量读取与等待

### B2.1 CommittedFactReader streaming
- 入口：`qaqh-session/src/canonical/reader.rs`、`canonical/log.rs:626-657`；测试 `committed_fact_reader.rs`、`canonical_log_contract.rs`。
- 第一阶段把验证改为 bounded BufRead + visitor，open 不积累 Vec<SessionFact> 或全日志 bytes；read_range 只收结果，并限制返回 backing capacity。
- 第二阶段引入 committed offset/验证代际与稀疏 seq 索引；身份/截短/重写变化使缓存失效。仅内存缓存不是跨重启权威。
- 验收：同样1条 range 随总历史增长不保留全量 Vec capacity；峰值受单行+返回页限制；open 对损坏前缀仍拒绝。若首开仍需 O(历史) 校验，必须如实记录，不能称 O(1)。

### B2.2 replay 快路径与锁边界
- 入口：`ringing/v2.rs:626-751,896-965`。
- since=None/current watermark 不从头重放；缺失段按需读取，避免为补帧重建全部临时 ProjectionSet。
- 在锁内捕获游标+注册live receiver，锁外读盘；定义 replay/live 拼接及 overflow 的顺序契约。
- 验收：snapshot→subscribe 竞态不丢事件；log mismatch/unknown cursor/lag reset 正确；零补帧不解析整史。

### B2.3 wait_agent 通知与 O(1) 终态判定
- 入口：`host_impl.rs:56-101,352-385,949-989`、`registry.rs:604-605`。
- commit 后通知 + 高水位 predicate，避免丢唤醒；空等不反复 reader.open。
- targets 完成判定用借用/contains，不克隆已完成 answer。不能持全局锁睡眠。
- 验收：无消息、刚好注册监听时收到消息、取消、deadline、多个 targets 逐个完成；空等30s对历史解析次数为0（初始化另计）。

### B2.4 重复恢复扫描与当前 turn 查询
- 入口：`canonical/store.rs:36-48`、`canonical/tool_ledger.rs:278-328`、`agent/request_context.rs:44-52`。
- 单次 streaming 恢复同时喂所需索引，或复用已验证 reader；TurnStarted 信息在创建时传递/缓存，不每 turn 查整史。
- 验收：恢复、open intent、lease/fence与request context现有契约保持；每次恢复扫描次数、bytes明确记录。

## 6. B3：生命周期和全局内存预算

### B3.1 先保证 ref 真正 durable
- 入口：`agent/loop_dispatch_conversation.rs:58-149`、`canonical/blobs.rs`、daemon `axum_impl/content.rs`。
- >8KiB input/communication 写 durable session blob，再发布 ref；传输缓存可选。写盘失败 fail-closed。
- 验收：put→TTL/cache eviction→close→restart 后 ref 仍能解析，且正确鉴权。此项先于扩大 content eviction。

### B3.2 子代理 collector/subscription 成对结束
- 入口：`host_impl.rs:532-579`、`ringing/v2.rs:811-873`、`qaqh-subagent/src/lib.rs:1138-1253,1560-1571`。
- 区分 Empty/Closed；显式 stop token + JoinHandle/可取消task；collector收尾/drop取消订阅桥，不依赖未来事件 send 才发现下游消失。
- 验收：模拟100次创建/终结collector，不再发任何新事件，线程/订阅计数仍回到基线。不得真实启动模型子代理。

### B3.3 resident session 卸载协议
- 入口：`service.rs:254-320`、`ringing/hub.rs:193-206`、`ringing/v2.rs:723-751`、`registry.rs:1710-1755`、TimelineStore索引。
- flush/join writer及reader后释放timeline、V2、lazy locks和重型索引；driver/observer订阅定义pin或显式reset，不静默断开。
- 父close导致子close也要逐个释放；只释放正文cache，保逻辑agent身份与durable结果指针。禁止在registry持锁join需要该锁的collector。
- 验收：1→10→50 synthetic sessions 打开关闭，resident gauges返回预算平台期；重新打开历史完整，pending审批/活跃turn不被驱逐。

### B3.4 统一预算，不新增全量 owner
- timeline replay global/per-session、ContentStore loaded bytes、projection cold residency分别有预算；所有路径使用同一记账/释放入口。
- 先提供集中策略与显式低内存档候选：replay global32–64MiB/per-session2–4MiB，content16–32MiB；值需实测后冻结，不把候选说成生产已生效。
- memwatch补深层V2/writer/queue/capture/subscriber/pin指标，标注估算而非allocator总账；所有采样有界且不保存正文。
- 验收：预算不足可恢复、无历史丢失；关闭与预算驱逐同语义；深层数据不再只报2328B浅下界。

## 7. B4：正文冷热分离

### B4.1 热模型历史不再携带 display/data 全文
- 入口：`qaqh-tool-core/src/tool_api/output.rs:180-246,340-398`、`qaqh-types/src/tool_result.rs`、`qaqh-message/src/store.rs:930-959,1034-1081`。
- 先消除top-level/display.diff双份owned正文；引入内部ModelMessage/request view，只传provider需要的数据。
- UI大diff/data持durable ref；旧消息读取兼容，provider实际发送字节和前缀缓存语义不变。仅用Arc包重容器不算完成冷热分离。
- 验收：model仅ok+4MiB展示diff，prepare/context metrics额外分配不随该diff增长；restart_request_body(_responses)/restart_prefix_cache回归通过。

### B4.2 请求hash/token记账增量化
- 入口：PrefixShape `agent/state/agent.rs:92-120,620-641`、token_calibration、ChatGPT plan retry `engine_turn.rs:1231-1239`。
- 定型消息缓存hash，revision包括工具/注入/annotation变化；借用model serializer边count/hash，不先复制图片再删；retry共享不可变请求。
- 验收：修改旧消息可检测、仅追加不误报缓存失效、图片placeholder计费不变；context模式对比基线报告before/after。

### B4.3 exec full capture转spool
- 入口：`qaqh-process-tools/src/process_registry.rs`、`exec/direct.rs:394-486`、`exec/pipe.rs`。
- capture具全局/每session内存阈值，超出写临时spool；seal之后释放全文内存只留tail/ref/status/pid。captured_len直接计数，不先clone正文。
- 后台仍排空、保truncated/EOF真值；完成条目主动有界清理；保墓碑与owner鉴权。
- 验收：100个noisy模拟entries/本地短进程，全文内存回到阈值；UTF8、取消、孙进程持pipe、kill/check不退化。

### B4.4 ContentStore/HTTP Range
- 持久store默认metadata-only，有限byte LRU才留正文；get用共享/stream而非整body.clone。
- daemon先鉴权/解析Range，再seek+chunk输出；不能先读取整个blob再slice。
- 验收：1KiB Range读64MiB合成blob，临时内存与chunk/page相关；hash/owner/range416/TTL pinned测试通过；canonical正文不被删。

## 8. B5：架构收敛与最终验证

### B5.1 封存正文只持久化一次
- 入口：`ringing/timeline_hub.rs` 的offload/rehydrate/persist，`timeline_store.rs`。
- 固定方向：sealed turn正文在durable append/CAS，checkpoint只写引用/索引/活跃tail；不再周期性全史rehydrate。
- 格式闸门：实现前用本节补充format version、旧snapshot导入、body先落盘→ref提交、崩溃恢复、水位与GC规则；仅需简短决策记录，不另写宏大spec。
- 验收：旧史1/10/100MiB时checkpoint额外内存/IO不线性跟旧史增长；旧文件可恢复，sidecar丢失不谎报完整，部分写入不损毁唯一历史。

### B5.2 单一 ProjectionSet owner
- writer保最小admission/recovery状态，daemon单一增量projection owner；调用端只读所需typed snapshot/page，不因读team克隆全conversation/timeline。
- 移除重复owner前先列明所有使用其snapshot/索引的调用方，保持durable-before-publish；不可引入同步全局锁把所有session串行化。
- upsert按ID索引，active尾部/冷页有界；旧终态幂等key持久索引或checkpoint，不全量常驻。
- 验收：同session第二套ProjectionSet不再常驻；projection replay等价；1000→10000事件重建不表现为全史Vec查找的平方增长。

### B5.3 低配压力集成
- 只用合成history/mock provider，组合多session、长history、noisy tools、observer lag、周期checkpoint、取消/close/reopen。
- 同时记录requested heap、RSS/PSS/Swap、线程和外部进程；记录feature/profile/编译模式。堆申请≠RSS，debug时延≠release SLA。
- 真正完成标准：visited sessions增加后工作集稳定；空等不扫描历史；旧史不被反复物化；close后subscriber/thread/capture落回平台期；gap与恢复正确。

### B5.4 收口
- 更新所有probe缺陷assert为修复契约；保存after结果而不覆盖baseline；把本文件状态全改VERIFIED仅当逐批证据完备。
- 列出仍未覆盖的真实daemon/平台项目；不把mock测试说成现场G2020全负载验证。
- 对外一句结论必须区分“机制已修”与“现场RSS收益待测”。

## 9. 验证命令与增量交接

在 qaqh-backend 根执行，单作业；不使用release LTO构建作为每个子项的常规门禁：

```bash
cargo build --offline -j 1 -p qaqh-runtime --lib
TMPDIR="$PWD/target" cargo run --offline --manifest-path probes/memory-hotspots/Cargo.toml --target-dir target -j 1 -- channels
# 按当前子项改mode：journal / persist / reader / v2 / context / display / processes / content / forget
TMPDIR="$PWD/target" cargo test --offline --manifest-path probes/memory-hotspots/Cargo.toml --target-dir target -j 1 -- --test-threads=1
```

按改动跑定向集成测试，不每次cargo test --workspace：

```bash
cargo test --offline -j 1 -p qaqh-session --test committed_fact_reader --test canonical_log_contract --test recovery_executor
cargo test --offline -j 1 -p qaqh-session --test projection_set --test projection_replay --test tool_ledger
cargo test --offline -j 1 -p qaqh-runtime --test timeline_checkpoint_cost --test timeline_stale_restore --test timeline_persist_deadlock_repro
cargo test --offline -j 1 -p qaqh-runtime --test timeline_deep_paging --test timeline_rebuild --test host_direct --test subagent_inprocess
cargo test --offline -j 1 -p qaqh-runtime --test restart_request_body --test restart_request_body_responses --test restart_prefix_cache
```

已知环境约束：runtime常规example/test曾因未缓存dev依赖ascii在offline失败；独立probe只依赖生产库，因此能运行。若定向suite也被依赖阻断，记录BLOCKED；允许在独立probe增加同逻辑的mock测试，不允许把未执行suite写成通过。后续允许下载时再补齐，不无声联网/安装。

**probe使用约定：** 初始assert有“journal arg报0”“forget后仍存在”“瞬态重写快照”等缺陷现状断言。对应修复必须反转/替换，其他未修mode保留并注明；测试绿不代表所有缺陷已修。每批只跑相关mode，最终all统一通过修复后的契约。

### 增量交接记录（每次执行结束必须更新，保持短）

- 当前批次/子项：B1.1（待开始）。
- 已完成：仅审计、当前源码基线探针、本文档；业务修复0项。
- 最后验证：baseline all exit0；runtime lib build exit0；token_calibration 10/10；probe格式检查通过。详见results基线文件。
- 本轮业务文件改动：无。既存用户修改不属于整改交付。
- 待补命令：当前批次新增针对性回归及before/after；完整suite仍未跑。
- blocker：无实施blocker；offline dev依赖限制见上。
- 下一步：给LoopChannels缩容加背压/取消测试，实施B1.1，再接B1.2；不要重新开展架构审计。

后续更新本段用以下格式：

```text
批次/子项：B?.?，DONE ... / NEXT ...
文件：路径 + 关键符号（行号以当前源码为准）
验证：命令 -> exit/result；before -> after（单位和测量口径）
未验证/风险：...
决策：新增内部格式/预算/生命周期规则；若无写“无”
阻塞：原因、可执行替代、补验命令
下一步：一个明确子项 + 首个入口，不写“继续优化”
```

## 10. 可直接用于下一窗口的指令

> 执行 @docs/plan-memory-hotspots-remediation-2026-10-22.md。先读状态与交接段，从首个未完成子项继续；禁止AI子代理，不重新全仓审计，不覆盖既存用户改动。单作业构建，按批补回归、实现、运行探针和定向suite；本窗口能推进多批就连续推进，不能验证则记BLOCKED而非宣称完成。结束前更新计划状态、before/after和下一具体子项。无需重新向我复述审计结论，也不要只写新计划不实施。
