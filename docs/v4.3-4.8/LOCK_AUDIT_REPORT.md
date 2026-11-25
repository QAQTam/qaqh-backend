# QAQ-Harness 全局锁、死锁风险与 Tokio 阻塞边界专项审计报告

- **报告名称**：LOCK_AUDIT_REPORT（对应 `LOCK_AUDIT_PROMPT.md` 委托）
- **审计基线**：`main` HEAD = `a1e5a9b4`（PR #27 merge；`git log`/`git status` 实测，与 `origin/main` 同步）。版本位 4.2.0（Cargo.toml:66）。
- **审计形态**：**只读**。未修改任何业务 Rust 代码，未执行 cargo build/test，未做 git 写操作。全部结论来自实际 read/grep/codegraph explore；关键跨组结论由主会话二次抽查复核。
- **资源基线（宪法 #7）**：Intel Xeon E3-1230 V2（4C/8T）· 内存 8GB（空闲约 5.2GB）· D: 剩余 61GB（已用 74%）。审计全程无编译负载；本机 worker 数 = 逻辑核数 = **8**（`Runtime::new()` 默认，daemon/src/main.rs:100/147）。
- **组织方式**：主会话负责 daemon 异步控制面（B 类）与跨模块锁序整合；5 个只读子审计分组并行覆盖 crate 群，原始分组报告存 `.qaqh/audit-parts/{workspace,file-process,lsp-mcp,config-gate-sandbox,session-runtime}.md`（含逐锁加锁点全清单，本报告为其整合裁定）。
- **MUST.md 现状校准**（审计实测，勿把计划当现状）：P0-1（FALLBACK_RT 物理删除）**已实施**（provider_job.rs:171-174、gate 全 src 无 block_on）；P0-2（thread_local 门面/TEST_RUNTIME_SERIAL/file_state LEDGER 单例）**已实施**（FileRuntime 实例化，file_state.rs:64-67，`clear()` 已不存在）；P0-4 **已实施**（PR #26）。P1-1~P1-4 **均未实施**（mcp/lsp 私建 Runtime 与 250ms 轮询桥仍在；SessionOwner 仍是每会话 OS 线程泵 owner.rs:280-285；四类野生 JSON/JSONL 仍在）。

---

## 〇、总判定

1. **全仓无存活的同线程自死锁、无 ABBA 锁序环。** PR #27 式「持 std 锁回调再入同锁」模式已被消除：`PREFLIGHT_STATE` 修复经逐点验证成立（`CACHED_WRITE_ISOLATION: AtomicU8` preflight.rs:71，`detect()` Windows 分支 capability.rs:96-98 不再触碰该锁；start/run_blocking 的 detect 均在锁前，探针在专用 std 线程）。五个分组各自绘制的锁序有向图全部为 DAG；**std 锁跨 await 全仓 0 命中**（逐段核对，句柄一律 clone/take 出 std 锁后再 tokio lock）。
2. **真实的系统性风险不是经典死锁，而是「全局 std Mutex × 持锁文件/SQLite IO × 在 Tokio worker 上取锁」的活锁性停摆**：`QaqhService.registry` 中央锁（service.rs:15）在多条热路径上持锁做 SQLite 读写、quota 文件锁+fsync、PTY 子进程创建、泵回执阻塞等待；同时 daemon 服务面/命令面把同步分派直跑在 8 个 worker 上（service_api.rs:179、commands.rs:542 无 spawn_blocking）。任一慢 IO（3~5s，本机慢盘/AV/WAL 竞争下现实存在）经 registry 锁放大为**全控制面冻结**，并经 worker 占用**间接饿死 /health**——与 PR #27 事故同构的级联，只是机理换成持锁 IO。`/health` 本身纯函数无锁（control.rs:9-13），db/quota 锁卡住不会*直接*阻塞它，但 ≥8 个并发慢调用占满 worker 后它一样死。
3. **三项独立于 MUST 主路线的 lsp P1 正确性缺陷（立斩类）**：SPAWN_PIDS 键错位可跨 root 误杀活进程组；`shutdown_global` 零接线致 Unix 孤儿子进程（acp 模式连 mcp 收尾也漏）；`conn.shutdown()` 的 Shutdown 请求无超时硬顶致 watchdog/热重载永久挂起+进程泄漏。
4. **一处潜在自死锁引信（P2，当前无存活触发路径）**：`RetryHub::drain` 的 while-let 临时 MutexGuard 活到循环体末（edition 2024 语义），持锁调用任意 `dyn FnMut` 回调（sdk_common.rs:639-648）；回调若再触 `record` 即同线程重入。另有一处 lsp `conns→state` 嵌套边（manager.rs:343）当前无反向路径，属未来 ABBA 引信。
5. 锁设计标杆（保留，不动架构）：process_registry 的 kill_gate 外置+plan 提取+墓碑防 pid 复用；mcp ServerConnection 的单锁状态机+句柄出锁再 tokio lock；工具执行三相（manager 锁不跨 Phase2 执行，execution.rs:142/273-275/325）；Db 单写者线程+async write/read（db/mod.rs:128-159）。

---

## 一、高危清单（P0 → P1 → P2）

> 每条给出：锁符号/位置、实测调用链、失陷场景、最小安全修复（保留锁缩边界 / 原子化 / spawn_blocking / 留待实例重构）与反证测试思路。所有 file:line 均经实际读源验证。

### P0（结构性复合停摆：高频异步控制面阻塞风险）

#### L-01 `QaqhService.registry: Arc<Mutex<AgentRegistry>>` — 持锁 IO × worker 同步分派
- **位置**：qaqh-runtime/src/service.rs:15；统一入口 `registry()` L597-601。AgentRegistry 内部零锁，instances/quota_ledgers/shells/exec_sessions/supervisor/catalog 全由这一把 std Mutex 保护（registry/mod.rs:272-311）。
- **实测调用链（持锁慢操作清单）**：
  1. **session.list（前端轮询热路径）**：`handle_service`(async, service_api.rs:37) → **:179 `service_methods::dispatch` 同步直调，无 spawn_blocking** → service.rs:486 `handle` → handlers/session.rs:17-19 → `list_sessions`(service.rs:825) → **:829 取 registry 锁** → :833/:840 两次 `read_blocking`（db/mod.rs:186 `read_conn` Mutex，busy_timeout=5s，db/mod.rs:193）→ 每行 :851 `session_run_status` → owner.rs:2026 `session_run_state` 再 read_blocking → 未挂工作区的行 **:890 `write_blocking`**（单写者线程 mpsc 投递+`rx.recv()` 阻塞等，synchronous=FULL 每提交 fsync，db/mod.rs:195-206）。**256 行上限 × 慢库 = 秒~分钟级持锁**。
  2. **命令准入主路径**：dispatch_session Route::Engine(command.rs:278, async worker) → `send_ringing_command`(service.rs:210 registry.lock) → `send_ringing`(messaging.rs:12)：冷会话 `ensure_loaded_for_command`(spawn.rs:522) → `spawn_session_inprocess`(spawn.rs:429-517) 持锁做 config 读+FS workspace 解析+多次 DB 读+owner 泵 OS 线程派生+完整 assemble；ConversationSendMessage 前 `session_effective_config`(messaging.rs:52) 持锁叠 config_io_lock+SQLite read_blocking+密钥解密；reload 分支持锁做 fs+provider_spec(messaging.rs:67-103)；**ConversationTruncate 持锁同步等泵+fsync**（messaging.rs:119 → session_loop.rs:454 → owner.rs:518-545 `rx.recv()` L544）。
  3. **子代理派生**：`spawn_subagent_v2`(spawn.rs:181-340) 持锁做 fs::canonicalize(:230)+read_blocking(:262)+write_blocking×2(:273/:289)+fork write(:321)+**quota reserve/commit/release（见 L-02）**。
  4. **终态路由（回合任务 worker 上）**：`notify_terminal`(session_loop.rs:2550，async 回合任务同步调用) → port 闭包(service.rs:108-119) → `schedule_after_terminal` **worker 上 read_blocking**(handoff_fork_schedule.rs:15-26) → `router_service.registry()` 取锁 → `route_turn_terminal`(lifecycle.rs:128-242) 持锁做 goal 记账读×4+写(goal.rs:183/226)+parent 读(:144)+`turn_answer_text` read_blocking(:154-162)+`send_ringing`(:230，可再触发冷加载装配)。**违反 db/mod.rs:178「调用方必须已在同步线程上」的明示契约**（codegraph 计 read_blocking 61 个调用方，大量 async 路径违约）。
  5. **shell.open**：service_api.rs:142(async) → service.rs:615 取 registry 锁 → shells().open → **shell/mod.rs:217 `spawn_pty` ConPTY 子进程创建持锁执行**（AV 扫描时可达数百 ms~秒级）。
  6. **delete_session**：✅ 已在 spawn_blocking 内（service.rs:448-476），但持 registry 锁跨 exec shutdown（子进程组杀+3s Condvar 等待，exec_session/manager.rs:676-698）+owner.flush+DB 删除+remove_dir_all——不钉 worker，但钉住所有 registry 消费者。
- **失陷场景**：慢盘/WAL checkpoint/quota 外部持锁/ConPTY 慢 spawn 任一发生 → registry 持有数秒 → 命令准入、activity（control.rs:19 同步取锁）、session.list、shell.*、终态路由全部排队，调用方 worker 逐个被钉死；8 worker 占满后 **/health 间接停摆**（前端启动检测超时全空，PR #27 事故同款表象）。前端对 session.list/todo.status/dashboard 的轮询是天然放大器。
- **最小安全修复**（保留锁，三步）：
  1. **分派面收口 spawn_blocking**：service_api.rs:179 一处 + commands.rs:541-543 同步变异臂 + shell.* 臂（service_api.rs:142/147/156）+ activity（control.rs:19）。仓内已有同款纪律先例：command.rs:113/166（D-4 约定）、control.rs:46/63、marketplace_api.rs:36/318/356。**零 wire 变化**。
  2. **list_sessions 缩边界**：registry 锁内只 clone is_running/activities 快照即放锁；DB 读写与逐行状态计算全部锁外；`session.meta` 同改（handlers/session.rs:29-44 的 guard 当前活到 read_blocking 之后）。
  3. 长期：registry 按根树分片/per-root Actor（归类 3，随 P1-2 方向），**不属于 P1-0**。
- **反证测试思路**：测试钩子向 `read_blocking` 闭包注入 3s 延迟（或临时慢盘 fixture），并发 8×session.list + /health 探针：旧实现 /health 超时必红；新实现（分派面 spawn_blocking + 缩边界）/health 与 session.activity 延迟 <50ms 变绿。`cargo test -p qaqh-daemon -p qaqh-runtime` 精准回归。
- **归类**：锁本体 **3（实例所有权化方向）**；修复 1/2 属 **1（立斩，零架构变更）**。

### P1

#### L-02 `quota.lock` OS 文件锁 — 无超时阻塞 × 全量 JSONL 重读 × fsync，registry 锁内
- **位置**：qaqh-agent-runtime/src/quota_ledger.rs:428-439（`file.lock()` L436 **阻塞、无超时**，主会话复核）；refresh 全量重读 L335-411；append `sync_all()` L423（主会话复核）。
- **调用链**：spawn_subagent_v2（registry 锁内，spawn.rs:371/374/386/408/416/419）→ reserve/commit/release/reconcile（quota_ledger.rs:198/250/271/294）→ lock_exclusive → refresh O(n) 逐行 serde（ledger 只增不缩）→ 多条 append 多次 fsync。外部链：`&mut self` ⇒ registry.quota_ledgers（registry/mod.rs:291）⇒ registry Mutex。锁序 registry→quota.lock 单向无 ABBA。
- **失陷场景**：另一进程（CLI/并发 daemon 实例）持有 quota.lock 或 fsync 慢 ⇒ registry 锁被**无限期**持有 ⇒ 全控制面停摆（L-01 的最坏放大器）。
- **最小修复**：根治=**归类 2（P1-3 并入 SQLite，文件锁+全量重读+逐条 fsync 一并退场）**。短期不建议锁外挪 reservation（durable-before-ack 语义，硬挪属破坏性）；L-01 修复 1 先保护 worker。若 P1-3 前必须加急：仅给 `file.lock()` 换限时尝试+明确错误（语义变化需所有者裁定，不列入 P1-0 零风险切片）。
- **反证**：外部进程持锁 fixture 下 spawn_subagent，断言其他服务方法延迟有界（旧实现无界红）。

#### L-03 SessionOwner 同步阻塞入口 — worker 上 `rx.recv()` 等泵+fsync
- **位置**：qaqh-session/src/owner.rs:518-545（truncate_from_turn，recv L544）、:838-845（flush）、:848-884（update_metadata，recv L882）、:303-321（title_state read_blocking）。
- **调用链**：① 撤回命令：command.rs:278(async) → service.rs:210 registry.lock → messaging.rs:119 → session_loop.rs:454 → truncate_from_turn recv——**worker 钉死且 registry 全程持有**；② rename/settings：command.rs:139(async 直呼) → session_meta.rs:112 → update_metadata recv；③ 模型自命名：session_loop.rs:2181/2193-2199 在 async 回合任务内同步阻塞；④ 装配期 title_state（assembly.rs:291，registry 锁内）。合规对照：flush 唯一生产调用在 delete_session 的 spawn_blocking 内（service.rs:463）。
- **失陷场景**：泵队列积压+写线程 fsync 慢 ⇒ 撤回/rename 卡 worker 数秒；撤回路径同时卡 registry。
- **最小修复**：owner 已有异步版 `submit_request`（owner.rs:897-915，tokio oneshot）——truncate/update_metadata 改异步投递、调用方 await（泵语义不变）；短期可先把 command.rs:139 与回合任务内调用点包 spawn_blocking。**归类 2/3**（随 P1-2 Actor 化彻底退场）。
- **反证**：慢泵 fixture 下 rename 与 /health 并发，旧红新绿。

#### L-04 `TODO_LOCK` — 进程全局一把 × 持锁文件 IO × worker 直达
- **位置**：qaqh-workspace/src/todo/model.rs:11；六个持锁点全做同步文件 IO：store.rs:42/52/107/232、typed.rs:315/358（read+write tmp+rename）。
- **调用链**：链 A（读，worker）：todo.status → service_api.rs:179 dispatch → task.rs:18 → store.rs:52 持锁 fs read；链 B（写，worker）：TodoSet/TodoCancel → commands.rs:542 → service.rs:524 → typed.rs:358/store.rs:107 持锁读+写+rename；链 C：session.dashboard → stats.rs:25 同链 A；链 D（工具，blocking pool）：session_loop.rs:1615 → typed.rs:315/358。无重入（read/write_store_for 故意无锁，store.rs:184-185）；`todo_list_for_typed`(typed.rs:277-285) 无锁读，靠 rename 原子不撕裂。
- **失陷场景**：前端轮询 todo.status/dashboard 撞慢盘 ⇒ worker 阻塞等链 D 持锁 IO；全局一把 ⇒ 会话 X 慢写卡会话 Y 查询（跨会话 convoy）。
- **最小修复**：L-01 修复 1 消 worker 暴露；锁按会话实例化/todo.json 入 SQLite。**归类 3**（P1-2/P1-3）。
- **反证**：8 线程不同会话并发 todo_write+todo.status（MUST 铁闸 3 同款探针），断言零跨会话延迟放大。

#### L-05 `config_io_lock` — 持锁全盘文件 IO × 五条 worker 路径争锁
- **位置**：qaqh-config/src/config.rs:700-703（`static LOCK: OnceLock<Mutex<()>>`，**生产锁**）；load:707/update:727/save:901 全持它；持锁期间 config.toml+providers.toml 读、首启模板写、迁移写回、原子 rename 落盘，update 还持锁取 LATEST 发布（watch.rs:50，单向无 ABBA）。
- **调用链（全在 worker，无 spawn_blocking）**：① ConfigSave/ConfigSetPermissionLevel：commands.rs:541-543 → system.rs:48 → Config::update；② ConfigLoad：service_api.rs:179 → system.rs；③ **发消息热路径**：command.rs:278 → messaging.rs:52 session_effective_config（session_config.rs:7/12/36-38 叠 SQLite read_blocking+密钥解密），registry 锁全程持有 ⇒ 锁序 registry→config_io_lock；④ 文件轮询器：service.rs:1073-1098 tokio::spawn 每 1.5s fs::metadata+mtime 变化 reload_from_disk 持锁全盘读；⑤ 60s idle tick：server.rs:278 authoritative()。
- **失陷场景**：磁盘紧张时一个慢写把多个 worker 钉死在 futex；③ 还把它复合进 registry 锁。
- **最小修复**：**锁保留（串行化语义正确，归类 4）**；L-01 修复 1 收口 ①②③，轮询器/idle tick 的配置读包 spawn_blocking。
- **反证**：慢盘 fixture 下 ConfigSave 与 /health 并发探针。

#### L-06 `state.pending` Mutex × 每命令全量 JSON 覆写
- **位置**：qaqh-ringing-runtime/src/pending_store.rs:237-273（persist：≤4096 条 HashMap 全量 serde+tmp write+rename，固定 `.json.tmp` 名）；触发点 record:339/rollback:352/mark_running:360/mark_terminal*:388/402。crate 本身零内锁，锁在 daemon AppState（axum_impl/mod.rs:79）。
- **调用链**：**每条命令准入** commands.rs:241 record_fingerprint_for_session（worker）；终态 command.rs:376-381 complete→mark_terminal；同步变异 commands.rs:561 mark_terminal_sync。guard 不跨 await（L234-265 块级，已核）。
- **失陷场景**：慢盘下全量覆写卡住 ⇒ pending 锁+worker 双卡，所有命令回执串行停摆。
- **最小修复**：短期 persist 挪 spawn_blocking/去抖（回执内存态先行）；根治 **归类 2（P1-3 SQLite command_receipts）**。
- **反证**：4096 条满载+慢盘注入，命令准入延迟旧无界新有界。

#### L-07 lsp `SPAWN_PIDS` 键错位 — 跨 root 误杀活进程组（正确性缺陷，立斩）
- **位置**：qaqh-lsp/src/adapter.rs:35（`BTreeMap<String,u32>` **只按 server 名键控**，主会话复核 L40-53）；但连接键是 (server,root) 双键（manager.rs:27-29）。record：connection.rs:295；take：connection.rs:440；组杀：connection.rs:471-486（`libc::kill(-pgid,SIGKILL)` L481，锁外）。
- **失陷场景**：两个 root 并发连同名 server：A record(pidA)→B record(pidB) 覆盖→A take 拿到 **pidB**→A 关闭/崩溃时 sweep_group 对 **B 的活进程组** SIGKILL，B 无辜被杀且 B 自身漏清扫。多会话不同 cwd 走 get_or_connect(server,root)（tool.rs:282/437）即可触发；connect_serializer 是 per-connection 的，挡不住。
- **最小修复**：**删表**——spawn_server 已返回 pid（adapter.rs:81-84/96-102），connect_once 手里就有，经 store_connected 参数直传本连接 pgid；删 lsp 侧 record/take 与 SPAWN_PIDS（mcp 需要表是 factory 输出拿不到 pid，lsp 无此约束）。改键打补丁属违宪 5，不采纳。**归类 1 立斩**，顺带消灭一把全局锁。
- **反证**：两 root 并发连同一 mock LSP server，关闭 A，断言 B 进程组存活且 B 关闭后自身被清扫（旧实现必红）。

#### L-08 lsp `shutdown_global` 零接线 — Unix 孤儿子进程；acp 模式连 mcp 也漏（立斩）
- **位置**：qaqh-lsp/src/bridge.rs:39-42（shutdown_global）与 :48-54（prime_all_async）**全仓零调用方**（grep 全域含 tests 确认）。daemon 退出只调 qaqh_mcp::shutdown_global（main.rs:105 server / :150 run）；**acp 分支（main.rs:111-119）两者都没调**。
- **失陷场景**：LSP manager 活在 static 槽，进程退出不跑 Drop ⇒ shutdown_all 永不执行 ⇒ Unix 上 rust-analyzer 等独立进程组子进程成孤儿（Windows 侧 JobObject kill-on-close 兜底**未逐行验证**，诚实标注）；acp 模式 MCP 子进程同样孤儿。prime_all_async 是死代码（宪 4：接线或删）。
- **最小修复**：main.rs 三处退出路径补 `qaqh_lsp::shutdown_global()`（acp 分支 mcp/lsp 都补）；prime_all_async 接线或物理删除。**归类 1 立斩**（一行级接线；Runtime 收敛本体归 P1-1）。
- **反证**：Unix 集成测试 spawn mock LSP server → daemon 优雅退出 → 断言进程组被收割（旧红新绿）。

#### L-09 lsp `conn.shutdown()` Shutdown 请求无超时硬顶（立斩）
- **位置**：qaqh-lsp/src/connection.rs:531-551：持会话 tokio guard 期间 `socket.request::<Shutdown>().await`（L541-544）**无 timeout**，之后才 driver.abort()（L547）。对照 mcp `close_with_timeout(2s)`（mcp connection.rs:996-998）。settings.close_timeout 已存在（connection.rs:69/79）竟无人消费。
- **失陷场景**：LSP server 卡死（RA 索引挂起）时：① watchdog 任务（L525 shutdown().await）永久挂起，旧子进程句柄扣在 guard 里 ⇒ 进程泄漏+重连再 spawn 一个；② lsp 热重载器（service.rs:1003-1023，**主 runtime** 任务）对 updated/removed server 调 conn.shutdown().await ⇒ 任务永久卡死，后续 [lsp] 配置变更全部失效；③ L-08 接线后 daemon 退出 block_on 也会永久挂起（当前因未接线未显形）。
- **最小修复**：Shutdown 请求包 `tokio::time::timeout(close_timeout,…)`，超时 abort driver+放 guard 走 sweep_group；随附小修：watchdog 回收后 return（对照 mcp connection.rs:709）、begin_call 补 gate/connected 检查（对照 mcp L718-755）。**归类 1 立斩**（补错误而非防御性补丁——mcp 同款已有，lsp 漏了）。
- **反证**：mock server 不回 Shutdown：旧实现 watchdog/热重载挂死必红；新实现 close_timeout 内收割变绿。

#### L-10 siwc/dsaccount ureq 同步网络在 worker（账号面 dispatch）
- **位置**：qaqh-siwc/src/http.rs:1（「HTTP 薄封装（ureq，同步）」）、manager.rs:65（http_timeout=30s）、:565-568（refresh_lock 持锁跨网络往返，单飞设计）；dispatch 面 accounts.rs:239-438（siwc.models→access_token→刷新+目录 HTTP；dsaccount.balance/bonuses/models/verify 网络）经 service_api.rs:179 **同步跑在 worker**。
- **澄清（已核）**：登录回调等待**不在** worker——siwc 专用 `siwc-sign-in` 线程（manager.rs:392-394）、dsaccount 专用线程（manager.rs:273-274，注释明示「绝不占用 daemon 的异步 worker」）。SIGN_IN_TTL 10 分钟（protocol.rs:40）只烧专用线程。
- **失陷场景**：IdP 不可达/慢 ⇒ 每个 siwc.models/usage 请求钉 worker 最长 30s；8 并发 ⇒ 控制面全停。refresh_lock 单飞使并发刷新串行排队（设计意图，但都在 worker 上排）。
- **最小修复**：账号面 dispatch 随 L-01 修复 1 包 spawn_blocking（锁语义全保留，**归类 4**）；长期随 P1-4 强类型/异步分派方向。
- **反证**：mock IdP 挂起 5s，siwc.models 与 /health 并发，旧红新绿。

#### L-11 shell stdin 持锁 PTY 写（registry 内 × ws worker）
- **位置**：qaqh-runtime/src/shell/mod.rs:298-305（stdin.lock 持锁 write_all+flush，PTY 缓冲满且子进程不读时阻塞）；经 service.rs:686 shell_stdin（registry 锁内）← shell_ws.rs:192（async ws 循环每 keystroke）。resize 同链（shell_ws.rs:207）。
- **失陷场景**：壳内进程停止读输入（如暂停的编辑器）⇒ 一次粘贴阻塞 ws worker+registry 锁 ⇒ 全控制面连带。
- **最小修复**：stdin 写包 spawn_blocking（短刀）；shell 实例所有权化 **归类 3**。info() 的 control→geometry 嵌套已核无反序（resize 先 drop control，shell/mod.rs:317-321）。
- **反证**：mock 壳不读 stdin，写满缓冲后断言其他服务方法延迟有界。

#### L-12 RingingHub 内容上传 write_blocking 在 worker
- **位置**：qaqh-ringing-runtime/src/hub.rs:49（put_content write_blocking）/ :69（get_content read_blocking）；上传 content.rs:253-255（async worker，大 BLOB 写全程同步等写线程 fsync）；下载 v2.rs:667；附件 hydration attachment.rs:31-34 ← command.rs:260（Engine async dispatch）。
- **最小修复**：改 `Db.write` 异步版（db/mod.rs:128-143 现成）或 spawn_blocking。**归类 2/4**（hub 无锁本体合理）。
- **反证**：大 BLOB 上传+慢写线程 fixture，/health 并发探针。

### P2（延迟/竞争/引信/worker 微扰，逐项给位置与修复）

| # | 对象 | 位置 | 事实与修复 | 归类 |
|---|---|---|---|---|
| L-13 | audit 锁族 fsync convoy | workspace/audit/mod.rs:38/362、v2.rs:42/45 | WRITER→{v1 APPEND, v2 APPEND→CHAIN} 单向无环；一次双写=2×fsync 全在 WRITER 内；v2 首次缓存未命中 recover_state 在 APPEND+CHAIN 双锁内整文件读（≤4MB×4 代）+截断重写（v2.rs:312→414-470）。工具链在 blocking pool，不卡 worker。缩边界：recover 读挪锁外；根治随 P1-3 | 2 |
| L-14 | JOURNAL_IO | file-tools/journal.rs:68（唯一锁点 :198） | 持锁 fsync+root 变更 repair+全量重扫；**无 audit 回调**（全 crate grep 零命中）；全局 seq 是契约。可选缩边界（repair/rescan 锁外预备） | 4 |
| L-15 | FileRuntime canonicalize 锁内 | file-tools/file_state.rs:309-326（insert:80、record_grep:231 等） | 已实例化（P0-2 完成）；网络盘/断连 SMB 可卡会话锁数秒。ledger_key 挪锁外计算 | 4 |
| L-16 | REGISTRY captured_full 大 clone | process-tools/process_registry.rs:727-743 | 全局锁内 clone ≤5MB×2 文本；kill/tasklist/wait 全锁外（设计标杆）。锁内取 Arc、锁外 clone | 4 |
| L-17 | IMAGE_REGISTRY async 直达+相邻 worker 落盘 | file-tools/read_image/mod.rs:45；session_loop.rs:2601/2626/2631 | 锁本身干净（解码/读盘锁外）；**assemble_context 在 worker 做 MB 级 base64 落盘**（:2626）。IO 挪 spawn_blocking | 4 |
| L-18 | exec env 热重载 secret IO 在 worker | runtime/service.rs:1029-1047（reloader :1052-1056） | SecretStore 读盘+解密在 worker，随后才拿 OVERLAY 写锁（锁内纯 CPU）。apply 整体 spawn_blocking | 4 |
| L-19 | SSE 诊断 tick worker 多短锁 | daemon/v2.rs:1070-1072 → service.rs:317/323/336/349 | registry+MCP/LSP conns+IMAGE_REGISTRY 短锁串行，均无 IO 持锁；降频或 spawn_blocking | 4 |
| L-20 | devices touch 全量覆写在鉴权热路径 | ringing-runtime/device_registry.rs:115-131；daemon/authz.rs:52-57 | 每设备请求 lookup+touch（60s 节流）持锁写盘。P1-3 根治 | 2 |
| L-21 | **RetryHub.drain 持锁回调（潜在自死锁引信）** | gate/sdk_common.rs:639-648 | while-let 临时 guard 活到循环体末（edition 2024，Cargo.toml:67），持锁调 on_event；当前无重入触发路径（record 由 SDK 重试循环调，drain 由桥接循环调）。缩边界：锁内 mem::take 到本地 Vec，放锁再回调 | 4 |
| L-22 | mcp try_runtime_handle 恒 None | mcp/bridge.rs:71-74 | checked 版读的是**另一个从未初始化的函数局部 static RT**（与 :47 不同 cell）⇒ Drop 兜底网络（manager.rs:360）永远走不到。RT 提模块级共享 cell；或随 P1-1 退场 | 2（先修则 1） |
| L-23 | lsp evict conns→state 嵌套边 | lsp/manager.rs:340-345（retain 闭包内 conn.status()） | 全分组唯一 std 锁嵌套；反向边不存在（connection 无 manager 回指）⇒ 当前无环，未来 ABBA 引信。锁内集候选→锁外查状态→短锁 remove | 4 |
| L-24 | mcp 代际竞态+重拉无去抖；lsp watchdog 空转/begin_call 无闸 | mcp/connection.rs:949-952/979-982、manager.rs:166-180、connection.rs:793-806；lsp/connection.rs:525-526/206-211 | 旧 conn 按名 remove 误删新代 CONN_NOTIFY/取走新代 pid（窗口窄）；server spam list_changed 在 2-worker 专属 RT 堆无界重拉。注册随连接代所有权化；重拉单飞；watchdog return；begin_call 补闸 | 3（小修可先行） |
| L-25 | PREFLIGHT 残留微扰 | sandbox/preflight.rs:253/309（is_process_elevated 持锁 Win32）、state_slot 初始化 detect（Linux bwrap 两次子进程 capability.rs:183/204-213） | 自死锁已消除（见总判定 1）；初始化经只读分派面发生在 worker。elevated 预计算挪锁外；分派面随 L-01 同刀；Linux 冷初始化可懒到后台 | 4 |
| L-26 | DAEMON_SPAWN_GUARD 持锁同步探测 | client/client.rs:1087/1098-1112 | tokio Mutex，guard 不跨 await（握手等待在放锁后 ✓）；临界区含 TCP 探测≤300ms+CreateProcess+fs 读，全在 worker。决策段 spawn_blocking；同步版 ensure_daemon_running 不取 guard（靠 daemon.lock 兜底，注释明示） | 4 |
| L-27 | tasklist.exe `.output()` 无超时 | types/platform.rs:413-438（Win 分支 :418-420） | 后代持管道写端 ⇒ EOF 永不来（registry/mod.rs:145-153 注记 BUG-2026-09-15-02 同款）。调用方仅启动期 main.rs:194 与客户端 discovery.rs:311，不在 async 热路径。换 OpenProcess 直查（与 MUST P2-2⑥ 重合，可先行）**——已清零（v4.5 P2-2b：`OpenProcess`+`GetExitCodeProcess` RAII / `libc::kill(pid,0)` 直查，client 复制版同刀收编单源，反证 756.3µs→1.1µs，见 ADR-0044）** | 1（小改）· 已修 |
| L-28 | wait_agent 长占 blocking pool | runtime/host_impl.rs:650-695 | 25ms sleep 轮询+每目标 read_blocking，超时可达分钟级；经 spawn_blocking 执行不钉 worker，但挤占 db.read 的 blocking 池。改 async 轮询+快照读 | 2/3 |
| L-29 | GLOBAL_TRUSTED worker 持锁文件写 | workspace/authorization.rs:558/562；session_loop.rs:1540（async fn 内 trust_folder） | 惰性 load+trust save 持锁 fs IO；Persistent 批准低频。调用点 spawn_blocking（一行级）；信任集随会话实例走 | 3（随 P1-2） |
| L-30 | session_hooks trust 持锁写文件 | agent-runtime/session_hooks.rs:132-134 | ToolPermissionRespond(trust_folder)→registry 锁→respond_permission→持锁写盘（worker）。写盘挪锁外 | 4 |
| L-31 | owners map 锁内派生 OS 线程 | session/owner.rs:1900/1942-1945→:280 | get_or_create or_insert_with 内 SessionOwner::new spawn 线程（~几十µs 无 IO）。随 P1-2 退场 | 3 |
| L-32 | ReplayRing 锁内 ≤4MB clone | session/owner.rs:64/132-135 | SSE attach/lagged 追赶在 worker，毫秒级有界。可保留 | 4 |
| L-33 | shared_runtime 惰性第二 Runtime | agent-runtime/session_loop.rs:140-150 | 2-worker，仅 Handle::try_current 失败（CLI/同步测试）构造；daemon 正常路径不触发（已核全部 spawn_task 调用点有宿主上下文）。保留+debug_assert 防 daemon 误入；P1-1 收敛清单 | 4 |
| L-34 | SplitFileLogger std Mutex 文件 IO | daemon/main.rs:66-69 | 每条日志持锁写文件；任何持锁路径内的 log::info! 都串上它（如 LATCHES observe 锁内 log，usage_semantics.rs:176）。无环（logger 不回触业务锁），保持短临界区纪律 | 4 |
| L-35 | secrets.toml.lock 跨进程 OS 锁 | config/secrets.rs:415/427-445 | provider_upsert（commands.rs:500-510 async）持锁读改写+fsync；与 config_io_lock 顺序非嵌套（providers.rs:162→87）。随 L-01 分派面同刀 | 4 |
| L-36 | sbx-win Provider.sessions 持锁 Win32 枚举回调 | sbx-win/projfs.rs:200/320-348 | ProjFS OS 线程池回调固有形态，非 tokio 面；与 notifications 锁零嵌套。可接受现状 | 4 |
| L-37 | context_usage thread_local 前提过时 | types/context_usage.rs:50-53 | 无锁 Cell；头注「一个 actor 线程只跑一个会话」前提已失效（回合跑共享 worker）；仓内无生产写者。语义隐患留痕，随消费方整改 | 4 |
| L-38 | client v2_streams→tasks 跨 await 持 tokio 锁 | client/client.rs:643-660 | tokio Mutex 跨 await 合法，tasks 叶子无反向；insert 可提前到 push_task 前消除微扰 | 4 |

---

## 二、全量全局可变锁台账表

> 覆盖全仓生产级 `static Mutex/RwLock`（含 OnceLock/LazyLock 包裹与 tokio 锁）+ 关键长生命周期字段锁。两轮 grep（`static\s+[A-Z_0-9]+\s*:` 全 crates）+ 五分组逐文件精读复核；`lazy_static`/`parking_lot`/`static mut` 全仓 0 命中。归类：**1** 立斩 / **2** 随 P1-1~P1-3 自然退场 / **3** 须实例所有权化（v4.4 后/v4.5）/ **4** 合理保留。

### 2.1 进程级 static 锁

| 符号 | 位置 | 形态 | 守卫不变量 | 定级 | 归类 |
|---|---|---|---|---|---|
| REGISTRY | process-tools/process_registry.rs:267 | LazyLock<Mutex<ProcessRegistry>> | 全局进程表+墓碑防 pid 复用+归属隔离 | P2(L-16) | 4 |
| OVERLAY | process-tools/exec/env_overlay.rs:21 | OnceLock<RwLock<BTreeMap>> | [exec.env] 覆盖层全量替换 | 无害(调用方 P2,L-18) | 4 |
| TOOL_MANAGER | workspace/runtime.rs:19 | OnceLock<Mutex<ToolManager>> | daemon 查询快照（不参与执行） | 无害 | 4 |
| GLOBAL_TRUSTED | workspace/authorization.rs:558 | Mutex<Option<TrustedFolderSet>> | 进程级信任目录集 | P2(L-29) | 3 |
| TODO_LOCK | workspace/todo/model.rs:11 | Mutex<()> | todo.json 读改写串行（全局一把） | **P1(L-04)** | 3 |
| AUDIT_APPEND_LOCK | workspace/audit/mod.rs:38 | Mutex<()> | v1 CSV rotate+append 互斥 | P2(L-13) | 2 |
| QUARANTINED_ROOTS | workspace/audit/mod.rs:53 | LazyLock<Mutex<HashSet>> | 隔离 root 快路径集合 | 无害 | 4 |
| AUDIT_WRITER_LOCK | workspace/audit/mod.rs:362 | Mutex<()> | 唯一审计 writer（intent/CSV/v2） | P2(L-13) | 2 |
| APPEND_LOCK(v2) | workspace/audit/v2.rs:42 | Mutex<()> | v2 rotate+append+链更新串行 | P2(L-13) | 2 |
| CHAIN(v2) | workspace/audit/v2.rs:45 | LazyLock<Mutex<HashMap>> | 每文件链状态缓存（seq+链头 hash） | P2(L-13) | 2 |
| IMAGE_REGISTRY | file-tools/read_image/mod.rs:45 | LazyLock<Mutex<HashMap>> | 会话键控图片索引（字节外置） | P2(L-17) | 4 |
| PENDING | file-tools/pending.rs:29 | LazyLock<RwLock<HashMap>> | dry-run 重放暂存+TTL（锁内无 IO） | 无害 | 4 |
| JOURNAL_IO | file-tools/journal.rs:68 | Mutex<JournalWriterState> | 全局单调 seq+追加串行 | P2(L-14) | 4 |
| MANAGER_SLOT(lsp) | lsp/lib.rs:26 | OnceLock<RwLock<Arc<LspManager>>> | 装配槽（guard 不逃逸） | 无害 | 4 |
| SPAWN_PIDS(lsp) | lsp/adapter.rs:35 | OnceLock<StdMutex<BTreeMap>> | pgid 登记（**键错位缺陷**） | **P1(L-07)** | **1(删表)** |
| MANAGER_SLOT(mcp) | mcp/lib.rs:69 | OnceLock<RwLock<Arc<McpManager>>> | 装配槽（写锁终生一次） | 无害 | 4 |
| SPAWN_PIDS(mcp) | mcp/adapter.rs:80 | OnceLock<StdMutex<BTreeMap>> | pgid 登记（factory 拿不到 pid，表有存在理由） | P2(L-24 代际) | 3 |
| CONN_NOTIFY(mcp) | mcp/connection.rs:61 | OnceLock<StdMutex<BTreeMap<Weak>>> | 通知重拉路由（防 Arc 环） | P2(L-24) | 4+去抖小修 |
| LATEST | config/watch.rs:21 | OnceLock<Mutex<Option<Arc<Config>>>> | 已落盘快照镜像（watch 兜底） | 无害 | 4 |
| config_io_lock | config/config.rs:701 | OnceLock<Mutex<()>> | config 读改写事务串行 | **P1(L-05)** | 4(修调用面) |
| LATCHES | gate/usage_semantics.rs:151 | OnceLock<Mutex<HashMap>> | cache-usage 口径锁定（纯内存） | 无害 | 4 |
| PREFLIGHT_STATE | sandbox/preflight.rs:67 | OnceLock<Mutex<SandboxPreflightReport>> | 自检状态机+run_id 代际（PR #27 已修） | P2(L-25) | 4 |
| PRJ | sbx-win/projfs.rs:116 | OnceLock<Option<Prj>> | DLL 函数指针表（无内锁） | 无害 | 4 |
| DAEMON_SPAWN_GUARD | client/client.rs:1087 | OnceLock<tokio::Mutex<()>> | 防双 spawn 决策串行 | P2(L-26) | 4 |
| RUNTIME(client) | client/client.rs:1071 | OnceLock<tokio::Runtime> | 前端桥共享 runtime（告诫：勿从 async 上下文调 connect 的 block_on） | 无害 | 4 |
| OBSERVED | agent-runtime/handoff.rs:399 | OnceLock<Mutex<HashMap>> | 实测上下文窗口分母（纯内存） | 无害 | 4 |
| shared_runtime | agent-runtime/session_loop.rs:141 | OnceLock<tokio::Runtime> | 同步调用方惰性兜底（2-worker） | P2(L-33) | 4(P1-1 收敛) |
| RT(lsp) | lsp/bridge.rs:23 | OnceLock<tokio::Runtime> | 工具裸线程 async 驱动宿主 | P1-1 计划内 | 2 |
| RT(mcp)×2 | mcp/bridge.rs:47/:72 | OnceLock<tokio::Runtime> | 同上；**:72 恒空缺陷(L-22)** | P2 | 2 |
| MANAGER(siwc) | siwc/global.rs:14 | OnceLock<Arc<SiwcManager>> | 深层调用点入口槽 | 无害 | 4 |
| MANAGER(dsaccount) | dsaccount/global.rs:12 | OnceLock<DsAccountManager> | 同上 | 无害 | 4 |
| HOST/TASK_HOST | subagent/host.rs:440/:380、goal/lib.rs:39、gitx/tools.rs:58 | OnceLock<Arc<dyn Host>> | 工具宿主反向槽（P2-1 消除对象，非锁） | 无害(锁视角) | 2(P2-1) |

### 2.2 关键长生命周期字段锁

| 符号 | 位置 | 守卫不变量 | 定级 | 归类 |
|---|---|---|---|---|
| QaqhService.registry | runtime/service.rs:15 | AgentRegistry 一致性单写者 | **P0 复合(L-01)** | 3(①②立斩先行) |
| Db.read_conn | session/db/mod.rs:82 | 只读连接串行（NO_MUTEX 外挂） | P1(L-01 复合) | 2 |
| Db 写执行器 | session/db/mod.rs:76-83/230-242 | 单写者线程（非 Mutex，mpsc 串行化点） | P1(调用方) | 4 |
| OwnerDirectory.owners | session/owner.rs:1900 | 会话→泵目录 | P2(L-31) | 3 |
| ReplayRing.state | session/owner.rs:64 | ≤2048 帧/4MB 环定序 | P2(L-32) | 4 |
| SessionLoop.inner+5 RwLock | agent-runtime/session_loop.rs:92-108 | 回合队列/活动槽/权限/ask | 无害 | 4(3 备选) |
| WorkspaceToolPort 锁群 | agent-runtime/tool_job.rs:325-328 | 会话 manager/policy/approvals | 无害 | 4 |
| FileRuntime.inner | file-tools/file_state.rs:64-67 | 会话私有文件基线（P0-2 落地形态） | P2(L-15) | 4 |
| ExecSessionManager.state+per-session 锁族 | process-tools/exec_session/manager.rs:126/165-172 | exec 会话表+core/process/writer | P2(stop_and_wait 3s 在 registry 内,L-01.6) | 4 |
| ShellRegistry.shells+LiveShell 6 锁 | runtime/shell/mod.rs:132/143-159 | 壳表+control/stdin/reader/output | P1(L-11) | 3 |
| LspManager.cfg/conns | lsp/manager.rs:33/36 | 配置快照+连接路由 | P2(L-23) | 4 |
| lsp ServerConnection 锁族 | lsp/connection.rs:115-118 | state/pgid/watchdog/serializer | **P1(L-09)** | 1(超时硬顶) |
| McpManager.cfg/conns | mcp/manager.rs:35/43 | 同上（零嵌套，纪律标杆） | 无害 | 4 |
| mcp ServerConnection 锁族 | mcp/connection.rs:176-181 | 同上（锁纪律最好文件） | 无害 | 4 |
| SiwcManager 6 锁 | siwc/manager.rs:108/120-125 | state/write/refresh/flows/discovery/jwks | P1(L-10 调用面) | 4 |
| DsAccountManager.flow/revoke | dsaccount/manager.rs:120/122 | 登录流程态 | P1(L-10 调用面) | 4 |
| RetryHub.pending | gate/sdk_common.rs:628 | 重试事件跨侧缓冲（每请求实例） | P2(L-21 引信) | 4(缩边界) |
| AppState.leases/pending/devices/pairings | daemon/axum_impl/mod.rs:78-85 | presence/回执/设备/配对（外部 Arc<Mutex>） | P1(L-06)/P2(L-20)/leases 无害 | 2/4 |
| Provider.sessions/notifications | sbx-win/projfs.rs:200/201 | ProjFS 枚举游标/通知 | P2(L-36) | 4 |
| JsonlSink.inner | sbx-win/events.rs:61 | 事件逐行串行（同步 exec 消费） | 无害 | 4 |
| session_hooks.trust | agent-runtime/session_hooks.rs:31 | hook 信任库 | P2(L-30) | 4 |
| ClientInner.tasks/v2_streams/attached_sessions | client/client.rs:262-269 | tokio Mutex 实例态 | 无害(L-38) | 4 |
| client credentials/state/renew_failures | client/session.rs:36/45/47 | 凭据 RwLock+续租态 | 无害 | 4 |
| SplitFileLogger.daemon/gate | daemon/main.rs:66-69 | 日志文件写串行 | P2(L-34) | 4 |

### 2.3 测试专用锁（不进生产二进制，一行带过 + 映射）

`TODO_ENV_LOCK`(workspace/todo/tests.rs:15)、`SERIAL`(workspace/tests/audit_fail_closed.rs:10、audit_ledger.rs:19)、`TOMBSTONE_SERIAL`(tests/process_registry_tombstone.rs:39)、`TEST_MANAGER_LOCK`(workspace/execution.rs:676)、`DELETE_LOCK`(file-tools/file_mutate.rs:945)、`JOURNAL_ISOLATION`(file_mutate.rs:974)、`TEST_LOCK`(journal.rs:669)、`IMAGE_ENV_LOCK`(read_image/mod.rs:396)、`env_overlay::test_lock`(env_overlay.rs:45-47)、`TEST_LOCK`(sandbox/preflight.rs:893)、`SERIAL`(sandbox/sbx_map.rs:165/179)、`DataDirGuard LOCK`(gate/sdk_common.rs:71)、`QAQH_DATA_DIR_LOCK`(agent-runtime/session_loop_tests.rs:1021)、`IMAGE_TEST_SERIAL`(session_loop_tests.rs:37，AtomicBool 自旋)、`ENV_LOCK`(runtime/service.rs:1367)、`CATALOG_LOCK`(runtime/tests/provider_catalog_cas.rs:13)、`SERVICE/ISOLATED_DATA_ROOT`(service_methods.rs:1794/1800)、client/shell_stream.rs:619/625 recorded。
**映射缺陷留痕**：`IMAGE_TEST_SERIAL`/`QAQH_DATA_DIR_LOCK`/`ENV_LOCK`/`DataDirGuard` 四把独立锁护同一个 `QAQH_DATA_DIR` 进程环境变量，**互不统属、无全局 TEST_LOCK/SERIAL 注册表**——跨 crate 测试并行时潜在互扰（非生产问题；MUST 铁闸 3 拆串行锁时一并治理）。

### 2.4 无害一次性初始化缓存（附录级，按 Prompt 要求一句话带过）

`OS_INFO`/`TOOLS_INFO`(agent-runtime/agent/prompt.rs:11/14，启动期 set 一次)、`SYSTEM_PATH`(runtime/registry/mod.rs:15，启动期 reg query 经 2s 限时 probe)、`DETECTED_SHELL`/`DETECTED_BASH_PATH`/`REGISTERED_SHELL_PATH`/`RESOLVED_SHELL_NAME`/`SYSTEM_CMD_PATH`/`SYSTEM_POWERSHELL_PATH`/`SHIM`(process-tools/shell.rs:56-627，fs 探测无子进程)、`RG_HABIT_RE`(exec/handler.rs:396)、`CHANNEL`(config/watch.rs:18)、`WARNED`(sandbox/lib.rs:460)、`BUBBLEWRAP_SUPPORT`(sandbox/capability.rs:31，Linux 一次性含子进程探测——冷启动 stall 见 L-25)、`PRJ`(sbx-win/projfs.rs:116)：均无内部可变状态或仅一次性初始化，无环无重入，不列为风险。

---

## 三、跨模块锁序有向图（整合裁定）

```
【控制面】（worker / blocking pool 均可达）
registry (runtime/service.rs:15)
 ├─→ Db.read_conn (session/db/mod.rs:82)          [list/spawn/终态/queries；busy_timeout 5s]
 ├─→ Db 写执行器 (mpsc → qaqh-db-writer, recv 阻塞) [spawn/goal/list attach/truncate 泵]
 ├─→ quota.lock (OS 文件锁, 无超时)                 [spawn 准入 → 全量 JSONL replay → fsync]
 ├─→ config_io_lock (config/config.rs:701)         [session_effective_config/update]
 │     └─→ LATEST (config/watch.rs:21)             [publish 单向；反向不存在]
 ├─→ owner 泵 (mpsc recv)                          [truncate/rename/title/flush]
 ├─→ shells map → LiveShell.{control,stdin,…}      [open=PTY spawn 持锁; stdin=write_all 持锁]
 ├─→ exec manager.state → per-session core/process [delete_session: 组杀+3s Condvar]
 └─→ SessionLoop.inner                             [send_input/cancel；inner 不反向取 registry]

【工具链】（spawn_blocking 池, session_loop.rs:1615）
MGR(prepare, execution.rs:142) → FileRuntime(last_hash:209) → AUDIT(intent:262)
  → [Phase2 锁外执行: FileRuntime | JOURNAL_IO | PENDING | kill_gate→REGISTRY→per-entry | OVERLAY(read) | IMAGE(peek)]
  → MGR(finalize:325) → FileRuntime(:361) → AUDIT(result:396)
AUDIT_WRITER_LOCK → { AUDIT_APPEND_LOCK(v1), APPEND_LOCK(v2) → CHAIN }   [单向, v1/v2 顺序兄弟不互嵌]
kill_gate → REGISTRY → per-entry 族；child → {last_exit_code,status,terminal_at}（try_wait 单向）

【MCP/LSP】
MANAGER_SLOT.read/write：guard 不逃逸（clone/replace 即放），与任何锁零嵌套
mcp：cfg/conns/state/CONN_NOTIFY/SPAWN_PIDS/pgid/watchdog 全顺序拿放，零嵌套
lsp：conns → conn.state（evict_recycled_conns, manager.rs:343）唯一嵌套边，反向不存在
专属 RT 内：connect_serializer(tokio) → factory spawn → state(短) → service/session(tokio, 跨 await 合法)

【审计/日志】
任何业务锁 → SplitFileLogger（log 宏持锁写文件）：logger 不回触业务锁，无环
```

**环判定**：全图 DAG。唯一回边候选 lsp `conns→state` 的 state 出边全部指向放锁后的 tokio 锁/锁外操作。**无 ABBA、无同线程重入环、std 锁跨 await 0 命中。** 风险全部集中在「持锁慢操作 + 取锁线程身份」，而非锁序。

---

## 四、与 MUST.md v4.4 切片的交叉映射（去留判定）

| MUST 切片 | 本审计中被其自然消除的锁/阻塞点 | 不被吸收、须独立处理的项 |
|---|---|---|
| **P1-1** Runtime 统一 + 砍 250ms 轮询桥 | RT(lsp bridge:23)、RT(mcp bridge:47/:72，L-22 恒 None 随之消亡)、shared_runtime(session_loop:140)、mcp recv_timeout×4+sleep 变体(bridge:225/451、resources:252/401/780)、lsp block_on sandwich(tool:265/436)、专属 RT worker 上的 secrets/fs IO | **L-07/L-08/L-09 三个 lsp P1 独立于 P1-1，不得被其吸收延期**（键错位/退出接线/超时硬顶与 runtime 收敛无关） |
| **P1-2** SessionOwner Actor 化 | owners map 锁内线程派生(L-31)、owner 同步 recv 入口(L-03)、SessionLoop inner(备选)、GLOBAL_TRUSTED/TODO_LOCK 实例化方向(L-04/L-29)、shell 实例所有权化(L-11)、registry 分片方向(L-01.3) | list_sessions 缩边界与分派面 spawn_blocking（L-01.1/2）应**先于** P1-2 落地（P1-0） |
| **P1-3** 野生 JSON/JSONL 入 SQLite | quota.lock+全量 replay+fsync(L-02)、pending 全量覆写(L-06)、devices touch 覆写(L-20)、todo.json(TODO_LOCK 根治)、audit v1/v2 锁族(L-13)、SessionManager 空壳 | JOURNAL_IO 全局 seq 是契约（file-tools 组判 4），若 P1-3 收编 journal 三头记账则随迁；secrets.toml.lock 跨进程锁不在 P1-3 清单（保留） |
| **P1-4** 强类型 service dispatch | 账号面/服务面 worker 同步网络与 IO 的调用面根治（L-05/L-10 长期） | 短期 spawn_blocking 收口（P1-0a）不依赖 P1-4，先行 |
| **P2-2** 手写轮子清剿 | tasklist.exe→OpenProcess（L-27 与 P2-2⑥ 重合，可提前单独执行） | — |

**Runtime 计数校准**：MUST.md 称「单进程 5 套 Tokio Runtime」；本审计实测 **daemon 进程内最多 4 套**（main + mcp 专属 + lsp 专属 + 惰性 shared_runtime），client RUNTIME（client.rs:1071）属前端/桥进程，不在 daemon 内。P1-1 验收口径建议按此校准。

---

## 五、可直接作为 P1-0 加急 PR 执行的切片建议

> 入选判据（Prompt 原文）：高确定性、零契约变更、不拆原有语义、仅修复持锁跨界或异步阻塞。全部为内核空间改造，`qaqh-contract-export check --against` 必须 0 wire 实变；按 MUST §三走 `../qaqh-wt-*` worktree + GitHub PR 四联单。每项附反证思路（宪法 #18：先证旧实现红）。

### P1-0a「分派面 spawn_blocking 收口」（一刀消解 L-01/04/05/06/10 的 worker 暴露 + L-25/L-35 同刀）
- **改动点**：service_api.rs:179（dispatch）、:142/147/156（shell.* 臂）、commands.rs:541-543（同步变异臂）、control.rs:19（activity）、command.rs:139（rename）包 `tokio::task::spawn_blocking`；service.rs:1073-1098 配置轮询器与 :1029-1047 exec env reloader 的同步 IO 段同改。先例即仓内 D-4 约定（command.rs:113/166、control.rs:46、marketplace_api.rs:36）。
- **不动**：任何锁语义、任何 DTO/路由/方法表。
- **反证**：慢 IO 注入 fixture（read_blocking 闭包延迟 3s / mock IdP 挂起 5s），并发 8×慢请求 + /health 探针：旧实现 /health 超时红；新实现 <50ms 绿。
- **分支**：`fix/v4.3-p1-0a-dispatch-spawn-blocking`，标签 `[kernel-refactor][v4.3][P1-0a]`。

### P1-0b「list_sessions / session.meta 缩边界」（L-01.2）
- **改动点**：service.rs:825-901 与 handlers/session.rs:29-44——registry 锁内只取 is_running/activities 快照即放；DB 读写、逐行状态、workspace attach 全在锁外。
- **反证**：慢库 fixture 下并发 session.list（慢）+ session.activity（快）：旧实现 activity 被 list 持锁拖住红；新实现 activity 延迟有界绿。
- **分支**：`fix/v4.3-p1-0b-list-sessions-lock-boundary`。

### P1-0c「lsp 三连立斩」（L-07/L-08/L-09，独立于 P1-1）
- **改动点**：① 删 lsp SPAWN_PIDS 表，pid 经 store_connected 参数直传（adapter.rs/connection.rs）；② main.rs:105/115-119/150 三处退出补 `qaqh_lsp::shutdown_global()`（acp 分支 mcp/lsp 都补），prime_all_async 接线或删；③ connection.rs:541-544 Shutdown 请求包 `tokio::time::timeout(close_timeout)`，超时 abort driver 走 sweep；随附 watchdog 回收 return、begin_call 补闸。
- **反证**：① 两 root 并发连同 mock server，关 A 断言 B 进程组存活（旧红）；② Unix 集成测试 daemon 退出后 mock server 进程组被收割（旧红）；③ mock server 不回 Shutdown，close_timeout 内完成清扫（旧挂死红）。
- **分支**：`fix/v4.3-p1-0c-lsp-pgid-shutdown-timeout`。

### P1-0d「P2 微修打包」（可选，低风险小刀集合）
- RetryHub.drain 锁内 mem::take 放锁回调（sdk_common.rs:639-648，拆引信）；
- mcp RT 提模块级共享 cell（bridge.rs:47/:72 合一，修恒 None）；
- lsp evict 缩边界（manager.rs:340-345 候选出锁查状态）；
- FileRuntime ledger_key 挪锁外（file_state.rs:80/231 等）；
- REGISTRY captured_full 锁内取 Arc 锁外 clone（process_registry.rs:727-743）；
- v2 CHAIN recover_state 文件读挪锁外（audit/v2.rs:312→414-470）；
- tasklist.exe → OpenProcess 限时直查（platform.rs:418-420，与 P2-2⑥ 重合可提前）；
- session_loop.rs:1540 trust_folder 调用点 spawn_blocking。
- **反证**：各项用既有单测回归 + 针对性微探针（如 drain 回调内 record 的重入探针旧实现必死锁红——用超时线程断言，勿真挂 CI）。
- **分支**：`fix/v4.3-p1-0d-lock-boundary-microfixes`。

### 明确不入 P1-0（防越界）
- quota/pending/devices/todo 入 SQLite（P1-3 本体，涉及启动吸入铁闸 2）；
- owner truncate/update_metadata 异步化（改 handler 签名/泵回执语义，随 P1-2/P1-4；短期已由 P1-0a 的 spawn_blocking 保护 worker）；
- registry 分片/Actor 化、shell/信任集实例所有权化（P1-2/v4.5）；
- mcp/lsp Runtime 收敛与轮询桥拆除（P1-1 本体）；
- **任何「一刀切删锁」**：本审计未发现可安全硬删的生产锁（唯一删表项 L-07 是因为该表本身多余且有害，pid 已有正确传递路径）。

---

## 六、诚实声明（未验证项与边界）

1. 本审计为**静态只读审计**：未构建、未运行任何反证测试；所有「失陷场景」是代码级推理，反证测试是**待执行提案**。
2. Windows JobObject kill-on-close 对 LSP 孤儿的兜底行为未逐行验证（L-08 的 Unix 结论确定，Windows 侧严重度待验证）。
3. sbx-win `View::drop` 与在途 ProjFS 回调的排空语义未做实例级验证（projfs.rs:874-881）。
4. P0-3（伪随机降级）现状不在锁审计范围：config-gate 分组确认 gate jitter（纳秒时钟，非安全用途）与 secrets 临时名（pid+时间戳+原子 nonce）非密码学降级点；siwc/dsaccount util 的 CSPRNG 降级是否已清剿**未核实**。
5. codegraph 一处误报已反证：blast radius 称 `Client::connect` 有 daemon/server.rs 调用方，grep 证实该处是 `UdpSocket::connect`（server.rs:107），仓内无 Client::connect 生产调用方。
6. 子代理最终答复回传通道故障（session id 大写被路径校验拒绝），分组报告经落盘文件回收；主会话对 L-07/L-21/L-02 三项头条结论做了独立读源复核，其余结论采信分组报告的 file:line 证据链（原始报告存 `.qaqh/audit-parts/`）。
7. `todo_meta_snapshot`（store.rs:232）文档声称 runtime 每次非 todo 工具调用求值，但全仓无生产调用方——接线不存在，不构成热点（留痕防误判）。
8. pending_id 无会话归属字段（跨会话 confirm 理论可行，靠随机 id 防猜）——属权限面而非锁面，移交后续安全审计酌情处理。

---

---

## v4.4 P1 收口处置（2026-11-24 回写）

- **已消除**：`TODO_LOCK`（#35）、quota 无超时 file.lock/全量 replay/逐条 fsync（#35）、
  devices/receipts 全量覆写账本（#35）、`SessionManager` 野生文件句柄（#35）、
  L-07/L-08/L-09（#32）、L-22（#33）、L-01 复合停摆控制面路径（#30/#31）、
  per-session OS 线程（#34）、结果侧二次 JSON 往返（#36/#37）、
  L-27（tasklist.exe/kill 子进程派生 → OpenProcess/libc 原生直查，v4.5 P2-2b · ADR-0044）。
- **`OnceLock<Arc<dyn Host>>` 槽（§P2-1）**：v4.4 按既有先例新增 **TodoHost** 同款槽
  （#35，端口化接 SQLite 的定案，理由见 PR 正文）——P2-1 消除清单现为
  GoalHost/SubagentHost/GitxHost/**TodoHost** 四件套，v4.5 一并处置。
- **`unload_idle_sessions` 句柄回收**：铁闸 3 验收由 #37 补交
  （`OwnerDirectory::take_session` 接线 + 回归测试）；`take_session` 只摘目录条目，
  不触碰 audit/journal 面。
- **未动项**（顺延 v4.5，逐项处置见 `V4_4_EXECUTION_PLAN.md` §六）：
  L-13/L-15/L-16/L-21/L-23/L-29（L-27 已于 P2-2b 清零）；`audit v1/v2` 与 `JOURNAL_IO` 按所有者裁定
  未在 P1-3 收编。

---

*报告完。原始分组证据：`.qaqh/audit-parts/`（5 份，含逐锁加锁点全清单与 codegraph 验证记录）。*
