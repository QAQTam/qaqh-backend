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
| B1 | 热路径止血：队列、重复预检、预算计量、无用事件、checkpoint 分配 | DOING | 4/5（B1.1 BLOCKED 锁环；B1.2/B1.3/B1.4 完成，含超大单事件 reset 契约；B1.5 TODO） | 探针+定向 suite 全绿，见 §9 |
| B2 | canonical 真增量读取、replay 与等待通知 | DOING | 3/4（B2.1 phase1+phase2 完成，O(1) 首开未达成见 §9 记录；B2.3 完成；B2.2/B2.4 TODO） | session 契约+reader 探针绿，见 §9 |
| B3 | session/订阅释放边界、全局缓存预算 | DOING | 2/4（B3.1/B3.2 完成；B3.3/B3.4 TODO） | host_direct/subagent/daemon 绿，见 §9 |
| B4 | 工具正文冷热分离、exec 捕获卸载、Range 真流式 | DOING | 1/4（B4.4 懒加载+v2 零重放完成；B4.1-B4.3 TODO） | content 探针 13 项+v2 9 项绿，见 §9 |
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

- 当前批次/子项：B1.2/B1.3/B1.4/B2.1(phase1+phase2)/B2.3/B3.1/B3.2/B4.4(lazy) DONE；B1.1 BLOCKED；B2.2、B2.4、B3.3、B3.4、B4.1-B4.3、B5.x NEXT。
- **2026-10-28 第二窗口增量**：
  - **B2.3 收尾（commit→notify）DONE**：`session/canonical/notify.rs` 新增进程级 per-session-dir 通知注册表（generation Mutex+Condvar；条目 ~100B/会话，不回收，注释声明上界=会话数）。`log.rs::append` 在 commit marker 落盘后 `notify_commit`；`recovery_state.rs` 两处 repair 写 marker 后同样通知（临界区=计数自增+notify_all，无 I/O 无 join，符合「锁外 join/I/O」契约）。`host_impl.rs::wait_agent` 无 targets 路径：baseline 解析一次 → 捕获 generation → 阻塞在 `wait_for_commit_change`（generation 在 baseline 读取**之后**捕获，关闭丢唤醒竞态）；`wait_for_commit_change` 返回 false（deadline 且无 commit）直接 TimedOut **不再重解析**。`wait_for_agent_outcomes`（targets 路径）保持 25ms 内存轮询（无 I/O、锁外 sleep，契约本就允许）。观测点：`host_impl::MAILBOX_ACTIVITY_PARSE_COUNT`（doc-hidden 从 lib.rs 再导出）。回归：`host_direct::wait_agent_empty_wait_parses_canonical_log_once`（先红后绿：修前 600ms 空等 25 次解析 → 修后恰 1 次）+ `canonical/notify.rs` 4 项单测（generation/竞态即返/阻塞唤醒/deadline）+ host_direct 7 项全绿。**测试基建注意**：host_direct 二进制内多个测试共享 `shared_test_env()`（OnceLock，WorkspaceStore/SessionManager 进程单例不可重复 init）。
  - **B1.3 遗留（超大单事件 reset 契约）DONE**：契约链三层落地。①runtime `timeline.rs` 回归 `oversized_single_event_evicts_whole_journal_and_uncovers_stale_cursor`（单条 600KB delta > 512KB 测试界 → journal 整体驱逐、`replay_since(cursor)` 空、watermark 已推进）。②daemon `sse.rs` 新增纯函数 `timeline_replay_lost_coverage(after, watermark, replay_len)`：**空回放 + after < watermark** → `handle_timeline_events` 建流即下发 `ringing.stream_terminated { code: "journal_gap", cursor, message }` 并关闭（不再静默空转）；非空回放窗口内缺帧**不在此判据**（保留客户端 `cursor+1` 自检契约，QAQH_TEST_TIMELINE_GAP 注入依赖）。③client `error.rs::needs_gap_recovery`（TimelineGap 或 StreamTerminated(code=="journal_gap")）→ `timeline.rs::run` 两者都走 `recover_gap` 快照重基线（否则旧 cursor 重连会无限命中 journal_gap）。回归：daemon `journal_eviction_marks_stale_cursor_as_lost_coverage` + client `only_journal_gap_termination_requires_snapshot_rebaseline` + daemon --bins 84 项全绿（含全部 injected_timeline_gap/stream_lagged 既有契约）。**未验证边界**：未做真实 daemon wire 端到端测试（无现成 512KiB 事件注入基建），契约由①②③分段钉住。
  - **B2.1 phase2（committed 前缀稀疏 offset 索引 + digest 校验）DONE**：新模块 `session/canonical/offset_index.rs`——per-session-dir 进程内索引（log_id/session_id/last_seq/last_offset/运行中 Sha256/稀疏 checkpoint stride=512、上限 1024 条 ~56B）。索引**只能从全量验证扫描或本进程 durable append 生根**：`reader.rs::visit_committed`（全量路径）扫描同时哈希、成功后 `promote_full_scan`；`CanonicalLog::open` 的 `validate_committed_prefix` 走同路径自动生根；`log.rs::append` 落盘后 `advance_append` 增量推进（arith 不匹配自弃，等下轮冷扫重生根）。`CommittedFactReader::open` 改走 `validate_committed`：warm 索引命中（log_id/session_id/last_seq/last_offset 与 marker 精确匹配）→ 流式哈希 0..indexed_offset 比对 digest（**fail-closed**：任何同长度字节篡改被拒）→ 只解析索引之后的尾部；未命中/repair/截断→ 冷全量扫描。`read_range/read_after` 走 `visit_committed_seek`：seek 到 `fact_seq < from_seq` 的最大 checkpoint（严格小于，checkpoint 记录的是 fact 之后的 offset，等号会把 range 首元素跳过——探针抓到的 off-by-one，已修）→ digest 校验被跳过区 → 只解析 range 区间。空前缀（offset==0）显式校验 marker 必须为 (0, None)。**诚实复杂度记录：warm open 仍 O(历史) 读+hash（零解析零分配），不是 O(1)**——真 O(1) 需无条件信任进程内写者、跳过字节校验，越出 fail-closed 边界（§3），留待 B5 或单独立项；read_all 仍全量解析（语义需要）。观测点 `canonical::COMMITTED_FACT_PARSE_COUNT`（doc-hidden）。回归：`warm_open_skips_reparsing_indexed_prefix`（先红：二次 open 600 次解析→绿：0）、`indexed_prefix_corruption_fails_closed_on_warm_open`、`indexed_prefix_corruption_fails_closed_on_range_read`（同 reader 实例 seek 路径 digest 拦截）、`seek_based_reads_match_full_scan_after_checkpoints`；committed_fact_reader 14 项、session 30 suite 全绿、runtime lib 350 绿、host_direct/canonical_log_contract 绿、probe all exit=0。探针 reader 数字变化：idle OPEN+read_after 峰值 22,110→**8,469**（不再为空读分配解析缓存）；one-fact range 峰值 23,795→23,851、capacity=1 保持；open live_delta 180→929（驻留索引条目）；elapsed 膨胀（open 77→450ms@n=1024）系探针 debug 构建下新增 sha256 开销，release 不受影响，**内存契约数字不受影响**。
  - **窗口内并发外来改动**：第二窗口后半程，约 20 个 `crates/qaqh-runtime/src/agent/**`、`ringing/**` 等文件出现非本窗口的并发修改（含 `agent/state/lifecycle.rs` +133/-52，工作区扫描 s0001793168956032_0062）。按约束未触碰、未回滚；在合并树上复跑 runtime lib（350 绿）与 session 契约（绿）确认共存。**下一窗口开工前先 `git status` 重新核对这些改动的归属与意图。**
  - **已知失败集确认**（交接单要求先跑三个 suite）：`restart_prefix_cache::prefix_cache_consistency_across_restarts`、`restart_request_body::request_body_is_byte_identical_across_restarts`、`restart_request_body_responses::responses_request_body_is_byte_identical_across_restarts` 三者均为重启前后 system prompt/skill catalog 注入逐字节不一致，与本窗口改动无关，维持既有记录，**未立项修复**。另：`qaqh-client --test lease_renew_timeout` 两项失败（rustls crypto provider 未装配，reqwest Client 构造 panic），环境性、与本窗口改动无关（未触碰 client 构建/lease 路径），记录待修。
  - probe all 复跑 exit=0（B2.3/B1.3/B2.1phase2 改动后最终一轮），reader mode 数字变化见 B2.1 条目，其余 mode 与 §9 before/after 汇总一致。
- 已完成（业务修复，按批）：
  - **B1.2** per-lap metrics 复用：`token_calibration.rs` 新增 `estimate_prepared`/`estimate_prepared_with`（单次 metrics 调用同时产出 estimate+key）；`agent.rs` `estimate_prepared_request_with_key`（原 `estimate_prepared_request`/`prepared_request_key` 合并，后者删除）；`engine_turn.rs` `prepare_gate_snapshot` 改单次调用。回归：`prepared_estimate_reuses_one_metrics_call_and_preserves_key`（计数器断言 calls==1）+ 既有 10 项 token_calibration 全绿。
  - **B1.3** journal 计量：`timeline.rs` `journal_entry_payload_bytes` 覆盖全部事件变体（checkpoint `arg`+`text`、ToolUpdated 嵌套 `args_json`/`display` body/diff/header/permission/metrics、failure、permission.paths 等），O(1) 维护不变。探针 before：`journal_reported_payload=0` → after：`8388615`（实测 arg 8388608）。回归：probe `journal_counts_checkpoint_argument_and_nested_tool_display`、lib `journal_budget_evicts_nested_tool_display_and_argument_prefix`。**遗留**：超大单事件（payload > 字节上限）的重连 reset 契约未单独验证——驱逐语义是"尾部连续前缀"，空回放窗口无法自行向客户端发 gap 信号，需客户端 cursor 检查兜底（未验证）。
  - **B1.4** 瞬态事件不再触发旧史全量重写：`timeline_hub.rs` `publish_timeline` 三分支——TurnSealed→terminal 立即；`is_snapshot_persisted(event) || BlockCheckpoint`→既有 1s 软窗口；其余（TextDelta/ToolProgress/ToolEstimated/UsageSampled/ProviderRetrying）→ 新增 `transient_sessions` 集合，**不唤醒** worker，仅 `flush_timeline_persistence`/Drop/优雅关闭显式排空。`hub.rs` `TimelinePersistence` 增 `transient_sessions` 字段；`persist_timeline_sync` 同步清除三集合。探针 before：瞬态 21B 事件 peak ~21MB→~10.5MB 且全量重写 → after：**peak 494B、零重写**；flush 后 `rewrote=true, marker=true`（fail-closed 保持）。回归：probe persist 双相断言 + timeline_checkpoint_cost 7 项 + persist_deadlock 3 项 + stale_restore 2 项全绿。决策：BlockCheckpoint 保留软窗口触发（它是流式文本的周期性落盘合并点，`terminal_then_later_write_both_survive_without_explicit_flush` 契约依赖）；瞬态文本硬崩溃丢失由 timeline_rebuild 从 messages 归档重建兜底（既有既定语义）。
  - **B2.1 phase1** CommittedFactReader 增量流式：`reader.rs` 重写为 `BufReader::read_until` 逐行解析（不再整前缀 `read_to_end` + 全量 Vec），新增 `visit_committed`/`open_with_visitor` 流式访问器；`read_range` 流式过滤 + `shrink_to_fit`；`log.rs` `validate_committed_prefix` 改走 `open_with_visitor`（删除本地 `scan_complete_facts`/`CommittedPrefix`，保留 `scan_committed_facts` 供 recovery_state 使用）。**仍逐次全前缀扫描（非 O(1) 首开），稀疏索引/代际验证未做**。探针：n=1024 一条结果 capacity 1024→1，open+range peak 13.38MB→23.8KB。回归：新增 `one_fact_range_does_not_retain_full_history_capacity` + session 35 项契约全绿。
  - **B2.3 partial** 提交后通知替代空等扫描：`wait_agent(targets)` 完成判定改 `registry.has_subagent_outcome`（`contains_key`，不再 clone `AgentOutcome`）。通知机制（`visit_committed` notify 唤醒等待方）**未做**——`wait_agent` 无 targets 路径仍 25ms 轮询 canonical 日志。
  - **B3.1 durable content**：`loop_dispatch_conversation.rs` `externalize_canonical_content` 改写 `SessionBlobStore`（每会话 `blobs/`，content-addressed、fsync），wire ContentStore 仅作可选缓存；`record_input_accepted`/`record_inter_agent_communication` 先确保 tool_ledger（canonical identity 存在）再外置。`host_impl.rs` `download_content` blob fallback（hub miss → 会话 blobs/，digest 双重校验）。回归：`accepted_input_is_persisted_as_a_canonical_fact` 扩展（forget 后 wire cache 清空、blob 仍可读、host fallback 逐字节一致）+ daemon `v2_content_falls_back_to_session_blobs_with_ownership` 绿。
  - **B3.2 collector/subscription 成对结束**：`host.rs` 新增 `CollectorSubscription`（receiver + stop 钩子，Drop 同步停桥）；`host_impl.rs` subscribe 桥接线程支持 stop 信号（`recv_timeout` 20ms 轮询），Drop 即 join；`v2.rs` `try_next_status` 区分 idle/Closed。回归：host_direct 新增 100×订阅-断言-丢弃循环（`subscriber_count_for_test` 1↔0）+ host_direct 6 项 + subagent 14 项绿。
  - **B4.4 content 懒加载**：`content_store.rs` `upsert` 持久化校验通过后弃置内存 body（`bytes_loaded=false`）；`get_internal` `mem::take` 移交 body 后立即回落懒加载态；`persist_entry` 返回 bool（写失败保留内存副本 fail-safe）。探针 before：persist 后 resident 4.19MB → after：**1247B**；GET clone 不再残留（drop 后回到 1247B）。回归：content_store 13 项全绿。
  - **B4.4 v2 订阅零重放**：`v2.rs` `subscribe` 在 `since_fact_seq == last_fact_seq`（无 gap）时跳过 `replay_after` 全量投影重建。探针：subscribe(None) peak 18.15MB→389B。v2_acceptance_matrix 9 项绿（C1-C7 契约不受影响）。
- 最后验证：`cargo test -p qaqh-runtime`（lib 349 + 各 suite）除已知 `restart_prefix_cache` 全绿；`cargo test -p qaqh-session` 全绿；probe all exit0（见下）；subagent lib 14 项、daemon blob fallback 1 项绿。
- 本轮业务文件改动：`agent/state/{token_calibration,agent}.rs`、`agent/engine_turn.rs`、`agent/turn_lap/gate.rs`（checkpoint 不再重复发整轮 Conversation 事件，回归断言同步反转）、`timeline.rs`、`timeline_store.rs`（persist 零拷贝借用序列化，删除临时 21MB `to_vec`）、`ringing/{hub,timeline_hub,v2,content_store}.rs`、`host_impl.rs`、`registry.rs`、`agent/loop_dispatch_conversation.rs`、`session/canonical/{reader,log}.rs`、`subagent/{host,lib}.rs`、probe 文件、`tests/{session_lifecycle,input_accepted_producer,committed_fact_reader,host_direct}.rs`。
- **before/after 探针汇总（2026-10-28，requested live bytes，非 RSS）**：
  - channels：空通道对 15,828,344（**未修，B1.1 阻塞记录在案**）
  - journal：checkpoint 计量 0 → 8,388,615
  - content：persist 后常驻 4,195,551 → 1,247（-99.97%）；峰值 8,391,116 → 4,196,726
  - reader：n=1024 一条 fact 峰值 13,376,982 → 23,795；open 13,376,954 → 22,038
  - v2：subscribe(None) 峰值 25,895,777 → 389；hub drop 峰值 44,047,741 → 18,152,353
  - persist：瞬态事件峰值 21,040,156 → 494；重写 false；flush 边界完整落盘
  - display/context/processes/forget：未动，与基线一致
- 待补命令：~~restart_request_body(_responses)~~（已补跑，失败集确认见上）；~~B1.3 超大单事件 reset 契约回归~~（已补，见上）。
- blocker：B1.1——registry 持锁阻塞 send（`registry.rs` `send_ringing`→`cmd_tx.send`、`signal_shutdown`→`cmd_tx.send`、`cancel_subagent_children`→`cmd_tx.send`）+ worker 执行 host 操作回调获取 registry 锁（`host_impl.rs` `finish_subagent`/`send_ringing` 等）构成锁环；直接缩容会放大死锁。解除需把 registry 内命令投递改为 try_send+显式拒绝语义并审计全部持锁点，超出止血范围，单独立项。
- 下一步：B2.2 replay 快路径与锁边界（入口 `ringing/v2.rs:626-751,896-965`）或 B2.4 重复恢复扫描合并（入口 `canonical/store.rs:36-48`、`canonical/tool_ledger.rs:278-328`）；B3.3 需先解 B1.1 锁环。

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
