# PLAN: Workspace 分层治理与进程内化

> 2026-08-15 事故驱动。分层原则已获用户确认，作为后续架构的顶层约束。

## 0. 事故复盘（2026-08-15，serve 死亡事件）

**现象**：前台长命令（`timeout 150 rust-analyzer`）投递后 serve 立即 RST（os error 10054），此后所有经 serve 的工具（exec/read/write/edit/grep/todo/skills）持续 `Connection refused`，仅 glob（宿主侧）与 spawn_subagent（内置通道）存活。serve 8+ 分钟未自愈。

**根因链**（代码级证据）：
1. **串行 worker 全会话互斥**（serve.rs L363-382）：长 exec 同步轮询（最长 3600s）阻塞所有会话工具；队列 64 满 → 429
2. **HTTP 线程无超时**（serve.rs L241-246 `recv()` 无超时）：每请求一线程无限挂起
3. **无取消传播**：协议无 cancel 字段；`CANCEL` 为 serve 进程内静态
4. **无稳态健康轮询**（workspace_supervisor.rs L320-381）：watchdog 只 `try_wait`，挂死不可自愈
5. **树杀生命周期**（L399-406）：daemon stop → `taskkill /T /F` 连 exec 子树一起杀
6. **回退本地从未生效**（backend.rs L176）：ureq 3 把 Connection refused 包装为 `Error::Io`（`From<io::Error>`），`Error::ConnectionFailed` 仅为自定义 connector 的 last resort——`matches!(ConnectionFailed)` 匹配不到 refused → 第一层工具可用性被 serve 单点绑架

## 1. 分层原则（用户确认，顶层约束）

工具按信任与故障域分三层，**依赖方向只允许上层依赖下层，故障只允许向下隔离**：

| 层 | 成员 | 性质 | 工程姿态 |
|---|---|---|---|
| **L1 核心** | bash/exec、edit_file_v2、glob、grep、read/write | 只依赖 fs+子进程；确定性；必须可用 | **最短路径**：进程内直调、无网络、无长等待、超时严格、失败只返回结构化错误（catch_unwind） |
| **L2 扩展** | skills、todo、image、ask、spawn_subagent 等 | 依赖外部服务/注入链；天然慢、可错 | **允许失败**：独立队列/超时（10-15s 快速失败），绝不阻塞 L1；429 可接受 |
| **L3 不可信** | MCP/ACP/LSP 对外协议 | 外部调用者输入 | **默认拒绝**：进程/任务隔离、permission 三态全走（默认 Read）、资源上限、全量审计、永不提权 |

**现状差距**（事故实证）：L1 与 L2 共用同一串行队列（13:13 rustup 下载阻塞 read_file）；todo/skills 挂 serve（todo 本可宿主侧）；serve 死 = L1 全死；glob/ask 在宿主侧存活 = 分层方向已被系统部分验证。

**特殊成员**：exec/bash 归 L1 可用性，但风险/超时按 L2 待遇（严格权限 + 默认短超时 30s + 后台化移交 + Job Object 管子树）。

## 2. 架构现状（探索结论）

```
Windows UI ── discovery/SSE ──► deepx-daemon（tokio）
  ├─ AgentRegistry：spawn worker = daemon 二进制 `agent` 子进程（env 注入 DEEPX_WORKSPACE_URL/TOKEN）
  │    worker 内：HostOnly 工具本地执行（已内嵌工具库）；Workspace 工具经 HTTP → serve
  ├─ WorkspaceSupervisor：拉起/重启 serve（Local：taskkill 树杀 | Wsl：wsl.exe + pkill）
  │    完整性校验：daemon-manifest.json SHA-256；watchdog：try_wait 500ms + 5s 退避
  └─ RingingHub：事件投影/sequencer/journal

serve 进程：tiny_http；/health /tools /execute /subagent（Bearer）
  ├─ 串行 worker（set_context + set_process_workspace → run_tool，catch_unwind）
  ├─ process 内联旁路（唯一并发路径，与 worker 有真实竞态）
  └─ /subagent 子代理注册表桥
```

**可复用资产**：`ToolExecutionBackend` trait（Local/Http 可插拔，`install_workspace_backend` 原子换）；`/execute` 请求结构 transport-neutral；worker 已内嵌工具库；supervisor WSL 模式已实现。

## 3. 紧急止血（已完成项见 git log / 本节勾选）

- [x] **止血1：回退本地修复**（backend.rs）——`is_connect_failure`：`ConnectionFailed | Io(ConnectionRefused | NotConnected)` → 安全回退本地（请求未到达 serve，无副作用）；`Io(ConnectionReset)`/TimedOut（mid-flight）→ 不回退（可能已执行）。**效果：serve 死后 L1 工具自动降级 worker 本地执行，可用性不依赖 serve**
- [x] **止血2：serve HTTP 线程 recv 超时**（serve.rs）——启用 `timeout_secs` 字段（daemon 侧已传），`recv_timeout(工具真实超时+30s)`；超时返回 200+error 结果（daemon 解析成功、不重跑），HTTP 线程释放，防连接/线程堆积
- [x] **止血3：稳态健康轮询**（workspace_supervisor.rs）——watchdog 每 500ms `GET /health`（2s 超时；/health 不经串行 worker，长 exec 不误伤），连续 3 次失败 → 杀进程走重启路径（挂死自愈）

## 4. 三阶段路线图（细化任务，验收标准见各阶段末）

### 阶段1：分层工具治理（进程内化）

**目标**：L1 工具 worker 进程内最短路径；L2 独立慢路径；消除串行单点。

| # | 任务 | 文件 | 验收 |
|---|---|---|---|
| 1.1 | **ctx 显式化**：`RUNTIME_CTX`（active_session/permission_level）与 `CURRENT_WORKSPACE` 改入 `ToolCallCtx`；`verify_active_session` 校验 proof 本身；`set_context`/`set_process_workspace` 退役 | runtime.rs, execution.rs | 并发执行两会话工具互不串味；全量测试过 |
| 1.2 | **state_delta per-call**：`file_state::take_pending()` 改函数返回值 | serve.rs, file_state.rs | 并发下账本无偷取（现 process 内联竞态消除） |
| 1.3 | **拆除串行 worker**：并发执行（catch_unwind 保留）；manager 锁拆 per-session inflight；队列/429 退役 | serve.rs, manager.rs | 长 exec 不再阻塞其他调用；健康探测不受影响 |
| 1.4 | **placement 按层重排**：L1 工具默认进程内；仅 WSL2 才启用远程 HTTP；exec 加 Windows Job Object | backend.rs, exec.rs, worker.rs, registry.rs | ✅ **部分落地（2026-08-15）**：worker 默认进程内（Local backend，L1 不经 serve）；仅 `DEEPX_WORKSPACE_MODE=wsl` 才装 Http 到 WSL2 serve。L1/L2 分层仍待细分（当前同 OS 全进程内） |
| 1.5 | **子代理注册表本地化**：worker 内注册（/subagent 桥退役） | serve.rs, subagent | 主代理 process check 查到同进程子代理 |
| 1.6 | **serve 定位转变**：从"唯一执行者"变"可选远程执行后端"（跨 OS 场景，见阶段3） | supervisor, backend | Local 模式下 serve 可停；WSL 模式仍工作 |
| 1.7 | **回归**：全量测试 + 并发语义验证（两 session 并行工具调用） | — | 全绿 + check 零警告 |

### 阶段2：外部协议（L3，默认不可信）

**目标**：对外 agent 协议接入，复用工具内核 + permission 三态，永不提权。

| # | 任务 | 验收 |
|---|---|---|
| 2.1 | **身份映射层**：连接→session 绑定表 + permission_level 配置（每连接独立身份） | 外部连接无法越权到其他 session |
| 2.2 | **MCP server**（第一刀）：`tools/list`←all_tools()；`tools/call`←execute 结构；`resources`←workspace；`prompts`←skills | 外部 LLM client 可调 L1 工具（权限内） |
| 2.3 | **ACP server**：`session/prompt`←Ringing 会话；`fs/*`←文件工具；`request_permission`←permission 三态 | Zed 类宿主可驱动会话 |
| 2.4 | **LSP server**：didOpen/documentSymbol/hover/definition←语义查询层（deepx-lsp 反转复用）；publishDiagnostics←diagnostics | 编辑器可读 DeepX workspace 语义 |
| 2.5 | 每协议：独立任务域 + 连接数/队列隔离 + 全量审计 | 一个外部连接不能饿死/拖垮内部调用 |

### 阶段3：跨进程/跨域（WSL2）

**目标**：Windows UI ↔ WSL2 daemon 稳定；跨 OS 工具执行可选。

| # | 任务 | 验收 |
|---|---|---|
| 3.1 | **HTTP 稳定性四件套**（serve 保留为远程后端时）：长请求 task 语义（立即返回 job_id + 事件推送）、cancel 字段、优雅关闭（stop accept→drain→exit）、幂等键 | 长 exec 不占连接；daemon 中断可取消 |
| 3.2 | **WSL2 模式完善**：supervisor Wsl spawn 已有（probe+pkill）；验证 WSL2 localhost 反射连通性；L1 工具在 Linux 侧原生语义（无 `\\?\`、原生 git/cargo） | Windows worker → WSL serve 执行 Linux 工具全链路 |
| 3.2a | ~~WSL 审批桥接~~ **已核实非缺口**：审批在 worker 进程内天然工作（engine_tool.rs:167 admit，Local/WSL 一致）；serve 强制 level 4（serve.rs:394）只做数据面执行、从不 AskUser——无需桥接（2026-08-15 核实） | 已端到端验证（permission_lifecycle `llm_approval_forwards_exec_via_http_backend`：审批→放行→Http backend→serve 执行） |
| 3.3 | **同 OS 场景 serve 退役**：进程内化后 L1/L2 全在 worker/daemon，serve 仅按需（远程执行/隔离需求） | Local 模式下无 serve 进程，工具全可用 |
| 3.4 | **传输可选**：localhost 反射（默认）→ vsock/镜像模式（更强隔离） | 跨边界稳定优先，隔离可选 |

### 参照架构：OpenAI Codex 双 server（2026-08-15 实地调研 F:\codex）

> **结论**：DeepX 的 `daemon + worker + serve` 三进程 = Codex 的 `app-server + core + exec-server` 形态。
> Codex 证明：**agent 留在 Windows（app-server 内嵌 core），工具执行远程化到 WSL（exec-server 独立进程）**。
> DeepX A 方案（worker 留 Windows、serve 去 WSL）即 Codex 形态——**不需要把 agent 搬到 WSL**（修正此前"远程 worker"设想）。

**Codex 拓扑（源码证据）**

| 层 | Codex | 证据 |
|---|---|---|
| Windows 主 server + 审批中枢 | app-server（内嵌 core） | app-server/src/lib.rs:455（MessageProcessor→EnvironmentManager）；app-server-daemon 仅监督进程 |
| WSL 执行后端 | exec-server 独立进程 | ws://TCP 或 stdio 监听；EnvironmentManager 用 WebSocketUrl / NoiseRendezvous / StdioCommand 连接（exec-server/src/client.rs:158-546） |
| 审批桥接 | WSL exec → JSON-RPC → core → app-server → Windows UI → 回环 | core/session/mod.rs:2365-2425; app-server/src/bespoke_event_handling.rs:587-712,1967-2019 |
| 路径 | PathUri 透传，远端 OS 解释，零转换 | remote_file_system_path_uri_tests.rs:47-96 |
| 协议 | JSON-RPC + websocket/stdio + Noise hybrid IK | exec-server-protocol; noise_channel.rs:33 |

**对 DeepX 的差距清单（按优先级）**

1. ~~WSL 模式审批桥接~~ **已核实非缺口**：审批在 worker 进程内天然工作（engine_tool.rs:167 admit，Local/WSL 模式一致）；serve 强制 level 4（serve.rs:394）只做数据面执行、从不 AskUser——WSL 下需审批操作与 Local 一样在 worker 进程内弹窗。Codex 的 bespoke_event_handling 桥接在此不需要（2026-08-15 核实）。
2. **exec 协议标准化 + 环境注册**（可选演进）：Codex exec-server 是标准 JSON-RPC + local/remote/cloud 多环境注册；DeepX serve 是自定义 HttpExecuteRequest 单环境。若 WSL 成为一等执行环境，参考此模型。
3. **路径语义收敛**（可选）：Codex 用"远端解释 PathUri"（零转换），比 A 方案的 /mnt 转换更简洁；可收敛为 serve 按自身 OS 解释路径、worker 按目标环境构造 URI。
## 5. 决策记录

| 决策 | 结论 |
|---|---|
| 分层模型 | 三层信任/故障域（L1 必须可用 / L2 允许犯错 / L3 默认不可信），用户确认 |
| 进程内化形态 | worker 内嵌工具（方案 A）：L1 本地最短路径；serve 保留为远程执行后端 |
| 外部协议顺序 | MCP → ACP → LSP server（复用 deepx-lsp 语义层） |
| 止血范围 | 三项（回退修复 / recv 超时 / 健康轮询），最小侵入直击根因 |
| WSL2 参照架构 | OpenAI Codex 双 server（app-server+exec-server，agent 留 Windows、exec 远程化、审批桥接）；DeepX A 方案即其形态；差距 = 审批桥接 + exec 协议/环境注册 + 路径语义收敛（2026-08-15 实地调研 F:\codex） |