# QAQ-Harness 全局锁、死锁风险与 Tokio 阻塞边界专项排查 Prompt

> 适用对象：后续接手排查的 Agent（Antigravity / Codex / Claude Code 等）
> 配套基线文档：`AGENTS.md`、`docs/v4.3-4.8/MUST.md`
> 背景事件：`PR #27`（`c50189f0` / merge `a1e5a9b4`）修复了 `crates/qaqh-sandbox/src/preflight.rs` 与 `capability.rs` 的同线程自死锁：`start()`/`run_blocking()` 持有 `PREFLIGHT_STATE` `Mutex` 期间调用 `SandboxCapabilities::detect()`，后者回调 `cached_write_isolation_verified()` 再次对 `PREFLIGHT_STATE` 加锁，直接卡死 `qaqh-daemon` 控制面 Tokio worker，导致前端启动检测超时并全空。

---

## 请直接发送给排查 Agent 的 Prompt 正文

```markdown
你现在负责对 `D:\Projects\qaqh-backend`（`main` 基线）做一次**只读深度审计**（先不出手改业务代码，最后只产出结构化排查报告与优先级建议）。

### 一、遵守纪律与工具使用要求
1. 严格遵守仓库根目录 `AGENTS.md` 与 `docs/v4.3-4.8/MUST.md`：
   - 若根目录存在 `.codegraph/`，优先使用 `codegraph explore` / `codegraph_explore` 追踪跨模块调用链与动态派发路径，再辅以 `rg` 与按行范围读取。
   - **严禁臆测或脑补**未实际读取的源码行号、锁顺序或函数行为；报告中的每条结论必须给出精确的 `file:line`、符号名与实际观察到的调用路径。
   - **禁止提出“一刀切直接删锁”的破坏性建议**：必须先判断该锁守卫什么不变量（例如哈希链/账本防并发撕裂、后台 PID 生命期、共享映射表），明确区分“必须保留锁但缩小持锁边界”与“必须先实例所有权化/Actor 化再拆锁”。
2. 过滤掉无害项，避免噪音：
   - 纯一次性只读初始化缓存（例如 `OS_INFO`、`TOOLS_INFO`、`SYSTEM_CMD_PATH`、`RG_HABIT_RE` 等无内部可变状态的 `OnceLock<T>`）**不列为风险**，仅在附录一句话带过即可。

---

### 二、重点排查三类问题

#### A 类：持锁跨界与死锁/自死锁风险（最高优，P0 级嫌疑）
检索全仓 `crates/` 下所有 `static OnceLock/LazyLock<Mutex/RwLock<...>>` 以及关键长生命周期字段锁（重点包括但不限于）：
- `crates/qaqh-process-tools/src/process_registry.rs` (`REGISTRY`)
- `crates/qaqh-process-tools/src/exec/env_overlay.rs` (`OVERLAY`)
- `crates/qaqh-workspace/src/runtime.rs` (`TOOL_MANAGER`)
- `crates/qaqh-workspace/src/authorization.rs` (`GLOBAL_TRUSTED`)
- `crates/qaqh-workspace/src/todo/model.rs` (`TODO_LOCK`)
- `crates/qaqh-workspace/src/audit/` (`APPEND_LOCK`、`CHAIN`、`AUDIT_APPEND_LOCK`、`QUARANTINED_ROOTS`、`AUDIT_WRITER_LOCK`)
- `crates/qaqh-file-tools/src/read_image/mod.rs` (`IMAGE_REGISTRY`)
- `crates/qaqh-file-tools/src/pending.rs` (`PENDING`)
- `crates/qaqh-file-tools/src/journal.rs` (`JOURNAL_IO`)
- `crates/qaqh-lsp/src/` (`MANAGER_SLOT`、`SPAWN_PIDS`、`connection.rs`)
- `crates/qaqh-mcp/src/` (`MANAGER_SLOT`、`CONN_NOTIFY`、`SPAWN_PIDS`)
- `crates/qaqh-config/src/` (`watch::LATEST`、`config_io_lock`)
- `crates/qaqh-gate/src/usage_semantics.rs` (`LATCHES`)
- `crates/qaqh-sandbox/`、`crates/sbx-win/`、`crates/qaqh-client/`、`crates/qaqh-runtime/`、`crates/qaqh-session/`、`crates/qaqh-daemon/`

对每一把锁逐一核查以下问题：
1. **同线程重入自死锁**：持有 `MutexGuard`/`RwLockReadGuard`/`RwLockWriteGuard`（注意临时表达式生命周期，如 `match x.lock().unwrap().foo()`、`if let` 作用域延长锁生命周期！）期间，是否直接或间接回调到本模块/本锁（`OnceLock::get_or_init` 嵌套、回调 `fn`/`dyn Trait`、日志钩子或状态探测器）？
2. **AB/BA 锁序反转**：是否存在 `Func1` 先锁 `A` 再锁 `B`，而 `Func2` 先锁 `B` 再锁 `A`？列出跨模块实际锁序图。
3. **持锁做慢操作**：持有互斥锁/写锁期间是否执行系统探测（Win32/registry/进程检测）、派生子进程、文件/网络阻塞 I/O、或跨 `await`（若使用 `std::sync::Mutex` 则严禁跨 `await` 或阻塞异步调度）？

#### B 类：Tokio 异步控制面阻塞与线程饥饿（P0/P1 级嫌疑）
重点检查 `crates/qaqh-daemon/src/axum_server/axum_impl/`（如 `service_api.rs` 中的 `handle_service` -> `qaqh_runtime::ringing::service_methods::dispatch`、`v2.rs`、`commands.rs` 等）及 `qaqh-session` / `qaqh-agent-runtime` 异步路径：
1. 列出所有从 `async fn` HTTP handler / Tokio task **直接同步调用**并可能触发以下行为的路径：
   - 拿取 `std::sync::Mutex` / `std::sync::RwLock` 并做较重工作；
   - 执行同步文件系统扫描、SQLite 阻塞操作、外部命令/系统信息探测。
2. 评估：如果其中某条路径卡住 3~5 秒或死锁，是否会像 Bug 1 一样连带把 `/health`、`session.list`、`bootstrap` 等关键接口一起拖死？

#### C 类：与 `v4.4`（`MUST.md` P1-1 ~ P1-4）的交叉映射与去留判定
逐一把排查出的全局锁归入以下四类之一：
1. **立斩类（Bugfix / 拟挂 `P1-0` 加急）**：已存在真实自死锁、ABBA 死锁环或高频异步控制面阻塞风险；仅需“先 `clone`/放锁再探测/IO、临时状态改原子量或 `watch`/`ArcSwap`、阻塞调度移入 `spawn_blocking`”，不改业务架构即可零风险修掉。
2. **`v4.4 P1-1~P1-3` 自然退场类**：会在 Runtime 统一、`SessionOwner` Actor 化或野生 JSON/JSONL 并入 SQLite WAL 时被消除的锁（标注具体依赖哪个 `P1-*` 切片）。
3. **必须延后至 `v4.4` 后/`v4.5` 实例所有权化重构类**：需要像 `P0-2` 那样把全局状态迁入 `QaqhService` / `Session` / `WorkspaceContext` 的锁（如跨工具共享注册表），列明直接硬删会导致什么功能/并发崩溃。
4. **合理保留类**：确实保护单点进程级不变量（如硬件/全局写盘串行锁），只需保持“极短临界区、持锁不跨界”纪律即可。

---

### 三、要求输出的报告结构
请输出 Markdown 报告（若需要落盘，可放在 `docs/v4.3-4.8/` 下并注明报告名称；当前阶段**不要修改业务 Rust 代码**），格式如下：
1. **高危清单（按严重度 P0 -> P1 -> P2 排序）**：
   - 编号 / 锁符号与位置（`file:line`）
   - 观测到的完整调用链（`调用方 file:line -> 中间层 -> 持锁点 -> 二次加锁/阻塞点`）
   - 失陷/崩溃场景（为什么会死锁、饥饿或撕裂）
   - 最小安全修复方案（明确：**保留锁缩边界** vs **改无锁原子/watch** vs **迁入 `spawn_blocking`** vs **留待实例重构**）与对应反证测试思路
2. **全量全局可变锁台账表**：覆盖所有查到的生产级 `static Mutex/RwLock`（含测试中对应的 `TEST_LOCK/SERIAL` 映射关系），列明归属分类（1/2/3/4）。
3. **可直接作为 `P1-0` 加急 PR 执行的切片建议**：仅包含“高确定性、零契约变更、不拆原有语义仅修复持锁跨界或异步阻塞”的条目，方便主线快速收口而不影响 `v4.4` 主路线。
```
