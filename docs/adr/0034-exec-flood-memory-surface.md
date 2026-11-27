# ADR-0034 exec 洪峰内存面：源端水位共享 + 全文权威源=日志文件（P0）

状态: `已采纳并实施` · 2026-11-20 所有者拍板（"可以落地，挂P0，LTS 不容许这类 bug 生产上线"）· 起草 QAQ agent · 承接 [ADR-0030](0030-exec-output-delta-pump-coalescing.md)（泵侧合帧），本 ADR 治其未覆盖的**每条目大小、条目数与 settle 落盘面**

> 一句话：exec 洪峰把 daemon OOM 的根因不在 capture 封顶（它一直是够的），而在**传输与落盘面**——
> 每条信箱消息背 16KiB preview 全量克隆（≈9x 放大）、每个 chunk 陪跑 1MiB head/tail snapshot
> clone（≈500x 瞬时放大）、settle 把整段日志读回内存再往 content store 复制一份。本 ADR：
> 源端合帧 + 水位 Arc 共享 + 全文权威源收敛为 append-only 日志文件本身。泵侧合帧按 ADR-0030 实施。

## 背景（2026-11-20 运行时复核的行文根因）

1. **信箱无界 × 每条目放大**：`spawn_reader` 以 2048B/批读管道（`exec_session/manager.rs:901`），
   `SessionOwner::send` 的通道无界且无投递预算（`owner.rs` 注释自证"通道无界"）。每条
   `OwnerMsg::ExecSessionOutput` 携带 `text`(2KiB) + `output_preview` **16KiB 全量克隆** ⇒
   200MB 突发 ≈ 100,000 条 ≈ 1.9GB 驻留峰值。泵侧每条还做三次同步 IO（文件 open/append +
   SQLite UPDATE + 广播），消费速率 ≪ 生产速率，积压全落在 daemon 堆上。
2. **snapshot clone 瞬时放大**：`publish_chunks` 每批 `capture.snapshot()` 一次、每 chunk 再
   clone 一次（manager.rs:946-958），head(≤512KiB)+tail(≤512KiB) 被 `OwnerExecPublisher`
   直接丢弃（只取 4 个标量 + preview）——约 500x 的无谓分配，全在 capture 锁临界区内。
3. **settle 整读回灌**：`ExecSessionSettle` 用 `read_to_string` 把整段日志读入（200MB 一份），
   经 `finish_exec_session` sha256 后**再复制进 `session_contents` BLOB**（第二份 + WAL 翻倍）
   并永久双存。`exec_sessions` 表本无 output_text 列，`exec_session_view` 只投
   `output_content_id`——该复制没有新增读者：全文读取面早由 0017 决策 2 的区间读承载。
   0017 §被否选项当时已预警"终态全量入 SQLite content 会与大日志重复存储"。

## 决策

1. **源端合帧 + 水位 Arc 共享**（manager.rs / capture.rs / tool_job.rs / owner.rs）：
   - reader 批量 2048B → **64KiB**（对齐 `exec::pipe` MAX_READ_CHUNK），洪峰 publish/信箱
     条目数按 32x 削减；
   - `publish_chunks` 把同批同源相邻段合为一帧（offset 分配口径不变——capture 内部仍按
     MAX_EVENT_BYTES 切块记账，合并只改可观测帧边界；解码最坏 ×3 膨胀 < 256KiB，与
     ADR-0030 泵批帽同界）；
   - `ExecSessionPublisher::output` 的 `OutputSnapshot` 参数换 `&ExecOutputWatermark`
     （`next_offset` / `preview: Arc<str>` / `preview_start_offset` / `capture_omitted_bytes`）
     ——head/tail 类型不再出现在发布签名里，结构性护栏而非口头约定；
   - `OwnerMsg::ExecSessionOutput.output_preview`: String → `Arc<str>`：同批共享一份物理
     preview，信箱积压字节 ≈ 输出字节 1:1。
2. **settle 不读日志内容**（owner.rs `ExecSessionSettle` 臂，修订 [ADR-0017](0017-exec-full-output-terminal-lifecycle.md) 决策 3 的**写入面**）：
   对账 = `metadata.len()` vs 库水位（O(1) 内存）；日志存在 ⇒ 不生成 content 复制
   （`output_text=None` → 行 `output_content_id=None`），落盘缺口如实降级 `output_complete`
   （前端"仅显示尾部"横幅依此字段，宪规 5）；日志缺席（append 曾失败/旧会话）照旧回退
   封顶 transcript（≤1MiB）。
3. **全文权威源 = append-only 日志文件**（0017 决策 1 的地位不变、独家化）；读取面唯一
   走 `exec_session.output` 区间读（0017 决策 2）。

## 被否选项

- **content store 复制保留、改流式 sha256 计算**：峰值减半但 200MB 级"文件+DB 双存"依旧、
  WAL 膨胀依旧；读取面区间读已全覆盖，复制零收益——宪规 4 砍掉。
- **行内写 head+tail 1MiB 摘要代替 None**：把 preview/区间读之外再造第三份 offset 口径，
  视图读者拿到的字节与日志不再是同一真相。缺口诚实降级 `output_complete` 已足够。
- **信箱满丢 delta**：ADR-0030 同论据仍成立——SSE delta 字节不许丢（缺口必须可补读是
  定序不变量的对偶）；条目削减靠源端合帧，不靠丢弃。
- **泵侧 fd 缓存/批量 writev**：合帧后 ≤50 批/s，open/close 摊销已无感；不为省 50 次/秒
  的系统调用引入泵内文件状态机（ADR-0030 两常量纪律同律）。
- **publisher 签名保留 OutputSnapshot、内部只取标量**：签名即事实——head/tail 只要还在
  类型里就会被 clone。删类型、留水位，护栏落在编译期。

## 后果

- **wire/契约形状零变更**：delta 事件与 `ExecSessionView` 字段增删为 0；
  `exporter check --against docs/api-surface/v4.1` 无新增差异（分类：非 wire 实变，
  不进 refreeze；宪规 15）。
- **值语义变化（诚实登记）**：日志存在时终态 `ExecSessionView.output` 由 `Ref{sha256}`
  变 `None`。全文读取唯一路径 = `exec_session.output` 区间读（0017 决策 5 的前端主路径本就
  如此）。若 qaqh-electron 仍有对 exec `output` ref 的急切水合，该分支变为无害 no-op——
  **electron 仓消费面本轮未复核**，留待前端侧对齐切片确认。
- **内存界（单会话 200MB 洪峰）**：信箱 ≈ 1× 输出字节（原 ≈9×）；publish 不再 clone
  head/tail（原每 chunk ~1MiB 陪跑）；settle O(1)（原 3× 全文）；日志文件 200MB 落盘
  不变（设计使然，磁盘口径见宪规 10）。
- **测试锚（已落）**：`qaqh-session` owner_tests——
  `exec_output_flood_coalesces_losslessly_into_source_runs`（ADR-0030 T1/T2/T4/T5/T6 +
  定序不变量：拼接无损、offset 全局连续、单帧 ≤256KiB、日志=到达拼接、DB 水位）、
  `settle_flushes_prefix_then_keeps_row_free_of_full_text`（T3 + 写入面 + 缺口对账）。

## 落地清单

- [x] 所有者拍板（2026-11-20，挂 P0）
- [x] manager.rs：reader 64KiB 批 + `publish_chunks` 同 run 合帧 + `watermark()`
- [x] capture.rs：`ExecOutputWatermark`（`Arc<str>` preview）
- [x] tool_job.rs / owner.rs：`Arc<str>` 水位贯穿至信箱与泵
- [x] owner.rs：`run_pump_loop` 合帧状态机 + `flush_exec_output`（ADR-0030 决策 1/2 实施）
- [x] owner.rs：settle `metadata` 对账、去 content 复制、`output_complete` 缺口降级
- [x] ADR-0030 状态升格（提议中 → 已采纳并实施，经本 ADR 承接）
- [x] `cargo test -p qaqh-session owner_tests` 14/14 绿；`cargo check` 三面绿
- [ ] CI 全量绿（`qaqh-process-tools` unix 面 manager tests 由 CI 覆盖；本机两处
      `exec::tests` 环境性预存红与本改动无关，见提交说明）
- [x] **daemon 级真实洪峰端到端验证**（`v4.6 P3-1` 补齐，见
      [ADR-0035](0035-exec-flood-model-spill-gate.md) 落地清单末项）：
      `qaqh-daemon::axum_server::exec_flood_smoke_tests` —— 32 MiB 真子进程经真 manager /
      owner 泵 / SQLite / append-only 日志 / 区间读全程：客户端事件面合帧零丢帧、
      日志字节 == 全量、库水位 `next_offset` 追平、`output_content_id IS NULL`、
      `stream_dropped_bytes == 0`、daemon 堆高水位增量有界（6× 洪峰内）。
