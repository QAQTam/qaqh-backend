# 低配平台内存与性能热点审计（2026-10-22）

> 证据报告保留于此；后续实施、批次状态和交接统一见 [内存热点整改执行计划](plan-memory-hotspots-remediation-2026-10-22.md)，不再重复审计。

## 0. 结论与证据边界

**确有架构性放大，不是“context 文本本来就这么费内存”，也不能主要归咎于 Rust、核显或 allocator。** 目前同时存在：

1. 关闭 worker 不等于卸载所有 session 投影；多 session 的历史工作集持续留在 daemon。
2. 同一 session 的 writer 与 V2 hub 各持一套完整 ProjectionSet，外加 MessageStore 和独立的流式 TimelineAppender。
3. 封存正文卸载后，周期 checkpoint 又把全部历史恢复成全文并全量序列化；异步化只移走阻塞点，没有消除内存/IO 成本。
4. “有界”的多个容器只是按条数或按 session 限制，没有全 daemon 字节预算；部分字节账本还漏算正文。
5. 发请求前深克隆整个内部 Message，再深克隆/序列化做预检；预检每 lap 重复两遍，甚至复制只供展示的 diff。
6. exec 完整捕获在命令结束后仍被注册表持有；子代理另有订阅桥生命周期缺口和结果正文滞留。
7. 监控遗漏上述多个重型持有者，导致“业务文本不大、组件估算也不大、RSS 却很大”的观测错觉。

本轮不启动任何子代理，不改业务行为，不访问真实会话正文。CodeGraph callers 用作导航，关键结论逐处对照源码。基线为 `80ddb3c` 上的工作树；开始时已有 service.rs、grouping.rs 修改及 docs 删除，不属于本轮修改。

验证：当前源码 `cargo build --offline -j 1 -p qaqh-runtime --lib` 成功；独立生产依赖探针编译、全部模式执行成功；包含的生产 token_calibration 模块 10 条测试通过。**没有运行整个后端测试套件，也没有拿实际用户 daemon 做 RSS 归因。** 不声称本报告中的 MiB 就是现场全部损失。

分配器探针统计 System allocator 的 requested live/peak heap bytes，不包括 allocator 元数据/碎片、栈、映射、文件页缓存、外部进程和 GPU。debug 时延只作方向证据，不能当发布版 G2020 延迟 SLA。

## 1. 可复现结果

探针：`crates/qaqh-runtime/examples/memory_hotspot_probe.rs`；独立入口：`probes/memory-hotspots/Cargo.toml`。完整读数：`probes/memory-hotspots/results-2026-10-22.txt`。

| 场景 | 当前源码实测 | 含义 |
|---|---:|---|
| 空 worker 的 cmd/event 两条队列 | 15,828,344 B ≈ 15.10 MiB | 尚无上下文，固定大容量队列已申请内存 |
| 8 MiB incremental checkpoint | 稳态增加约 16 MiB；journal payload 报 0 B | block 全文 + replay arg 双持有；预算漏算 |
| 上述 turn 模拟成功 offload | 仍留约 8 MiB | 正文壳化不释放 journal 内的那份正文 |
| 4 MiB user_text timeline 后 forget_session | 约 8 MiB 不下降；snapshot 仍存在 | forget 不清 timeline；并非 allocator 未归还已释放对象 |
| ContentStore 写入并落盘 4 MiB | 常驻约 4 MiB；GET clone 持有时约 8 MiB | 磁盘外置不等于内存卸载；GET 整体深克隆 |
| 12 个已完成条目，各 stdout 1 MiB | 留约 12.05 MiB | get_info 只显示 4000 B tail，完整捕获仍在 |
| reader 历史从 256 增至 1024 条（每条含 4 KiB cwd） | open/读单条的峰值由约 3.19 增至 12.76 MiB | 返回一条不等于只物化一条 |
| 同上 read_range 只返回一条 | Vec capacity 分别仍为 256、1024，SessionFact=560 B | filter/collect 此构建保留原 Vec 容量，返回值本身也被历史规模放大 |
| V2：1024 个各 8 KiB inline input，合计 8 MiB | 常驻约 17.31 MiB；bootstrap 在手时约 34.05 MiB | conversation/timeline 双正文投影 + bootstrap 深克隆 |
| 同一 session 再开 writer CanonicalSessionStore | 额外常驻约 16.74 MiB | writer 与 hub 双 ProjectionSet，不是 Arc 共享同一对象 |
| V2 上述常驻态的 memory_components | 仅 2328 B 的浅下界 | 不是可以拿来与 RSS 对账的完整估算 |
| V2 subscribe(None)，cursor 已追平 | 额外瞬时峰值约 24.70 MiB | 零历史补帧仍全量 replay/reduce |
| 8 MiB MessageStore build_context | 额外约 8.19 MiB | owned request 深克隆 |
| 上述 request 再做一次 production metrics | 额外瞬时约 24.21 MiB | accounted 深克隆 + JSON buffer 等；原 request/store 仍在 |
| 模型仅 `ok` 两字，展示 diff 4 MiB | ToolResult 新持有约 8 MiB；clone 再加约 8 MiB | 同一 diff 在顶层和 display.diff 各一份 |
| 已 offload 的 4 MiB 历史，增加 21 B TextDelta | 快照被重写为约 4 MiB；额外瞬时约 20.07 MiB | 瞬态触发全历史 rehydrate/clone/serialize |

注意：V2 输入实验专门选合法的 ≤8 KiB inline input，以隔离副本机制。assistant/tool 的 canonical 正文多为 ContentRef，**不能把这个倍数直接套在全部 conversation 文本上**。read fixture 是 schema-valid 的重复 SessionCreated，目的是隔离 reader，不代表真实对话事件组成。

## 2. 内存持有图：为何 context 只是总量的一小部分

```text
QaqhService / daemon
 ├─ AgentRegistry.instances[session]
 │   ├─ worker thread: AgentState
 │   │   ├─ MessageStore: 模型历史 + ToolResult（含 data/display/diff）
 │   │   └─ ToolLedger
 │   │       ├─ CanonicalSessionStore -> ProjectionSet A
 │   │       └─ entries / input_accepteds / communications / terminal facts 等索引
 │   ├─ event reader thread
 │   └─ cmd/event 预分配队列（每 worker 实测约 15.1 MiB）
 ├─ V2ProjectionHub.sessions[session] -> ProjectionSet B + broadcast + replaceables
 ├─ RingingHub.timeline.sessions[session]
 │   ├─ turns/rounds/blocks（封存后部分壳化）
 │   ├─ journal（独立 owned event，包括正文）
 │   └─ next_fragment 与 session lazy-load 锁
 ├─ ContentStore -> bytes（已落盘仍可常驻）
 ├─ ProcessRegistry -> full stdout/stderr + tails（所有 session 共享）
 └─ task/board stores、agent outcomes 等另有 daemon 级索引

瞬时：
MessageStore -> owned Vec<Message> -> accounted Vec<Message> -> JSON
             -> provider DTO/body；ChatGPT plan 还保留重试用 messages.clone()
timeline checkpoint -> replay clone + audit clone + 全历史 rehydrate
                    -> snapshot.clone() -> serialized JSON Vec<u8>
```

依据：`agent/state/agent.rs:153-216`、`canonical/tool_ledger.rs:252-273`、`canonical/store.rs:30-48`、`ringing/v2.rs:93-118`、`timeline.rs:1352-1367`、`process_registry.rs:115-145`（以上均按 crates 对应目录）。这些角色的逻辑区分可以成立，但**逻辑视图独立不要求每个视图同时拥有所有历史正文**。

## 3. P0：释放边界错误与 daemon 无总预算

### 3.1 worker close 只回收部分持有者

`crates/qaqh-runtime/src/service.rs:289-296` 只 reset_images + hub.forget_session。
`crates/qaqh-runtime/src/ringing/hub.rs:197-206` 只清 live_interaction_content、live_workers、content；没有清 timeline、lazy_loads、timeline store 索引。
`crates/qaqh-runtime/src/ringing/v2.rs:723-751` 按需插入 sessions，当前没有配套卸载/LRU/TTL；service close 也未调用 V2 释放。

因此 close 掉 worker 后，MessageStore/worker 队列可以释放，但 V2 投影、timeline replay、壳和键索引继续活着。浏览过/运行过的 session 数成为隐式内存增长维度。forget 探针已直接证明 timeline 持有不变。

worker idle unload 实现存在（`registry.rs:1984-2009`），但默认 `session_idle_unload_secs=0`（`qaqh-config/src/config.rs:144-147,802`）。即使启用，仍走同样不完整的驻留释放链。

**判定：生命周期/所有权架构缺口。** 不是必须维持历史完整性的代价；完整历史可在磁盘保留。卸载前必须 durable flush、停止产生端并协调 live subscribers，不能简单 remove 后让活 writer 重新插回。

### 3.2 每 session 的大上限不是全局上限，且账本漏算

`crates/qaqh-runtime/src/ringing/persistence_policy.rs:42-53`：8192 entries / 256 MiB **per session**。16 个 session 单算名义 payload ceiling 就是 4 GiB，未包括 MessageStore、投影、快照峰值与队列；这不是实际占用预言，但证明该预算与 8G 主机的总资源无关。

`timeline.rs:1372-1384` 的估算漏项：
- BlockCheckpoint 只数 text，漏 arg；正常增量恰好把正文放在 arg（`timeline.rs:787-799`）。
- ToolUpdated 漏 args_json、display、error 等嵌套分配。
- TurnOpened.user_text、BlockOpened 的 nested payload 等落到 `_ => 0`。

当前构建 `TimelineEntry` 本体 792 B，仅 8192 个结构 slot 就约 6.19 MiB，尚未算字符串和集合 slack。注释中“约 200 B 信封”的规划不能用来评估 Rust 进程内 enum 大小。

seal 不裁 journal 是为断线恢复保留最近尾部的合理语义（`timeline.rs:1191-1204`）。**不能粗暴恢复“seal 即删光”而重新制造 gap 缺终态问题**。正确方向是 global byte budget + short replay window + snapshot/reset 自愈，而不是给每个历史 session 留 256 MiB。

## 4. P0：卸载又回灌——快照持久化路径的架构反复

`ringing/timeline_hub.rs:446-468` 对所有非 TurnSealed intent 都 request_timeline_persistence，未判断事件分类。
`ringing/persistence_policy.rs:75-89` 的“不让 TextDelta/Checkpoint 触发重写”函数只有自身测试消费；CodeGraph callers=0，并经源码搜索确认无生产调用。

worker（`timeline_hub.rs:79-111`）执行：offload sealed turns → snapshot → replay_since(0) → journal.clone 供轻量 audit → prune → rehydrate 全部 offloaded turns → persist。
`timeline_store.rs:187-193` 又构造持有 `snapshot.clone()` 的 PersistedTimeline，并用 to_vec 生成完整 JSON buffer。

**关键冲突：** 内存优化把 sealed turn 壳化，checkpoint 为“快照脱离 sidecar 也可独立恢复”又将全部全文恢复；每个 session 的旧历史在每次活跃 checkpoint 中反复变热。4 MiB 封存历史 + 21 B delta 探针测到约 20 MiB 瞬时峰值，且确实改写快照。

这不是异步持久化已经解决的问题：异步只让前台不直接付出延迟，CPU/带宽/文件缓存/内存压力仍在，且共享锁使其他 session 承受间接反压。

**整改：**
1. 先接通分类，不让每个瞬态 intent 都排快照；定义清楚流中 crash checkpoint 的独立周期，而非把所有事件当结构事件。
2. audit 只传 `(seq,ts,kind)`，不深克隆整条含正文 journal。
3. checkpoint 使用借用序列化/to_writer，先去掉 snapshot.clone + 大 JSON Vec。
4. 根治：封存 turn 正文只落一次 append-only/CAS；快照持有索引、引用和活跃 tail。持久恢复必须把 sidecar/blob 纳入一致性契约，不再同时要求“已卸载”与“每次生成完整独立全文快照”。

## 5. P0：canonical reader 的“范围接口、全量实现”

`crates/qaqh-session/src/canonical/reader.rs:31-45`：open 就 read_committed()，全量解析结果随后丢弃。
`:68-101`：read_all 再解析一次；read_range 先读全量再 filter；read_after 复用此路径。
`:104-150`：整个 committed prefix 的原始 bytes 和全部解析 SessionFact 同时在内存。

读最新一条不是 O(一条)，而是 O(全历史 bytes + 全历史对象)。本构建还实测到返回单条 Vec 保留全量 capacity。这不是 Rust 抽象的必然成本，而是 iterator ownership + collect 实现使“逻辑长度小”掩盖 backing allocation。

**放大入口：**
- ToolLedger::open：CanonicalSessionStore 先重建全 ProjectionSet，再 from_store 又全量建各索引（`canonical/store.rs:41-47`、`canonical/tool_ledger.rs:287-328`）。
- V2 load_session_state/read replay 均 open + read_all（`ringing/v2.rs:896-918,933-965`）。
- V2 subscribe 不论 since=None 或已追平，仍 replay_after 并从头重建临时 ProjectionSet（`:689-700`）。期间还持该 session state mutex（`:636-639,711`）。
- 无 targets 的 wait_agent：每轮 25ms sleep 后重新打开 reader（`host_impl.rs:352-380`）；即使 read_after 返回空，open 已完整扫描（`:79-91`）。实际频率受扫描成本限制，并非每秒保证 40 次，但空等也会反复烧 CPU 和搬历史。
- model_request_context 为找当前 TurnStarted 从全量 Vec 反向找（`agent/request_context.rs:44-52`）。

**整改顺序：** read_after high-water/no-replay 快路径 → open 不物化全量 Vec → streaming validation/iterator → commit-offset/cursor 增量读取与稀疏索引 → recovery/checkpoint 验证与日常查询解耦。不能以优化名义取消 committed-prefix、identity、seq 连续性和 fail-closed 校验；应保存验证水位/代际，在稳定前缀上只验证增量。

wait_agent 改为提交后通知 + 序号 predicate（防丢唤醒），timeout 只当兜底，不反复从磁盘重演整个 mailbox。

## 6. P1：重复投影与无界历史视图

`CanonicalSessionStore` 与 `V2SessionState` 各自拥有 ProjectionSet；append 时 writer apply 一次（`canonical/store.rs:98-101`），sink 又 apply 一次（`ringing/v2.rs:771-774`）。ToolLedger 在 worker lifetime 保留（`agent/state/agent.rs:201-206,274-293`）。

ProjectionSet 对每条 fact 调用七个 reducer（`qaqh-session/src/projection/set.rs:53-112`），snapshot 对七个视图全部 clone（`:117-126`）。V2 HTTP team 只要 team 却调用全 bootstrap（`qaqh-daemon/src/axum_server/axum_impl/v2.rs:305-307`）；bootstrap 正常返回还会 clone conversation/control（`:236-253`）。

conversation/timeline/control 的多个 upsert 通过 Vec.iter().position 查历史（`projection/conversation.rs:349-429`、`projection/timeline.rs:215-244`）；大量不同 id 重建具有 O(N²) 形态。Conversation compaction 会裁 context（`conversation.rs:338-346`），但 timeline 保留历史 entries，turn/control/ledger 多个索引仍持续积累。因此压缩模型窗口不等于压缩 UI/控制/幂等工作集。

**判定：** CQRS/事件日志/可重建视图本身不是错误。错误是“每个读者重建全套 + 多个全生命周期 owner 同驻留 + Vec 全史线性查找”。应明确唯一增量 projection owner，writer 使用最小 admission/recovery 索引，UI 取 bounded page/control summary；按 key 索引定位，payload 用不可变引用而不是嵌套 String 深复制。

本报告不建议再加一层通用事件总线/全局缓存抽象来“统一”现有副本；那容易把 N 套工作集变成 N+1 套。

## 7. P1：工具结果把模型面和展示面绑进同一个重容器

`qaqh-tool-core/src/tool_api/typed.rs:139-160` 同时生成 model_text、JSON output_value、display。
`tool_api/output.rs:180-234` to_tool_result 克隆 output 到 data、model、display 等；`:201` 顶层 result.diff 克隆一次，`:340-343` display.diff 又克隆一次。
`qaqh-file-tools/src/file_mutate.rs:173-200` 的 mutation_display 会附完整 diff（body 有 clamp，但 diff 字段不在该 clamp 中）。

`qaqh-message/src/store.rs:930-959` 存整份 ToolResult；`:1034-1081` build_context 深克隆整套 Message/ToolResult，而不是只取 model projection。因此 diff 即便绝不发 provider，也被请求快照和预检再复制、序列化。

实测 4 MiB diff，model 仅 2 B，wire result 持 8 MiB diff，两份完全相同；request/history clone 又加 8 MiB。工具文本短不能作为内存规模指标。

**整改：** 热 MessageStore 只持 ModelMessage/ModelToolResult；展示的 diff/大 data 存 CAS ref，UI 按需读取。过渡期至少消除 top-level/display.diff 重复物化；用 into_tool_result 转移拥有权、正文 `Arc<str>`/Bytes 减少兼容层复制。Arc 只能减副本，**不能代替淘汰和卸载**。

标准模式确有模型侧截断：`qaqh-types/src/tool_result.rs:16` 24K chars、`qaqh-workspace/src/tool_side_fold.rs:68-85` exec 8K chars 等；它们只限制模型可见面，不是 ToolResult 的总堆限额。不要以为再调小模型截断就能回收 diff、data、full capture。

## 8. P1：请求预检成为常态化大内存计算

`agent/engine_turn.rs:1169-1183` 在每 lap 做 estimate_prepared_request，再 prepared_request_key。
`agent/state/agent.rs:358-369,389-395` 两者均调用 prepared_request_metrics。
`agent/state/token_calibration.rs:221-248` 每次都 messages.to_vec → redact inline image → JSON String → count_tokens → hash。

结果：同一请求的整套复制、序列化、计数、哈希重复执行两遍。两遍通常是顺序发生，**不能把两次峰值直接相加成同时驻留**，但累计分配/CPU 成本确实翻倍。8 MiB 纯文本案例，单次 metrics 已额外约 24.21 MiB 瞬时 requested heap；build_context 的约 8.19 MiB request copy 和原 store 还在。

token calibration 在自动压缩停用后仅用于遥测（`agent/state/agent.rs:180-190`），但重计算无条件运行。PrefixShape 又对每条完整 Message JSON 序列化再 hash（`:92-120,620-641`）。ChatGPT plan 为 401 重试额外保留 messages.clone（`engine_turn.rs:1231-1239`）。

**判定：** 遥测开销侵入生产热路径、未共享中间结果，是明确冗余设计。

**整改：** metrics 一次返回 estimate/key；按 context_revision + tools revision 增量记账/缓存；遥测采样或 debug 开关；对借用的模型投影边写边 count/hash，图片占位在 serializer 级处理，避免“先复制 base64 再删”。前缀 hash 在消息定型时计算并缓存，不逐 lap 重序列化整史。重试共享不可变 request/body，而非另留一套 nested Messages。

## 9. P1：exec 捕获驻留不是 process check 的尾部大小

`qaqh-process-tools/src/process_registry.rs:75-110,130-145`：full stdout/stderr 每流 cap 5 MiB，同时另存小 tail。
`:605-617` mark_exited 不清 full；`:565-578` captured_full 仍返回 clone；TTL 600s 只由 register 中惰性触发（`:151,269-270,395-403`），`get_info` 不驱逐（`:673-730`）。停止运行新命令不保证十分钟后自动掉内存。

256 是条目软上限（`:164-169`），并非字节上限；256×两流5MiB=2.5GiB 名义 payload，活进程可超软上限，String capacity 还可能超过 len。不能称这是全局硬内存界。

exec 收尾为统计 len 都先 captured_full clone 一次，再 captured_full clone、combined、strip_ansi、token_truncate（`exec/direct.rs:406-469`）；display streams 和模型 output 还会分别生成（`:472-486`）。四个并发工具是每 batch 的上限（`agent/tool_runtime.rs:44-45,173-184`），不是整个 daemon 的外部进程/捕获总额。

**整改：** seal 完成后将完整输出转移到磁盘 spool/CAS，注册表仅保 status/os_pid/exit/tail/ref。进程监督信息与正文 TTL 分离；全局/每 session 捕获字节额度及并发 admission；timer 主动扫而非下一次 register 才扫。读泵仍要继续排空管道，不能通过停止 read 制造子进程死锁；不能为了省正文误删清理孤儿所需的 pid/墓碑。

## 10. P0/P1：子代理是完整 session 成本，且完成后还有桥与结果持有

子代理不复制父 MessageStore 的全史：任务按显式 task/context 构造（`qaqh-subagent/src/lib.rs:802-810,970-1000`）。不能把“fork 父上下文”当未查证根因。

但它有完整 worker+reader（`registry.rs:1172-1242`），另有 collector thread（`qaqh-subagent/src/lib.rs:1192,1238`），还有 host subscribe bridge（`host_impl.rs:532-579`）。空 cmd/event 队列约 15.1 MiB 对 child 同样适用，未计栈和上下文。

完成后确实会 close worker（`qaqh-subagent/src/lib.rs:1560-1571`），不是全然没有 auto-unload。剩余问题：

1. **订阅桥线程退出缺口，静态确认，未做生命周期运行复现。** bridge 仅在 tx.send 失败或 Reset 时退出；无事件时只 sleep(20ms)。collector 结束丢 rx 后，只要不再有可转发事件，就不会 send、不检测断开。V2 sessions 保持 sender；`V2Subscription::try_next` 又把 Empty/Closed 都返回 None（`ringing/v2.rs:822-836`），没有闭流判据。桥可继续持有 receiver/线程栈，未来发送会在这个无人消费的订阅中积压，直至新事件/lag 才可能收敛。
2. `subagent_outcomes` 存完整 answer String（`registry.rs:587-597`），close 不清它；唯一 remove 在重新启动 child 的路径（`:1056`）。保留结果可查询是合理语义，永久留全文不是唯一实现，可用 durable ref + bounded cache。
3. `wait_for_agent_outcomes` 为 `.is_some()` 调用返回 cloned AgentOutcome 的接口（`host_impl.rs:972-976`、`registry.rs:604-605`），多个 targets 等最后一个时，会反复克隆已完成者正文。存在性判定应借用/contains_key。
4. 子 session 同样逃不出 V2/timeline 卸载缺口；完成 worker 数变少，visited projection 数不一定减少。

**整改：** subscription 具有显式 cancellation token/关闭事件/JoinHandle；结束 collector 就取消并 join bridge，区分 Empty 与 Closed，改成一个可取消的 async task 而非每 child 独立轮询桥；结果正文磁盘引用，元数据和 pending 状态保留。逻辑 agent identity 可以长期存在，worker、subscription、正文 cache 必须短生命周期。

## 11. P1：ContentStore 是缓存，却像正文仓库一样驻留

`ringing/content_store.rs:117-141` 256 entries；pinned 另有每 session 4MiB 配额，但普通 body 没有总 bytes cap。
`:397-405` put 将 bytes 保留，clone entry 再持久化；`:408-421,444-464` 首次读全量加载并缓存，get 再返回整份 clone。
`:621-650` TTL sweep 是惰性的，驱逐按条数/到期而非驻留 bytes。纯 metadata 启动加载是好的，但“第一次读取后一直持正文”的热集合仍可很大。

HTTP Range 也只是响应切片：`qaqh-daemon/src/axum_server/axum_impl/content.rs:28,86-118` 先 get/load 全 body，再 `bytes[start..=end].to_vec()`。请求 1 KiB 不等于只读/分配 1 KiB。

**整改：** 磁盘型 store metadata-only 默认，有限字节 LRU 才缓存热点；Range 先校验归属/范围再 seek+stream。内容归属与正文驻留预算分离。SessionBlobStore（`qaqh-session/src/canonical/blobs.rs:26-75`）只保 dir，不是第二个内存大 body cache，不能把两个 store 因名字相似直接判为同一重复实现。

### 附带一致性风险，不作为本轮内存量化结论

`agent/loop_dispatch_conversation.rs:70-73,115-126,141-144` 的 >8KiB input/communication 仍把 canonical ref 指到 TTL ContentStore；与 `canonical/blobs.rs:3-7` 的“不允许事实引用传输缓存”契约相冲突。当前 content endpoint 有 blobs fallback，但此写入点没有 put_blob。close/TTL 后可能影响 refs 可恢复性。应单列数据正确性回归，不借修内存之名直接清 body 而丢正文。

## 12. 不应优先攻击或误判的部分

- 一个有界 MessageStore 的 model history 是必要工作集；其大小高于 UTF-8 文本，包含 Message/Vec/enum/JSON 对象、capacity，不能要求 RSS=文本字节。
- canonical facts/blob/messages 多格式落盘不等于等量内存常驻；本报告只对已经查到的 owned 持有链下结论。
- image registry 现为 SHA/mime 索引，不常驻 base64（`qaqh-file-tools/src/read_image/mod.rs:48-54`）；旧注释“存 base64”不代表当前实现。
- file_cache 只存 path/hash/line_count 且 cap=64（`qaqh-fs-core/src/file_cache.rs:13-22`）；不是全文缓存嫌疑主犯。
- memwatch 自身 4096 samples/512 session gauges/64 component gauges（`qaqh-memwatch/src/lib.rs:13-15`），保 metadata；比起修它的几条记录，更应补其对 ToolLedger/V2 深层/队列/ProcessRegistry/临时工作集的遗漏。
- glibc 碎片和线程 arena 确会抬高 RSS 水位，但 `malloc_trim` 只能还已空闲页，不能释放活着的 String/Vec。当前 close 已调用 trim（`service.rs:278,318`、`service/common.rs:7-15`）；继续加 trim 不会治愈本报告的活对象滞留。
- LSP/MCP 外部 server、代码索引、编译器等可能是整机热点，但本轮没对其工作负载做进程 RSS 归因，不能把全部系统内存记到 daemon。队列申请量不等于已触碰的 resident pages；线程虚拟栈大小不等于物理占用。

## 13. 整改优先级与低配目标（提案，不是已实现能力）

### 第一批：低风险止血，先做可验证的工作集收口

1. P0：修 journal payload 深度计量与漏项，补大 args/display/TurnOpened 回归；引入 daemon 总 budget，而非只减 per-seed 数字。
2. P0：接通 persist 分类，audit 传小元数据，去掉 snapshot.clone/JSON 大缓冲；验证 TextDelta 不再触发全史重写。
3. P0：reader/no-replay 快路径、wait_agent 提交通知；空等不解析历史；不破坏 recovery fail-closed。
4. P0：collector/bridge 明确成对取消、join、unsubscribe。
5. P1：每 lap metrics 算一次；hash/count 缓存或采样；请求只含模型需要的字段。
6. P1：worker cmd/event 队列缩至较小可配置容量，保留背压；用真实吞吐/等待时间选容量，不继续按“能堆很多帧”设计。先试 cmd 64–256、event 256–1024；大 enum 可改 boxed/Arc envelope，但不能无预算缓冲 payload。
7. P1：exec seal 后卸正文、保监督元数据；ContentStore 按 byte cap 运行。

### 第二批：真正解决多 session 的结构性问题

8. lifecycle coordinator 定义 resident session，关闭/idle/memory pressure 共同驱逐 writer/view/cache/index。已订阅 observer 不意味着全史永久热驻留；未完成 turn、pending approval、正在落盘对象要明确 pin。
9. 每 session 单一增量投影 owner；writer 最小索引；typed snapshot/page API 只返回所需视图，不为读 team clone conversation/timeline。
10. MessageStore model history 与 UI display/blob 分离。封存全文只落一次，内存保 active tail + summaries/refs；恢复/分页按 byte/page 预算。
11. durable idempotency 需要可查，但不要求每个历史 receipt/fact 常驻 HashMap；把旧 key/terminal 索引放磁盘或可验证 checkpoint。

### 建议的 8G 档起步预算

以下是测试起点，不是从现场测得的最优值：默认最多 1–2 个主动发模型请求的 worker、全 daemon 工具并发 2；热 idle workers 1–2、120–300s idle TTL；timeline replay 全局 32–64MiB、每 session 2–4MiB，超限 reset/rebaseline；ContentStore loaded bodies 全局 16–32MiB；capture 内存全局 16–32MiB，超出 spool 而非静默丢输出。daemon 非模型正文缓存合计先争取 ≤128–256MiB，并保留明确 telemetry。

先按压力信号排队/卸载，不建议自动改变用户已明确选择的模型上下文或恢复已禁用的自动压缩。模型上下文太大时可提示手动压缩；这只能减模型工作集，不能代替 session projection 驱逐。

## 14. 修复验收：必须证明工作集有平台期

| 回归场景 | 必须满足 |
|---|---|
| 1→10→50 个 session 逐个打开再关闭 | 无 subscriber/pin 时 resident projection/body 回到 global cache 上限，而非随 visited session 数增长 |
| sealed 旧史 1/10/100 MiB，当前追加 21 B | 每次 transient 额外内存/IO 不随全部旧史线性增长；快照不全历史 rehydrate |
| read_after 已追平、持续等待 30s | 不重复读/解析已验证历史；即使日志大也近 O(无新增) |
| 8MiB 模型文本 + 大展示 diff | 请求准备不复制 diff/data；预检只做一次；校准 key/缓存稳定性测试仍通过 |
| 100 次 noisy exec | seal 后 full capture 回到全局字节上限；tail/pid/墓碑仍可用于 check/kill |
| 连续完成 100 个子代理 | collector/bridge/subscriber 数回落；逻辑 identity 可保留，正文 cache 有界 |
| 1KiB HTTP Range 读大 blob | 临时内存与 range/chunk 大小相关，而非整个 blob |
| gap/reconnect、crash/recovery、compaction | 历史可恢复、终态可见、exactly-once admission 与引用完整性不退化 |

观测应至少包含：store/model bytes、display/blob refs、timeline shell/journal 深层 bytes、V2 与 writer projection bytes、queue slot/payload bytes、capture bytes、subscriber/thread/pin 数、每阶段 requested heap/allocator live（可选）、RSS/RssAnon/PSS/Swap、外部子进程 RSS。**不得把目前 shallow heap estimate 相加后当作 allocator 总账。**

## 15. 重跑方式与本轮未修事项

```bash
# 在 qaqh-backend 根目录；TMPDIR 要指向可写目录。
TMPDIR="$PWD/target" cargo run --offline \
  --manifest-path probes/memory-hotspots/Cargo.toml --target-dir target -j 1 -- all
TMPDIR="$PWD/target" cargo test --offline \
  --manifest-path probes/memory-hotspots/Cargo.toml --target-dir target -j 1 -- --test-threads=1
```

探针的若干 assert 是“现状缺陷证据”，不是修复后应保持的行为；修完相关问题需反转为新契约。全部临时 fixtures 在 probe 的 Scratch 目录，正常返回后删除。初期 prebuilt-rlib 探索不作为最终结果；最终证据来自当前源码编译的独立入口。

本轮仅落分析与探针，不实施以上业务/架构改造。业务库构建通过不等于全套测试通过；没有用真实 daemon 跑多 session 压测，没有现场 RSS/Swap 时间序列，没有 release benchmark，也没有实际启动子代理来复现订阅桥线程问题。
