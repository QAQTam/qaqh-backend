# ADR-0028 用户 shell 终端落后端：复用 `qaqh-pty`；数据面走 ws（supersede ADR-0017 决策 6）

状态: **已采纳 · 切片 1–4 已实施（实测，见 §切片 4 落地记录）· 切片 5 未实施（§切片 5 是交给 qaqh-electron 的施工清单；前端一行未改）· §D3 已裁定 = 候选 B（ws 数据面），并显式 supersede ADR-0017 决策 6 与 §被否选项第 4 条**
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
| 3 | `shell.*` 命令 + `MethodInfo` 新形状 + scope 档 + 穷举闸 + ~~`COMMAND_CONTRACT_REVISION` bump~~ + 金标准机器重切 | **已实施**（commit `c0ed5d37`）。`cargo test -p qaqh-runtime --lib` → 55 passed（含三条穷举闸）；`cargo test -p qaqh-daemon --bins` → 158 passed；`export` + `check` 均 exit 0；`types/qaqh/Shell*`（6）+ `cs/Shell*`（6）在位。**revision 未 bump 是对的**（`COMMAND_CONTRACT_REVISION` 管命令目录，`shell.*` 是服务方法），详见 §更正记录最后一行——但那一行也说明「服务面要不要另设 revision 位」仍待所有者裁定 |
| 4 | ws 数据面：一条路由 + 4 种控制帧（`attach`/`stdin`/`resize`/`closed`）+ 裸字节数据帧；`axum` 加 `features=["ws"]`；握手复用同一套身份/ scope | **已实施**（本轮，实测见 §切片 4 落地记录）。`cargo check -p qaqh-daemon --all-targets` exit 0；`cargo test -p qaqh-runtime --lib shell::` → **6 passed**（新增两条：退出被观察并收割、已记录的原因不许被 close 改写）；`cargo test -p qaqh-daemon --bins -- shell_stream route_table` → **3 passed**（真 `axum::serve` + 真 ConPTY + 真 ws 客户端跑完整链：open→回放→打字→resize→第二个观看者回放→断联不杀→close→`closed{reason:"closed"}`）；`export`/`check` 均 exit 0，且 `types/qaqh/ShellStream{Control,Event}.ts` + `fixtures/shell-ws-{control-stdin,event-closed}.json` 确实在金标准里（不是「两边一致地缺」） |
| 5 | 前端切桥（删 node-pty/pty-host/preload，`ShellTerminal` 改接） | **未实施**，交 qaqh-electron 仓；施工清单 + 净行数验收口径 + 一条待所有者裁定见 §切片 5 |

## 已知代价（诚实）

- 切片 2 的 registry 目前**还没有任何调用方**（没有命令、没有路由），所以它是"绿测试下的死代码"状态，价值要等切片 3/4 兑现。D3 裁定后这条欠账有了明确去处：切片 3 的 DTO 形状由 B 决定（`shell.open/close/list` 三条命令，`stdin/resize/输出`全走 ws，**不**做 `shell.write`/`shell.output` 命令）。
- `Cargo.lock` 已因新依赖（`qaqh-runtime` → `qaqh-pty`）变动，只多一行 `"qaqh-pty"`，无版本漂移。
- 走 B 兑现掉的代价（切片 4 实测核销）：
  `axum = { version = "0.8", features = ["ws"] }` ⇒ `Cargo.lock` 新增
  `tungstenite v0.29.0` + `tokio-tungstenite v0.29.0`（cargo 输出原文
  `Adding tokio-tungstenite v0.29.0` / `Adding tungstenite v0.29.0`，本机一次
  `cargo check -p qaqh-daemon` 20.11s 编过；**CI 三平台重编未验证**）；
  另加 `base64 = "0.23"`（与 `qaqh-file-tools` 已声明的同版，只多一条边）与
  dev-dependency `tokio-tungstenite = "0.29"`（真 ws 客户端，与 axum 拉进来的同版）。
  前端那条代价（它从来没有过 WS 基建）**没有**在本仓兑现，转成 §切片 5 的施工清单，
  并把"净行数"设成判定线。
  D3 第 3 条的"必须有测试钉住"已落地：`shell_stream.rs` 的词表闸（四种帧名各自解析、
  `ack`/`resync_required`/`cursor`/`ping`/`frame` 全拒且拒绝理由必须报出词表）
  + `shell_ws_tests.rs` 的两条端到端。
- 一条 ws 连接会**长期占用** `MAX_CONNECTIONS=128` 的一个并发额度
  （`ConcurrencyLimitLayer`，`axum_impl/mod.rs:217`）。这与既有的会话 SSE 同类，
  不是新问题，但 shell 流活得比一次回合同久，桌面端 1–2 条无碍，多设备同连时要记得
  这条额度是被长连接吃着的。刻意**没**给它单独加限流层（再加一套配置面就是新的臃肿）。
- `service_api::shell_owner` 的签名 `Result<String, Response>` 会吃一条
  `clippy::result_large_err` warning。不加 `#[allow]`、也不装箱：这与本仓既有
  6 处同形（`v2.rs:1359 require_lease_on_session`、`authz.rs:65 require_scope`、
  `marketplace_api.rs:32/99`、`pairing.rs:106`），错误本来就是**最终响应本体**。
  单点抑制会让读代码的人以为这是新发明，反而更难核。
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

## §切片 4 落地记录（2026-11-17，裁定=候选 B 之后）

落地位置与 §D3 五条硬约束的逐条对账：

| 约束 | 落点 | 机器面 |
|---|---|---|
| 1 只加一条路由 | `qaqh-ringing/src/v2/mod.rs` 表里加 `("GET", "/ringing/v2/shells/{shell_id}/stream")`；`axum_impl/mod.rs` 表驱动 match 加一条 arm | `route_table_tests` 的长度闸 27→28 + `synth()` 补 `{shell_id}`；非升级请求经 `WebSocketUpgrade` 抽取器当场 400（不是 fallback），反向幽灵路径断言不受影响 |
| 2 鉴权复用同一模型 | `service_api::shell_owner()` —— **RPC 三条与 ws 臂调同一个函数**（`require_scope(Interact)` + `v2::caller_lease_bound_to_identity` + `leases.instance_for_session`）；`shell_ws.rs` 内零 token/scope 解析 | 握手前的四类拒绝各有一条断言：未知 shell_id=404 `shell_not_found`、view 档=403、**有效 token 借别人 `client_session_id`=401 `lease_required`**（RPC 与 ws 两条路各自验一次）、非升级 GET=400 |
| 3 不建第二套游标 | `qaqh-ringing/src/v2/shell_stream.rs`：上行 `attach{from_offset?}`/`stdin{bytes(base64)}`/`resize{cols,rows}`，下行只有 `closed{reason}` + 裸字节；**没有** seq/ack/`resync_required`；落后（`broadcast::Lagged`）= 服务端拿自己已交付水位重新 `attach`，与客户端重连是同一条路 | 词表闸在 `shell_stream.rs` 的三个测试里：四种帧名各自解析成功、`ack`/`resync_required`/`cursor`/`ping`/`frame` 全部被拒且拒绝理由必须把词表报出来、下行只承认 `closed` 一种 |
| 4 字节不进 `SessionEventFrame` | 数据面只碰 `ShellRegistry`；`EVENT_CHANNEL_CAPACITY`/回放尾/`epoch:seq` 一个字未动 | e2e 里断言 shell 的裸字节流不含 `session_event`/`"seq"` 文本 |
| 5 显式声明这条流 | `RingingV2Capabilities.shell_stream: bool`（导出 DTO），daemon `clients/open` 答 `true` | 契约面 `[contract-refreeze]`：exporter 机器重切 `types/qaqh/{RingingV2Capabilities,ShellStreamControl,ShellStreamEvent}.ts` + `fixtures/{shell-ws-control-stdin,shell-ws-event-closed}.json` + `coverage.json` 新增 `shell.stream` 记录（含 7 条 `wire_notes`） |

切片 4 顺手补掉的两处内核缺陷（都是 ws 把它逼出来的，不是顺手改）：

1. **子进程自己退出无人知晓**。ConPTY 的输出管道写端由 master 持有，子进程退出
   不产生 EOF（`qaqh-pty/src/lib.rs:119-127`），所以只盯 read 循环的实现会把用户敲了
   `exit` 的会话永远报成 `alive:true`，还要那份 256KiB 环占到 daemon 关停（`sweep_idle`
   按 D5 只动活会话）。现在每会话两条线程（读泵 + `shell-reap-*` 以 100ms 轮询
   `try_wait`，与 exec 面 `PROCESS_WATCH_INTERVAL` 同值同律），观察到退出即
   先到先得地落 `ChildExited`、摘表、关 master 放开读泵。
2. **终止原因没有单一真源**。改成注册表里的 `watch::Sender<Option<ShellStopReason>>`
   （`ShellAttach::stopped`）：先到先得（`mark_stopped`），所以已经 `child_exited` 的
   会话被 `shell.close` 收走时，回给调用方的仍是 `child_exited` —— 不是第二个版本的事实。
   观看者顺序也因此天然正确：先把尾字节搬完（broadcast 断线才 break），再落 `closed`。
   刻意**不**往裸字节流里塞哨兵帧。

一处权限收紧（切片 3 的漏口，本轮补上并留测试）：`shell.*` 臂原先只要 header 里有
`x-qaqh-client-session-id` 就把 `instance_for_session(cs)` 当 owner，于是「自己的有效
token + 猜到的别人 cs」能把 shell 挂到**别人** owner 名下并操作别人已开的壳。`shell_owner`
里加了 `caller_lease_bound_to_identity` 那一步，正是 `authz.rs` 顶部"绝不采信客户端自报
字段"的原文要求。

错误出口只用 ws 标准 close 码，不造第五种帧：**1002** = 帧形状不合法（JSON 解析失败、
`stdin.bytes` 不是 base64），**1008** = 策略拒（上行发裸字节帧、词表外的帧名、会话还活着
却被内核拒的 resize）。close reason 按字符截到 96（RFC 6455 §5.5.1 上限 123 字节，
错误文案里带路径，按字节切会切在 UTF-8 中间）。

## §切片 5：交给 qaqh-electron 的切桥清单（一行前端代码未改）

### 前提（不是建议，是既有事实）

`webui/src/lib/transport/index.ts:5` 与 `src/main/ringing-client.ts:1-12` 写明：**daemon
token 不进 webview**，全部 Ringing 传输由主进程经 `crates/qaqh-napi-client`（Rust
`qaqh-client`）承担。所以 shell 的 ws 也必须开在**主进程**，渲染层继续走现有
`invoke` + `listen("app://…")` 桥 —— 与今天 node-pty 的位置完全一样，不改信任边界。
浏览器 `WebSocket` 既不能带 `Authorization` 也不能带 `x-qaqh-client-session-id`，
在渲染层直连等于要求后端开第二条凭证通道，那是 §D3 约束 2 明文禁止的。

### 删除清单（实测行数，2026-11-17）

| 位置 | 内容 | 行数 |
|---|---|---|
| `package.json:45` + `:50` | 依赖 `node-pty@1.2.0-beta.9`、`pnpm.onlyBuiltDependencies:["node-pty"]` | 2 |
| `src/main/pty-host.ts` | 整个宿主：`qaqh:pty-{create,write,resize,kill}` 四个 IPC + `app://pty-frame` 推帧 + `before-quit` 清理 + `canControl` 校验 | 107 |
| `src/main/index.ts:19,407-408` | `import { registerPtyIpc }` + 注释 + 注册调用 | 3 |
| `src/preload/index.ts:11,58-63` | `"app://pty-frame"` 事件白名单 + `pty:{create,write,resize,kill}` 出桥 | 7 |
| `webui/src/lib/desktop-host.ts:75,174-190,209` | 事件联合项 + `PtyFrame`/`PtyBridge` + `DesktopHostBridge.pty?` | 19 |
| 合计删除 | **不含** `ShellTerminal.tsx` 内部改接 | **138** |

`electron-builder.yml:17-19` 的 `asarUnpack: ["**/*.node"]` **保留**（napi 协议客户端仍需要）。

### 新增/改接清单

1. `ShellTerminal.tsx`（现 132 行，pty 触点 11 处：L15,26,29,79-86,103,114-117,124）：
   几何测量（按容器算 cols/rows 的 0.6/1.25 那套）**整块保留**，只把
   `bridge.create/write/resize/kill` 换成 `shell.open`/`shell.close` 服务面调用 +
   桥上的 `stdin`/`resize` 发送；`app://pty-frame` 换成 `app://shell-frame`
   （`{id, data?: Uint8Array, closed?: {reason}}`）。
2. 主进程新增一个 shell 桥（替代 `pty-host.ts`）：`shell.open`/`shell.close` 经现有
   RingingClient 走服务面，ws 订阅 `GET /ringing/v2/shells/{id}/stream`，把下行裸字节
   原样转成 `app://shell-frame`，把渲染层来的 keystroke 编成
   `{"type":"stdin","bytes":"<base64>"}` 文本帧，尺寸变化编成
   `{"type":"resize","cols":C,"rows":R}`。词表只有这四个帧名，见 `coverage.json` 里
   `shell.stream` 的 `wire_notes`（金标准，机器写的，别抄第二份）。
3. 契约类型从金标准同步（不手写）：`ShellStreamControl`、`ShellStreamEvent`、
   `ShellView`/`ShellOpenParams`/`ShellCloseParams`/`ShellCloseResult`/`ShellListResult`、
   以及 `RingingV2Capabilities.shell_stream`（连接后先读这一位，**不许**按 daemon 版本猜）。

### 验收口径（净行数，D3 约束 5 的正面回答）

- 令 `Δ = (新增主进程桥 + ShellTerminal 改接的新行) − 138`。**判定线：Δ ≤ 0**。
- 若 Δ > 0（换传输反而更重），按 §被否方案第 6 条回到**候选 A**
  （长轮询 `shell.output{from_offset,wait_ms}` + `shell.write`），并开一条新 ADR 记录
  推翻 D3；**禁止**两套传输并存（那是 §被否选项第 5 条点名的"静并存"）。
- 行为验收（必须真点一次，不接受只看单测）：① 开终端立刻看到 shell 提示符（回放环生效）；
  ② 打字/粘贴有回显；③ 拖窗口尺寸后 `shell.list` 的 cols/rows 跟着变；④ 输入 `exit`
  后 UI 收到 `closed{reason:"child_exited"}` 并关页签；⑤ 关掉应用再打开，`shell.list`
  里旧壳已随 daemon 生命周期收干净（所有者已裁定不必跨重启存活）。

### 施工前置：ws 客户端放哪 —— 所有者 2026-11-17 已裁定 = **A**

两个候选，差别只在"谁承载帧编解码与重连"：

- **A（已裁定采用）：Rust `qaqh-client` 加一条 `shell_stream` 能力（cargo feature，
  默认不开）+ napi 面上加 `shellSubscribe/shellStdin/shellResize/shellClose`**。
  前端 npm 依赖零增长，TUI/Android/HarmonyOS 复用同一份协议运行时，代价是 napi 面变宽。
- **B（未采用）：主进程用 TS `ws` 客户端**（新增 npm 依赖 `ws` + 自己写 base64/帧编解码）。
  改动局限在 Electron 仓，代价是协议运行时出现第二份实现（与本仓"一份协议运行时"的
  既有取向相反）。

> 命名易混，施工前必读：这里的 **A** 指"ws 客户端放 Rust SDK"；本 ADR §D3 里的
> **候选 A** 指"长轮询 offset 区间读"（§被否方案第 6 条，只在净行数判定不过时才启用）。
> 两者无关，别把"选 A"理解成回退到长轮询。
