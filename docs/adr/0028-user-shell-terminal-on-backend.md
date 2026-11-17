# ADR-0028 用户 shell 终端落后端：复用 `qaqh-pty` + RingingV2 新增一条 ws 数据面流，前端 node-pty 退役

状态: **已采纳 · 切片 1 已实施（`qaqh-pty` resize/terminal_size，Windows 实测绿）· 切片 2–5 未实施**
· 起草日期 2026-11-17 · 起草人 QAQ agent · 裁定人 所有者

> 引用本 ADR 时必须看状态字段：**「已采纳」不等于「已完成」**（规则 17）。
> 切片 2–5 目前只有设计，没有任何代码。

## 背景（全部为实测事实，file:line 可查）

### 1. 现状是两份 PTY 实现，前端那份更不完整

| 侧 | 事实 |
|---|---|
| 前端 | `package.json` dependencies 里有 `node-pty 1.2.0-beta.9`；`src/main/pty-host.ts` 107 行宿主（`qaqh:pty-{create,write,resize,kill}` + `qaqh:event:app://pty-frame` + `before-quit` 清理 + `canControl` 校验）；`src/main/index.ts:19,408` 已接线；`src/preload/index.ts:60-63` 已出桥；`webui/src/exec/ShellTerminal.tsx` 已按容器测量 cols/rows |
| 后端 | `crates/qaqh-pty` 已把 ConPTY 的坑踩平：`lib.rs:119-127`（master 是 HPCON 唯一宿主，spawn 即 drop = `ClosePseudoConsole` = 子进程被杀且输出丢失，即"tty=true 无输出"根因；ConPTY 输出管道写端由 master 持有 ⇒ 子进程退出**不产生 EOF**，须经 `close_terminal` 显式关）、`lib.rs:282-314`（非阻塞 master + `WouldBlock` 重试纪律）、`lib.rs:827`（`env_clear` + `spec.env` 是**完整替换表**） |

`pty-host.ts` 头注释自己已经写了正确结论：*"那边 PTY 尺寸钉死 80×24 是解析器口径；这里用户终端的 cols/rows 由 UI 测量并可 resize——两者语义不同，不要互相「顺手统一」"*。

### 2. 生命周期考据（所有者点名要查的那条）：**断联不 kill**

- `lease_store.rs:181-190` `expire_connections()` 过期时**只删记账**（`connections.remove` / `instance_of.remove`），没有任何 kill/shutdown 动作；注释 `lease_store.rs:12` 明确"连接不会永远有效，过期后需重新 open"。
- `ExecStopReason`（`qaqh-ringing-contract`）取值只有 `Interrupt | Terminate | TurnCancel | Timeout | Shutdown` —— **没有"客户端断联"这一档**。这是面纪律的机器证据，不是风格偏好。
- 真正会杀子进程的只有三条路：① `service.rs:252-266`「关闭即终结附属后台 exec(与 delete 同律)。先停 exec 再卸循环」，按 `session_tree` 逐个 `shutdown_session`；② `exec_session/manager.rs:665-672`「Stop every live child during daemon shutdown」+ `:715` 3s deadline；③ exec 自身 `Timeout` / agent 的 interrupt/terminate/TurnCancel。
- **idle-unload 不停 exec**：`service.rs` 的 `unload_idle_sessions` → `release_session_process_state` 只做 `reset_images` + memwatch 记账；`server.rs:267-293` 每 60s 读 `config.session_idle_unload_secs`，**默认 0 = 关闭**。也就是说"会话循环被卸掉而 exec 子进程继续活"已是既成事实，并且已有跨这条线的补投先例：`sweep_active_goals` / `sweep_exec_notices` / `sweep_handoff_forks` 全挂在 `server.rs:274-345` 那**一条** 60s 任务里。

### 3. 传输与闸门的现状（决定"新增一条流"的最小代价）

- 现行 RingingV2 流只有 SSE：`ringing.rs:49` 路由 → `sse.rs:34` `EventSource`；`crates/qaqh-daemon/Cargo.toml:21` 的 axum features 是 `["fs","json","query","sse"]`，**没有 ws**，全仓无 `tungstenite`/`upgrade()`。
- 控制面不是"一堆路由"：`CONTROL_PLANE_ROUTES`（`contract.rs:594-597`）只有 2 条静态路由，业务全挤在**一个 POST RPC**（`contract.rs:601`）+ `METHODS` 表（`service_methods.rs:785`）。⇒ 本仓的臃肿向量是**命令与 DTO 的数量**，不是路由数量。
- 帧纪律：游标是 `SessionEventFrame.seq: u64`（`events.rs:131-132`，注释"跨 SSE 重连单调、永不重置"）+ `attach {last_seen_seq}`（`command.rs:221-223`）；缺口 → `ResyncRequired{Snapshot}`（`stream.rs:137-163`、`:274-280`）；背压 = `broadcast`，容量 `event_broadcast_capacity()` 缺省 **16**（`config.rs:50`），`Lagged` 即 `clear()` + resync。
- 鉴权只有一个模型：`Authorization: Bearer <token>` → `auth.rs:6-18`（sha256 + `ct_eq`），scope 从 DB 读（`authorization.rs:67 store::device_scope`），`Scope::{View,Interact,Admin}`；SSE 有独立的 `sse_auth.rs:18`（注释"不得改成默认拒绝"）。`x-qaqh-client-session-id` **不参与鉴权**。
- 新增一条 wire 命令的固定成本 = **15 处**（命令枚举 + serde rename + as_str + params/result DTO + 两个 From + METHOD_KIND + TYPED_METHODS + METHODS + service_methods match + impl + 两个 route_table snapshot + capabilities/coverage/lock），并由 `command_surface.rs`、`wire_fixtures.rs`、`lts_command_surface.rs` 三道穷举闸反查；`command_surface.rs:30-35` 钉死"新增 wire 命令面必须 bump `COMMAND_CONTRACT_REVISION`"（ADR-0007 决策 3/6）。

## 决策

**D1 shell 落后端，前端 node-pty 退役。** 复用 `qaqh-pty`（`spawn_pty`/`SessionSpec`/`kill_group`/`close_terminal`，切片 1 新增 `resize`/`terminal_size`），删除前端的 `node-pty` 依赖、`pty-host.ts`、`index.ts:19/408`、`preload/index.ts:60-63` 与打包侧 asar/external 例外；`ShellTerminal.tsx` **保留**，只把 `host.pty` 换成 `shell.open` + ws 订阅。

**D2 面分离必须是机器事实，不是纪律。** shell **不进** `TimelineEntry`/DB/模型上下文，**不注册任何 agent tool**，**不复用 `ExecSessionManager` 的注册表**。理由不是洁癖：exec 会话的 owner 是 `session_id`，而 `service.rs:252-266` 规定会话 close/delete 连带杀其 exec —— 若把用户 shell 塞进同一张表，用户 shell 就间接绑进了 agent 会话的生命周期，正是要避免的耦合。shell 的 owner 取 **device 稳定身份 `instance_key`**（`lease_store.rs:24-25` 注释：`instance_key` 是 device token 反推的 `device_id`，admin 为固定 key）。

**D3 数据面允许新增一条 ws 流（所有者裁定），但约束写死。** 允许的理由：shell 字节**不属于会话事实流**，给它 `seq`/`ResyncRequired`/journal 才是污染 —— 所以它不是"同一目标的第二条路"（那正是 `docs/adr/*` 里 `qqh-widget://` 特权协议被裁撤的原因：*"同一目标两条路，属冗余攻击面"*）。硬约束五条：
1. 只加**一条**路由（进 `RINGING_V2_HTTP_ROUTES`，规则 16），不建第二套控制面；
2. 鉴权**必须复用** `auth.rs authorize_request` + `store::device_scope`，禁止在 ws handler 里另写一套 scope 逻辑（那才是臃肿的起点）；
3. 消息族取最小集，不引入分帧/合帧/压缩/游标/重放；断线即重新 attach，shell 不存历史所以无 resync 语义；
4. shell 字节**永不**进 `SessionEventFrame`（否则等于把 UI 资产写进会话事实流，D2 破防）；
5. 在 `RingingV2Capabilities` 里显式声明这条流的存在，禁止靠版本猜测（现状是 capabilities 里没有任何 transport 协商，见勘察事实）。

**D4 控制面只两条命令，不是四条。** `shell.open` / `shell.close`（+ 一个只读 `shell.list` 供抽屉重建）。stdin 与 resize 走 ws 数据面，**不**做成 `shell.write`/`shell.resize` 命令：keystroke 级 HTTP POST 往返 + JSON 转义 + 16 容量 broadcast 只会制造 resync 风暴，而且每条命令都要付 15 处的固定成本。新增一个 `MethodInfo` 形状：client-scoped、`requires_session: false`、非 admin（现表只有 `READ/READ_ADMIN/READ_SEEDED/WRITE_ADMIN` 四形状，`service_methods.rs:756-775`）。

**D5 生命周期：与 exec 面同律，不引入第三种机制。** kill 只有三条路，全部沿用既有语义 —— 显式 `shell.close`、device revoke（`device_registry.rs:166`，吊销已覆盖 lease 强失效）、daemon shutdown（`manager.rs:665-672` + 3s deadline）。**断联不 kill**（对齐事实 2）。shell 独有一条：**无人观看 idle 回收**，实现为 `server.rs:274-345` 那条既有 60s 任务里新增一次 `service.sweep_idle_shells()`，配置项 `shell_idle_reap_secs`（缺省 600，0=关），与 `session_idle_unload_secs` 同纪律。理由：终端是纯 UI 资产，raw 环形缓冲长期挂着没有意义；而这条正好复用 sweep 先例，不新造定时器/看门狗。

**D6 尺寸纪律：两边不许统一。** exec 面继续硬编码 80×24（`manager.rs:331`、`lib.rs:77-78`），因为模型侧 ConPTY 重排判据是「CUP 列参数 == 终端列数」（`exec/truncate.rs:135`），脱钩就退回「字符串随机损坏，first diff 恒在第 80 列」（`truncate.rs:131`）。shell 面吃 UI 测量值 + 活 resize，**完全绕开 `ConptyReflower`/`clean_pty_model_text`**，因此该陷阱与 shell 无关。

## 已知代价（诚实清单）

- **`COMMAND_CONTRACT_REVISION` 必须 bump**（`ringing-commands-10` → `-11`，`command_surface.rs:30-35` 是机器闸）。这与 ADR-0027 那次的"未 bump"欠账**不同**：那次是追认已发布现实，这次是主动新增命令面，没有理由复制欠账。
- ws 需要 axum `ws` feature ⇒ 新增依赖（tungstenite 族）与 `Cargo.lock` 变化，CI 三平台都要重编；本机 `cargo build --release` 不做（规则 10/20，交 CI）。
- 切片 1 的 unix 分支（`TIOCSWINSZ`/`TIOCGWINSZ`）本机是 Windows，`libc` 只在 `cfg(unix)` 依赖里 ⇒ **该分支未经编译验证**，需 unix CI 或一台 unix 主机补验。
- durable 语义变化：daemon 更新/重启会杀掉用户 shell（D5 沿用 `manager.rs:665`）。所有者已裁定「不用担心，前端更新更频繁」，故接受。

## 被否方案（留痕防重提）

1. **前端继续 node-pty**：双实现，违反反重复；所有者裁定否。
2. **复用 `exec_session` 注册表，加 `exec_session.stdin`/attach**：让用户可以往 LLM 操控的 exec 面里打字，直接违反所有者的面分离裁定；且 owner=session 使 shell 被会话 close 连带杀（`service.rs:252-266`）。
3. **全命令面（`shell.{open,write,resize,close}` + 字节走 SSE 帧）**：4 条命令 × 15 处固定成本，且 shell 字节进 `SessionEventFrame` 就是进会话事实流（D2 破防），还要背 16 容量 broadcast 的 resync 风暴。
4. **独立 `qaqh-shell` 子进程以求跨 daemon 重启存活**：第三份生命周期管理，且所有者已明确不需要该存活度。
5. **shell 输出也走 `exec_session.output` 那套 append-only 日志 + offset 游标**：把 UI 资产伪装成执行事实，等于第 2 条的另一种写法。

## 落地切片与验收

| 片 | 内容 | 验收 | 状态 |
|---|---|---|---|
| 1 | `qaqh-pty::SpawnedSession::{resize, terminal_size}` | `cargo test -p qaqh-pty` 本机 2 passed（90×30 起→132×44 读回一致；退化尺寸拒），且既有 ConPTY 回归 1 passed 未破 | **已实施** |
| 2 | shell registry（owner=`instance_key`、raw 环形缓冲、idle 回收） | 单测 open/write/resize/close/idle-reap；断言不进 DB/timeline | 未实施 |
| 3 | `shell.{open,close,list}` + 新 `MethodInfo` 形状 + authz 档 + 15 处 + revision bump + 金标准机器重切 | 三道穷举闸绿；`check --against` 绿；`[contract-refreeze]` | 未实施 |
| 4 | ws 数据面流（一条路由 + 复用鉴权 + 最小消息族 + capabilities 声明） | 鉴权矩阵测试补一条"无 token/错 scope 拒"；与 SSE 路由互不引用 | 未实施 |
| 5 | 前端切桥（删 node-pty/pty-host/preload，`ShellTerminal` 改接） | 交 qaqh-electron 仓执行，本仓只出清单 | 未实施 |
