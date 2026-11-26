# ADR-0035 exec 洪峰模型消费面泄洪闸：超限转引用 + 不恢复 + 阈值硬编码

状态: `已采纳并实施`（2026-11-27 `v4.6 P3-1` 落地）· 2026-11-22 所有者拍板（四项裁定：①泄洪不恢复 ②阈值硬编码，不暴露给用户 ③契约面不必为规避变更而妥协 ④支持「限幅→泄洪→熔断」分级）· 起草 QAQ agent · 承接 [ADR-0034](0034-exec-flood-memory-surface.md)（daemon 存活面已闭环）与 [ADR-0030](0030-exec-output-delta-pump-coalescing.md) 决策 4 非目标（折叠/熔断）的分级化重审

> 一句话：ADR-0034 落地后洪峰已不再打爆 daemon，但合帧只是**限幅**——大输出仍会以合帧形态全部流经**模型上下文**（200MB 级输出 ≈ 数千帧）。本 ADR 在模型消费面（session_loop/tool_job 链）加「泄洪闸」：单 exec 会话模型面输出累计超硬编码阈值即停止直灌 delta，改投一条合成通告帧（异常流量说明 + 权威日志路径 + tail 预览），此后该会话**不恢复**直灌——模型稍后自行 `write_stdin` 轮询或按通告内路径直接读取权威日志。客户端 SSE 面不设闸，继续流给人看。

## 背景

1. ADR-0034 把传输面放大钳住（reader 64KiB 批 + 同 run 合帧 + 水位 `Arc<str>` 共享），泵侧 ADR-0030 合帧 20ms/256KiB——daemon 内存/写放大面已闭环。但限幅保护的是 **daemon 存活**，不是**模型上下文**：限幅后的合帧仍会持续灌入模型窗口，长输出（构建/日志转储/死循环打印）照样把上下文淹掉。
2. 全文权威源已是 append-only 日志文件本身（ADR-0034）——**泄洪区已存在**（日志文件就是泄洪道），缺的只是闸门与告示牌，不需要新增存储路径。
3. 折叠（不可逆丢信息）与熔断（杀进程）对「长构建、持续打日志的合法高产进程」都过于粗暴：进程不该被杀，信息不该被丢。泄洪 = **引用转发**：进程继续跑、信息不丢、上下文不淹——补上限幅与熔断之间的中间档。

## 决策（2026-11-22 所有者拍板）

1. **闸门装在模型消费面**（session_loop/tool_job 消费链），不在 capture/pump 面；客户端 SSE 面不设闸。
2. **触发即不恢复**：单 exec 会话一旦泄洪，本会话余下输出不再直灌模型上下文；模型经通告得知后，稍后自行 `write_stdin` 轮询，或按通告内路径直接读取后端定向的权威日志流（`read` 分段/`grep` 按需取，tail 优先）。
3. **阈值硬编码**：常量进代码，不经 `SessionSettingsPatch` 暴露——用户不需要为这类专业知识负担。
4. **通告帧为合成帧**：内容含「异常流量说明 + 权威日志路径 + tail 预览」；词表归属与三戳是否 bump 随实现期契约核对定夺，契约面不为规避变更而妥协（所有者明示）。
5. **分级响应**：限幅（ADR-0030/0034，已实施）→ 泄洪（本 ADR）→ 熔断（仍开放，需独立裁定；作为最后手段保留，不因泄洪落地而废弃）。

## 非目标

- 不做重复错误行折叠（ADR-0030 决策 4 维持非目标）
- 不在本 ADR 落地熔断
- 不改客户端 SSE 行为
- 不提供用户级阈值配置

## 落地清单

- [x] 阈值常量定值与落位：`qaqh_agent_runtime::spill_gate::MODEL_FACE_BUDGET_BYTES = 256 KiB`
      （与 capture 侧 `MODEL_PENDING_BYTES` 同界＝"一整份未消费模型待读缓冲"即单会话预算；
      硬编码在模型消费面 crate，不经 `SessionSettingsPatch`，无任何配置入口）
- [x] 消费链泄洪闸：`spill_gate::admit_model_face` 在 `tool_job` 的 exec 宿主**两个出口**
      （`exec_command` / `write_stdin`）各一行接线；`session_loop.rs` **零改动**（净行数 `+0`）
- [x] 通告帧落地 + 契约核对：通告是模型可见**文本**，不动任何 DTO/字段；
      `cargo run -p qaqh-contract-export -- check --against docs/api-surface/v4.2` `EXIT=0`（零漂移，不 bump 三戳）
- [x] 定向测试：`spill_gate` 8 条单测（触发/通告/不恢复/水位/空帧/终态释放/UTF-8 尾部）
      + 1 条**真实 2 MiB 洪峰进程**端到端反证；daemon 侧客户端面不设闸由冒烟守卫
- [x] daemon 级真实洪峰冒烟：`qaqh-daemon::axum_server::exec_flood_smoke_tests`
      （32 MiB 真子进程 → 真 manager → 真 owner 泵 → 真 SQLite/日志 → 真区间读）

## 实施记录（2026-11-27 · `v4.6 P3-1`）

1. **闸门位置**：`crates/qaqh-agent-runtime/src/spill_gate.rs`（宪法 #8 独立子模块）。
   调用点只有 `tool_job.rs` 的两行 `admit_model_face(...)`；`session_loop.rs` 未改。
   直接落在 `ExecSessionHost` 边界而不是更上游，是因为 exec 产出的**唯一**模型可见载体
   就是那一处返回的 `output` 文本——装在边界上，任何新增 exec 出口都不可能绕过闸门。
2. **权威日志路径单源**：新增 `crates/qaqh-session/src/exec_log.rs`，owner 泵写日志、
   runtime `exec_session.output` 区间读、本 ADR 的通告三处共用一条规则
   （此前是 owner 泵一处写死）。`ToolLifecycle` 新增 `exec_output_log_path()` 供闸门取址；
   `owner.rs` 净增 6 行（≤10 行薄接线预算内，且原路径函数体被同源化净删 7 行）。
3. **不恢复与记账诚实**：开闸后该 exec 会话余下每一帧都被替换成水位行；
   被扣字节逐帧累加到 `omitted_bytes` 且置 `output_truncated=true`——模型被扣了多少说得清。
   终态帧过闸后释放账本（exec 会话终结后不可能再有输出来"恢复直灌"）。
4. **客户端面零影响**：闸门只改写 `ExecSessionResult.output`（模型面），
   发布者（`OwnerExecPublisher`）、SSE 事件泵、权威日志一行未动。
5. **反证（红→绿）**：
   - 合成帧：40 帧 × 32 KiB = 1.25 MiB 直灌（5× 预算）→ 设闸后只出 1 条通告 + 31 条水位行，
     模型面总字节 < 预算 + 32 KiB；
   - 真实进程：`sh` 分 32 批产 2 MiB，模型面原始输出 1 MiB 级多帧 → 设闸后 1 条通告，
     `gated_bytes × 4 < ungated_bytes`；同时断言发布者收到的字节 == 2 MiB（客户端面与日志面零削减）、
     进程 `Completed`（泄洪不杀进程）、权威日志 `== 2 MiB`。
6. **daemon 级冒烟实测**（本机 Linux，`--nocapture`）：
   `[flood-smoke] 洪峰 33554432 B：源端 513 块 → 客户端 128 帧 / 33554432 B；
   权威日志 33554432 B；堆高水位增长 108404736 B`——客户端面合帧但零丢帧、
   日志与库水位 `next_offset` 全等、`output_content_id IS NULL`（settle 未复制全文）、
   区间读从 0 读回全量。
7. **未验范围（如实登记）**：
   - 堆高水位断言是**一侧保守**探测（`VmHWM` 只增不减 ⇒ 并行测试只会让它更松，不会假红），
     阈值取 6× 洪峰；它能抓住 ADR-0034 的**信箱放大类**回归（9× 会到 288 MiB），
     抓不住"每 chunk 陪跑 head/tail 500×"那类**瞬时**峰值（归 ADR-0034 的类型级护栏管）。
   - 仅在本机 Linux 实测；Windows / macOS 分支本轮未跑（`#[cfg(unix)]` 用例在非 unix 上不编译）。
   - 泄洪后 `write_stdin` 的水位行为由单测覆盖；未接真实模型会话做端到端录制（本切片不含 LLM 调用）。

## 与既有 ADR 的关系

- 承接 ADR-0034：权威日志文件即泄洪区；其未闭环的「daemon 级真实洪峰冒烟」并入本 ADR 落地清单末项
- 重审 ADR-0030 决策 4：折叠维持非目标；熔断降级为分级响应第三档（保留待裁定，不废弃）
- 熔断（杀进程）**仍未实施**：本 ADR 只落中间档，进程被杀与否的裁定权继续留给独立裁定
