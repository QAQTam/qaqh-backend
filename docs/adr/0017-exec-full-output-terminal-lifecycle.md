# ADR-0017 exec 完整输出与只读终端生命周期：持久化 offset 日志 + 区间读

状态: `已采纳（部分实施）` · 决策日期 2026-11-13 · 决策人 仓库所有者（批准时附两条附加决策：移除 exec 终态唤醒注入链，见决策 8；基线 revision 不 bump）· 修订 2026-11-13：决策 2 的 result 形状实施时定夺为单 `text`（见决策 2 修订注与被否选项）；切片 0 后端半 + 切片 1 已落（见落地清单证据）

> 前端（`../qaqh-electron`）已移除 exec 工具卡片渲染，改以只读终端面板管理每个执行会话，
> 期望**完整输出**而非尾窗。本 ADR 在不改动模型侧、不改动事件词表、不引入第二传输的前提下，
> 补齐后端持久化与读取面，并给出前端对齐工作项。引用本 ADR 时注意状态：**提议中 ≠ 已实施**。

## 背景（已核对事实，附 file:line）

### 后端：三条通路，截断只发生在模型侧与持久化面

1. **采集层三分离**（`crates/qaqh-process-tools/src/exec_session/capture.rs`）：
   `append_text`（L140-165）把输出切成 ≤8KiB 块（`MAX_EVENT_BYTES` L9），同时喂三个独立缓冲：
   - `model_pending` 256KiB（`MODEL_PENDING_BYTES` L7）——模型预算消费；
   - `transcript head+tail` 共 1MiB（`TRANSCRIPT_BYTES` L6，head/tail 各 512KiB L12-13）——终态 transcript；
   - `preview` 16KiB 尾窗（`OUTPUT_PREVIEW_BYTES` L8）——视图/进度携带。
   返回给发布路径的块**无任何截断**。
2. **实时发布全量**：`manager.rs` `spawn_reader`→`append_output`（L994-1003）→`publish_chunks`
   （L1017-1030）逐块调 `publisher.output(全量 text + offset)`；`owner.rs` `ExecSessionOutput`
   （L1287-1330）只把 next_offset/preview/计数器写库（`update_exec_session_progress`），
   **delta 文本不落库**，随后原样 emit `ExecSessionOutputDelta`。
3. **模型侧独立预算**：`tool_job.rs` L123/L145 `max_output_bytes = max_output_tokens × 4`
   （默认 10k tokens → 40KB）；`manager.rs` `take_model_pending`（capture.rs L94-121）溢出丢弃计
   `omitted_bytes`；模型出口 `strip_ansi`（manager.rs L834，注释 L829-833 明确前端通路保留原样 ANSI）。
4. **终态 transcript 1MiB 封顶**：`finalize_session`（manager.rs L1274-1291）拼
   head + `[N bytes omitted from captured transcript]` + tail；`finish_exec_session`
   （`store.rs` L1327-1389）把 transcript 以 sha256 content_id 存入 content store
   （`put_session_content` L1598），`ExecSessionView.output` 给 Ref。
5. **重连只剩尾窗**：bootstrap 读面 `exec_sessions_for_bootstrap`（store.rs L1392，全部非终态 +
   最近 32 个终态摘要）SELECT 不含输出正文；运行中会话只有 16KiB preview。SSE 广播深度 1024
   （owner.rs L35），回放尾 2048 帧 / 4MiB（L40-41）；超出 → `ringing.resync_required`
   （`axum_impl/v2.rs` L967-981）→ 重 bootstrap → **中段输出永久丢失，且无按 offset 补读的接口**。
6. **`stream_dropped_bytes` 是死计数器**：exec_session 路径只初始化为 0（manager.rs L385），
   全仓无递增点；前端自己在 reducer 里按 offset 缺口累计（见下）。
7. **生命周期**：内存保留终态 10 分钟（`TERMINAL_RETENTION` manager.rs L23，`reap_expired`
   L1537-1547 只清内存 map）；配额每会话 8 / 全局 32（L21-22）；DB 行不随内存回收删除；
   重启恢复把运行中会话置 finished/interrupted、`output_complete=0`（store.rs L1222-1229），
   内存 capture 全丢，只剩 preview。
8. **控制面已存在**：命令 `ExecSessionInterrupt`/`ExecSessionTerminate`
   （`axum_impl/commands.rs` L439-483）；service 方法 `exec_session.list`
   （`qaqh-runtime/src/ringing/service_methods.rs` L569-579）；内容读取
   `GET /ringing/v2/content/{content_id}`（路由表 `qaqh-ringing/src/v2/mod.rs` L48）。
   **无 exec 输出区间读取面，无 exec 专属 HTTP 路由。**
9. **单流doctrine**：`events.rs` 头注（L1-5）"一条流、一个游标（epoch:seq）……不创建第二条
   进程 SSE（spec §6.1）"。路由只能登记在两张表（宪规 16）：`RINGING_V2_HTTP_ROUTES`
   （v2/mod.rs L37-54）与 `CONTROL_PLANE_ROUTES`；`POST /ringing/v2/service/{method}` 已在表内（L49）。
10. **会话文件布局**：`<data_dir>/sessions/<session_id>/`（`qaqh-session/src/manager.rs` L34），
    `SessionManager::delete`（L25-49）整目录删除；业务库在 `<data_dir>/qaqh.db`
    （`qaqh-runtime/src/service.rs` L84），`Db::path()`（db/mod.rs L123）可派生 data_dir。
    owner pump 单线程串行（owner.rs L1145-1151），持有 `session_id` 与 `db`。

### 前端：与后端"对齐地不完整"（qaqh-electron/webui，已读源码）

11. **reducer 只留 16KiB 尾窗**：`session/reducer.ts` L786-796，`exec_session_output_delta`
    追加进 `output_preview`（`appendUtf8Tail(..., 16*1024)`），offset 去重，缺口累计
    `stream_dropped_bytes`，**其余全丢**。
12. **终端渲染**：`tools/ReadOnlyTerminal.tsx` xterm 80×24（L8-9，与后端 PTY cols:80/rows:24
    一致）、scrollback 1000（L197）、`disableStdin`（L193）、`convertEol=!tty`；按 offset 增量写
    （`stream {startOffset,endOffset,data,offsetEncoding:"utf8"}`），落后窗口即 reset+rehydrate；
    终态快照（live→false）整体替换为全文。
13. **正文选择**：`tools/presentation.ts` `execSessionContent`（L169-179）运行中读 preview，
    终态优先已水合 inline 权威输出；truncated 横幅判据 = `capture_omitted_bytes>0` /
    `preview_start_offset>0` / `stream_dropped_bytes>0`。
14. **面板**：`exec/ExecSessionDrawer.tsx` + `panel-store.ts` 瞬态开/关+选中，数据读会话 store
    `execSessions`；**单台 xterm keyed by execId，切换即重建**（L280-281）；中断/终止走命令；
    "仅显示输出尾部"横幅（L267-268）。关闭/切换后无持久 scrollback。
15. **终态水合**：`reducer.ts` `pendingContentRefs`（L45-69）把 `execSessions[].output` 的 ref
    列入**急切水合**（`readContentText` → IPC `content_read_text`）；exec_command 的工具输出 ref
    故意不水合（L62-64，消息区只留命令+exit 徽章）。
16. **service 调用与防偏移闸**：`transport.query` → IPC `service_query`
    （`lib/transport/electron.ts` L106-109）→ `POST /ringing/v2/service/{method}`；
    ServiceMap 手写方法表与 native addon `serviceMethods()` 白名单的一致性由 **Electron 侧
    packaging 测试强制**（`api/contract.ts` L6-8 注释）。
17. **前端契约测试**：`tests/exec-terminal-stream.test.ts` 已锁定"offset 增量 + 重复帧去重 +
    窗口滚动增量 + 终态 ref 换全文"。应用代码**零 WebSocket**（仅 CDP 测试脚本用）。

### 契约治理现状

18. ADR-0004 先例：新增导出物（`PermissionRequestDetails` 599→602）即判 `contract-refreeze`，
    只落 main，`docs/api-surface/v3.0` 一字节不动。ADR-0010 是 refreeze 账本，§待决 2 已决
    "v3.1 先不切"（所有者 2026-11-10）；远端当前仅 `main`（`git ls-remote --heads origin`
    实测 2026-11-13），冻结锁即 main 上的 `docs/api-surface/v3.0`。

## 决策

1. **每 exec 会话一份 append-only 输出日志**，落 `<data_dir>/sessions/<session_id>/exec/<exec_session_id>.log`。
   日志内容 = capture 已分配 offset 的解码文本按序拼接；offset 即字节 offset（与 delta 同口径）。
   写入点在 **owner pump 的 `ExecSessionOutput` 处理处**：先追加日志 → 再 `update_exec_session_progress`
   → 再 emit delta。定序不变量：**客户端见过 offset X 的 delta ⇒ 日志必可以读到 X**。
   不在 capture 锁内做磁盘 IO（reader 线程保持轻）。日志随 `SessionManager::delete` 整目录清理，
   **不随 manager 10 分钟内存回收消失**。
2. **新增 service 方法 `exec_session.output`（区间读）**，注册进 `service_methods` 表
   （`MethodKind::Read`、`requires_session: true`），类型化 DTO 导出：
   - params：`{session_id, exec_session_id, from_offset, max_bytes?}`（max_bytes 默认 256KiB、上限 1MiB，客户端循环拉取）；
   - result：`{exec_session_id, next_offset, output_complete, text}`，UTF-8 字符边界切片。
     > **修订（2026-11-13，实施时定夺）**：原形状 `chunks:[{source,…}]` 依赖日志携带
     > source 信息，而决策 1 的日志只存解码文本按序拼接（无 source 维度）；为读面升级
     > 日志格式只会把 settle 的全文读取复杂化，wire 面换不来新能力，故定夺为单 `text`
     > 形状（被否方案见被否选项）。`next_offset` = 本次返回文本之后的字节游标（客户端
     > 下一次 `from_offset`）；`text` 为空且游标未前进 ⇒ 已追平水位或日志缺席（旧会话
     > 如实返回空）。前端 `ReadOnlyTerminal` 消费形状（text + stream offset）不受影响。
   - 读取上界钳制到 DB 行的 `next_offset`（pump 先写文件后写库 ⇒ 库水位 ≤ 文件长度，永不读到半块）。
   - 运行中/终态都可读。**不新增 HTTP 路由**（走既有 `service/{method}`，宪规 16）。
3. **终态权威输出 = 完整日志**入 content store（砍掉 `finalize_session` 的 1MiB head+tail 封顶）；
   head+tail 只留给模型侧 preview 语义。`ExecSessionView.output` ref 指全量，前端现有水合路径可用。
   `capture_omitted_bytes` 对前端归零（capture 对前端通路不再丢字节）。
4. **`stream_dropped_bytes` 保留 wire 字段**；capture 从不丢字节，0 是诚实值。缺口从"永久丢失"
   变"可补读"：前端按 offset 连续性触发区间补读，不再把它当永久损失累计。
5. **前端对齐工作项**（防偏移清单，落 `../qaqh-electron`）：
   - ServiceMap + native addon `serviceMethods()` 白名单加 `exec_session.output`（packaging 测试即闸）；
   - exec 输出 ref 改**懒水合**（抽屉打开才拉），从 `pendingContentRefs` 的急切集合移除——
     全量 transcript 无上界后，急切水合会把所有终态会话全文灌进内存；
   - 抽屉 open/select 拉 `[0, next_offset)` 喂 xterm；live delta 续写；发现
     `start_offset > next_offset` 缺口 → 区间补读拼接；`resync_required` → 重 bootstrap + 重拉当前会话区间；
   - xterm scrollback 1000 → 加大（如 10k）；"仅显示输出尾部"横幅只在 `output_complete=false`
     或真实捕获缺失时出现；
   - `exec-terminal-stream.test.ts` 增补：缺口补读、fetch-on-open 两个用例。
6. **不引入 WebSocket**；SSE 单流单游标、回放尾、resync 语义一律不动；事件词表零变更。
7. **模型侧零改动**：`take_model_pending` 预算、`max_output_tokens`、`strip_ansi` 原样。
8. **移除 exec 终态唤醒注入链**（所有者附加决策，已实施并编译验证）：原机制在后台 exec
   终态且模型未读时，由 registry `route_exec_exit` 向 owner 会话注入 `<qaqh_exec_session_exit>`
   system 输入（`as_system:true`、`TriggerTurn`，含 output tail）触发回合——把输出重复塞给
   模型，与 write_stdin 的读取语义重复。整链移除：manager 的 `ExecExitNotice`/`ExecExitPort`/
   `set_exit_port`/`terminal_unread`、支撑字段 `terminal_delivered`/`model_waiters`/
   `notice_suppressed`（含 `stop_and_wait` 的 suppress 参数与 `wait_for_call` 等待者计数合并）、
   runtime 装配点与 `route_exec_exit`、`preview_tail`、`windows_tests` 模块及其测试。
   本机构为内核内部（非 wire 面），无契约影响。**行为代价如实登记**：后台 exec 完成不再
   唤醒空闲模型——模型在下次交互/poll 时经 write_stdin 读到终态；前端面板仍经 Ringing
   事件实时看到终态（finish 面），只读终端不受影响。

## 被否选项

- **WebSocket 第二传输**：否。RingingV2 是最高准则（宪规 9），事件面明文"一条流、一个游标……
  不创建第二条进程 SSE（spec §6.1）"（events.rs L1-5）。WS 意味着第二套游标/去重/重连语义，
  是平行协议，属点名批判的"拧巴设计"。且不对症：缺口在持久化与重连读取，不在传输——
  SSE delta 在发射端本就全量无损。前端零 WS 基建，引 WS 才是制造协议偏移。只读终端不需要
  双向通道，控制面（interrupt/terminate）已有命令承载。
- **delta 逐条落库（SQLite 行）**：否。8KiB 行灌爆业务库，写放大与 WAL 压力远超 append-only
  文件；区间读变成 SQL 范围查询，offset 语义还要再维护一遍。文件天然就是 offset 地址空间。
- **单纯加大 preview 窗口（16KiB→N MiB）**：否。仍是窗口：重连/resync 缺口依旧，bootstrap
  快照随窗口膨胀，每个会话视图都背全文。治标不治本，且把成本摊给所有列表行。
- **为兼容旧前端写转换层（新旧两套输出语义并存）**：否（宪规 4/9）。事件词表不变、新增读取面
  是纯增量，旧前端不消费新方法即可继续跑，不需要转换层。
- **`exec_session.output` 的 result 带 `chunks/source`（决策 2 原形状）**：否（2026-11-13
  实施时定夺）。决策 1 的日志是纯解码文本按序拼接，没有 source 维度；带 chunks 就得为
  读面升级日志格式，settle 全文读取跟着复杂化，而前端消费形状（text + offset）并不需要
  source。wire 面更小、与决策 1 自洽的形状是单 `text`。若未来真要按 source 分段（如
  stdout/stderr 分流），届时日志格式与读面一起重开 ADR，不做兼容转换层（宪规 4）。
- **现在就改 file-backed content store**：否（本轮）。终态全量入 SQLite content 会与大日志
  重复存储，但改 content 存储形态触及内容 wire 面，是更大的独立改动。本轮先复用既有
  content 面（零前端形状变更），DB 膨胀真成问题再另开 ADR。

## 后果

- **wire 实变**：新增 service 方法 + 2 个导出 DTO（params/result，形状修订见决策 2）= 产物新增 ⇒
  `[contract-refreeze]`（ADR-0004 先例），只落 main/新线，禁入冻结面；记入 ADR-0010 账本，
  切线时一次吸收。判据以 `scripts/check-surface-change-policy.py` 实算为准，不由本 ADR 声明。
  `SESSION_CONTRACT_REVISION` 是否 bump 沿 ADR-0010 §待决 1 统一裁决，本条不偷跑。
- **存储重复**：终态日志同时存在于文件与 content blob。配额 8/32、典型输出 KB–MB 级可接受；
  超大输出（百 MB 级构建日志）会放大 DB，届时另议 file-backed content（见被否选项）。
- **日志写失败 = 响亮降级**：append 失败按硬错误处理（log + 完整性标记
  `output_complete=false`/缺口如实暴露），不静默吞掉（宪规 5）。客户端靠 offset 连续性发现缺口，
  未持久化的字节不可补读——这是诚实的"不完整"，不是假装完整。
- **前端急切水合必须先改懒**：决策 3（无上界 transcript）与决策 5（懒水合）有依赖顺序，
  否则终态全文 ×32 会话灌内存。切片排序已体现。
- **重启语义更诚实**：日志在重启后仍在，面板可读已捕获前缀；`output_complete=false` 标注未收尾，
  不再"只剩 preview"。
- **闸门冲击**：main 上 `check --against docs/api-surface/v3.0` 的红继续是设计状态
  （ADR-0010 §后果）；本轮不动金标准（宪规 16）。无新增 CI 闸门；前端 packaging 测试
  是现成的方法表一致性闸。

## 落地清单

- [x] 仓库所有者拍板本 ADR（2026-11-13：批准，附「移除唤醒注入链（决策 8，已实施）」
      「revision 不 bump」两条附加决策）
- [x] **切片 0（无 wire 变更）· 后端半**：终态 transcript 去 1MiB 封顶——settle 读日志全文
      入 content store（2026-11-13 落，`owner.rs` ExecSessionSettle arm；日志缺席回退旧封顶
      transcript）。验证：`cargo test -p qaqh-process-tools -p qaqh-session` 绿（76+29 passed）。
- [ ] **切片 0 · 前端半**：exec 输出 ref 懒水合——移到 `../qaqh-electron` 随切片 2 一并落
      （后端无上界 transcript 已就位；懒水合落地前前端不得急切水合全量 ref，否则终态全文
      ×32 会话灌内存，见后果）。
- [x] **切片 1（wire 实变，refreeze）**：append-only 日志（owner pump 写入点 + 定序不变量）；
      `exec_session.output` 方法 + DTO 导出 + service_methods 注册（`TYPED_METHODS` +
      `METHODS` 两表）；ADR-0010 账本登记（含决策 2 形状修订）。已落（2026-11-13）。
      验证：`cargo test -p qaqh-session -p qaqh-runtime` 全绿（63+30 passed；区间读
      水位钳制/字符边界/丢字节如实语义有专项测试）；导出器实测产物 **608 → 614**（仅 +6，
      全为本条：2 类型 × ts/cs + 2 fixtures；基线 = 当日工作区 golden），本条未触碰
      `docs/api-surface/v3.0`（该目录工作区另有并发在途改写，非本条所为，留痕见
      ADR-0010 账本），`check --against` 在 main 上红为设计状态（差额与 hash 见
      ADR-0010 账本实测记录）。
- [ ] **切片 2（前端消费升级）**：ServiceMap/白名单 + fetch-on-open + 缺口补读 + scrollback +
      横幅语义 + 测试增补。验证：Electron packaging 测试 + vitest。
- [ ] **切片 3（生命周期打磨）**：面板历史深度（bootstrap 最近 32 终态上限是否调）、重启后会话
      可读性 UX、日志磁盘配额（如需）。逐项另议，不默认全做。
- [ ] 动共享文件前 `git fetch` 比对并发写入者（宪规 19）；本机 8 核/7.9GB，不做全量 release
      构建（宪规 7/10），`--release` 全量交 CI。
