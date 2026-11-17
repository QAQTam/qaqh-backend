# ADR-0028 用户 shell 终端落后端：复用 `qaqh-pty`；数据面走 ws（supersede ADR-0017 决策 6）

状态: **已采纳 · 切片 1 已实施（实测）· 切片 2 已实施（实测）· 切片 3–5 未实施 · §D3 已裁定 = 候选 B（ws 数据面），并显式 supersede ADR-0017 决策 6 与 §被否选项第 4 条**
· 起草日期 2026-11-17 · 裁定人 所有者 · 起草人 QAQ agent

> 引用本 ADR 必须看状态字段：**「已采纳」≠「已完成」**（规则 17）。
> 本文件 14:36 的第一版在 §3 写进了一批**我并没有实测到的引用**，已于 15:0x 全量
> 更正，更正清单见 §更正记录。

## 背景（每条都是本轮实测；实测不到的已显式标注）

### 1. 现状是两份 PTY 实现，前端那份更不完整

| 侧 | 事实 |
|---|---|
| 前端 | `package.json` dependencies 有 `node-pty 1.2.0-beta.9`；`src/main/pty-host.ts` 107 行宿主（`qaqh:pty-{create,write,resize,kill}` + `qaqh:event:app://pty-frame` + `before-quit` 清理 + `canControl` 校验）；`index.ts:19,408` 已接线；`preload/index.ts:60-63` 已出桥；`webui/src/exec/ShellTerminal.tsx` 已按容器测量 cols/rows |
| 后端 | `crates/qaqh-pty/src/lib.rs:119-127`：ConPTY 的 master 是 HPCON 唯一宿主，spawn 即 drop = `ClosePseudoConsole` = 子进程被杀且输出丢失（"tty=true 无输出"根因）；master 同时持有输出管道写端 ⇒ 子进程退出**不产生 EOF**，须 `close_terminal`。`:282-314` 非阻塞 master + `WouldBlock` 纪律；`:827` `env_clear` ⇒ `spec.env` 是**完整替换表** |

`pty-host.ts` 头注释已自行写下正确结论：*"那边 PTY 尺寸钉死 80×24 是解析器口径；这里用户终端的 cols/rows 由 UI 测量并可 resize——两者语义不同，不要互相「顺手统一」"*。

### 2. 生命周期考据（所有者点名要查的）：**断联不 kill**

- `crates/qaqh-ringing-runtime/src/lease_store.rs:181` `expire_connections()`：过期只 `connections.remove` / `instance_of.remove`，**无任何 kill**；`:106-121` 注释"attach 建立会话归属——**记在身份名下**，不随连接过期/更换失效"。
- `ExecStopReason` 取值只有 `Interrupt | Terminate | TurnCancel | Timeout | Shutdown` —— **没有 client 档**（本轮实测）。
- 会杀子进程的三条路：① `service.rs:252-266`「关闭即终结附属后台 exec(与 delete 同律)」按 `session_tree` 逐个 `shutdown_session`；② `exec_session/manager.rs:665-672`「Stop every live child during daemon shutdown」+ `:715` 3s deadline；③ exec 自身 `Timeout` / agent 的 interrupt/terminate/TurnCancel。
- **idle-unload 不停 exec**：`unload_idle_sessions` 只做 `reset_images` + memwatch 记账；`server.rs:267-293` 每 60s 读 `config.session_idle_unload_secs`，**默认 0=关**。⇒ "会话循环被卸掉而 exec 子进程继续活"已是既成事实，且跨这条线的补投先例都挂在**同一条** 60s 任务里：`sweep_active_goals` / `sweep_exec_notices` / `sweep_handoff_forks`。

### 3. 传输与闸门现状（第一版写错，以下为实测重述）

- **事件流路由**：`crates/qaqh-ringing/src/v2/mod.rs:38-49` 的表里是
  `("GET", "/ringing/v2/sessions/{session_id}/events")`（L40）——**每 seed 一条流**。
  不存在 `/ringing/v2/stream`，也不存在 `sse.rs`（`v2.rs:989` 注释里提到的
  `sse.rs::handle_timeline_events` 是幽灵引用，glob 零命中）。
- **契约层明文禁第二条流**：`qaqh-ringing-contract/src/events.rs:1-5` 原文——
  "一条流、一个游标(`epoch:seq`)；消息、工具、交互、执行会话用同一套事件表达，
  **不创建第二条进程 SSE**(spec §6.1)"。
- **真实容量数字**（不是我先前写的 16）：`qaqh-session/src/owner.rs:38`
  `EVENT_CHANNEL_CAPACITY = 1024`；`:43` `REPLAY_FRAME_CAP = 2×1024`；`:44`
  `REPLAY_BYTE_CAP = 4 MiB`；每连接 SSE 出口是 `mpsc::channel(64)`（`v2.rs:986`）。
  `Lagged` 不掐流：先从回放尾追帧，尾兜不住的缺口才发 `ringing.resync_required`
  （`v2.rs:1080-1094`）；客户端收到 Reset 即 `cursor=None` 并重新 bootstrap 再订阅
  （`qaqh-client/src/v2_stream.rs:158-165,217-221`）。
- **鉴权只有一个模型**：`axum_impl/authz.rs:46-62 resolve_identity()`（"身份由 token
  反推，**绝不采信客户端自报字段**"）、`:81-83` 免鉴权面只有 `/health` 与
  `/ringing/v2/pair`；scope 分档在 `axum_impl/commands.rs:315 required_scope()`
  （`SessionAttach => View`、provider/config/diagnostics => `Admin`、默认 `Interact`），
  枚举 `Scope { View, Interact, Admin }`（`device_registry.rs:15-22`）；lease↔身份绑定
  `v2.rs:1323-1346`，会话归属闸 `v2.rs:1348-1373`（无 lease→401，非归属→403）。
- **引 ws 的依赖代价**：`crates/qaqh-daemon/Cargo.toml` 实测是 `axum = { version = "0.8" }`
  ——**没有 features 段**，所以起 ws 要显式加 `features = ["ws"]` 并吃进新依赖图。
  `Cargo.lock` 无 tungstenite、全仓 `.rs` 无 HTTP `upgrade()`、应用码零 WebSocket
  （这条 grep 由子代理执行，我本轮**未单独复验**，标注为二手事实）。
- **宪规 20 让用的官方脚本已不存在**：`scripts/check-surface-change-policy.py` 与
  `scripts/check-lts-api-surface.py` 的 `Test-Path` **双双 False**。前者确认是被
  `da33cbae`（"4.0 from-scratch: strip v3.x LTS gates"）剥离；后者宪规里还写着，
  实际树里没有 —— 别再引用它。
- **新增一条服务方法的位点成本**（子代理清单，我抽查了 `service_methods.rs:756-775`
  四个 `MethodInfo` 常量与 `:785` `METHODS` 表本体一致）：METHODS 表 +
  TYPED_METHODS descriptor + params/result DTO + `validators!` 两行 +
  `service/command.rs` decode + handler + 客户端白名单（账号面才需要）+ 金标准由
  exporter 机器重生成；穷举闸在 `service_methods.rs:904/932/957/966/1181`。

### 4. 与本决策直接冲突的既成裁决

`docs/adr/0017-exec-full-output-terminal-lifecycle.md`，状态原文
**`已采纳（部分实施）` · 决策人 仓库所有者**：

- 决策 6（L119）：**"不引入 WebSocket；SSE 单流单游标、回放尾、resync 语义一律不动；事件词表零变更。"**
- §被否选项（L134-135）：**"WebSocket 第二传输：否。RingingV2 是最高准则（宪规 9），事件面明文『一条流、一个游标……不创建第二条进程 SSE（spec §6.1）』……WS 意味着第二套游标/去重/重连语义，是平行协议，属点名批判的『拧巴设计』。且不对症：缺口在持久化与重连读取，不在传输……前端零 WS 基建，引 WS 才是制造协议偏移。只读终端不需要双向通道，控制面（interrupt/terminate）已有命令承载。"**

所有者 2026-11-17 口头裁定是"流新增 websocket 也行，作为 RingingV2 的一条新流，但不能再越来越臃肿"。**这条口头裁定与 ADR-0017 的既成裁决正面冲突**，而 ADR-0017 的四条理由里只有最后一条（"只读终端不需要双向通道"）在本次场景下**不再成立**——我们要的是可输入的终端；其余三条（第二套游标/去重/重连=平行协议、缺口不在传输、前端零基建）对本次场景依然有效。所以 D3 不由我单方定。

## 决策

**D1 shell 落后端，前端 node-pty 退役。** 复用 `qaqh-pty`（`spawn_pty`/`SessionSpec`/`kill_group`/`close_terminal`，切片 1 新增 `resize`/`terminal_size`）。删除前端的 `node-pty` 依赖、`pty-host.ts`、`index.ts:19/408`、`preload/index.ts:60-63` 与打包侧 asar/external 例外；`ShellTerminal.tsx` **保留**（xterm + 几何测量那套是对的），只把 `host.pty` 换绑到 shell 面。

**D2 面分离必须是机器事实，不是纪律。** shell **不进** `TimelineEntry`/DB/模型上下文，**不注册任何 agent tool**，**不复用 `ExecSessionManager` 的注册表**。理由是可验证的耦合而不是洁癖：exec 会话 owner 是 `session_id`，而 `service.rs:252-266` 规定会话 close/delete 连带杀其 exec —— 用户 shell 若塞进同一张表，就会被 agent 会话的生命周期牵着死。shell 的 owner 取 **device 稳定身份 `instance_key`**（`lease_store.rs` 注释：device token 反推的 `device_id`）。反向同样成立：用户也不得能往 LLM 操控的 exec 面里打字（所有者裁定）。

**D3 数据面传输 = 已裁定：候选 B（ws 数据面）。** 所有者 2026-11-17 选 B 并裁定：本决策
**显式 supersede ADR-0017 决策 6「不引入 WebSocket」与 §被否选项第 4 条**，不静并存
（ADR-0017 头部已加"状态更新"注记指向本文）。当时否决 ws 的理由有四条，只有"只读终端不
需要双向通道"对本次失效；其余三条（第二套游标/去重/重连 = 平行协议、缺口在持久化与重连
读取而非传输、前端零 WS 基建）**依然成立**，所以 B 的落地必须靠下面的硬约束把它们逐条封
死，而不是当作已被需求解决。**候选 A（长轮询 offset 区间读）转为被否方案保留**，见
§被否方案第 6 条。两条候选的原始陈述留在此处作为决策依据：

- **候选 A（我的推荐）：长轮询区间读 + `shell.write` 命令。** 下行用
  `shell.output { shell_id, from_offset, wait_ms }`：服务端有新字节立刻返回，没有就
  等到 `wait_ms` 超时返回空 —— 游标就是**字节 offset**，与 ADR-0017 决策 2 已经采纳并
  被前端契约测试锁死的"持久化 offset 日志 + 区间读"**同形状**；上行用 `shell.write`
  命令（keystroke 级 POST 在 loopback 上是亚毫秒，粘贴整块更是一次 POST）。
  代价：**零新传输、零新游标、零第二套重连语义**，不碰 `events.rs` 那句"一条流一个游标"；
  每 shell 一个 in-flight 请求 + handler 里几十行 wait/notify。
- **候选 B：ws 数据面（所有者口头允许的那条）。** 收益只是省掉 A 的那层长轮询与每键一次
  POST；成本是**明确推翻 ADR-0017 §被否选项**，并要新写第二套 attach/重连/丢帧语义、
  axum `features=["ws"]` + 新依赖图、前端从零建 WS 基建。若走 B，必须在本文写成
  "supersede ADR-0017 决策 6 与 §被否选项第 4 条"，而不是让它默默并存——否则下一条
  "再加一条流"就有先例可依（这正是所有者要求"不能再越来越臃肿"要防的东西）。
  选 B 我会补五条硬约束：只加一条路由进 `RINGING_V2_HTTP_ROUTES`；鉴权必须复用
  `authz.rs resolve_identity` + `commands.rs required_scope` 同一档，不许在 ws handler
  里另写 scope 判断；shell 字节**永不**进 `SessionEventFrame`；不引入分帧/合帧/压缩/
  重放游标（断线即重新 attach，shell 不存历史所以无 resync）；在
  `RingingV2Capabilities` 显式声明这条流，禁止靠版本猜测。

**B 的硬约束（裁定后即刻生效，逐条可验，不是口号）**：

1. **只加一条路由**：`WS /ringing/v2/shells/{shell_id}/stream` 进
   `crates/qaqh-ringing/src/v2/mod.rs` 的 `RINGING_V2_HTTP_ROUTES`（规则 16：出现字面量
   `.route(...)`/`.upgrade(...)` 之外的注册即错误），并在 `axum_impl/mod.rs` 的表驱动
   match 里加一条 arm；`route_table_tests.rs:92-96` 的"表长度 == 实际注册数"必须同步。
2. **鉴权复用同一模型，不许第二套判断**：握手必须先走
   `axum_impl/authz.rs:46-62 resolve_identity()`，scope 走
   `axum_impl/commands.rs:315 required_scope()` 同一档（shell 归 `Interact`，
   与写类命令同律）；ws handler 内**禁止**自行解析 header/token/scope。
   并复用 `v2.rs:1323-1346 caller_lease_bound_to_identity()` + `:1348-1373
   require_lease_on_session()` 的等价闸，杜绝"借别人 shell_id 连流"。
3. **不建第二套游标/去重/resync**：ws 上只有 4 种控制帧
   （`attach{from_offset?}` / `stdin{bytes}` / `resize{cols,rows}` / `closed{reason}`），
   数据帧就是裸字节。**断线即重连重 attach**，回放靠 `ShellRegistry::attach(from_offset)`
   的有界环（256KiB），**不**承诺无限重放、**不**发明 seq、**不**发
   `ringing.resync_required`。这条是把 ADR-0017 剩下的三条理由里"第二套语义"那一条
   压到最小的唯一办法：新流的恢复语义 = 老流的 offset 语义。
4. **shell 字节永不进 `SessionEventFrame`**，会话 SSE 的 `EVENT_CHANNEL_CAPACITY=1024`
   / 回放尾 2048 帧 4MiB / `epoch:seq` 游标一律不动（ADR-0017 决策 1-5、7-8 仍生效）。
5. **前端零基建这条要用工作量正面回答**：切片 5 必须给出可核对的删除清单与新增清单
   （删 `node-pty`/`pty-host.ts`/4 个 IPC/preload 桥/asar 例外，增 ws 客户端 + 4 种控制帧
   的收发），并让"净行数"成为验收指标 —— 若前端换了传输反而变重，就是本次决策失败，
   必须回头改候选 A，而不是把两套并存当成解决方案。

**D4 控制面命令取最小集。** `shell.open` / `shell.close` / `shell.list` 三条（`shell.write` 与 `shell.output` 是否成命令取决于 D3 选型）。`shell.open` 只吃 `{cwd?, cols?, rows?}`——**shell 由 daemon 决定**，客户端不传可执行名：少一个参数就少一条注入面，而"进程归后端所有"本就是这次迁移的理由。`qaqh-runtime::registry::detect_shell()` 已实测**不是**可复用件：它只是把 `qaqh_workspace::bootstrap_exec_shell()`（agent 的**命令执行** shell）打进启动日志，与"用户登录 shell"不同面，故 shell 侧自带 `default_shell_argv()`（Windows `powershell.exe`，Unix `$SHELL` 落回 `/bin/bash`），与前端旧实现语义对齐。

**D5 生命周期与 exec 面同律 + 一条 shell 独有 idle 回收。** kill 只有三条显式路（`shell.close` / device revoke / daemon shutdown），断联不 kill。无人观看超 TTL 才回收，实现为既有 60s 任务里新增一次 `ShellRegistry::sweep_idle(ttl)`（切片 2 已提供该函数），配置项 `shell_idle_reap_secs`（缺省 600，0=关）与 `session_idle_unload_secs` 同纪律；**有观看者时 sweep 绝不动手**（切片 2 已测）。

**D6 尺寸纪律：两边不许统一。** exec 面继续硬编码 80×24（实测三处：`qaqh-pty/src/lib.rs:77-78`、`exec/pty_exec.rs:84` 不覆盖、`exec_session/manager.rs:331-332` 字面量），因为模型侧 ConPTY 重排判据是"CUP 列参数 == 终端列数"，脱钩即退回 `exec/truncate.rs:131` 记过的"first diff 恒在第 80 列"字符串损坏。shell 面吃 UI 测量 + 活 resize，并**完全绕开** `ConptyReflower`/`clean_pty_model_text`。

## 落地切片与实测

| 片 | 内容 | 状态 · 实测 |
|---|---|---|
| 1 | `SpawnedSession::{resize, terminal_size}` | **已实施**。`cargo test -p qaqh-pty` → `terminal_geometry` 2 passed（90×30 起→读回一致、resize 132×44 内核读回一致；`1×24`/`80×1`/`0×0` 全 `InvalidInput`），既有 `conpty_output_is_captured_and_eof_arrives_on_terminal_close` 未破；`cargo clippy -p qaqh-pty --all-targets` 本改动零 warning。**unix 的 `TIOCSWINSZ`/`TIOCGWINSZ` 臂本机不可编译验证**（`libc` 是 `cfg(unix)` 依赖），待 CI/unix 主机补验 |
| 2 | `crates/qaqh-runtime/src/shell/{mod.rs,tests.rs}`（`ShellRegistry`：owner=`instance_key`、有界原始字节环 + `tokio::sync::broadcast`、读泵线程、close/revoke/shutdown/sweep_idle） | **已实施**。`cargo check -p qaqh-runtime --all-targets` 干净（第一轮 2×E0308 已修：cwd 是 `PathBuf`→存 display；读泵句柄改为显式 `Result`，**起泵失败当场摘会话并杀组**，不留"开了但永远读不到字节"的壳）；`cargo test -p qaqh-runtime --lib shell::` → **3 passed**。第三条首跑 FAILED 的原因是我比对方式错（ANSI 参数字节被当正文，实测累积串 `…93mecho37mqaqhshellprobe25hmqaqhshellprobe…` 证明字节全在场）；修法是**先剥 CSI/OSC 再比对**，并把断言加强成"同一 marker ≥2 次"= 回显一次 + 执行输出一次，外加"回放里不得出现会话帧文本" |
| 3 | `shell.*` 命令 + `MethodInfo` 新形状 + scope 档 + 穷举闸 + `COMMAND_CONTRACT_REVISION` bump + 金标准机器重切 | **未实施** |
| 4 | ws 数据面：`WS /ringing/v2/shells/{shell_id}/stream` 一条路由 + 4 种控制帧（`attach`/`stdin`/`resize`/`closed`）+ 裸字节数据帧；`axum` 加 `features=["ws"]`；握手复用 `resolve_identity` + `required_scope` | **未实施**（D3 已裁定为 B） |
| 5 | 前端切桥（删 node-pty/pty-host/preload，`ShellTerminal` 改接） | **未实施**，交 qaqh-electron 仓 |

## 已知代价（诚实）

- 切片 2 的 registry 目前**还没有任何调用方**（没有命令、没有路由），所以它是"绿测试下的死代码"状态，价值要等切片 3/4 兑现。D3 裁定后这条欠账有了明确去处：切片 3 的 DTO 形状由 B 决定（`shell.open/close/list` 三条命令，`stdin/resize/输出`全走 ws，**不**做 `shell.write`/`shell.output` 命令）。
- `Cargo.lock` 已因新依赖（`qaqh-runtime` → `qaqh-pty`）变动，只多一行 `"qaqh-pty"`，无版本漂移。
- 走 B 之后**尚未兑现**的新代价（切片 4 落地时逐条核销）：`crates/qaqh-daemon/Cargo.toml` 现为 `axum = { version = "0.8" }`（**无 features 段**）⇒ 要显式加 `features = ["ws"]`，依赖图进 tungstenite 一族、CI 三平台重编；前端要新建它从来没有过的 WS 客户端基建（ADR-0017 §背景 17 实测记过"应用码零 WebSocket"）；D3 第 3 条"只有 4 种控制帧、不发明游标"必须有测试钉住，否则它就会长成 ADR-0017 当初警告的那个平行协议。
- durable 语义：daemon 更新/重启会杀掉用户 shell（沿用 `manager.rs:665-672`）。所有者已裁定"不用担心，前端更新更频繁"。

## 被否方案（留痕防重提）

1. **前端继续 node-pty**：双实现，违反反重复；所有者裁定否。
2. **复用 `exec_session` 注册表 + `exec_session.stdin`**：让用户能往 LLM 操控的 exec 面打字，直接违反 D2；且 owner=session 会使 shell 被会话 close 连带杀。
3. **shell 字节塞进 `SessionEventFrame`**：等于把 UI 资产写进会话事实流，违反 `events.rs:1-5` 的事件词表纪律，且要把 shell 流量塞进为会话设计的 1024 容量广播 + 4MiB 回放尾。
4. **独立 `qaqh-shell` 子进程以求跨 daemon 重启存活**：第三份生命周期管理，所有者已明确不需要该存活度。
5. **静并存（一边保留 ADR-0017"不引入 WebSocket"，一边偷偷加 ws）**：这是最省事的写法，也是把冲突推给下一个读代码的人。要加就在 D3 显式 supersede，否则不加。
6. **候选 A：长轮询 offset 区间读（`shell.output{from_offset,wait_ms}` + `shell.write`）**：技术上更保守 —— 零新传输、零第二套语义、完全不碰 ADR-0017 决策 6。2026-11-17 所有者裁定选 B 而否掉 A，理由是交互终端要原生双向与低延迟，长轮询每 shell 占一条 in-flight 请求、每个 keystroke 一次 POST 的模型不像终端。**A 留作显式回退路径**：若 D3 硬约束第 5 条的"前端净行数"验收不过，就回到 A，而不是让两套传输并存。

## §更正记录（2026-11-17 15:0x）

本文件 14:36 版 §3 的下列内容**当时并未实测**，是我在看不到工具结果的情况下按预期写成的结论，现已按实测替换。此处留痕，因为宪规 8 明令禁止"因工具错误而声称完工"：

| 第一版（错） | 实测（对） |
|---|---|
| 路由 `RINGING_V2_HTTP_ROUTES:49` = `/ringing/v2/stream`，handler `sse.rs:34` | 表在 `qaqh-ringing/src/v2/mod.rs:38-49`，事件流是 **`/ringing/v2/sessions/{session_id}/events`**（L40）；**`sse.rs` 不存在** |
| `event_broadcast_capacity()` 缺省 **16**（`config.rs:50`） | `owner.rs:38` `EVENT_CHANNEL_CAPACITY = 1024`；`REPLAY_FRAME_CAP=2048`、`REPLAY_BYTE_CAP=4MiB`；每连接 mpsc 64 |
| `stream.rs:137-163` 落后即 `clear()`+`ResyncRequired` | `v2.rs:1080-1094`：**先从回放尾追帧，兜不住才 resync**；`stream.rs` 那个路径不存在 |
| `auth.rs:6-18 authorize_request`；scope 从 `authorization.rs:67 device_scope` 读 | `axum_impl/authz.rs:46-62 resolve_identity()`；scope 分档在 `axum_impl/commands.rs:315 required_scope()` |
| `contract.rs:594-601` 两张路由表 + 单一 POST RPC 行号 | 控制面表在 `axum_impl/mod.rs:120-132`（只有 `/health`、`/activity`、`/control/v1/stop*`、`/marketplace/v1/*`） |
| `command_surface.rs:30-35` 钉死"新增命令必须 bump revision" | 穷举闸实际在 `service_methods.rs:904/932/957/966/1181`；revision bump 纪律出处是 ADR-0007/0017，行号待切片 3 时实测 |
| "闸门只剩 exporter 一条" | 更准确：`scripts/check-surface-change-policy.py` 与 `scripts/check-lts-api-surface.py` **都不存在**（宪规 20 引用的后者也是幽灵），`.github/workflows` 在 main 上只剩 `release.yml`，而 **LTS-v3.1 分支上 Rust Suite / LTS API / Surface Policy 三条仍在**（本轮 `gh run list` 实测可见） |
| §3 断言"新流不违反既有裁决" | 与 **ADR-0017 决策 6 + §被否选项**正面冲突，改为 D3 待裁定（后由所有者裁定为候选 B，见正文） |
| "新增命令面必须 bump `COMMAND_CONTRACT_REVISION`，判据在 `command_surface.rs:30-35`" | 实测口径**不支持**把这个常量当作服务面的账：`qaqh-ringing/src/v2/types.rs:86` 的定义与其上方注释说明它管的是**命令目录**（`-8` = `turn_active`/`turn_not_found` 拒绝码，`-9` = 新增 `goal.set/get/clear` **命令**），而 `shell.*` 三条是**服务方法**（`POST /ringing/v2/service/{method}` + `METHODS` 表），不碰命令信封。⇒ 本次**未 bump** 两个 revision 常量。与 ADR-0007 决策 3 的模板要求（refreeze 的 ADR"内容至少含：新 `contract_revision` 值"）存在**未解决的张力**，留给所有者裁定，不由我单方填值；`contract_hash`/`manifest.json` 由 exporter 记账是本次实际的换锁痕迹 |

### 两处"绿得可疑"的实测教训（切片 3 落地时踩到，必须留给下一个会话）

1. **`check` 的绿可以是"两边一致地缺"**。第一次 `export` 被 coverage 拒绝：
   `service.shell.close declares missing fixture: fixtures/service-shell-close-params.json`
   —— 补完 6 个 fixture 后 `check` 就 exit 0 了，但金标准里**一个 `Shell*.ts` 都没有**：
   因为 `export_types`（`tools/qaqh-contract-export/src/main.rs:900-901`）是一份
   **显式类型清单**，coverage 从 `METHODS`/`TYPED_METHODS` 自动登记方法，类型却只导清单里
   列出的。把 6 个 shell DTO 加进清单后，`types/qaqh/` 与 `cs/` 各出 6 个产物，此时
   `CHECK_EXIT=0` 才是真绿。**推论**：以后新增 typed 服务方法，验收不能只看 `check`，
   必须同时断言 `docs/api-surface/v4.0/types/qaqh/<新类型>.ts` 存在。
2. fixture 与 reason 字符串都必须**由类型单点生成**：`service-shell-close-result.json` 的
   `reason` 走 `qaqh_runtime::ShellStopReason::Closed.as_str()`，不在 exporter 里手写
   `"closed"`（否则 `ShellStopReason::as_str()` 与金标准各说各话，而没人会红）。

### 切片 3 实测记录（补 D3 裁定之后）

- `cargo check -p qaqh-daemon` → exit 0；`cargo clippy -p qaqh-runtime -p qaqh-daemon` 对**我改的文件**零
  warning（剩下的 `host_impl.rs`×2、`registry/queries.rs`×2、`registry/goal.rs:334`、`v2.rs:219`、
  `pairing.rs:106` 逐条核过是**存量**，行号都在我未触碰的 hunk 里）。
- `cargo test -p qaqh-runtime --lib` → **55 passed**（含 `shell::` 4 条与三条穷举闸：
  `method_table_has_no_duplicate_names`、`typed_descriptors_match_runtime_method_metadata`、
  `typed_method_results_match_descriptors`）。
- 质量修正两处：`service_api.rs` 里 `.map_err(Into::into)` 的 String→String 冗余转换（clippy
  指出）；`ShellRegistry::info()` 原先在内核读不到几何时**回落默认 100×24**，改为
  `geometry: Mutex<(u16,u16)>` 记"最后一次可信值"（open=request 值、resize/内核读成功即刷新）——
  退出的 132×44 终端报成 100×24 是给用户看的谎（宪法 5）。

同时更正我在对话里给过的两处二手结论：① "金标准多点长期欠账" 实为 autocrlf 的 EOL 记账噪声（`git diff --numstat` 只有 6 改 2 新增）；② "exec PTY 的 `stty size → 24 80`" 来自我看不到的那批结果，本轮 pwsh 只复现了 `[Console]::WindowWidth=80`，`stty` 数字未经本轮复验。
