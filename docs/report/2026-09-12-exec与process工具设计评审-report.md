# exec 与 process 工具设计评审（2026-09-12）

## 0. 元信息

| 项 | 值 |
|---|---|
| 报告日期 | 2026-09-12 |
| 分析对象 | `crates/qaqh-workspace`（`src/exec/*`、`src/process_registry.rs`、`src/process_inspect.rs`）+ 联动面 `crates/qaqh-runtime`（timeline / ringing / agent 工具线程）、`crates/qaqh-domain`、`crates/qaqh-types`、`crates/qaqh-subagent` |
| commit | `e61efe0`（分析时工作区；`exec/pipe.rs`、`exec/direct.rs`、`exec/tests.rs` 有本会话未提交的截断修复） |
| 触发方式 | 用户提问：「是我们的 exec 设计有问题吗？如果你来重新设计（包括 exec 和 process 工具的联动），你会怎么重构？如果现有还行，你会怎么优化？」 |
| 执行者 | 本会话（读码 + 单点实测） |
| 结论 | **管道读取层（刚修的）方向是对的，不该推翻；但 exec↔process 的四个结构面（输出通道语义、进程身份与授权、生命周期收敛、事件流合并）是真设计缺陷，其中 D-1 与 D-2 已在代码注释里被"文档化接受"，本报告主张改判。** |
| 续报 | `docs/report/2026-09-13-codex-exec设计对照与修订-report.md`（Codex 源码对照；**5 处结论/处方修订见本文附录 C**） |

> 行号以 `e61efe0` 为准。本报告是设计评审，**主体证据是 E2（代码实证）**；仅一处 E1 实测（`taskkill` 持锁时长）；所有推断集中在 §10。

> ⚠ **修订锚注（2026-09-13 追加）**：本报告正文按 `TEMPLATE.md` §7 保持"当时快照"不变。经 Codex 源码横向对照，其中 **1 处定性、4 处处方**需要修订（R-1..R-5，逐条见**附录 C**）；受影响的小节（D-1 / D-5 / D-6 / D-7）、§6.2 改造表与 §7 路线已就地挂锚，**但未改动原有文字**。阅读时请以附录 C 为准。

---

## 1. 结论摘要

| ID | 严重度 | 状态 | 类型 | 位置 | 影响（一句话） |
|---|---|---|---|---|---|
| D-1 | P1 | open（已被注释"接受"，主张改判）；**修订见附录 C R-1/R-2** | 设计缺陷 / 输出通道语义 | `exec/direct.rs:92,249,269`、`exec/handler.rs:218-225,300-307`、`qaqh-types/src/tool_result.rs:15,122-128`、`qaqh-runtime/src/ringing/content_store.rs:23` | 四层限额（读线程 5 MiB → 截断 10K token → 模型 24K 字符 → 外置 10 MiB）互不知情，**外置路径对 exec 结构性不可达**；命令输出的中段被永久丢弃，模型没有任何取回手段 |
| D-2 | P1 | open | 安全边界 | `process_registry.rs:119,187,272,294,364,428`（全 API 无会话参数）、`process_inspect.rs:13-38`、`permission.rs:426-430` | 进程注册表是**进程级全局表且无归属**：任意会话可 `check` 任意 `id`；`check/wait` 被归类为 Read 从而在 Level≥2 免批准；`kill` 只校验 `id` 存在，不校验它属于谁 |
| D-3 | P2 | open | 并发/锁粒度 | `process_registry.rs:428-484`（`kill`）、`112-114`（`with`）、`325-362`（`append_output`/`append_stderr`） | `kill` 在**持有全局注册表 Mutex** 期间同步 spawn `taskkill` 并 `child.wait()`（E1 实测 108.7 ms / 9 子进程树）；窗口内所有会话的读线程 `append_output` 与所有 exec 的 `try_wait` 轮询（50 ms 周期）一起停顿 |
| D-4 | P1 | open | 生命周期收敛 | `process_registry.rs:86-88,428-471`（`os_pid` 快照 + `taskkill /pid`）、`direct.rs:47`（仅 `CREATE_NO_WINDOW`） | 进程树收敛依赖 **pid 快照**：句柄被 `try_wait` 回收后按 `os_pid` 执行 `taskkill /pid`/`killpg`，pid/pgid 已被系统复用时**会误杀无关进程树**；Windows 侧无 JobObject（同仓库 `qaqh-lsp`/`qaqh-mcp` 已在用） |
| D-5 | P2 | open；**修订见附录 C R-5** | 契约缺位 | `process_inspect.rs:13-38,160-186`、`process_registry.rs:92,161,216-256`、`exec/direct.rs:193-229`、`exec/handler.rs:226-234` | 三处契约失真：① `process.write` 是**死功能**（`pty_writer` 全仓只被写成 `None`，stdin 恒为 `null`），schema 仍在承诺；② 无 `read`/`list`，后台进程的 `captured_full`（5 MiB）**模型永远读不到**；③ `timeout_secs` 到点**不超时**（只移交后台）却在结果里报 `timed_out: true` |
| D-6 | P1 | open；**修订见附录 C R-3** | 事件流 / 前端成本 | `exec/pipe.rs:126-138`（每 chunk 一次）、`qaqh-runtime/src/agent/engine_tool.rs:41-55`、`qaqh-runtime/src/timeline.rs:457,751-753,762`、`ringing/hub.rs:669-682` | 活跃期每 8–64 KiB 输出产生 **1 个 timeline 事件 + 1 次 `tool.progress` 无界 `push_str` + 1 次有损 `broadcast(1024)` 投递**；单条命令最多向时间线注入 10 MiB 进度文本，并占用该 seed 回放尾的条目与字节预算，挤掉重连补帧窗口。与 `docs/archive/2026-09/report/2026-09-12-多会话高频输出热路径串行化与切会话401-report.md` 的写放大族同源 |
| D-7 | P2 | open；**修订见附录 C R-4** | 生命周期 / 内存 | `process_registry.rs:38-45,119-140`（驱逐只在 `register` 内做，终态保留 600 s）、`direct.rs:236` | 条目可达 **10 MiB/条**（两流 × 5 MiB）且终态后再保留 10 分钟，驱逐只在"下一次 register"时发生；daemon/工具宿主退出**不清理任何子进程**（全仓无 `kill_all`/`Drop` 收敛），孤儿既跑又不可达 |
| D-8 | P2 | open | 耦合 | `qaqh-subagent/src/lib.rs:185-240,299-320,590-605`、`serve.rs:311-374` | 子代理与 exec 子进程共用同一张全局表：`process kill` 对子代理是**协作式**的（collect 线程每轮 `killed()` 一次，`recv_timeout` 300 ms），Remote 模式下 `killed()` **每轮发一次 HTTP POST**（≈3.3 req/s/子代理），而 `kill` 的返回值在子代理未真正停下前就已声称 killed |

**保留项（本报告认为这些是对的设计，重构时不要推翻）**

| ID | 内容 | 依据 |
|---|---|---|
| V-1 | **读线程与封口解耦**：`captured_full` 作权威 + seal 侧有界 join，任何路径不等管道 EOF | `exec/direct.rs:239-258`、`exec/pipe.rs:258-270`；这是 2026-09-02 冻结事故（`ddcd606`）的正确解，本次截断修复正是在此结构内完成的 |
| V-2 | **就绪抽象与平台胶水分层**：`Readiness{Ready(n)/Empty/Closed}` + per-OS 探测闭包 | `exec/pipe.rs:18-32`、`direct.rs:107-164`；语义分层清晰，问题是消费方用错了（本次已修），不是抽象错 |
| V-3 | **进度通道有界且丢弃**：绝不为慢 UI 阻塞读线程 | `lib.rs:516-547`（`try_send` + `dropped_bytes`） |
| V-4 | **截断保留头尾**而非只留尾 | `exec/truncate.rs:52-87`；对"开头有命令回显、结尾有结论"的输出形态正确 |
| V-5 | **`Shell` 枚举收敛 pwsh 特判**（`-EncodedCommand`/`-CommandWithArgs` 降级链）与 `rg -rn` 习惯陷阱防御 | `exec/shell.rs:139-206`、`exec/handler.rs:21-56` |

---

## 2. 分析方法与证据链

按"自下而上读链路，再自上而下找契约"的顺序收敛：

1. **读 exec 全链路**（`exec/{mod,handler,direct,pipe,shell,truncate,register}.rs`）→ 得到 spawn/读取/封口/截断四段的事实。**E2**
2. **读 process 面**（`process_registry.rs`、`process_inspect.rs`、`serve.rs` 的 `/subagent` 与内联分支）→ 得到注册表 API 面（无会话参数）、锁结构、`pty_writer` 死字段、`get_info` 只吐 tail 三个事实。**E2**
3. **顺输出向下游追**：`ExecProgressEvent` → `engine_tool::drain_progress_external/emit_progress_tail` → `TimelineIntent::ToolProgress` → `TimelineAppender::append_tool_progress` → `SeedTimeline.journal`/`TimelineTool.progress` → `hub::fanout` → `broadcast(1024)`。**E2**（`engine_tool.rs:786-802` 的文档声称"每 (tool_call_id, stream) 只保留最后 4 KB 尾部…前端替换而非拼接"，但 `emit_progress_tail` 直接转发原 chunk，全链路无任何 tail 化实现——文档是设计意图，不是现状）。
4. **顺截断向上游追限额**：读线程 `max_bytes`（`direct.rs:92` = `FULL_CAPTURE_BYTE_CAP` 5 MiB）→ `token_truncate`（`handler.rs:218-225` 默认 10K token）→ `ToolResult::ok`（`tool_result.rs:15` 24,000 字符）→ `externalize_large_content`（`registry.rs:752` 门槛 10 MiB，`content_store.rs:23`）。四者各自有据，**合起来的外置路径是否可达需要算数**——算出对 exec 不可达（见 D-1 §4.2）。**E2 + 算术**
5. **对照同仓库已有的正确做法**：`qaqh-lsp/src/adapter.rs:68`、`qaqh-mcp/src/adapter.rs:125` 使用 `process_wrap::tokio::JobObject` 做整树containment；`qaqh-workspace` 未依赖 `process_wrap`。**E2**
6. **单点实测**：`taskkill /T /F` 对 9 子进程树的耗时（用于给 D-3 的锁持有窗口定量）。**E1**
7. **读并发上限与落点**：`agent/turn_lap/admit.rs:83` `MAX_PARALLEL_TOOL_WORKERS = 4`；`backend.rs:44-48` 本地后端同步就地执行；`actor.rs:97-108` 仅 WSL 模式装 HTTP 后端（故 native 模式下 exec 在 daemon 进程内的工具线程上跑，注册表就是 daemon 的静态）。**E2**

---

## 3. 现状架构（阅读发现的前置）

```
会话 actor 线程（每会话 1 条，qaqh-runtime）
  └─ 回合工具批 → ≤4 条工具 worker 线程（admit.rs:83）
       └─ exec handler（exec/handler.rs:101）——同步 fn 指针，工具接口本身不可 async
            └─ direct_exec（同步阻塞直到子进程退出或移交）
                 ├─ std::process::Command：stdin=null / stdout=piped / stderr=piped
                 │    Windows: +CREATE_NO_WINDOW（无 JobObject）
                 │    Unix   : +process_group(0)
                 ├─ ProcessRegistry::register → u32 全局 id（表是 static，无会话归属）
                 ├─ 每流 1 条读线程：poll 循环（PeekNamedPipe / O_NONBLOCK + 50ms tick）
                 │    ├─ 解码（UTF-8 / OEM 兜底）→ 进度通道（容量 256，try_send 有损）
                 │    └─ append_output/append_stderr → tail 视图(5K/3K 字符) + FullCapture(5 MiB)
                 └─ 主循环 try_wait（50 ms）→ 退出/取消/超时移交后台
       └─ seal：有界 join(500ms) → captured_full（权威）→ stderr+'\n'+stdout → strip_ansi
                 → token_truncate(max_output_tokens, 默认 10K) → ToolResult（标准 24K 字符）
       └─ process 工具（process_inspect.rs）：check/wait/write/kill，只经 get_info（tail 视图）
```

**四条关键不变量（评审基准）**

| # | 不变量 | 现状 |
|---|---|---|
| I-1 | 输出的**字节真相**只应有一处权威 | ❌ 三处各说各话：`captured_full`（前台模型结果）、`output/stderr` tail（`process check`）、`tool.progress`（前端）。三者上限与裁剪口径都不同 |
| I-2 | 进程的**所有权**应绑定会话 | ❌ 注册表无 owner 字段；工具上下文 `ToolCallCtx` 也没有会话字段（但 `crate::runtime::context()` 在工具线程上已携带精确会话，见 §6.2） |
| I-3 | 进程的**生命周期**应由创建者收敛 | ❌ 无 JobObject、无退出清理、无 orphan 策略；终止靠 pid 快照 |
| I-4 | 高频输出到 UI 必须**合并**，到模型必须**可回溯** | ❌ 前者未合并（每 chunk 一帧），后者不可回溯（截断即丢） |

---

## 4. 发现详情

### D-1：截断即丢失——四层限额互不知情，外置通道对 exec 不可达

> ⚠ 2026-09-13 修订（R-1/R-2）：**"外置不可达 = 缺陷"的定性撤销**（Codex 无外置通道且默认预算更紧）；**"spill-first 属 P0"降级为可选**。见附录 C。下文保持原样。

#### D-1.1 现象

命令输出超过一定程度后，模型看到的是"头 70% + 尾 30% + 一行提示"，**中段永久消失**，且没有任何工具能取回。提示文本让模型"用更窄的参数重调命令"（`exec/truncate.rs:65,75`）。

#### D-1.2 根因（E2 + 算术）

四道限额由四个模块各自设定，没有任何一处做交叉校验：

| 层 | 限额 | 位置 |
|---|---|---|
| 读线程 | 5 MiB/流（超出即丢弃，`capped=true`） | `process_registry.rs:45` ← `direct.rs:92` |
| 模型文本（标准模式） | `max_output_tokens` 默认 10K token，`token_truncate` 保头 70%/尾 30% | `handler.rs:218-225`、`truncate.rs:53-87` |
| `ToolResult` 硬顶 | `TOOL_MODEL_MAX_CHARS = 24_000` 字符 | `tool_result.rs:15` |
| 内容外置 | 模型文本 > `CONTENT_STORE_THRESHOLD_BYTES = 10 MiB` 才外置 | `content_store.rs:23`、`registry.rs:752-760` |

**外置不可达的算术**：标准模式下模型文本 = `output` 字段 ≤ 10K token ≈ 40 KB ≪ 10 MiB，条件恒假。NoFold 极限模式下 `ok_with_limit(json, None)` 解除 24K 字符顶（`handler.rs:300-307`），模型文本上限 = 两条流各 5 MiB + 1 字节分隔 = 10 MiB + 1，再叠加 JSON 信封（`status/command/exit_code/...` 约百来字节）——**仅仅因为信封开销才可能越过 10 MiB 门槛**，且要求 stdout 与 stderr 双双顶满 5 MiB。即：外置在标准模式恒不可达，在极限模式是"恰好踩线"的偶然。`qaqh-domain/src/timeline.rs:104-110` 的注释已经写明了这个差距（"二者差两个数量级——外置路径在标准模式下永远不会触发"）。

#### D-1.3 影响面

- 该注释给出的对策是"想看更多应让模型用更窄的参数重调工具"。这条对策在**可重放的命令**（`ls`、`rg`）上成立；在**不可重放的输出**上不成立：编译日志、测试日志（重跑结果不同）、一次性长输出（`cargo tree`、`docker logs`）、需要中段上下文的错误（头尾都正常，报错在中间）。这正是 agent 最常遇到的形态。
- 与本次截断修复叠加后的实际观感：修好之前是"静默丢 13 KB"，修好之后是"到 5 MiB/10K token 处诚实截断"——**静默性去掉了，可恢复性仍然没有**。
- 前端同样拿不到：`TimelineTool.output` 就是这份截断文本（domain 注释已声明"`output` 就是前端能拿到的全部"）。

#### D-1.4 复现（E2，可执行）

```
cargo run -p qaqh-workspace -- serve --port 0 --token t
# 另一终端（或经 daemon）：exec 一条产出 >10K token 且中段含唯一标记的命令
python -c "print('A'*200000); print('NEEDLE-MIDDLE'); print('B'*200000)"
# 期望：结果 truncated=true，output 中部无 NEEDLE-MIDDLE；任一工具都无法取回
```

#### D-1.5 修复建议（最小 diff 方向）

**spill-first**：读线程把解码后的字节追加到 per-process spill 文件（会话临时目录），内存只留 head/tail 环；`ExecOutput` 增 `spill_path` + `bytes` + `lines`，截断提示改为"完整输出在 `<path>`，用 read/grep 取中段"。

- 不需要新协议：既有 `read`（offset/limit）与 `grep` 就能读回，且后台进程持续写文件时 `read` 天然可取增量。
- 顺带消灭 `captured_full` 的 5 MiB 内存常驻与 D-1 的全部限额错位。
- 建议分两条文件（`stdout.log`/`stderr.log`）：现状 `combined = stderr + '\n' + stdout`（`direct.rs:259-266`）把 stderr 整段提到最前，已经破坏了流身份；落盘应保留身份而不是复制这个拼接。

若暂不做 spill，退一步的最小改动是：把 `max_output_tokens` 的上限（现 50000）与 `FULL_CAPTURE_BYTE_CAP` 对齐并把截断提示改为可操作指令——但这只是把"丢得更多"变成"丢得少一点"。

#### D-1.6 验收清单

| 步骤 | 期望 |
|---|---|
| `exec` 产出 200 KB 且中段含唯一标记 | 结果含 `spill_path`；`bytes` 等于命令真实输出字节数 |
| 用 `read(spill_path, offset, limit)` 读中段 | 能读到 `NEEDLE-MIDDLE` |
| 后台进程持续输出后 `process check` | `bytes` 单调增长；再 `read(spill_path, offset=上次末尾)` 能取到增量 |
| 5 MiB 以上输出 | 不再出现"内存里 5 MiB、模型 40 KB、前端 40 KB"的三份不同真相 |

---

### D-2：进程注册表无会话归属——跨会话可见、可查、可杀

#### D-2.1 现象

`process` 工具的 `id` 是全局自增 `u32`（`process_registry.rs:141-145`），注册表是进程级 `static`（`96-97`）。任意会话拿到（或猜到）一个 id，就能读取别的会话的进程状态、tail 输出，甚至整树终止它。

#### D-2.2 根因（E2）

- **注册表 API 面无会话参数**：`register(name)`、`get_info(id)`、`try_wait(id)`、`is_running(id)`、`kill(id)`、`wait_for(id,..)` 全部只吃 `id`（`process_registry.rs:119,187,272,294,364,428,495`）。没有任何字段记录创建者。
- **工具上下文无会话**：`ToolCallCtx`（`lib.rs:549-562`）没有 session 字段；`process_inspect.rs` 也没有任何会话校验。
- **授权分类放宽**：`process` 的 `check`/`wait` 被归类为 `ToolCategory::Read`（`permission.rs:426-430`），在 Level≥2 直接 `AutoApprove`（`permission.rs:450-452`）——即"跨会话读别人的进程输出"不需要任何批准。`kill` 归为 Exec，但审批界面上呈现的是"kill 进程 7"，用户无从得知 7 属于哪个会话；Level 4（Unrestricted）下连这一步都没有。
- **id 可猜**：`next_id` 从 1 单调自增、`saturating_add`、不做随机化。同 daemon 内跨会话枚举成本极低。

#### D-2.3 影响面

多会话 daemon 下这是**隔离边界的击穿**：会话 A 的模型（或被注入的提示词）可以读走会话 B 的命令输出（可能含凭据、路径、业务数据），也可以杀掉 B 的构建/测试/子代理。注意这不是"提权"——同处一个 OS 用户下的工具本来就有同等能力（`exec` 能起任意进程）——但它是**授权语义的错位**：`process` 被设计成"管理我自己后台进程的工具"，实现却是"管理本 daemon 全部进程"。子代理（`subagent:*`）与 exec 共用一张表，使影响面进一步扩大（见 D-8）。

#### D-2.4 复现（E2）

```
# 会话 A：exec(command="sleep 300", background_after_secs=1) → process_id = N
# 会话 B：process(action="check", id=N)         → 返回 A 的进程状态与 output_tail（无需批准）
# 会话 B：process(action="kill",  id=N)         → 终止 A 的进程树
```

#### D-2.5 修复建议

1. **给条目盖 owner**：`ProcEntry` 增 `owner_session: String`；写入点在 `register` 时取 `crate::runtime::context().map(|c| c.active_session)`（`runtime.rs:113-131` 已把会话装进工具线程的 `RUNTIME_CTX`，`execution.rs:37` 已绑定——**基础设施现成，不需要新管道**）。
2. **API 面加会话参数**：`get_info(id, caller)`/`kill(id, caller)` 等，内部不匹配即返回 `NOT_FOUND`（不要返回"无权"，避免暴露存在性）。
3. **`process` 工具层先筛**：`handle_check/wait/kill/write` 在调用注册表前做一次 `owner == caller` 判定。
4. 兼容性：`id` 从整数改为不透明字符串（`p_<sess8>_<n>`）会破坏模型已学的调用形态；可先保留整数 id + owner 校验，再在句柄化重构（§6）里一并换字符串。

#### D-2.6 验收清单

| 步骤 | 期望 |
|---|---|
| 会话 A 起后台进程，会话 B `check` 该 id | `NOT_FOUND`（且不泄漏名字/输出） |
| 会话 A 自己 `check` | 正常返回 |
| Level 4 下会话 B `kill` A 的 id | 拒绝 |
| 会话 A 的 id 在 A 结束后被 B 复用尝试 | 仍拒绝（owner 随条目，不随 id 号段） |

---

### D-3：`kill` 在全局注册表锁内执行外部进程与等待

#### D-3.1 现象

一次 `process kill` 会让**所有会话**的读线程与 exec 轮询一起停顿，停顿时长等于 `taskkill` 的整树终止耗时。

#### D-3.2 根因（E2 + E1 定量）

`ProcessRegistry::kill` 的整个函数体跑在 `Self::with(...)` 里（`process_registry.rs:428-484` → `112-114`），也就是持有 `REGISTRY: LazyLock<Mutex<ProcessRegistry>>`；函数体内同步执行：

```rust
let _ = Command::new("taskkill").args(["/pid", .., "/T", "/F"]).status();  // 439-441：spawn + 阻塞等待
let _ = c.wait();                                                          // 442
```

同锁的竞争者包括：每条读线程每个 chunk 的 `append_output`/`append_stderr`（`325-362`）、exec 主循环每 50 ms 的 `try_wait`（`direct.rs:179`）、`process check/wait`、以及 `register` 里的惰性驱逐（`119-140`，还会顺带 `entries.remove` + 输出释放）。Unix 分支同理（`killpg` + `c.wait()`，`446-452`）。

**E1 实测（本机）**：

```
tree: parent=9240 direct_children=9
taskkill /T /F elapsed_ms = 108.7
```

即典型窗口 ≈ 0.1 s；进程树越大、树内有悬挂 I/O 的进程时窗口可达秒级。窗口内 `append_output` 阻塞的是**读线程**——读线程停止排空管道 → 子进程写端可能被管道缓冲反压 → 一次 kill 变成对无关会话的可见卡顿。

#### D-3.3 影响面

多会话并行 + 高频输出场景下的延迟尖刺来源之一；与 `docs/archive/2026-09/report/2026-09-12-多会话高频输出热路径串行化与切会话401-report.md` 记录的热路径串行化是同一类问题（"锁跨阻塞系统调用"）。单会话场景影响可忽略（P2）。

#### D-3.4 复现（E1，见附录 B）

```
tree: parent=9240 direct_children=9
taskkill /T /F elapsed_ms = 108.7
```

配合 E2（锁范围 = 整个函数体）即得结论；直接测量"kill 期间 `append_output` 的停顿"需要改代码埋点，**未做**（§10）。

#### D-3.5 修复建议

- 锁内只做"取出句柄 + 置状态"，锁外做 `taskkill`/`killpg`/`wait`：

```rust
// 锁内
let (handle, os_pid, prev_exit) = Self::with(|r| { /* take child_opt, clone os_pid */ });
// 锁外
terminate_tree(handle, os_pid);
Self::with(|r| { /* 落 Killed + last_exit_code */ });
```

- 进一步：条目内部改 `Arc<Mutex<..>>`（现状已经是 `Arc<Mutex<..>>` 字段，但**外层还套了一层全局表锁**），把全局锁降级为"只在增删/查找条目时持有"。

#### D-3.6 验收清单

| 步骤 | 期望 |
|---|---|
| 两个会话各有 exec 在产输出，同时对第三个进程 `kill` | 两个读线程无可见停顿（用 `process check` 的 `output_size` 采样或日志时间戳验证） |
| `kill` 大进程树（≥50 子进程）期间并发 `exec` | 新 `exec` 的启动不被推迟到 kill 之后 |
| 单元测试：持锁不变量 | 断言 `kill` 期间 `append_output` 可在 ≤5 ms 内完成 |

---

### D-4：进程树收敛依赖 pid 快照（pid 复用可误杀；Windows 无 JobObject）

#### D-4.1 现象

`kill` 对一个**已经退出**的条目仍会执行 `taskkill /pid <os_pid> /T /F`（`process_registry.rs:454-471`，注释明确说是为 `backgrounded` 场景清理残留后代）。此时子进程句柄已被 `try_wait` 回收（`196-202` 置 `child_opt = None`），`os_pid` 只是一个**历史数字**。

#### D-4.2 根因（E2）

- Windows：`CREATE_NO_WINDOW` 只控制窗口，不建立容器（`direct.rs:43-48`）；没有 JobObject，因此没有"父死子随"的内核保证，只能靠 `taskkill /T` 按 pid 遍历。
- Unix：`process_group(0)`（`direct.rs:53-54`）+ `killpg(pid)`（`446-452`）——pgid 恰好等于首进程 pid；一旦该进程被回收，pgid 可被系统复用给无关进程组，`killpg` 会**整组误杀**。
- 回收与快照之间存在窗口：`try_wait` 在任何查询路径都会自动置终态并丢弃句柄（`179-213`，`process check/wait` 也调它），所以"句柄还活着所以 pid 安全"这个前提在 backgrounded 场景下**必然失效**——那正是这个分支存在的理由。

#### D-4.3 影响面

误杀范围是任意进程树（含 daemon 自身？不会：`taskkill /T` 从给定 pid 向下，不含祖先；但可以杀掉同用户下任意进程组）。触发条件：条目终态后 10 分钟内（驱逐门槛，`132`）有人对该 id 调 `kill`——包括模型"清理一下"的常规行为。属小概率高破坏，按 P1 记（安全边界 + 不可逆副作用）。

#### D-4.4 复现（E3，未实测）

需要构造 pid 复用窗口（Windows 需大量进程创建/销毁），未做实测。判定依据是 E2：`os_pid` 在句柄回收后仍被用于终止（`86-88` 注释 + `454-471` 实现）。

#### D-4.5 修复建议

1. **借仓库已有的正确做法**：`qaqh-lsp/src/adapter.rs:68`、`qaqh-mcp/src/adapter.rs:125` 已 `wrap.wrap(JobObject)`（Windows）/ `ProcessGroup::leader()`（Unix）。`qaqh-workspace` 未依赖 `process_wrap`，应引入并让 exec 走同一条 spawn 路径——kill-on-job-close 让进程树收敛变成内核保证，pid 快照只作为"日志信息"保留。
2. 过渡期最小加固：`kill` 对**终态条目**不要用 `os_pid` 执行破坏性终止；改为只在句柄仍在时杀，并对终态条目返回 `already_exited`（让模型的"清理"意图明确失败，而不是静默杀一个陌生 pid）。
3. 记录 `os_pid` 时同时记录**创建时刻**，kill 前校验进程创建时间是否早于记录（Windows 可经 `GetProcessTimes`，Unix 需 `/proc`）——成本高于方案 1，仅作无 JobObject 时的兜底。

#### D-4.6 验收清单

| 步骤 | 期望 |
|---|---|
| 后台 exec 起一个派生孙进程后 `process kill` | 整树消失（`Get-CimInstance Win32_Process` 无残留） |
| 对已退出条目 `kill` | 不执行任何按 pid 的终止；返回 `already_exited` |
| daemon 进程被强杀 | 其拉起的 exec 子进程全部随之终止（JobObject kill-on-close） |

---

### D-5：`process` 契约三处失真

> ⚠ 2026-09-13 修订（R-5）：**"补 `process read(offset)`"的处方被简化**为 drain-per-poll 语义（每次读取抽干缓冲，天然成为游标，无需 offset 记账）；另新增两条契约细节（超时用 exit code 124、非 tty 只接受 Ctrl-C）。**超时语义的完整对照（三路径两种语义、124 的两套机制、backgrounded 被报成 error 的机制）见续报 §13。** 见附录 C。下文保持原样。

#### D-5.1 现象

- `process(action="write", ...)` 永远失败，但 schema 仍在宣传它。
- 后台进程的完整输出（内存里最多 5 MiB/流）**没有任何工具能读**。
- `timeout_secs` 到点后结果里带 `timed_out: true` / `status: "backgrounded"`，但进程既没被超时也没失败。

#### D-5.2 根因（E2）

1. **`write` 死功能**：`pty_writer` 字段（`process_registry.rs:92`）全仓只在构造时写 `None`（`161`），唯一读取点是 `write_to`（`216-256`）→ 必定走 `None => Err("process {id} has no PTY stdin (not interactive)")`。而 exec 侧 stdin 恒为 `Stdio::null()`（`direct.rs:59`），结构上不可能有交互输入。schema（`process_inspect.rs:16-20`）把 `write` 列为四个 action 之一，描述"write: stdin"。
2. **完整输出不可达**：`captured_full`（`process_registry.rs:272-289`）全仓唯一调用方是 `direct.rs:249`（前台 seal）。`get_info`（`364-420`）只暴露 tail 视图，且 tail 自身在 `append_output` 里被持续裁剪（`>5000` 字符时切到 4000，`335-339`）。所以后台进程的输出，除了最后 ~4 KB，全部只在内存里等着被驱逐。
3. **超时语义**：`direct.rs:193-203` 到 `deadline` 时 `timed_out = true; break`（**不 kill**），随后返回 `status:"backgrounded"` 且 JSON 内 `"timed_out": true`（`213-229`）。`handler.rs:291-295` 又用 `!timed_out` 参与 `success` 判定。命名与行为相反：`timeout_secs` 是"观察窗口"而不是"超时"。同时 `background_after_secs`（`handler.rs:232-234`）与它做的是同一件事（转后台），两个旋钮语义重叠。

#### D-5.3 影响面

模型侧的可靠性问题：schema 承诺的能力不存在 → 模型会反复尝试 `write` 并浪费回合；`timed_out: true` 让模型以为命令失败并在下一轮重跑（**重复副作用**）；想看后台输出只能反复 `check` 轮询 tail，永远看不到早期输出。

#### D-5.4 复现（E2）

```
exec(command="cat", background_after_secs=1)   → process_id=N（后台）
process(action="write", id=N, text="hello\n")  → {"error":{"code":"WRITE_FAILED","message":"process write: process N has no PTY stdin (not interactive)"}}
process(action="check", id=N)                  → 只有 output_tail（≤500 字符 / 退出后 ≤~4000 字符）
```

#### D-5.5 修复建议

- `write`：**二选一，不要留着第三种状态**。要么把 stdin 接成真管道（`Stdio::piped()` + 写端注册进条目 + 显式 `stdin_mode: "pipe"` 参数），要么从 schema/文档删除该 action。
- 新增 `process(action="read", id, offset, limit)`，把 `captured_full` 暴露出来（或按 D-1 的 spill 方案读文件）。这是**最便宜的高收益改动**：数据已经在内存里，只差一个出口。
- 新增 `process(action="list")`：无它则 id 丢失即永久失联（会话恢复后注册表为空，见 D-7）。
- 命名：`timeout_secs` → `handoff_after_secs`（或新增 `kill_after_secs` 表达真超时），结果字段 `timed_out` → `handed_off`；`status` 保持 `backgrounded`。

#### D-5.6 验收清单

| 步骤 | 期望 |
|---|---|
| `process list` | 返回本会话全部条目（id/name/status/bytes） |
| `process read id=N offset=0 limit=2000` | 能读到该进程**最早**的输出（不再只有 tail） |
| `write` | 要么成功写入子进程 stdin 并被对方读到，要么该 action 不存在 |
| 观察窗口到期 | 结果字段为 `handed_off: true`，`success` 判定不受其影响 |

---

### D-6：进度事件流未合并，且与回放尾预算耦合

> ⚠ 2026-09-13 修订（R-3）：**"加进度合并器"的处方撤销**。Codex 明确不做服务端合并，改用"单帧 ≤8 KiB + 每调用 ≤10 000 帧"的**帧预算** + **UI 侧有界预览**（聚合不失真，只截实时流）。见附录 C。下文保持原样。

#### D-6.1 现象

命令高速产输出时，前端收到的是**逐 chunk 的时间线事件**，数量与 chunk 数同阶；时间线里累积的进度文本可达 10 MiB（两流各 5 MiB），并占用该会话的回放尾（SSE 重连补帧窗口）的条目与字节预算。

#### D-6.2 根因（E2）

1. **逐 chunk 发射**：读线程 `continue` 快路径（`exec/pipe.rs:139-145`）每个读到的块都会走 `forward_progress` → `send_progress`；上游 `drain_bounded`（`engine_tool.rs:906-936`）虽然批量 `try_recv`，但**逐个 `emit`**；`emit_progress_tail` 直接把 `event.chunk` 转给 `TimelineIntent::ToolProgress`（`engine_tool.rs:41-55,789-802`）。全链路无合并、无节流、无尾化。
   - `engine_tool.rs:786-788` 的文档描述的是"每 (tool_call_id, stream) 只保留最后 4 KB 尾部、前端替换而非拼接"——**该协议在实现中不存在**（`ExecProgressEvent` 也没有 `seq_start` 字段）。这是文档与实现的漂移，应作为 E2 事实记录：现状是"前端渲染器自己合并"（`paced_emitter.rs:1-7` 明确把合并责任推给渲染层）。
2. **两份累积**：
   - `TimelineTool.progress`（`qaqh-domain/src/timeline.rs:118`）在 `append_tool_progress` 里无条件 `push_str`，**无上限**（`qaqh-runtime/src/timeline.rs:457`）。上限间接来自读线程的 5 MiB/流，故最坏 ×2 = 10 MiB，且它在快照投影里（`TimelineTool` 随 `ToolUpdated` 物化）。
   - 同名事件同时计入内存回放尾：`journal_entry_payload_bytes` 按 `chunk.len()` 计费（`timeline.rs:758-765`），`enforce_journal_budget` 双限驱逐（`768-780`：8192 条 / 256 MiB，`persistence_policy.rs:32,40`）。一条 5 MiB 输出按 8 KiB 分块 = 640 条 + 640 次字节计费，**把该 seed 的重连补帧窗口挤掉**。
3. **投递有损且共享**：`hub::fanout` 走 `tokio::sync::broadcast`，容量 1024（`ringing/hub.rs:669-682`）；ToolProgress 属瞬态不落盘（`persistence_policy.rs:15,74`）但又 `occupies_replay_tail`（`82-85` 恒 true）。慢客户端只会 `Lagged` 丢事件，且没有"尾化自愈"实现（协议未落地，见上）。

#### D-6.3 影响面

这是"多 session + 多并行 + api 输出过快 → 前端视觉变慢"的直接机制之一：单条命令能注入 640+ 帧、最多 10 MiB 进度文本，同时压缩所有会话共用的回放尾。与 `docs/archive/2026-09/report/2026-09-12-多会话高频输出热路径串行化与切会话401-report.md` 的写放大族同源；本报告只补 exec 侧的注入源与限额事实，不重复该报告的结论。

#### D-6.4 复现（E2）

```
exec(command="python -c \"[print('x'*8192) for _ in range(200)]\"")   # ~1.6 MB
# 观察：SSE 上出现 ~200 条 tool_progress；timeline 条目数同阶上升
```

#### D-6.5 修复建议

1. **合并器（收益最高、改动最小）**：在 `emit_progress_tail` 之前加一层 per-(tool_call_id, stream) 合并——`≥100 ms` 或 `≥16 KiB` 才发一帧，帧内携带增量或（更省）只带尾部；被合并掉的字节以 `dropped_bytes` 上报。注意 `drain_bounded` 的收尾语义（`tool_done` 后丢弃在途事件）必须保留。
2. **`tool.progress` 有界**：改为 ring（如 16 KiB 尾部）+ `progress_truncated: bool`，与 `TOOL_SUMMARY_MAX_CHARS` 的既有口径对齐。
3. **把文档变成实现**：要么实现 4 KB 尾化协议（前端替换语义），要么删掉 `engine_tool.rs:786-788` 的错误描述——两者都比现状好。
4. **完整输出不走时间线**：全文归 spill 文件/D-1 的通道，时间线只承载"看得见的尾巴"。

#### D-6.6 验收清单

| 步骤 | 期望 |
|---|---|
| 1.6 MB 输出 | `tool_progress` 帧数 ≤ 输出时长/100 ms + 常数（不是 ≥200） |
| `TimelineTool.progress` | 任何时刻 ≤ 设定 ring 上限 |
| 同一 seed 并发两条大输出 | 回放尾条目消耗可预测（不随输出字节线性膨胀） |
| 慢客户端重连 | 不需要"替换语义"也能自愈（帧自带 tail + total） |

---

### D-7：生命周期未与创建者绑定（daemon 退出不清理；内存与驱逐不对称）

> ⚠ 2026-09-13 修订（R-4）：**"终态保留 10 分钟是缺陷"改判为"驱逐策略不明确"**——Codex 同样无空闲超时（只靠 kill-on-drop / 会话 shutdown / 64 上限 LRU），关键在策略明确与 Drop 收敛。见附录 C。下文保持原样。

#### D-7.1 现象

daemon 退出/重启后，先前 `backgrounded` 的子进程仍在运行，且新 daemon 的注册表为空——它们既不被回收也**不可再管理**。同时单条终态条目可占 10 MiB 内存并保留 10 分钟。

#### D-7.2 根因（E2）

- **无退出收敛**：`ProcessRegistry` 没有 `Drop`、没有 `kill_all`，`process_registry.rs` 全文无任何"清理全部"入口；`direct.rs` 只在正常/取消路径对**单条**调 `mark_exited`/`kill`（`233-237`），不覆盖进程退出。对照：仓库内确实有该类机制——`qaqh-mcp` 的 `shutdown_all`（`crates/qaqh-mcp/src/manager.rs:366` 注释、`tests/orphan_reap.rs:164` 断言"shutdown 后整树消失"）——**exec 这条路径没有对应物**。
- **驱逐不对称**：驱逐发生在 `register` 内部（`119-140`），门槛是"终态且 `started` 起超过 600 s"。判据用 `started`（创建时刻）而不是"进入终态的时刻"——一个跑 11 分钟、刚退出的进程，其条目在下一次 `register` 时会**立即**被当成 stale 移除（`now.duration_since(e.started) > 600`），而 `captured_full` 也随之释放。反之若长期没有新 `register`，条目就一直留着（含 10 MiB）。
- **内存上限**：`FULL_CAPTURE_BYTE_CAP = 5 MiB` × 2 流 = 10 MiB/条（`process_registry.rs:38-45`），没有"全表总量上限"或 LRU。

#### D-7.3 影响面

P2：不阻塞主流程，但两张账都不清——孤儿进程（资源泄漏、端口占用、后续构建失败）与内存峰值（多会话各留几条大输出即达数百 MB）。

#### D-7.4 复现（E2）

```
# 1) exec background_after_secs=1 起长驻进程，记下 process_id
# 2) 停 daemon（正常退出）
# 3) 该进程仍在运行；新 daemon 中 process(check, id=N) → NOT_FOUND（且无法 kill）
```

#### D-7.5 修复建议

- **收敛策略显式化**：`orphan_policy: "kill" | "detach"`（默认 kill）。JobObject（D-4）让 kill 变成"关掉 job 句柄"的零成本操作。
- 驱逐判据改为 `terminal_at`（进入终态时刻）并增加全表字节上限（如 256 MiB，超出按 LRU 释放 `FullCapture`）；驱逐也不再只在 `register` 触发，`kill`/`wait_for` 收尾时顺手清理。
- 会话结束时释放该会话条目的内存（保留 id 墓碑以便 `check` 返回"已清理"）。

#### D-7.6 验收清单

| 步骤 | 期望 |
|---|---|
| 强杀 daemon | 其 exec 子进程在 ≤1 s 内全部消失 |
| 长跑 11 分钟才退出的进程，随后立即 `check` | 条目仍可查（不被 `started` 判据误清） |
| 连续 20 条大输出后 | 全表字节占用有界；`process check` 不因驱逐返回 NOT_FOUND（在保留窗口内） |

---

### D-8：子代理寄生同一张注册表（kill 是协作式，且 Remote 每轮一次 HTTP）

#### D-8.1 现象

`spawn_subagent` 会在同一注册表登记一个条目（`qaqh-subagent/src/lib.rs:299-320`、`serve.rs:311-344`），于是 `process` 工具把它与 exec 子进程同等对待。但子代理不是 OS 子进程：注册表里没有 `child` 也没有 `os_pid`，`ProcessRegistry::kill` 对它只是把状态置 `Killed`（`process_registry.rs:428-484` 的 `None` 分支 + `481`），**实际什么都没杀**，却返回 `true`（工具层于是回答"Process N killed."）。

#### D-8.2 根因（E2）

- 取消是**协作式轮询**：子代理的 collect 循环每轮检查 `registry_ref.killed()`（`lib.rs:590-604`），而循环的等待粒度是 `recv_timeout(300 ms)`（`605`）——最坏 300 ms 才看到取消，且若循环卡在别处（如子代理正在跑一个长 exec）会更久。
- Remote 模式（serve 进程持有注册表）下 `killed()` **每次都是一次 HTTP POST `/subagent`**（`lib.rs:204-212` → `263-274`），即每个在跑的子代理 ≈3.3 req/s 的控制面流量，且走 `tiny_http` 每请求一个线程 + 全局注册表锁。多子代理并行时这是可观测的放大器。
- `kill` 的语义在工具层被表述为"已终止"，与"已请求取消"不可区分。

#### D-8.3 影响面

P2：功能可用但语义与代价都不对——控制面被轮询打满、模型对"是否真的停了"判断错误（可能继续等一个已经"被 killed"却仍在产出的子代理）。

#### D-8.4 复现（E2）

```
spawn_subagent(...) → process_id = N
process(action="kill", id=N)     → "Process N killed."（返回 true）
# 但子代理要到下一个 300 ms 轮询边界才真正收到取消；
# 期间 process check 显示 killed，ring 上仍有该子代理的事件
```

#### D-8.5 修复建议

- **控制面走控制面**：子代理取消用既有的 Ringing 命令通道（`ConversationCommand::ConversationCancel`，`lib.rs:595-597` 已经这么发了），把"kill 请求"从轮询改为**推送**；注册表只保留"状态查询"职责。
- 若必须轮询，把它换成带退避的共享订阅（或让 serve 侧在状态变更时向 worker 推送），消除每轮 HTTP。
- 语义分离：`process kill` 对 `kind=Subagent` 的条目要么返回"cancel requested"（诚实），要么路由到控制面并等确认。

#### D-8.6 验收清单

| 步骤 | 期望 |
|---|---|
| `process kill` 子代理 | 返回值区分"已取消"与"取消已请求"；子代理在 ≤100 ms 内停止产出 |
| 3 个子代理并行 10 s | serve `/subagent` 端点的请求数与子代理数**无关**（推送模型） |

---

## 5. 判定：整体设计有没有问题

**分级结论**（这是用户问题的正面回答）：

| 面 | 判定 | 理由 |
|---|---|---|
| spawn / 读线程 / 就绪探测 | ✅ **方向正确，不重构** | `Readiness` 抽象 + per-OS 胶水 + 有界 join 是这次截断修复能落地的前提（V-1/V-2）。剩余问题（settle 启发式）是"用轮询近似就绪"的代价，可用 `poll`/`WaitForMultipleObjects` 消除，但不是缺陷 |
| 封口与截断 | ⚠️ **结构对、口径散** | 以 `captured_full` 为权威是对的；错在**同一个"输出"被四层各自设限**且外置通道不可达（D-1）。属"设计缺陷"而非"实现 bug"：每一层单看都合理 |
| 进程身份与授权 | ❌ **真缺陷** | 全局无归属表 + `check/wait` 免批准（D-2）。这不是权衡，是漏设计——基础设施（工具线程携带会话）已经具备，只是没用 |
| 生命周期收敛 | ❌ **真缺陷** | pid 快照 + `taskkill` 取代内核级 containment（D-4），且无退出清理（D-7）。同仓库另两个 crate 已有正确做法可抄 |
| exec ↔ process 契约 | ❌ **真缺陷** | `write` 死功能、无 `read`/`list`、`timed_out` 名不副实（D-5）。schema 是对模型的契约，承诺了不存在的能力就是缺陷 |
| 事件流 | ❌ **真缺陷**（与既有报告同源） | 逐 chunk 发射 + 无界累积 + 有损广播（D-6）。合并责任被推给渲染层（`paced_emitter.rs:1-7`）在"模型 token 流"上或许合理，在"命令输出流"上不成立——后者的速率比 token 流高 2–3 个数量级 |
| 锁粒度 | ⚠️ **真缺陷但影响面窄** | 全局锁跨阻塞外部调用（D-3）。单会话无感，多会话是尖刺来源 |
| 子代理共用注册表 | ⚠️ **耦合缺陷** | 协作式 kill + 轮询控制面（D-8）。方向（统一可观测）不错，实现选了最贵的方式 |

**一句话**：exec 的**读取机制**在本次修复后是站得住的；exec 的**产品契约**（输出怎么交付、进程归谁、谁来收敛、怎么告诉 UI）四处都不成立。这四处里，D-1/D-2/D-5 是"漏设计"，D-4/D-6/D-7 是"选了更差的机制"，都不是不可避免的架构宿命。

---

## 6. 如果我重新设计（exec + process 联合）

### 6.1 核心转变

> **`exec` 不再是"一次会阻塞的函数调用"，而是"一个会话拥有的进程资源"；输出不再是"返回的字符串"，而是"落在会话目录里的文件 + 一个可读的尾巴"。**

三段式返回取代两段式：`started(handle)` / `completed(result)` / `handed_off(handle)`，句柄是唯一对外身份。

```
ProcessHandle {
    id: String,              // 不透明：p_<session8>_<seq>，会话内可用短号别名
    owner: SessionId,        // 创建者（授权基准）
    kind: Exec | Subagent | Service,
    spec: { argv, cwd, env_digest, shell },   // 可审计、可重现
    started_at, status, exit_code,
    output: { dir, stdout_path, stderr_path, bytes, lines },  // spill，权威
    alive: ProcessGuard,     // JobObject/进程组句柄，kill-on-close
}
```

### 6.2 逐项改造（标注"是否必须动架构"）

> ⚠ 2026-09-13 修订：本表 **R-1（spill-first）降级为可选**、**R-6（进度合并）处方替换为帧预算 + UI 有界预览**；另补 R-2 的更优形态（"表属会话"）与两个新项（有损通道游标兜底 X-1、Unix `PDEATHSIG`）。修订后的完整顺序见续报 §6，下文保持原样。

| # | 改造 | 是否动架构 | 说明 |
|---|---|---|---|
| R-1 | **spill-first 输出**：读线程把解码字节追加到 `stdout.log`/`stderr.log`（会话临时目录），内存只留 head 64 KiB + tail 64 KiB + `bytes/lines` 计数；`captured_full` 退役 | 动（读线程写目标从内存改文件，但接口不变） | 一次性消灭 D-1 的四层错位、D-7 的 10 MiB/条内存、以及"中段不可达"。读回复用既有 `read`(offset/limit)/`grep`，**不需要新协议**；后台进程持续写文件时天然支持增量读 |
| R-2 | **句柄化 + 会话归属**：`id` 改字符串、条目带 `owner`，`register` 时取 `crate::runtime::context()` 的会话（现成基础设施）；全部注册表 API 加 caller 参数 | 动（API 面） | 修 D-2。兼容期可保留整数 id + owner 校验 |
| R-3 | **内核级 containment**：`process_wrap`（`JobObject`/`ProcessGroup`），kill-on-close；`os_pid` 只作展示不再作终止依据 | 动（spawn 层） | 修 D-4、D-7。仓库另两个 crate 已有先例，属"统一既有做法"而非新技术 |
| R-4 | **锁粒度**：`RwLock<HashMap<SessionId, Arc<SessionProcs>>>`；条目内 `Arc<Mutex<Entry>>`；`kill` 锁内只取句柄、锁外终止 | 动（注册表结构） | 修 D-3。改动局限于 `process_registry.rs` 单文件 |
| R-5 | **就绪模型**：`poll(2)`/`WaitForMultipleObjects` 同时等 {进程退出, 管道可读}，删除 50 ms tick 与 300 ms settle | 动（读线程） | 消除启发式（本次修复的残留风险）。**收益最低、风险最高，可最后做** |
| R-6 | **进度合并 + 有界进度**：合并器（≥100 ms / ≥16 KiB），帧带 tail+total；`tool.progress` 改 ring；实现或删除 4 KB 尾化协议文档 | 半动（runtime 侧为主） | 修 D-6。合并器可先在 runtime 侧落地，不必等 exec 改 |
| R-7 | **契约修正**：`handoff_after_secs`（原名 `timeout_secs`）+ 可选 `kill_after_secs`；结果 `handed_off` 取代 `timed_out`；`process` 增 `list`/`read`；`write` 二选一 | 不动（局部） | 修 D-5。全部是签名/字段/文档级改动 |
| R-8 | **子代理与控制面解耦**：取消走 Ringing 命令推送，注册表只做状态；`kind` 区分并对模型如实表述 | 不动（局部） | 修 D-8 |

### 6.3 刻意保留

- 读线程与 seal 解耦、`captured_full` 式"权威快照 + 有界 join"的**思想**（换成 spill 文件后依旧成立：权威从内存字符串变成文件）。
- 进度通道**有界丢弃**、绝不为慢 UI 阻塞读线程（V-3）。
- 截断保留头尾（V-4）——但有了 spill，截断只影响模型首屏，不影响可达性。
- `Shell` 枚举、`rg` 防御、`Readiness` 抽象（V-2/V-5）。

### 6.4 明确不做（反过度设计）

1. **不把 exec 全链路改 async**。工具 handler 是同步 `fn` 指针（`backend.rs:24`），整条链同步是既有约束；为 exec 单独引入 async 只能换来 R-5 一项收益，代价是全链路传染。
2. **不为"完整性"恢复"无限等 EOF"**。那正是 2026-09-02 冻结事故（`ddcd606` / `d30eb9b` 的由来）。有界退出必须保留。
3. **不把完整输出塞进模型上下文或时间线**。5 MiB 进上下文是另一类事故；正确做法是 spill + 按需 `read`。
4. **不给 `process` 加 PTY**（除非确有交互需求）。exec 的 stdin 设计是 `null`，加 PTY 会引入一整套终端语义（回显、CR/LF、SIGWINCH）；若只是为了"让 `write` 不撒谎"，删 action 更便宜。
5. **不做跨 daemon 的进程持久化**。孤儿策略 + JobObject 足够；把注册表落盘会引入"id 回收/所有权校验/跨重启状态同步"三类新问题。

---

## 7. 落地路线（不重构也能做；按收益/代价排序）

> ⚠ 2026-09-13 修订：**本表的阶段顺序与 S1/S4 内容已被续报 §6 替换**（S1 由 spill 改为"进程表会话私有化"；S4 由合并器改为帧预算 + UI 有界预览；S3 增加 JobObject 原子入作业）。**下文保持原样，勿按此表排期。**

| 阶段 | 内容 | 覆盖 | 代价 | 验收 |
|---|---|---|---|---|
| **S1** | spill-first 输出（R-1）：读线程写文件 + `ExecOutput` 增 `spill_path/bytes/lines` + 截断提示改写 | D-1、D-7（内存半） | 中（读线程 + `direct.rs` + 截断文案；`captured_full` 可保留一段时间做双写对照） | D-1.6 四项 |
| **S2** | 会话归属 + `process read/list`（R-2 最小版 + R-7 半） | D-2、D-5 | 小（owner 字段 + 入口校验 + 两个 action；`captured_full` 已有出口数据） | D-2.6、D-5.6 |
| **S3** | JobObject/进程组 + `kill` 锁外执行 + 退出清理（R-3、R-4、D-7 收敛） | D-3、D-4、D-7 | 中（引入 `process_wrap` 或直接 `windows`/`libc`；注册表结构小改） | D-3.6、D-4.6、D-7.6 |
| **S4** | 进度合并器 + `tool.progress` 有界 + 协议文档对齐（R-6） | D-6 | 小-中（主要在 `qaqh-runtime`，与 exec 改动解耦） | D-6.6 |
| **S5** | 契约命名清理 + `write` 二选一 + 子代理解耦（R-7 余、R-8） | D-5、D-8 | 小（但要同步改 prompt/文档/schema 快照测试 `schema_spot_check.rs`） | D-5.6、D-8.6 |
| **S6**（可选） | 就绪模型换 `poll`/`WaitForMultipleObjects`，删 settle 预算（R-5） | 残留风险 | 高（跨平台重写读线程） | 冻结事故与截断事故的双向回归测试 |

**S1 与 S2 应优先做**：两者都不依赖 `process_wrap`、不触碰并发模型，却各自消掉一个 P1（"输出不可恢复"与"跨会话越权"）。

> 按仓库约定，本节若要执行，应拆成 `docs/plan/2026-09-12-exec与process重构-plan.md`（plan 承载待办，report 只留结论与证据）。

---

## 8. 次要观察

| # | 位置 | 观察 | 级别 | 证据 |
|---|---|---|---|---|
| O-1 | `exec/direct.rs:259-266` | 结果把 stderr **整段前置**（`stderr + '\n' + stdout`），流身份与交错顺序都丢失。spill 方案应落两个文件而不是复制这个拼接 | P3 | E2 |
| O-2 | `exec/pipe.rs:87-111` | `Closed` 分支在 `is_running` 为真时仍会在 settle 到期后 `break`：孙进程持写端且仍在写的窗口内，尾部数据仍会丢（只是不再丢已缓冲的数据）。本次修复把"静默丢"降级为"有界丢"，应补 `saw_eof=false` 的 `log::warn!`（已列入上一份报告的 O-2） | P2 | E2 |
| O-3 | `exec/pipe.rs:431-443` | 模块文档仍写"settle 到期即退出…**绝不等待 EOF**"，与本次修复后的语义（`Closed` 时继续排空到 `Ok(0)`）相反，是下一个人的陷阱 | P3 | E2 |
| O-4 | `process_registry.rs:335-339,355-358` | tail 视图每 chunk 做 `chars().count()`（O(n) 扫描）+ `char_safe_tail` 拷贝；量级小（几 ms 级），但属纯浪费，改 ring buffer 可一并消除 | P3 | E2（量级为 E3 估计） |
| O-5 | `exec/handler.rs:64-68` | **同一句文档注释被复制了两遍**（`64`、`65`）；`shell_available` 内还每次调用都执行一遍 `Shell::detect()`/`Shell::from_name("bash")`（首次有缓存副作用，其后为幂等空转；`available_shells` 会对三个 shell 各调一次，故每轮 6 次空转） | P3 | E2 |
| O-6 | `exec/handler.rs:371-374` | `max_output_tokens` 的 schema 文案写死 `"Max output tokens (10000, 100-50000)"`，与实际默认（StandardPolicy 10K / NoFold `u32::MAX`）不一致 | P3 | E2 |
| O-7 | `exec/handler.rs:285-290` | `&` 后台派生提示只在 `status == "completed"` 时追加，`backgrounded` 路径不提示；而后台移交恰恰是最需要"把输出重定向到文件"的场景 | P3 | E2 |
| O-8 | `serve.rs:249-282` | WSL 模式下所有 Workspace 工具经**单条串行 executor 线程**（`394-412`，队列 64，满则 429 `277-281`）执行，`process` 是唯一内联例外（`242-247`）；native 模式走本地后端不受此限（`actor.rs:97-108`）。文档化这条差异可避免后来者把 WSL 的串行误判为全局行为 | P2（WSL 专项） | E2 |
| O-9 | `exec/direct.rs:204` | 轮询间隔固定 50 ms：命令在 1 ms 内结束也要等一个 tick 才被发现（`try_wait` 首次调用其实很快，但 sleep 无条件执行）——`deadline` 检查顺序可优化为"先 try_wait 再 sleep"已有，属可接受 | P3 | E2 |
| O-10 | `process_registry.rs:141-145` | `next_id` 耗尽只 `log::error!` 后继续 `saturating_add`，之后所有进程共用 `u32::MAX` 一个 id（条目互相覆盖） | P3 | E2 |

---

## 9. 不确定性与未验证假设

1. **未做端到端跨会话越权实测**（D-2）：结论由"注册表 API 无会话参数 + permission 分类为 Read"两条 E2 推出，**没有**在两会话 daemon 上实际执行 `check`/`kill` 验证。属 E2→E3 之间的推断。
2. **未直接测量 `kill` 期间的 `append_output` 停顿**（D-3）：E1 只测了 `taskkill` 耗时（108.7 ms），"锁被持有时长 ≈ 该耗时"是 E2 读码结论。要坐实需在 `kill` 前后埋点。
3. **未构造 pid 复用窗口验证误杀**（D-4）：风险判定基于 `os_pid` 在句柄回收后仍被用于终止（E2）。误杀是否在真实负载下发生**未观测**。
4. **`tool.progress` 峰值内存与快照体积未实测**：10 MiB 上界是"两流 × 5 MiB"的算术推导（E2 输入 + E3 合成）；实际是否进入某个具体快照/检查点写入路径、以及放大倍数，属上一份多会话报告的范围，本报告未重复测量。
5. **"逐 chunk 一帧"对前端帧率的影响未量化**：帧数由 chunk 数推出（E2），前端实际合并/丢弃行为未观测（前端不在本仓库，无法读其实现）。
6. **`process_wrap` 引入成本未评估**：仅确认 `qaqh-lsp`/`qaqh-mcp` 在用（E2）；`qaqh-workspace` 引入它对构建时间/依赖树/edition 2024 兼容性的影响未验证（上一轮曾遇到 `process_wrap` 9.x/10.x 双版本冲突的构建报错，来源是临时基准 crate，不代表本仓库）。
7. **`check/wait` 免批准是否为有意的产品决策**未知：`permission.rs:422-424` 的注释只解释了"按调用形态细分"，未说明是否考虑过跨会话影响。

---

## 10. 产物与复现物清单

| 路径 | 类型 | 是否落盘 | 说明 |
|---|---|---|---|
| `docs/report/2026-09-12-exec与process工具设计评审-report.md` | 本报告 | ✅ 已落盘 | 新建 |
| `crates/qaqh-workspace/src/exec/pipe.rs` | 代码改动 | ⚠️ 工作区未提交（本会话早前） | 截断修复（`Readiness::Ready(Option<usize>)` / `Closed` 排空 / 退出判定顺序），与本次评审无关但被本报告引用 |
| `crates/qaqh-workspace/src/exec/direct.rs` | 代码改动 | ⚠️ 工作区未提交（同上） | 传入 Peek 水位 + `hard_trunc` |
| `crates/qaqh-workspace/src/exec/tests.rs` | 代码改动 | ⚠️ 工作区未提交（同上） | 2 行适配 |
| `%TEMP%` 下临时基准 crate | 临时物 | 已删/未跟踪 | 上一轮 `process_wrap` 版本冲突排查用，不在仓库内 |
| **本报告未做任何代码改动** | — | — | 评审只读代码；D-3 的 `taskkill` 实测为一次性 shell 命令（见附录 B），未留脚本 |

---

## 11. 后续工作与建议排期

| 优先级 | 工作 | 责任面 | 备注 |
|---|---|---|---|
| **P0** | ~~把 S1/S2 拆成 `docs/plan/2026-09-12-exec与process重构-plan.md`~~ → **暂缓**（2026-09-13：用户决定先做外部终审，plan 待终审后再拆） | workspace / exec + process | 拆分时的阶段顺序请用续报 §6（本表 §7 已作废） |
| **P0** | D-2 的越权风险先出一行 buglist 条目（安全边界不应只躺在报告里） | buglist | 一行一缺陷，详情链回本报告 §4.D-2 |
| **P1** | S3：JobObject/进程组 + `kill` 锁外执行 + 退出清理 | workspace / process_registry | 可借 `qaqh-lsp` 既有写法 |
| **P1** | ~~S4：进度合并器~~ → **帧预算 + UI 有界预览**（修订 R-3，见附录 C） | runtime / timeline | 与既有 401 报告同源，**建议并案处理** |
| **P2** | S5：契约命名 + `write` 二选一 + 子代理解耦；同步 prompt 与 `schema_spot_check.rs` | workspace / subagent | schema 变更要过 `schema_spot_check` |
| **P2** | O-2（`saw_eof=false` 告警）与 O-3（订正 `pipe.rs:431-443` 模块文档） | exec | 上一份报告已列，仍未做 |
| **P2** | **X-1（新增，续报 §1.3）**：有损实时通道缺游标兜底 | runtime / ringing | 建议单出一行 buglist |
| **P3** | S6 就绪模型重写（可选，需双向回归测试护航） | exec / 平台层 | 收益最低，最后做 |

---

## 附录 A：环境快照

| 项 | 值 |
|---|---|
| OS | Windows 11（build 26300） |
| Shell | pwsh 7.6.6 |
| 仓库 | `D:\project\QAQ-Harness`，commit `e61efe0`（2026-09-12 22:02:26 +0800） |
| Rust | edition 2024 workspace，16 crates |
| 构建 profile | release `opt-level="z"` + LTO + strip（本报告未构建，全部结论来自读码 + 一次 shell 实测） |
| 相关既有报告 | `docs/archive/2026-09/report/2026-09-12-exec输出静默截断与引入点考证-report.md`（截断根因与引入点）、`docs/archive/2026-09/report/2026-09-12-多会话高频输出热路径串行化与切会话401-report.md`（热路径写放大族，**挂起中**） |

## 附录 B：复现命令

**B-1（E1）`taskkill /T /F` 整树耗时——D-3 的锁持有窗口定量**

```powershell
$parent = Start-Process pwsh -PassThru -ArgumentList '-NoProfile','-c','1..8 | % { Start-Process pwsh -ArgumentList ''-NoProfile'',''-c'',''Start-Sleep 90'' -WindowStyle Hidden }; Start-Sleep 90' -WindowStyle Hidden
Start-Sleep -Seconds 3
$kids = (Get-CimInstance Win32_Process -Filter "ParentProcessId = $($parent.Id)").Count
"tree: parent=$($parent.Id) direct_children=$kids"
$t = Measure-Command { taskkill /pid $parent.Id /T /F | Out-Null }
"taskkill /T /F elapsed_ms = $([math]::Round($t.TotalMilliseconds,1))"
```

实测输出（本机，2026-09-12）：

```
tree: parent=9240 direct_children=9
taskkill /T /F elapsed_ms = 108.7
```

**B-2（E2）关键事实的读码定位**

```powershell
# 注册表 API 面无会话参数（D-2）
Select-String -Path crates\qaqh-workspace\src\process_registry.rs -Pattern 'pub fn (register|get_info|try_wait|is_running|kill|wait_for|write_to)'

# pty_writer 全仓只被写成 None（D-5）
Get-ChildItem -Recurse crates -Filter *.rs | Select-String -Pattern 'pty_writer'

# 四层限额（D-1）
Select-String -Path crates\qaqh-workspace\src\process_registry.rs -Pattern 'FULL_CAPTURE_BYTE_CAP: usize'
Select-String -Path crates\qaqh-types\src\tool_result.rs -Pattern 'TOOL_MODEL_MAX_CHARS: usize'
Select-String -Path crates\qaqh-runtime\src\ringing\content_store.rs -Pattern 'CONTENT_STORE_THRESHOLD_BYTES: usize'

# 每 chunk 一帧 + 无界 progress（D-6）
Select-String -Path crates\qaqh-runtime\src\timeline.rs -Pattern 'progress\.push_str|ToolProgress \{ chunk'
Select-String -Path crates\qaqh-runtime\src\ringing\hub.rs -Pattern 'fn fanout'

# kill 的锁范围（D-3）：函数体整体在 Self::with 内，含 taskkill/wait
Select-String -Path crates\qaqh-workspace\src\process_registry.rs -Pattern 'pub fn kill|taskkill|fn with'

# 仓库既有的 JobObject 正确做法（D-4）
Select-String -Path crates\qaqh-lsp\src\adapter.rs,crates\qaqh-mcp\src\adapter.rs -Pattern 'JobObject'
```

**B-3（E2）行为复现（需两步，起工具服务）**

```powershell
cargo run -p qaqh-workspace -- serve --port 0 --token t
# 另开终端，经 /execute 发 3 次调用：
#  1) exec  {"command":"python -c \"print('A'*200000);print('NEEDLE');print('B'*200000)\""}   → 观察 truncated 与中段是否可读（D-1）
#  2) exec  {"command":"cat","background_after_secs":1}                                        → 拿 process_id（D-5）
#  3) process {"action":"write","id":N,"text":"hi"}                                            → WRITE_FAILED: has no PTY stdin（D-5）
#  4) process {"action":"check","id":N}                                                        → 只有 output_tail，无完整输出（D-5）
```

## 附录 C：续报修订锚点（2026-09-13 追加）

依 `TEMPLATE.md` §7「报告一经写入视为当时快照，后续修复不得回改历史结论，只能新增续报或标注」，
**本附录只做锚注，不改动正文任何结论**。修订依据：`docs/report/2026-09-13-codex-exec设计对照与修订-report.md`
（OpenAI Codex 源码横向对照，codex 侧证据全部为 E2 静态阅读）。

| # | 受影响位置 | 原结论 / 处方 | 修订后 | 证据（续报） |
|---|---|---|---|---|
| **R-1** | §1 D-1、§4 D-1.2/D-1.5、§6.2 R-1 | "外置通道（10 MiB）对 exec 不可达" = **设计缺陷** | **撤销定性**，降为改进项：Codex 同样两层截断、中段同样不可恢复、**exec 输出完全不落盘**，且默认给模型的预算（`Bytes(10_000)` = 10 KiB）**比我们更紧**。业界取舍是"有界 + 诚实上报"，不是"可回溯" | 续报 §1.1 D-1 行、§1.3、§5 R-1 |
| **R-2** | §4 D-1.5、§6.2 R-1、§7 S1 | **spill-first 属 P0** | **降为可选（S6）**：Codex 有 spill 能力（`hooks/src/output_spill.rs`）却不接 exec；前置条件是"确有不可重放的长输出场景"。替代处方 = 排空式采集 + 双计数（见 R-5） | 续报 §1.3、§5 R-2、§6 S3 |
| **R-3** | §4 D-6.5、§6.2 R-6、§7 S4 | 加 **进度合并器**（≥100 ms / ≥16 KiB 合并）+ `tool.progress` 改 ring，收益最高 | **处方撤销**：Codex 明确**不做服务端合并**，改用①单帧上限 8 KiB；②**每次调用帧预算 10 000 帧**（"只截实时流，不动聚合"）；③UI 侧有界预览（1 MiB → 头尾各 50 行，**每行再做头尾切分**）。合并会引入延迟且与"渲染器负责合并"的既有约定冲突 | 续报 §1.1 D-6 行、§5 R-3、§6 S4 |
| **R-4** | §1 D-7、§4 D-7.2、§6.2 R-4 | "终态保留 10 分钟 + 10 MiB/条" = **缺陷** | **改判为"驱逐策略不明确"**：Codex 同样**没有空闲超时**（只靠 kill-on-drop / 会话 shutdown / 每会话 64 上限 LRU：保护最近 8 个、优先驱逐已退出、尊重进行中的交互）。处方 = 明确 LRU 驱逐 + `Drop → terminate` | 续报 §1.1 D-7 行、§5 R-4、§6 S2 |
| **R-5** | §4 D-5.5、§6.2 R-7、§7 S2 | 新增 `process(action="read", offset, limit)` 暴露 `captured_full` | **处方简化**为 **drain-per-poll**：每次读取 `mem::take` 抽干缓冲，天然形成游标，**无需 offset 记账**；并补两条契约细节——超时统一 **exit code 124**、非 tty 时 `write` **只接受 Ctrl-C**（其余显式 `StdinClosed`） | 续报 §1.1 D-5 行、§5 R-5、§6 S3/S5、**§13（超时语义专节）** |

**R-1..R-5 之外仍成立的部分**（经 Codex 对照**加强**）：D-2（注册表无会话归属——Codex 的进程表是会话私有字段，属构造隔离）、D-3（`kill` 持锁——Codex 锁内只 `Arc::clone`、锁外终止）、D-4（pid 快照——Codex 用 JobObject，pid 终止仅作兜底）、D-8（子代理寄生）。

**新增缺陷候选（本报告 §4 未覆盖）**：

| ID | 内容 | 位置 | 级别 |
|---|---|---|---|
| **X-1** | **有损实时通道缺游标兜底**：Codex 的实时流同样会 `Lagged`，但权威缓冲**先写入再发通知**，消费者用 `read(after_seq)` 补齐；我方 `drain_bounded` 在工具结束后直接丢弃残留事件，`hub::fanout` 无补齐路径，且文档声称的"4 KB 尾化协议"**在实现中不存在** | `qaqh-runtime/src/agent/engine_tool.rs:786-788,923-931`、`qaqh-runtime/src/ringing/hub.rs:669-682` | P1 |

**遗留决策（待外部终审）**：本报告 §7 的落地路线已作废，请以续报 §6 的阶段顺序为准；`docs/plan/` 暂不拆分
（2026-09-13 用户决定：先由 GPT/Claude 终审，终审后再拆 plan）。
