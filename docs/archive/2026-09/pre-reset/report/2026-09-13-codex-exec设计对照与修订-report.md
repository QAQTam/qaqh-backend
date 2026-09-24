# codex exec 设计对照与修订建议（2026-09-13）

## 0. 元信息

| 项 | 值 |
|---|---|
| 报告日期 | 2026-09-13 00:46 (+08:00) |
| 分析对象 A | `D:\project\codex-main`（OpenAI Codex Rust monorepo 快照，Rust workspace 在 `codex-rs/`）。**快照无 `.git`**，`codex-rs/Cargo.toml` 为 `version = "0.0.0"` / `edition = "2024"`，故本报告**无法给出 codex 侧 commit**，行号以该快照文件内容为准 |
| 分析对象 B | `D:\project\QAQ-Harness` @ `e61efe0`（对照基准 = 同日评审报告） |
| 触发方式 | 用户提供 codex 源码快照并询问"OpenAI 是怎么做 exec/shell 或类似工具的" |
| 执行者 | 本会话（主读者）+ 4 路并行 subagent（分别覆盖 unified_exec 核心、PTY/containment、exec-server、UI/截断/安全） |
| 关联 | 续报：`docs/report/2026-09-12-exec与process工具设计评审-report.md`（下称"评审报告"，其结论用 D-1..D-8 / V-1..V-5 编号）；`docs/archive/2026-09/report/2026-09-12-exec输出静默截断与引入点考证-report.md` |
| 结论 | **Codex 验证了我方 4 条诊断（D-2/D-3/D-4/D-5），证伪了 2 条处方（D-1 的 spill-first、D-6 的"加合并器"），并暴露出我方 1 处比 Codex 更差的地方（有损通道无游标兜底）。真正该抄的是"按会话拥有进程表 + 帧预算 + 排空式读取 + kill-on-drop"，不是"落盘"。** |

> 证据等级：本报告 codex 侧结论**全部为 E2（代码实证）**，由逐行阅读 + 四个 subagent 交叉核对得到，**未编译、未运行任何 codex 测试**。所有"Codex 会/不会"均为静态结论。QAQ 侧行号同前一份报告（`e61efe0`）。

---

## 1. 结论摘要

### 1.1 逐条对照

| ID | 我方原结论（评审报告 §4） | Codex 的实际做法 | 判定 |
|---|---|---|---|
| **D-1** | 四层限额互不知情、外置通道（10 MiB）对 exec 不可达，中段永久丢失 = 设计缺陷；处方 = spill-first | 同样是**两层截断**（采集 1 MiB head/tail → 模型 `Bytes(10_000)` head/tail），**中段同样永久不可恢复，无落盘、无分页、无 offset 接口**。但：① 两层损耗**分别上报**（`output_omitted_bytes` + `original_token_count`）；② 标记带原始 token 数与总行数；③ **后台进程可反复 drain 拉取新输出**；④ spill 能力存在但只接在 hook 上（`hooks/src/output_spill.rs`），**没接 exec** | **处方被证伪，诊断被强化**：外置不可达不是 QAQ 独有缺陷，而是"双方都接受的折中"。真正该改的是**用一个布尔 `truncated` 表达两种不同丢失**，以及**后台输出不可拉取**。spill 从 P0 降为可选 |
| **D-2** | 全局注册表无 owner → 跨会话可查可杀（安全边界） | 进程表是 `SessionServices.unified_exec_manager`（**会话私有字段**，`state/service.rs:51`）→ 按构造隔离，无跨会话命名空间；生产 id **随机** `1000..100000`（`process_manager.rs:462`）防猜；`terminate_process` 用 `Arc::ptr_eq` 做世代校验防 id 复用（`:1735`） | **完全验证，且形态更好**：不是"给全局表加 owner 列"，而是"**让表属于会话**"。随机 id 与世代校验可直接抄 |
| **D-3** | `kill` 在全局锁内跑 `taskkill` + `wait`（实测 108.7 ms 全局停摆） | `terminate_process` 锁内只 `Arc::clone(entry.process)`，锁外 `terminate_confirmed().await`（`process_manager.rs:1717-1749`） | **完全验证**，Codex 就是我一直建议的写法 |
| **D-4** | 靠 pid 快照 `taskkill /pid`，reap 后 pid 复用可误杀；无 JobObject | Windows：`JobObject` + `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE \| JOB_OBJECT_LIMIT_BREAKAWAY_OK`（`utils/pty/src/win/job.rs:52`）+ `spawn_contained`/`assign_and_resume_process`；Unix：`kill_process_group`（`utils/pty/src/pipe.rs:65-89`）；JobObject 分配失败才退化单 pid `TerminateProcess`（`:102`） | **完全验证**。且 `qaqh-lsp`/`qaqh-mcp` 已在用 `process_wrap::JobObject`，属"抄自己家" |
| **D-5** | `write` 死功能；无 `read`/`list`；`timed_out` 名不副实 | `write_stdin(session_id, chars)` 是真 stdin，但**非 tty 时只接受 Ctrl-C（`\u{3}`）→ 进程组中断，其它输入报 `StdinClosed`**（`process_manager.rs:922-929`，`INTERRUPT` 常量 `:106`）；tty 写入后固定 sleep 100 ms 再轮询（`:934`）；有 `list_processes`（`:1698-1715`）；one-shot 超时**统一报 exit code 124**（`process.rs:203-206`）；交互路径 yield 到期**不杀进程**并返回 session_id 续读 | **验证 + 给出更细的契约**。"124" 与 "非 tty 只许 Ctrl-C" 两条可直接抄；`write` 要么上 PTY 要么学它明确拒绝。**超时语义的完整对照见 §13** |
| **D-6** | 每 chunk 一帧 + `tool.progress` 无界 = 前端变慢；处方 = 加合并器 + progress 改 ring | **服务端也不合并**：单帧 ≤ `UNIFIED_EXEC_OUTPUT_DELTA_MAX_BYTES = 8192`（`async_watcher.rs:42`），且**每次调用最多 `MAX_EXEC_OUTPUT_DELTAS_PER_CALL = 10_000` 帧**（`core/src/exec.rs:83`），注释明说 "Aggregation still collects full output; only the live event stream is capped"；UI 侧自带 `LiveCommandOutput`（1 MiB → 头 50 行 + 尾 50 行，**每行再做头尾切分**，`tui/src/exec_cell/live_output.rs:5-11`） | **处方被证伪**：Codex 用"**帧预算 + UI 侧有界预览**"而不是合并器。S4 应改为这条（成本更低、不引入延迟） |
| **D-7** | 条目可达 10 MiB 且终态保留 10 分钟；daemon 退出不清理 | 回收四路：`Drop for UnifiedExecProcess { terminate() }`（kill-on-drop，`process.rs:648-651`）、会话 shutdown、`CleanBackgroundTerminals`、**每会话 64 上限 LRU 驱逐**（保护最近 8 个、优先驱逐已退出、尊重 `interaction_lock`、驱逐即 terminate，`process_manager.rs:1607-1678`）；**没有空闲超时**；exec-server 侧另有 30 s detach TTL | **验证 + 修正我的一处口径**：Codex 同样没有空闲超时——关键不是"多久过期"，而是**驱逐策略明确 + Drop 收敛**。我原报告把"10 分钟惰性驱逐"当缺陷，应改判为"策略不明确" |
| **D-8** | 子代理寄生同一注册表，kill 是 300 ms 协作轮询 | Codex 的进程表只装进程；模型只能经 `write_stdin(session_id)` 寻址，宿主侧另有 `list_processes`/`terminate_all_processes` | **维持我方判定**（无对应物可比，但"表里只放一种东西"这条被间接支持） |

### 1.2 新增：我方没有、值得抄的（N-1..N-12）

| # | 机制 | 位置 | 抄的成本 |
|---|---|---|---|
| N-1 | **进程表属会话**（不是全局表 + owner 列） | `core/src/state/service.rs:51`、`unified_exec/mod.rs:150-153` | 中（注册表结构改动，但一次到位） |
| N-2 | 生产 id **随机** `1000..100000` + `reserved_process_ids` 防重 | `process_manager.rs:447-472` | 极小 |
| N-3 | `Arc::ptr_eq` 世代校验：终止前确认"还是那个进程" | `process_manager.rs:1735` | 极小 |
| N-4 | Windows `JobObject` kill-on-close + `Drop → terminate()` | `utils/pty/src/win/job.rs:52`、`process.rs:648` | 中（仓库已有先例） |
| N-5 | **读任务与采集者解耦**：collector 有 deadline，reader 永不因 collector 放弃而退出；kill 时 `abort()` 读任务 | `process_manager.rs:1478-1568`、`utils/pty/src/process.rs:219-270` | 中 |
| N-6 | **排空式读取（drain-per-poll）**：`std::mem::take` 整体抽干缓冲，合并本次 head/tail，`omitted_bytes` 累加 | `process_manager.rs:1506-1513,1560` | 小（我们已有 `captured_full`，改语义即可） |
| N-7 | **帧预算**：单帧 ≤8 KiB + 每调用 10000 帧上限，且"只截实时流、不动聚合" | `async_watcher.rs:42`、`core/src/exec.rs:83` | 小（runtime 侧） |
| N-8 | UI 有界预览：1 MiB → 头尾各 50 行 → **每行再头尾切分**（防无换行输出撑爆） | `tui/src/exec_cell/live_output.rs:5-11,13-17` | 小（前端/时间线侧） |
| N-9 | 超时统一 **exit code 124**（`timeout(1)` 惯例） | `process.rs:203-206` | 极小 |
| N-10 | **`Lagged` 用游标重读兜底**：`read(after_seq, wait_ms=0)` 补齐缺口，而非丢弃了事 | `process.rs:487-526`、`client_recovery.rs:472-520` | 小-中 |
| N-11 | 采集策略**按调用方**给定（`ExecCapturePolicy::{ShellTool 有界, FullBuffer 无界, SensitiveFullBuffer}`） | `core/src/exec.rs:269-287` | 小 |
| N-12 | **暂停即延长 deadline**（用户在答 elicitation 时 yield 计时暂停） | `process_manager.rs:1496-1501,1571-1605` | 小 |
| N-13 | Unix 侧 **`PR_SET_PDEATHSIG(SIGTERM)` + 父 pid 复检**（防 fork/exec 竞态），让"宿主死亡 → 子进程收敛"变成内核行为 | `utils/pty/src/pipe.rs:160-168`、`process_group.rs:27-39` | 小（仅 Unix；我们是 Windows 为主，见 §12.2） |
| N-14 | **孤儿释放策略显式化**：正常退出时 `preserve_descendants()` 主动摘掉 kill-on-close，只留 `BREAKAWAY_OK`，让后台孙进程按设计存活 | `utils/pty/src/win/job.rs:193-205`、`utils/pty/src/pipe.rs:280-300` | 小（与评审报告提的 `orphan_policy` 同构，此处有参考实现） |
| N-15 | **原子入作业**：ConPTY 路径经 `PROC_THREAD_ATTRIBUTE_JOB_LIST` 在 `CreateProcessW` 时入 job，失败即拒绝 spawn；通用路径则 `CREATE_SUSPENDED` → `AssignProcessToJobObject` → `NtResumeProcess` | `utils/pty/src/win/procthreadattr.rs:84-95`、`win/psuedocon.rs:177`、`win/job.rs:125-127,169-185` | 中 |
| N-16 | **yield 窗口与 timeout 分离**：默认形态只给"产出窗口"（不杀、返回 session_id 可续读），只有 one-shot/legacy/app-server 形态才"到点杀树 + 124 + 明确不可续"；`write_stdin` 的轮询窗口另有独立上界 | §13.1、§13.5 | 小（契约层） |

### 1.3 我方比 Codex 差的一处（新增缺陷候选）

| ID | 内容 | 证据 |
|---|---|---|
| **X-1** | **有损实时通道缺游标兜底**。Codex 的 UI 事件走 `broadcast`，`Lagged` 时**客户端/消费者用 `read(after_seq)` 从服务端权威缓冲补齐**（`core/src/unified_exec/process.rs:487-526`），且服务端"**先入缓冲再发通知**"（`exec-server/src/local_process.rs:985-1020`）。我方 `drain_bounded` 在 `RecvTimeoutError::Timeout` + `tool_done()` 后**直接丢弃残留事件**（`qaqh-runtime/src/agent/engine_tool.rs:923-931`），`hub::fanout` 用容量 1024 的 `broadcast` 且无补齐路径（`ringing/hub.rs:669-682`），而 `engine_tool.rs:786-788` 文档声称的"4 KB 尾化协议"**在实现中不存在**。等价于：Codex 丢帧→重读真相；我方丢帧→指望不存在的协议自愈 | E2（双方 `file:line` 均已核对） |

---

## 2. 分析方法与证据链

1. **侦察**：`codex-rs` 目录树 → 锁定 `core/src/unified_exec/`（16 文件）、`utils/pty/`（含 `win/job.rs`）、`exec-server/`（含 `exec-server-protocol`）、`shell-command/`、`tui/src/exec_cell/`。
2. **主读者逐行读核心**（E2，本人核对）：`unified_exec/{mod,process,head_tail_buffer,async_watcher,oneshot}.rs`、`process_manager.rs` 关键段（id 分配/剪枝/kill/drain）、`tools/context.rs`（模型可见契约）、`utils/output-truncation`、`utils/string/src/truncate.rs`、`tools/handlers/shell_spec.rs`、`write_stdin.rs`、`win/job.rs` 标志位、`core/src/exec.rs` 常量段、`tui/src/exec_cell/live_output.rs`、`protocol/src/openai_models.rs:960`。
3. **四路并行 subagent**（各自出 file:line 报告）：
   - A：unified_exec 核心（身份/生命周期、采集、模型契约、yield、无 EOF 存活、异步模型）
   - B：PTY 与 containment（**进行中，截止成稿未回执**；其覆盖面已由 A 的补充段 + 本人核对 `win/job.rs` 覆盖，缺口见 §10）
   - C：exec-server 架构（客户端/服务端、会话注册表、恢复、输出传输、shell snapshot、RPC 面）
   - D：UI 流与截断策略、命令安全层
4. **交叉核对**：对"A 报告 + 本人阅读"重复覆盖的常量（1 MiB、8 KiB、10 000 帧、10 KiB、64、124、`\u{3}`）逐一亲自打开文件确认，避免单一来源误读。
5. **对照**：与评审报告 D-1..D-8 / V-1..V-5 逐条比对，区分"验证/证伪/口径修正"三类，并对被证伪的处方给出替代方案（§6）。

---

## 3. Codex 侧事实（阅读发现的前置）

### 3.1 分层与关键常量

```
模型/宿主
  └─ exec_command / write_stdin（工具，codex-rs/core/src/tools/handlers/）
       └─ UnifiedExecProcessManager（**SessionServices 的字段 → 每会话一张表**）
            ├─ ProcessStore { processes: HashMap<i32, ProcessEntry>, reserved_process_ids: HashSet<i32> }
            │    entry: call_id / cwd / tty / environment_id / permissions / session: Weak<Session> / last_used
            ├─ 启动：ToolOrchestrator（审批 → 选沙箱 → 起进程；沙箱拒绝则去沙箱重试一次）
            └─ UnifiedExecProcess { ProcessHandle::{Local(PTY), ExecServer(远程)}, output, state_tx: watch, ... }
                 ├─ 输出任务：broadcast(64) 收 chunk → HeadTailBuffer<1 MiB> 累加 → 再 broadcast 扇出
                 ├─ 采集者：collect_output_until_deadline → mem::take 抽干 → HeadTailBuffer 合并
                 └─ 流式：start_streaming_output → ExecCommandOutputDelta（≤8 KiB/帧，≤10 000 帧/调用）
```

| 常量 | 值 | 位置 |
|---|---|---|
| 采集缓冲上限 | 1 MiB（head/tail 各 50%） | `unified_exec/mod.rs:80`、`head_tail_buffer.rs:18-19` |
| 模型可见默认预算 | `Bytes(10_000)`（10 KiB，head/tail 各 50%） | `protocol/src/openai_models.rs:960`、`utils/string/src/truncate.rs:126-129` |
| 单帧上限 / 帧预算 | 8 KiB / 10 000 帧每次调用 | `async_watcher.rs:42`、`core/src/exec.rs:83` |
| 每会话进程上限 | 64（LRU 驱逐） | `unified_exec/mod.rs:82` |
| yield 区间 | 250–30 000 ms（默认 10 000；**Windows 下限抬到 10 000**） | `unified_exec/mod.rs:73-77,210-217` |
| 空轮询等待上限 | 5 000–300 000 ms（默认 300 000） | `mod.rs:76,78` |
| 退出后宽限 | 采集 50 ms（`POST_EXIT_CLOSE_WAIT_CAP`）/ 流式 100 ms（`TRAILING_OUTPUT_GRACE`）/ 早期退出判定 150 ms（`EARLY_EXIT_GRACE_PERIOD`） | `process_manager.rs:1483`、`async_watcher.rs:34`、`process.rs:39` |
| 内联 IO 排空上限（legacy 路径） | 2 000 ms（`IO_DRAIN_TIMEOUT_MS`） | `core/src/exec.rs:92` |
| 进程 id 空间 | 随机 `1000..100000` | `process_manager.rs:462` |
| stdin 审批上限 / 中断字符 | 8 000 B / `\u{3}` | `process_manager.rs:105-106` |
| 超时退出码 | 124 | `process.rs:203-206` |

### 3.2 输出采集：两端截断 + 排空语义

- `HeadTailBuffer<MAX_BYTES>`：`HEAD_BUDGET = MAX/2`、`TAIL_BUDGET = MAX - HEAD`；先填 head，溢出进 tail，tail 满则丢最旧并累加 `omitted_bytes`；`to_bytes_with_omission_marker()` 在中间插 `... {N} bytes omitted ...`（`head_tail_buffer.rs:18-19,45-91,106-124`）。
- **采集者每次读取抽干缓冲**：`drained_output = std::mem::take(&mut *guard)`（`process_manager.rs:1507`），再用 `collected.push_buffer(drained_output)` 合并（`:1560`）。这意味着**每次 `exec_command`/`write_stdin` 拿到的都是"上次以来"的新输出（head+tail）**，而不是全量快照。
- 第二份 `transcript`（同 1 MiB）专供终态 `ExecCommandEnd` 事件（`async_watcher.rs:466-476`）。
- stdout/stderr **合流为单条流**（到达序），模型结果里 stderr 字段恒空（A 报告 + `utils/pty/src/process.rs:320-352`）。
- **无任何文件落盘**（`unified_exec/` 内无文件写；exec-server 亦只在内存保留 1 MiB / 50 000 块环形缓冲）。仓库唯一的 spill 是 hook 输出（~2 500 token 阈值 → 写文件 + 上下文留 path+preview，`hooks/src/output_spill.rs:59-90`）。

### 3.3 模型可见契约

`ExecCommandToolOutput`（`tools/context.rs:356-370`）字段：`raw_output`（未截断字节）、`truncation_policy`、`max_output_tokens`、`process_id`、`exit_code`、`original_token_count`、**`output_omitted_bytes: Option<NonZeroUsize>`**。最终文本 = header + 截断后正文（`response_text()`，`:526-551`）：

```
Chunk ID: ...
Wall time: 0.1234 seconds
Process exited with code 0        （或 Process running with session ID N）
Original token count: 12345
Output:
Warning: truncated output (original token count: 12345)
... 524288 bytes omitted ...
<head 50%>…N tokens truncated…<tail 50%>
```

策略取紧：`min(Tokens(max_output_tokens 默认 10 000), model.truncation_policy)`（`:458-465`），再按 20% 序列化余量循环收缩，避免历史层二次截断（`:526-551`）。

### 3.4 无 EOF 时的存活判定（与我方机制的正面回答）

- 读端**只以 EOF/err 结束**，绝不 poll；孙进程持写端时读任务永久 pending，但**不阻塞任何判定**：退出来自独立 `child.wait()` 任务（本地）或 exec-server `Exited { seq }` 事件（远程）。
- 采集循环 `collect_output_until_deadline`：有数据就继续搬；无数据时 `select!` {有输出 / 退出信号 / deadline / 暂停变更}；收到退出信号后最多再等 **50 ms** 即 break，**不等 `output_closed`**（`process_manager.rs:1495-1568`）。
- **kill 时 `abort()` 读任务**，所以杀进程永不因孙进程持管道而挂（`utils/pty/src/process.rs:219-270`）。

### 3.5 收敛与 containment

- Windows：`CreateJobObjectW` + `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK`（`win/job.rs:52`），`spawn_contained` 挂进程，JobObject 句柄关闭即整树终止；`terminate()` 显式终止；`preserve_descendants()` 临时允许 breakaway（`:193-205`）。分配失败才退化单 pid `TerminateProcess`（`pipe.rs:102`）。
- Unix：`kill_process_group`（macOS 带成员回退，`pipe.rs:65-89`）。
- 生命周期绑定在对象上：`Drop for UnifiedExecProcess { self.terminate(); }`（`process.rs:648-651`）。

### 3.6 exec-server（远端形态，与我方 `qaqh-workspace serve` 同题）

- 存在理由：让"进程 + 文件系统"能力落到**另一个进程或另一台机器**（可跨 OS），本地模式**不启 sidecar**（`exec-server/src/environment.rs:743-756`）。
- 隔离是**结构性**的：进程表是每会话 `HashMap<ProcessId, ProcessEntry>`（`local_process.rs:168`），跨会话表现为"unknown process id"；另有连接绑定校验（活跃会话被二次 attach 返回 `-32010`）。
- **掉线不杀进程**：`detach` 摘掉通知端并起 30 s 计时，到期才 `process.shutdown()`；客户端 25 s 内 `resumeSessionId` + `after_seq` 补读输出（`client_recovery.rs`）。
- 输出**先入缓冲再发通知**，`process/read(after_seq, max_bytes, wait_ms)` 可重读（`local_process.rs:544-623,985-1020`）。

---

## 4. 对我方 8 条诊断的逐条裁决（要点）

（详细对照见 §1.1；此处只记裁决与理由。）

- **D-1 修订**：保留"限额错位"的事实，**撤销"外置不可达 = 缺陷"的定性**——Codex 证明业界主流选择是"有界 + 诚实上报"，而非"外置可回溯"。改为两条新问题：`truncated` 布尔混淆两种丢失（我方唯一，Codex 用两个计数字段）；后台输出不可拉取（我方唯一，Codex 用 drain 解决）。
- **D-2 升级**：从"加 owner 校验"升级为"**让表属于会话**"；补两条加固（随机 id、`Arc::ptr_eq`）。
- **D-3 确认**：处方不变。
- **D-4 确认**：处方不变；并确认"JobObject 分配失败才退化 pid 终止"这种**分层兜底**写法（我方应至少做到"终态条目不再按 pid 终止"）。
- **D-5 收窄**：`write` 的正确形态是"要么 PTY，要么对非 tty 输入显式拒绝并说明只接受 Ctrl-C"；超时改用 124；`list` 应补。
- **D-6 改处方**：删掉"合并器"（Codex 明确不做合并，且合并会引入额外延迟），改为 **单帧上限 + 每调用帧预算 + UI 侧有界预览**（N-7 + N-8）。
- **D-7 修口径**：删掉"10 分钟保留是缺陷"；改为"**驱逐策略要明确**（64 上限 + 保护最近 8 + 优先已退出 + 尊重进行中交互）+ **Drop 收敛**"。Codex 同样无空闲超时。
- **D-8 维持**：无对应物；可借鉴的只有"一张表只放一种东西"。

---

## 5. 被证伪 / 需要修订的我方结论（诚实清单）

| # | 我原来的说法 | 事实 | 修订 |
|---|---|---|---|
| R-1 | "外置通道对 exec 不可达"是**设计缺陷** | Codex 无外置通道，且默认给模型的量（10 KiB）**比我们还少 4 倍**（我们 10 K token ≈ 40 KB）。业界把"有界 + 报明损耗"视为可接受折中 | 降级为"**改进项**"：不是缺陷，是我方与业界取舍不同。若坚持可回溯，那是**超出业界**的产品选择，须自证收益 |
| R-2 | "spill-first 应在 P0" | Codex 有能力（hook spill）却不接 exec | 降为 **S6 可选**，前置条件是"确有不可重放的长输出场景" |
| R-3 | "加进度合并器收益最高" | Codex 不做合并，改用帧预算 + UI 有界预览，且注释写明理由（聚合不失真，只截实时流） | 处方替换为 N-7 + N-8；**不引入时间窗合并**（会把 `paced_emitter` 的"渲染器负责合并"约定搞成双头） |
| R-4 | "终态保留 10 分钟 + 10 MiB/条"是缺陷 | Codex 同样无空闲超时（只靠 Drop / shutdown / 64 上限 LRU） | 改判为"**策略不明确**"，处方 = 抄 N-4 + 明确 LRU 驱逐 |
| R-5 | "后台进程完整输出读不到"（D-5） | 成立，但**正解不是 `read(offset)` 而是 drain**：Codex 每次读取抽干缓冲，天然形成游标，无需 offset 记账 | 处方简化为 N-6；超时契约细节（yield ≠ timeout、124、不可续）见 **§13** |

---

## 6. 修订后的落地路线（替换评审报告 §7 的 S1–S6 顺序）

| 阶段 | 内容 | 覆盖 | 来源 |
|---|---|---|---|
| **S1** | **进程表会话私有化**：`ProcessRegistry` 从进程级 `static` 改为会话作用域（或退一步：全局表分片 `HashMap<SessionId, …>` + caller 校验）+ 随机 id + `Arc::ptr_eq` 世代校验 | D-2、D-8 | N-1/N-2/N-3 |
| **S2** | **终止与收敛**：`kill` 锁内取句柄、锁外终止；Windows JobObject(kill-on-close)（**原子路径 = `CREATE_SUSPENDED` + 分配作业**，接受"事后归属"的逃逸竞态）；`Drop → terminate`；**终态条目不再按 pid 终止**（最小加固，可先落地）；补 `list` | D-3、D-4、D-7 | N-4/N-14/N-15 |
| **S3** | **采集语义**：缓冲改为**排空式**（每次读取抽干）；`truncated` 拆成 `omitted_bytes`（采集层丢弃）+ `original_token_count`（模型层截断）两个字段；后台输出可反复拉取 | D-1、D-5 | N-6/N-10 |
| **S4** | **事件流预算**：单帧上限 + 每调用帧上限；`tool.progress` 加上界；UI 侧有界预览（头尾 N 行 + 每行头尾） | D-6 | N-7/N-8 |
| **S5** | **契约清理**：超时 exit code 124；`timeout_secs` → `handoff_after_secs`（+ 可选 `kill_after_secs`）；`write` 二选一（PTY 或显式拒绝 + 只允许 Ctrl-C）；`X-1` 的有损通道补齐游标（或实现文档承诺的尾化协议） | D-5、X-1 | N-9 |
| **S6**（可选） | spill 落盘（**超出业界**，需先论证场景） | — | R-2 |

> 与评审报告相比的净变化：**新增 S1 的"表属会话"形态**；S3 由"spill"改为"排空 + 双计数"；S4 由"合并器"改为"帧预算 + UI 有界预览"；S2 增加 kill-on-drop；`X-1` 入 S5。

---

## 7. 不建议抄的（含理由）

| 项 | 理由 |
|---|---|
| **PTY 双形态**（注意：Codex **默认不分配 PTY**） | `exec_command` 的 `tty` **缺省 false**（`tools/handlers/unified_exec.rs:34,70-72`），默认路径 = 管道 + `stdin=null`（`sandboxing/src/spawn.rs:131-139`、`utils/pty/src/pipe.rs:183-185`）——**与我们完全一致**。PTY 是模型显式 opt-in 的第二形态，代价：PTY 无独立 stderr（`utils/pty/src/pty.rs:190` 直接丢弃发送端）、需伪造终端应答（DSR/窗口查询，`sandboxing/src/spawn.rs:46-53`）、Windows 走 ConPTY blocking 读 + `WouldBlock` 5 ms 兜底、还要 `VEOF` 等终端语义。我们若要支持交互，应引入**双形态**而非"默认 PTY"；在无交互需求前不值得付这份复杂度 |
| exec-server 级分布式架构 | QAQ 是单机常驻 daemon；`codex` 的多传输（WebSocket/Noise/stdio）+ 25 s/30 s 恢复窗 + seq 游标是跨机需求驱动。我们只需其中一条：**权威缓冲 + 游标补齐**（N-10），不需要协议层 |
| Starlark 策略语言（`execpolicy`） | 我方的 permission level + 路径授权已覆盖；引入 DSL 需要新解析器、新配置面、新测试面。其"危险命令硬 deny 而非转审批"的思路值得单点借鉴 |
| Shell snapshot（bash/zsh 状态捕获还原） | **Unix-only**（非 Unix 直接报错，PowerShell/Cmd `unreachable!()`），且预算可观（解析 ≤512 KiB、捕获 ≤4 MiB、超时 10 s、缓存 16 条）。QAQ 以 Windows/pwsh 为主，收益/成本不匹配 |
| 默认 10 KiB 模型预算 | 比我方现值（10 K token ≈ 40 KB）更紧，直接抄会伤害可用性 |

---

## 8. 次要观察

| # | 位置 | 观察 | 级别 | 证据 |
|---|---|---|---|---|
| O-1 | `exec-server/README.md:171-172` vs `server/session_registry.rs:184-198` | 文档说"连接关闭即终止托管进程"，实现是保留 30 s 供 resume。**README 与代码不一致**（与 QAQ 文档漂移同病） | P3 | E2 |
| O-2 | `utils/pty/src/win/job.rs:52` | `JOB_OBJECT_LIMIT_BREAKAWAY_OK` 与 kill-on-close 并用，再由 `preserve_descendants()` 临时放行——**受控逃逸**设计，比"一刀切 kill-on-close"更精细 | 参考 | E2 |
| O-3 | `core/src/exec.rs:269-287` | `ExecCapturePolicy` 让"要不要限、限多少"成为**调用方参数**；我方是全局常量 + 单一策略 | P3（设计参考） | E2 |
| O-4 | `core/src/exec.rs:1209` | legacy 路径注释："Continue reading to EOF to avoid back-pressure" —— capped 之后**继续读到 EOF 以防反压**。经核对，我方 `pipe.rs:125-145` 在 `capped` 后同样继续 read（只是不再保留），**故我方无此缺陷**；此条仅作交叉验证记录，避免误报 | — | E2 |
| O-5 | `process_manager.rs:922-929` | 非 tty 只接受 Ctrl-C；`write_stdin` 的"写"在非交互场景其实退化为"发中断"——契约诚实 | 参考 | E2 |
| O-6 | A 报告未验证项 | `allocate_process_id` 在 id 空间用尽时为**无限循环**（`while` + `continue`，无重试上限），理论风险；实际 99 000 空间下不可达 | P3 | E2（推断可触发性） |

---

## 9. 不确定性与未验证假设

1. **codex 侧全部为静态阅读**：未编译、未运行其任何测试，未做任何运行时验证。凡是"Codex 会/不会"的表述都是代码层结论。
2. **快照无 `.git`**：无法给出 commit，也无法确认该快照与上游当前版本的关系（用户说明为"截至昨天下午的快照"）。
3. **sandbox 面未展开**：PTY/containment 专线已回执（见 §12），但其中的 ConPTY 细节、`windows-sandbox-rs`、`linux-sandbox`、`process-hardening` 只做了入口级阅读；`linux-sandbox/src/bwrap.rs`（107 KB）与 `windows-sandbox-rs/src/setup.rs`（93 KB）未读。
4. **"孙进程持写端 ⇒ 读任务永久 pending"** 属管道语义推断（A 报告亦标注为推断）：代码层只确认"读任务仅以 EOF/err 结束"与"kill 时 abort 读任务"。
5. **模型 `truncation_policy` 的实际取值随模型数据下发**：`Bytes(10_000)` 是仓库内默认之一，测试中出现过 12 000/8 000/200 等值（D 报告），故"Codex 默认给 10 KiB"应理解为"该快照下的默认模型配置"，不是不可变常量。
6. **`MAX_UNIFIED_EXEC_PROCESSES = 64` 是 core 侧自裁剪**，exec-server 服务端对每会话进程数**无上限**（C 报告）——故"Codex 每会话 64"仅在本地/unified 路径成立。
7. **未验证 app-server 传输层在大流量下是否有隐式批量 flush 或反压**（D 报告），因此"服务端无合并"严格指**事件映射与分发路径无显式合并**。
8. **未评估把 N-1（表属会话）移植到 QAQ 的改造成本**：我方 exec 与 subagent 共用一张表，且工具线程经 `crate::runtime::context()` 携带会话（评审报告 §6.2 已确认基础设施具备），但"子代理条目归属哪个会话"需产品决策。
9. **超时专节（§13）的两处未验证**：① 远程（exec-server）路径的 `terminate_confirmed()` 是否等待服务端进程真正死亡——只确认到 RPC 完成；服务端 `process/terminate` **返回 `running: bool`**，但未确认 unified 层是否检查该字段（`§13.4` 的"不保证已回收"结论因此**仅在本地路径被证实**）。② `POST_EXIT_CLOSE_WAIT_CAP = 50 ms` 在真实大流量下是否会裁掉尾帧，未实测。

---

## 10. 产物与复现物清单

| 路径 | 类型 | 是否落盘 | 说明 |
|---|---|---|---|
| `docs/report/2026-09-13-codex-exec设计对照与修订-report.md` | 本报告 | ✅ 已落盘 | 新建 |
| subagent 报告 A/C/D | 调研中间产物 | ❌ 仅在会话内 | 关键结论已带 `file:line` 摘入本报告 §3–§5、§12 |
| subagent B（PTY/containment） | 调研 | ✅ 已回执 | 结论见 §12；其中"Codex 默认不是 PTY"一条**修正了本报告初稿的表述** |
| `D:\project\codex-main` | 只读分析对象 | — | **未修改任何文件**（全部 subagent 均被要求只读） |
| QAQ-Harness 侧代码 | — | — | 本报告**未改动任何代码** |

---

## 11. 后续工作与建议排期

| 优先级 | 工作 | 说明 |
|---|---|---|
| **P0** | 把 §6 的 S1–S5 拆进 `docs/plan/`（替换评审报告 §7 的旧顺序） | **暂缓**（2026-09-13 用户决定：先做外部终审，终审后再拆）；顺序已按"实施成本 × 缺陷严重度"重排，S4 的处方已替换 |
| **P0** | ~~在评审报告上标注修订锚点~~ → **已完成**（2026-09-13）：评审报告已加续报指针、4 处章节锚注、4 行状态列注记与**附录 C（R-1..R-5 + X-1 + 遗留决策）**，正文未改 | 符合 `TEMPLATE.md` §7"不回改历史结论" |
| **P1** | `X-1`（有损通道缺游标兜底）——**这条在 Codex 对照中才显形**，建议单出一行 buglist | 与既有 401 报告的热路径族相关 |
| **P2** | 补齐 PTY/containment/sandbox 面的调研（ConPTY 细节、`windows-sandbox-rs`、`linux-sandbox`、`process-hardening`） | 若我们要做 JobObject 收敛（S2），这份细节有直接参考价值 |
| **P3** | 评估"危险命令硬 deny"（Codex 的 `is_dangerous_command` + `execpolicy`）是否值得在我方 permission 层借鉴 | 与 exec 设计正交，单独议题 |

---

## 12. 补录：PTY / containment 专线回执（subagent B）

### 12.1 一处初稿修正

本报告初稿在 §7 写作"PTY 默认化"，**该表述错误**：`exec_command` 的 `tty` 缺省为 **false**（`core/src/tools/handlers/unified_exec.rs:34,70-72`），默认路径是管道 + `stdin=null`（`sandboxing/src/spawn.rs:131-139`、`utils/pty/src/pipe.rs:183-185`），与 QAQ-Harness 的形态**一致**。选路由平台无关的单一分支决定（`spawn.rs:110-141`：`tty` → PTY；`stdin_open` → 管道留 stdin；否则管道 + null），而 unified exec 传入的 `stdin_open` 恒等于 `tty`（`process_manager.rs:1351-1352`）。

**因此对照关系比初稿更"同题"**：双方默认都是"管道 + 双流 + stdin 关闭"，差别在**谁为进程树的收敛负责**（内核作业对象 vs pid 快照）。这把 D-4 的严重性抬高了，而不是降低。

### 12.2 containment 细节（补 §3.5）

| 项 | Codex | 位置 |
|---|---|---|
| Windows 原子入作业 | ConPTY 经 `PROC_THREAD_ATTRIBUTE_JOB_LIST` 在 `CreateProcessW` 时入 job，**失败即拒绝 spawn** | `utils/pty/src/win/psuedocon.rs:177`、`win/procthreadattr.rs:84-95` |
| Windows 非原子入作业 | 通用路径 `CREATE_SUSPENDED` → `AssignProcessToJobObject` → `NtResumeProcess` | `win/job.rs:125-127,133-166,169-185` |
| **Codex 自认的竞态** | 管道路径注释原文：`Accept the small race: a descendant created between spawn and assignment is not guaranteed to join the job and can escape termination.` | `utils/pty/src/pipe.rs:195-197` |
| 正常退出释放 | 根进程退出时 `preserve_descendants()` 主动去掉 kill-on-close（只留 `BREAKAWAY_OK`），**故意**让后台孙进程存活 | `win/job.rs:193-205`、`win/mod.rs:68-92`、`pipe.rs:280-300` |
| Unix 宿主死亡兜底 | 管道路径 `PR_SET_PDEATHSIG(SIGTERM)` + `getppid()` 复检（防 fork/exec 竞态） | `utils/pty/src/pipe.rs:160-168`、`process_group.rs:27-39` |
| Unix 无句柄式终止 | 仓库内**无 `pidfd`**；PTY 路径把 pgid 视为 session-leader PID，注释承认**缓存的 PGID 可能过期** | `utils/pty/src/pty.rs:76-88,182-185` |
| pid 型 kill 的地位 | **仅当作业分配失败时兜底**（`OpenProcess`+`TerminateProcess`），老 shell 路径才 `getpgid`+`killpg` | `utils/pty/src/pipe.rs:80,91-107,207-215`、`process_group.rs:289-295` |

**结论**：Codex 也承认"事后归属"方案的固有缺陷（逃逸竞态），但它把 pid 型终止降级为**兜底**、把正确性放在创建期；我方目前是把兜底当唯一手段。这正好支持评审报告的处方，也说明了实施的优先项：**先做 `CREATE_SUSPENDED` + 作业分配（或接受"终态不再按 pid 终止"这一最小加固），而不是一步到位追求零竞态**。

### 12.3 `process-hardening` 的定位（避免误判我方缺口）

`process-hardening` 只加固 **Codex 自身/桥接进程**（`#[ctor::ctor]` pre-main），**不下发给被执行的命令**；调用点仅三处（`voice-host`、`responses-api-proxy`、`linux-sandbox` 的桥接进程），本检出主 CLI/TUI 二进制甚至未调用；**Windows 分支是空实现 TODO**（`process-hardening/src/lib.rs:119-122`）。故我方"没有 process-hardening"相对 Codex 的**命令执行路径**不构成差距（差距在子进程沙箱：landlock/seccomp/受限令牌，属另一个议题）。

### 12.4 工具面补录（补 §3.3）

- `exec_command`：`cmd`（唯一必填）、`workdir`、`tty`、`yield_time_ms`、`max_output_tokens`、`shell`、`login`、`environment_id`（条件出现）、`sandbox_permissions`/`justification`/`prefix_rule`/`additional_permissions`（`tools/handlers/shell_spec.rs:35-93,232-278`）。
- one-shot 形态（`UnifiedExec` feature 关闭时）：**清掉 `tty` 与 `yield_time_ms`，改为 `timeout_ms`**，结果里也不带 `session_id`（`tools/handlers/unified_exec/exec_command.rs:310-318,486-513`、`spec_plan.rs:1104-1112`）。即"**能否续读**"是形态级开关，不是运行时猜测。
- 工具有 description 说 "Runs a command in a PTY"（`shell_spec.rs:99,103`），与默认管道**不一致**——与 O-1 同类漂移。

### 12.5 新增次要观察

| # | 位置 | 观察 | 级别 | 证据 |
|---|---|---|---|---|
| O-7 | `utils/pty/src/pty.rs:190`（及 `:366`） | **PTY 会话没有独立 stderr**：发送端被立即丢弃，`stderr_rx` 恒空。我方 `combined = stderr + '\n' + stdout` 虽然破坏流身份，但至少两流都在；Codex 的 PTY 形态则彻底没有 stderr | 参考 | E2 |
| O-8 | `core/src/unified_exec/process_manager.rs:930`、`utils/pty/src/process.rs:163-173` | `write_stdin` 的写入是 `send().await` 到有界通道且**未包超时**：子进程不读时该调用可能长时间挂起（推断）。我方 `write` 是死功能，反而"不会挂" | P3 | E2（挂起为推断） |
| O-9 | `windows-sandbox-rs/src/unified_exec/` | Windows 沙箱后端把 stdout/stderr 经 IPC 单线程解帧分发的 `broadcast(256)`，并把管道异常**注入成 `runner error: ...` 文本**混入输出流 | 参考 | E2 |

---

## 13. 超时语义专节（2026-09-13 追加）

> 触发：用户追问"Codex 怎么处理命令超时"。本节是在同一分析对象上的一次定点补证，结论进入 R-5（契约清理）分支。

### 13.1 三条路径、两种语义（核心结论）

**Codex 默认形态下没有"命令超时"**——`exec_command` 的默认路径只有"产出窗口"（`yield_time_ms`），到点**不杀进程**，返回 `session_id` 供 `write_stdin` 续读。真正的"超时即杀"只出现在三种非默认/旁路形态里。

| 路径 | 参数 | 到期行为 | 位置 |
|---|---|---|---|
| **unified exec 交互**（默认；`UnifiedExec` 为 `Stage::Stable`） | `yield_time_ms`（默认 10 000，clamp 250–30 000；**Windows 下限抬到 10 000**） | **不杀**：仅停止采集，返回已产出 head/tail + `Process running with session ID N`，条目留在表内 | `unified_exec/mod.rs:73-77,210-217`；`process_manager.rs:605-630`；`features/src/lib.rs:945` |
| **unified exec one-shot**（feature 关闭时的形态） | `timeout_ms`（默认 `DEFAULT_EXEC_COMMAND_TIMEOUT_MS = 10_000`） | **杀整棵树** → 退出码 **124**、`process_id = None`（不可续）；**超时前已产出的输出照样返回** | `oneshot.rs:77-82`；`process_manager.rs:622-630`；`core/src/exec.rs:61` |
| **legacy `shell` 工具**（非 unified 经典路径） | `ExecExpiration::{Timeout, DefaultTimeout, Cancellation, TimeoutOrCancellation}` | **立即硬杀**进程组 → 合成假信号 → 归一化为 **124** + `timed_out = true` | `core/src/exec.rs:994-1009,749-792` |
| **app-server `command/exec`（JSON-RPC）** | `timeoutMs`：有值 → 该值；**`null` → 不超时（仅可取消）**；缺省 → 10 000 | `session.request_terminate()` → 退出后 `break 124`；另有 2 s IO 排空兜底 | `app-server/src/request_processors/process_exec_processor.rs:112-122`；`app-server/src/command_exec.rs:534-544,548-551` |

### 13.2 one-shot 超时的代码链（E2）

```rust
// core/src/unified_exec/process_manager.rs:609-630
let wait = completion.as_ref().map_or_else(
    || Duration::from_millis(yield_time_ms),   // 交互：产出窗口
    |completion| completion.timeout,            // one-shot：真超时
);
let collected_output = Self::collect_output_until_deadline(..., deadline).await;
if let Some(completion) = completion.as_mut() && !process.has_exited() {
    completion.timed_out = true;
    process.mark_timed_out();                   // → exit_code() 此后恒返回 124
    if let Err(err) = process.terminate_confirmed().await { ... }
}
```

`mark_timed_out()` 使 `exit_code()` 直接返回 `Some(124)`（`unified_exec/process.rs:203-206`）；`terminate_confirmed()` 再经 `signal_exit(Some(124))` 把状态强制置为已退出 → `refresh_process_state` 随即把条目从表内移除并返回 `Exited{124}`（`process_manager.rs:1067-1091`）。模型最终拿到：**exit 124 + 无 session_id + 超时前输出**。

### 13.3 同一个 124，两套实现机制

| 路径 | 机制 |
|---|---|
| unified | `exit_code()` 内硬编码 `Some(124)`（`unified_exec/process.rs:204`） |
| legacy | 先造**合成假信号** `synthetic_exit_status(EXIT_CODE_SIGNAL_BASE + TIMEOUT_CODE)` = `from_raw(192)`（Unix 上表现为"被信号 64 杀死"），再由 `finalize_exec_result` 把 `signal == TIMEOUT_CODE(64)` 翻译成"超时"并把退出码改写为 `124`（`core/src/exec.rs:762-774`）。**其它任何信号死亡**（如 SIGKILL=9 → `from_raw(137)`）不走这条路，而是被当成 `SandboxErr::Signal` 报错 |

常量：`TIMEOUT_CODE = 64`、`EXIT_CODE_SIGNAL_BASE = 128`、`SIGKILL_CODE = 9`、`EXEC_TIMEOUT_EXIT_CODE = 124`（`core/src/exec.rs:65-68`）。

### 13.4 两个反直觉点

1. **超时比取消更粗暴**：超时 = 立即 `kill_child_process_group` + `start_kill`，**无宽限**（`exec.rs:1002-1009`）；而**取消**反而先 `terminate_process_group`（TERM），给 `CANCELLATION_TERMINATION_GRACE_PERIOD = 50 ms` 让进程清理，再按需升级到 KILL（`exec.rs:1010-1040`）。直觉上应相反。
2. **超时响应不保证进程真的没了**：`terminate_confirmed()` 的 "confirmed" 指"终止请求已送达/已执行"（远程分支 `await` 完成），**不是**"进程树已回收"；本地分支只是一次 `killpg` / `TerminateJobObject`（`utils/pty/src/process.rs:221,246`），不等待 reap，随后立刻 `signal_exit` 标记为已退出。故超时返回时 OS 侧进程可能仍在退出中——这是"响应性优先于清理确认"的有意取舍。

### 13.5 没有超时的地方（同样重要）

- **交互会话没有空闲超时**。`background_terminal_max_timeout`（默认 300 000）**只**用于空 `write_stdin` 轮询的等待上限，不是进程存活上限：其全部使用点只有 `config → session.rs:1536-1538 → UnifiedExecProcessManager::new()`，再用于 `time_ms.clamp(MIN_EMPTY_YIELD_TIME_MS, max)`（`process_manager.rs:952-961`）。会话可一直活到进程自行退出、被 kill、被 64 上限 LRU 剪枝或会话结束。
- **`write_stdin` 到期不杀进程**：空轮询 clamp `[5 000, 300 000]`，非空写 `≤30 000`，到期只是结束采集窗口（`process_manager.rs:952-965`）。
- 退出后宽限：采集 **50 ms**（`POST_EXIT_CLOSE_WAIT_CAP`）、流式补尾 **100 ms**（`TRAILING_OUTPUT_GRACE`）；legacy 与 app-server 的 IO 排空兜底 **2 000 ms**（`IO_DRAIN_TIMEOUT_MS`）。

### 13.6 对我方的影响（D-5 ③ 的机制细化）

对照双方对"超时不该让命令白跑"这一点其实**都对**，差别在兜底手段：

| | 到点行为 | 兜底 | 到点后的可续读性 |
|---|---|---|---|
| Codex 交互（默认） | 不杀 | **可续读**（`write_stdin(session_id)`） | ✅ 完整 |
| Codex one-shot / legacy / app-server | 杀树 | 明确契约（124 + 警告头 + 部分输出） | ❌ 不可续（如实告知） |
| **QAQ `exec`** | **不杀**（转后台） | 仅 `process check` 的 5 000 字符 tail | ❌ **不可续且不告知** |

**这把我方 D-5 ③ 的后果说得更准**：`timeout_secs` 到期走 `status:"backgrounded"` + `timed_out:true`，而 `success` 判定为 `None => !timed_out && !cancelled`（`exec/handler.rs:291-295`）→ `success = false` → `ToolResult::error(json)`（`:308-310`）。即：**一次成功的"长驻进程移交"会被当成工具执行失败呈现给模型**，而 Codex 的默认形态根本不会产生这种状态（它不杀、也不报失败；真要超时则返回带输出与 124 的正常结果，legacy 才升级为携带 output 的 `SandboxErr::Timeout`）。

> 归属：本条是 D-5 ③ 的**机制细化**，不是新缺陷，故**不进 buglist**；处方并入 R-5（契约清理：`timeout_secs` → `handoff_after_secs`，`timed_out` → `handed_off`，并以 124 表达真超时）。

---

## 附录 A：环境快照

| 项 | 值 |
|---|---|
| 本机 | Windows 11（build 26300）、pwsh 7.6.6 |
| 报告时间 | 2026-09-13 00:46 +08:00 |
| codex 快照 | `D:\project\codex-main`，无 `.git`，`codex-rs` workspace `version = "0.0.0"` / `edition = "2024"`；含 `exec-server`、`windows-sandbox-rs`、`linux-sandbox`、`utils/pty`、`app-server` 等约 120 个 crate |
| QAQ 基准 | `D:\project\QAQ-Harness` @ `e61efe0`（2026-09-12 22:02:26 +0800） |
| 方法 | 4 路后台 subagent 并行 + 主读者逐行核对关键常量；**无编译、无运行** |

## 附录 B：关键 `file:line` 索引（便于复核；路径相对 `D:\project\codex-main\codex-rs`）

```text
# 身份 / 生命周期 / 上限
core/src/state/service.rs:46-51                     SessionServices（unified_exec_manager 属会话）
core/src/unified_exec/mod.rs:82                     MAX_UNIFIED_EXEC_PROCESSES = 64
core/src/unified_exec/mod.rs:150-197                ProcessStore / ProcessEntry（session: Weak<Session>）
core/src/unified_exec/process_manager.rs:447-472    随机 id 1000..100000 + reserved 防重
core/src/unified_exec/process_manager.rs:1607-1678  LRU 剪枝（保护最近 8 / 优先 exited / 尊重 interaction_lock）
core/src/unified_exec/process_manager.rs:1680-1749  terminate_all / list / terminate(+Arc::ptr_eq)
core/src/unified_exec/process.rs:648-651            Drop → terminate（kill-on-drop）

# 采集 / 截断 / 模型契约
core/src/unified_exec/mod.rs:79-81,219-225          UNIFIED_EXEC_OUTPUT_MAX_BYTES=1MiB / omission marker
core/src/unified_exec/head_tail_buffer.rs:18-19     HEAD=MAX/2, TAIL=MAX-HEAD
core/src/unified_exec/head_tail_buffer.rs:106-124   to_bytes_with_omission_marker
core/src/unified_exec/process_manager.rs:1506-1513  mem::take 抽干（drain-per-poll）
core/src/tools/context.rs:356-370                   ExecCommandToolOutput 字段
core/src/tools/context.rs:458-465,471-498,526-551   min(策略) / 标记拼接 / 20% 余量循环收缩
core/src/tools/context.rs:500-524                   response_header（exit code / session ID / original token count）
protocol/src/openai_models.rs:960                   truncation_policy = bytes(10_000)
utils/string/src/truncate.rs:126-137                split_budget 50/50 + 标记文案
utils/pty/src/lib.rs:14                             DEFAULT_OUTPUT_BYTES_CAP = 1 MiB

# 事件流预算 / UI
core/src/unified_exec/async_watcher.rs:34,42        TRAILING_OUTPUT_GRACE=100ms / 单帧 8192 B
core/src/exec.rs:81-83                              MAX_EXEC_OUTPUT_DELTAS_PER_CALL = 10_000
tui/src/exec_cell/live_output.rs:5-11               UI 有界预览（1 MiB / 50 行 / 每行再头尾）

# 无 EOF / 收敛
core/src/unified_exec/process_manager.rs:1478-1568  collect_output_until_deadline（50ms post-exit cap）
utils/pty/src/process.rs:219-270                    terminate 时 abort 读任务
utils/pty/src/win/job.rs:43-64,115,208              JobObject + KILL_ON_JOB_CLOSE
utils/pty/src/pipe.rs:65-89,102,190-216             进程组终止 / 单 pid 兜底
core/src/exec.rs:86-92                              IO_DRAIN_TIMEOUT_MS = 2000（孙进程持管道注释）

# exec-server（远端形态）
exec-server/src/environment.rs:743-756              本地/远端后端选择
exec-server-protocol/src/protocol.rs:284-286        ProcessId = "scoped to this connection/session"
exec-server/src/server/session_registry.rs:18-46,184-198  detach + 30s TTL
exec-server/src/local_process.rs:85-88,985-1020     保留 1 MiB/50000 块 + 先入缓冲再发通知
core/src/unified_exec/process.rs:487-526            Lagged → read(after_seq) 补齐

# shell snapshot / 安全
exec-server/src/shell_snapshot.rs:239-252           状态还原（-pc / -fc + carrier 分片）
shell-command/src/command_safety/is_dangerous_command.rs:34,54-75   deny 清单 + 递归深度 8
execpolicy/src/decision.rs:9-16                     Decision::{Allow,Prompt,Forbidden}
```
