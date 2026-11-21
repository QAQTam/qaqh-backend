# ADR-0030 exec 输出 delta 泵侧合帧：帧频与 DB 写放大的源端上限（P0 修复 F 第一半）

状态: `已采纳并实施`（经 [ADR-0034](0034-exec-flood-memory-surface.md) 承接，2026-11-20 所有者拍板"可以落地，挂 P0"）· 起草日期 2026-11-19 · 起草人 QAQ agent

> 一句话：exec 输出洪峰今天**逐块直发**，每 ≤8KiB 块在单线程 owner 泵上消耗一次日志 append +
> 一次 SQLite 写事务 + 一次 SSE 广播帧；本 ADR 在泵上加"合帧批"，把帧频钳到窗口/字节帽上限，
> **wire 与读面零变更**。它治的是帧频（CPU/IPC/写事务），不是带宽——带宽与共享回放尾隔离
> 是另两个议题（见决策 4 非目标）。

## 背景（每条均已对当前工作区源码核实，附 file:line）

### 1. 源端零节流，逐块直发

- capture 按 `MAX_EVENT_BYTES`（8KiB，ADR-0017 §背景 1）切块；`publish_chunks` 对每块各调一次
  `publisher.output`，无合帧、无节流、无折叠、无熔断
  （`crates/qaqh-process-tools/src/exec_session/manager.rs:946-958`）。
- **P0-2026-11-10 报告把 exec delta 无节流列为 L2 放大器**（帧频决定活锁振荡频率，
  `docs/p0-2026-11-10-ringingv2-attach-livelock-report.md` §4-L2），修复方案 F
  （重复行折叠 + 速率熔断，§6-F）在案；本轮 grep 全仓验证 **F 未落地**——exec_session
  路径无任何 fold/throttle/熔断实现。10MB/s 输出 ≈ 1250 delta/s。

### 2. 泵侧每条 delta 的真实代价：一次写事务，全在单线程上

- owner 泵是 `std::thread` + `std::sync::mpsc`（`owner.rs:18,240`），循环
  `while let Ok(msg) = lifecycle_rx.recv() { pump.process(msg) }`
  （`owner.rs:250-257`）；通道无界（`owner.rs:1201-1206` 注释）。
- `ExecSessionOutput` 臂对**每条**消息做：日志 append → `update_exec_session_progress`
  **SQLite 写事务** → emit 广播（`owner.rs:1400-1439`）。三步全串在泵线程上，而同一泵
  还串行承载 Request/Settle/Upsert/Output 等全部业务消息（`OwnerMsg` 词表
  `owner.rs:186-209`）。1250 delta/s = 1250 写事务/s，泵先于前端被拖死——P0 那次
  "洪水窗口内零 turn 行落盘"正是泵饥饿的形状（P0 报告 §1）。

### 3. 帧洪峰还冲刷全会话共享面

- `EVENT_CHANNEL_CAPACITY=1024` 广播 + 回放尾 `REPLAY_FRAME_CAP=2048` / `REPLAY_BYTE_CAP=4MiB`
  （`owner.rs:38-44`）是**全会话所有事件共享**的。exec delta 洪峰把消息/工具/交互事件
  挤出回放尾 → 全体 SSE 消费者更容易 Lagged→追帧→resync（`axum_impl/v2.rs:1082-1096`）。
- ADR-0028 §D3 约束 4 以同一论证把 shell 字节挡在 `SessionEventFrame` 外
  （`0028:195`："把 shell 流量塞进为会话设计的 1024 容量广播 + 4MiB 回放尾"）；
  但 exec delta 是事件词表**合法居民**（`events.rs:160-166`），结构上挡不住。

### 4. 消费端已有缓冲，但只限内存不限频率

- 前端 reducer 只留 16KiB 尾窗 + offset 去重 + 缺口补读（ADR-0017 §背景 11、决策 5 切片 2）；
  读面区间读钳制 DB 水位（ADR-0017 决策 2）。内存有界，**每帧一次 JSON parse + IPC +
  store 更新 + xterm.write 的频率无界**——"打爆前端"打的是渲染进程事件循环，不是内存。
- 模型侧不受影响：`take_model_pending` 走 capture，不经 SSE。

### 5. 不变量与写入点（合帧必须原样保持的约束）

ADR-0017 决策 1 的定序不变量：**客户端见过 offset X 的 delta ⇒ 日志必可以读到 X**；
写入点在泵：先 append 日志 → 再 update progress → 再 emit。现行实现即此顺序
（`owner.rs:1400-1438`）。delta 事件携带 `start_offset/end_offset/source/text`
（`events.rs:160-166`）；`ExecOutputSource = Stdout|Stderr|Terminal`（`events.rs:42-46`）。

## 决策

1. **泵侧合帧批**（`owner.rs` 泵循环，唯一改动文件）。收到 `ExecSessionOutput` 开批，
   以 `recv_timeout` 持续收取；满足任一条即 flush：
   (a) 窗口超时 `EXEC_OUTPUT_COALESCE_WINDOW = 20ms`；
   (b) 批内累计字节 ≥ `EXEC_OUTPUT_COALESCE_MAX_BYTES = 256KiB`；
   (c) 到达任何**非** `ExecSessionOutput` 消息（或不同 `exec_session_id` 的消息）。
   flush 先行，随后按到达顺序处理那条消息。两参数为**常量**，不设配置面
   （ADR-0029 决策 6 同律：不留无人安装的死钩子）。
2. **flush 语义 = 现单条语义原样放大，三步顺序不变**：
   - 一次日志 append：批内全部 `text` 按到达序拼接（日志无 source 维度，ADR-0017 决策 1 原样）；
   - 一次 `update_exec_session_progress`：取批内**最后一条**的累计字段
     （`next_offset`/`output_preview`/`preview_start_offset`/`capture_omitted_bytes`/
     `stream_dropped_bytes`，均为单调状态，末条即全批末态）；
   - emit **按 source run 切分**：连续同 source 的合并段为一个 run，每 run 一个
     `ExecSessionOutputDelta`，跨 run offset 连续（事件每帧只带一个 source，不能跨 run 合帧）。
3. **wire 与读面零变更**：事件形状、offset 连续性、日志格式、`exec_session.output`
   行为全部不动。变更的只有可观测的帧边界：更少、更大的 delta。
4. **非目标（本 ADR 不做，另行立项）**：
   - 重复行折叠 / 速率熔断（终止子进程）——P0 修复 F 的另一半，改变输出内容与子进程
     生命周期，需独立裁定；
   - 前端过载降级 pull（消费端按积压阈值切换 `exec_session.output` 区间读，落
     qaqh-electron）；
   - exec 专属 ws 隔离流（把 exec 字节流挪出共享回放尾；需 ADR-0028 §D3 式显式裁定）。

## 被否选项

- **publisher 侧合帧（`manager.rs` publish_chunks）**：只能合并单次 reader poll 拿到的
  chunk 列表，无时间维度；跨 poll 洪峰下每块仍是一条 `OwnerMsg`，泵的写事务一帧未减。
  且定序不变量的家在泵（append→DB→emit），测试锚应与不变量同文件。
- **SSE 层合帧（`axum_impl/v2.rs` 每连接）**：N 个消费者 N 份重复逻辑；泵的 DB 写放大
  一帧未减；回放尾里仍全是小帧。层次错误。
- **丢帧/限流（速率限制器丢弃 delta）**：违宪 5 诚实性——SSE delta 流的字节不许丢，
  缺口在日志里必须可补读；生产端丢帧会把消费端"过载降级 pull"变成"永远 pull"。
- **窗口/字节帽做成配置项**：ADR-0029 决策 6 同律，常量优先；真有定制需求再按
  `hooks::set_exec_max_output_tokens` 门面模式接线。
- **连 `OwnerMsg::Output`（ToolOutputDelta）一起合帧**：未实测为 offender，通用性先行
  违反宪 5；实测需要时按本 ADR 同形扩展，不默认全做（ADR-0017 切片 3 同款纪律）。

## 后果

- **wire/契约零变更**：exporter `check --against docs/api-surface/v4.0` 不应有新差异
  （实施时实算留痕，宪 15）。
- **延迟代价（诚实登记）**：批内 delta 的 emit 最多延迟一个窗口（20ms）。终端渲染不可
  感知；`write_stdin` 长轮询读的是 capture，不经此路径，不受影响。
- **字节不减速（诚实登记）**：合帧只限**帧数**不限**字节**——10MB/s 输出合帧后仍是
  10MB/s（约 40 帧 × 256KiB/s），SSE 通道吞吐与回放尾换手率不变。本 ADR 治帧频
  （泵写事务、IPC 频率、JSON parse 次数），治不了带宽；共享面隔离是决策 4 第三条的议题。
- **resync 面收益**：回放尾 2048 帧在合帧后覆盖的时间窗显著变长，慢消费者更不易跌出环。
- **失败语义不变**：append 失败仍 log+继续、DB 失败仍中止 emit（现行为
  `owner.rs:1404-1428` 原样放大到批）。
- **测试锚**（`qaqh-session`，与不变量同 crate）：
  - T1 定序不变量：灌 N 块洪峰 → 断言批内合并、每个 emit 的 `[start,end)` 连续，
    且 emit 前日志已含至 `end_offset` 的全文；
  - T2 source run：stdout,stderr,stdout 三段 → flush 产出 3 个事件，run 内合并、
    跨 run 不合并、offset 连续；
  - T3 让位：`ExecSessionOutput` 后随 `ExecSessionSettle` → flush 先于 settle 处理，
    settle 的日志对账（`owner.rs:1442-1468`）看到已 flush 的批；
  - T4 字节帽：窗口内灌 >256KiB → 按帽切批，多事件，帧频上限成立；
  - T5 孤条：单条 delta → 窗口超时后 flush，行为与现状等价（无批也走同一 flush 路径）；
  - T6 混合流：`ExecSessionOutput` 与 Request/Settle/Upsert 交错 → 全局处理顺序与
    到达顺序一致。
- **闸门冲击**：无新增 CI 闸门；`qaqh-session` 现有专项测试（区间读水位钳制/字符边界/
  丢字节如实语义）不受影响（读面未动）。

## 落地清单

- [x] 仓库所有者拍板（2026-11-20，随 ADR-0034 一并落地）
- [x] `owner.rs`：两常量 + 泵循环批状态机 `run_pump_loop`（开批/续批/让位/超时 flush；
      flush = 一次 append + 一次 DB 写 + 按 run emit = `flush_exec_output`）
- [x] T1–T6 落 `qaqh-session` 测试（`exec_output_flood_coalesces_losslessly_into_source_runs`
      承载 T1/T2/T4/T5/T6，`settle_flushes_prefix_then_keeps_row_free_of_full_text` 承载 T3；
      `cargo test -p qaqh-session owner_tests` 14/14 绿，2026-11-20）
- [x] exporter `check --against docs/api-surface/v4.1` 零新差异，留痕（线已推进 v4.1）
- [ ] P0 报告 §6 修复 F 条目加注："合帧一半由 ADR-0030 承接；折叠/熔断仍开放"
- [x] `docs/adr/README.md` 索引登记

## 实施时补充决策（2026-11-20，经 ADR-0034，不构成对本 ADR 被否选项的翻案）

- 生产端 reader 批量 2048B→64KiB、`publish_chunks` 同批同源合帧、水位 `Arc<str>` 共享——
  这削减的是**信箱条目数与每条目大小**（本 ADR 决策 4 自认"字节不减速"的上半段缺口），
  不是替代泵侧合帧：泵的写事务上限仍由本 ADR 的窗口/批帽钳制。
- "被否选项·publisher 侧合帧"的否决理由（无时间维度、泵写事务一帧未减）依然成立且未被
  推翻；本节是"泵侧合帧 + 源端条目削减"两半互补的落地记录，见 ADR-0034 决策 1。
