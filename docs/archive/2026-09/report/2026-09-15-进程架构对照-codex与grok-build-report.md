# 进程架构对照：TUI 与 agent 运行时（codex / grok-build / deepseek-harness）

> 读者：决定 qaqh 的 TUI 与 daemon 要不要「缝合」的人。
> 本文每条都是**实测**（附 `文件:行号`），素材来自三份本地检出：
> `~/Downloads/codex`、`~/Downloads/grok-build`、`~/Downloads/deepseek-harness-master`。
>
> 起因：qaqh 当前是两仓两进程（TUI 自动拉起 daemon），痛点是**不太好运行**。
> 疑问是「能不能合成一个二进制，同时保住崩溃隔离与协议纪律」。

## 0. 一句话结论

**两家都做成了「一个二进制、两种模式」，而且默认就是同进程。**

- 他们满足「一个二进制 + UI 崩不带走 agent」的方式，**不是**在同进程里做隔离，
  而是**用同一个二进制把自己再 exec 成第二个进程**。
- 没有任何一家试图「既要同进程又要崩溃隔离」——他们承认这是二选一，然后让你选。
- **协议纪律在两种模式下都保住**：同进程用类型化 channel（零序列化），
  跨进程用 JSON-RPC/socket，但**共用同一套协议类型**。

## 1. codex：一个 `codex` 二进制，启动时三态选择

`codex-rs/cli/src/main.rs:148-180` 的 `Subcommand` 覆盖 TUI / `exec` / `app-server` /
`app-server daemon` / `apply_patch` / sandbox 等——**一个可执行文件承担全部角色**。

进程模型由一次启动决策选定（`codex-rs/tui/src/lib.rs:299-304`）：

```rust
pub(crate) enum AppServerTarget { Embedded, LocalDaemon { .. }, Remote { .. } }
```

| | 嵌入（默认） | daemon / `--remote` |
|---|---|---|
| 进程 | 一个 | 两个；daemon 用 `setsid()`（Win 用 `DETACHED_PROCESS \| CREATE_BREAKAWAY_FROM_JOB`）**分离启动** |
| 传输 | **类型化 mpsc，零序列化** | Unix socket / WebSocket / stdio 上的 JSON-RPC 风格协议 |
| 崩溃隔离 | **无**（shared fate） | TUI 退出后 server 存活 |
| 多客户端 | N/A | 支持；`ConnectionId` 作用域、`thread/resume`、`thread/unsubscribe`、pidfile + 启动锁 |

**关键实现点**：

- 探测优先，**不自动拉起**：`maybe_probe_default_daemon_socket`
  （`codex-rs/tui/src/lib.rs:461-496`）只探测
  `$CODEX_HOME/app-server-control/app-server-control.sock` 是否存在；不存在就
  **退回嵌入**（`:512-541`，`:520-524` 是隐式 daemon 失败后的回退）。
- daemon 怎么起：`Command::new(&codex_bin)`
  （`codex-rs/app-server-daemon/src/backend/pid_start.rs:81, 236`）——**同一个二进制
  把自己再 exec 一次**。
- 同进程那层被明确描述为「避免进程边界」
  （`codex-rs/app-server-client/src/lib.rs:289-299`），且 README 说明
  **信封形状故意保留 JSON-RPC 风格**、但热路径是类型化的（`app-server-client/README.md:27-43`）。
- **崩溃隔离的代价写在 UI 上**：`DisconnectInfo`（「你可以重连 / 停止」）只对分离模式
  生成，对 `Embedded` 直接返回 `None`（`codex-rs/tui/src/app/exit_summary.rs:32-64`）
  ——因为只有分离的 server 能在 TUI 退出后存活。

## 2. grok-build：一个 `xai-grok-pager` 二进制，默认同进程，可选 leader

**crate 结构本身就是答案**：pager 库与 shell 库都不是独立二进制，唯一的产物是
composition root（`crates/codegen/xai-grok-pager-bin/Cargo.toml:7-16`），
**同时 link 两者**。

- **默认 `Embedded`**：`MvpAgent::with_models(...)` 跑在**同进程的 worker OS 线程**
  上（`crates/codegen/xai-grok-pager/src/acp/spawn.rs:338, 347-348`），
  走 typed in-memory ACP channel（`:324` `acp_channels()`，零序列化）。
  模块头注释原文：「Simplified to only support GrokShell (**in-process**) mode」。
- **可选 `Leader`**：`cmd.arg("agent").arg("leader")` + `process_group(0)` +
  `--no-exit-on-disconnect`（`crates/codegen/xai-grok-shell/src/leader/mod.rs:1601-1652`）
  ——**还是同一个二进制重新 exec 自己**。传输是 Unix socket 上的
  u32 大端长度前缀 + JSON 帧（`leader/protocol.rs:11-45`），flock 单例在
  `~/.grok/leader.sock`（`leader/lock.rs:33-37, 76-79`）。
- **默认 off**：`resolve_leader_mode()` 的注释把优先级写死，最后一项是
  「default off」（`crates/codegen/xai-grok-pager/src/app/mod.rs:446-478`）。
- **leader 是给多客户端用的**：模块头原文——「single-leader-per-machine architecture
  where one leader process manages the agent state while **multiple clients (TUI, IDE
  extensions, headless)** communicate via Unix domain sockets」
  （`leader/mod.rs:1-33`）。断线时把 session driver **移交给下一个订阅者**
  （`leader/server.rs:1568-1586`）。
- **同进程没有隔离，且更彻底**：发布版 **`panic = "abort"`**
  （`xai-grok-pager/src/app/mermaid_worker.rs:16` 原文：「The shipped CLI profiles
  build with panic = "abort", so the catch_unwind inside […] is a no-op there.」），
  panic hook 只做终端恢复然后调原 hook（`app/mod.rs:1812-1827`）。

## 3. deepseek-harness：永远是分离的

没有 TUI 包。三个 shipped 形态都是 UI 与运行时不同进程：

| 形态 | 拓扑 |
|---|---|
| `dsh web` | 一个 Node 进程承载 agent + HTTP 服务，UI 在**浏览器** |
| Electron Desktop | renderer **spawn 出独立 host 子进程**（`apps/desktop/src/host-process.ts:109`），走 framed byte pipes + Node IPC，**不开端口** |
| WebWorker preview | 同进程，但被架构文档明确排除在应用入口之外 |

边界是 **Typert Remote 的 API/service 边界**（unary 走 HTTP POST、流走 WebSocket），
不是共享内存、也不是直接读文件。`session.page` 的定位注释是
「Read one message-aligned history page **without activating an Agent**」。

## 4. 对 qaqh 的结论

**处境是反的。** codex/grok 是每 shell 一个 CLI 工具（一个用户、一个终端、一个会话），
嵌入是自然形态，daemon 是给 SSH/远程/IDE 的**高级模式**且**默认不自动拉起**。

qaqh 的 daemon 是**主形态**：`daemon.lock` 单实例、`stop_if_idle`、
`process_group(0)` 脱离、三端（winui/web/TUI）共享——`BUG-2026-09-15-04` 修的正是
「daemon 必须比 shell 活得久」。

**所以可行的形态是：单二进制、双角色、daemon 仍是默认与主形态。**
不要抄他们的「默认嵌入」——那会同时放弃三端共享与「关掉 TUI 会话继续跑」，
而这两样是 qaqh 已经付代价换来的。

**一个值得注意的保守点**：qaqh 的 TUI↔daemon 之间隔着 HTTP/SSE，而两家的同进程模式
**根本不序列化**。做「单二进制 + 两进程」时通信仍是 HTTP/SSE——比他们更保守，
也更符合本项目刚用 G1/G2/G3 三轮焊死的协议纪律。

**借过来的一件**：单二进制 + 角色子命令（消除 `daemon_executable()` 的
`target/debug/` 猜测），daemon 用 `current_exe()` + 子命令重新 exec
（codex `pid_start.rs:81`、grok `leader/mod.rs:1603-1604` 都是这么做的）。

## 5. 未决

本文只归档对照与结论，**没有做决策**。要不要走单二进制、以及它的工作量评估，
另行决定。
