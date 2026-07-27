# WSL2 Tool Runner：首轮落地计划

## 1. 结论与目标

DeepX 当前把模型编排、会话状态、权限判断和操作系统工具执行放在同一 daemon
进程中。这个结构在单机 Windows 模式简单可靠，但无法让 `exec`、文件操作等开发
工具自然运行在 WSL2 的 Linux 环境。

本计划的目标是增加一个**可选的 WSL2 工具执行器**（`deepx-tool-runner`）：

- Windows 上的 `deepx-daemon` 继续作为唯一的控制面：模型循环、会话、授权、审批、
  审计、UI 事件和持久化都保持原有所有权；
- WSL2 runner 只作为数据面执行经授权的、工作区受限的工具；
- 第一阶段只迁移 `exec`，保留默认 Windows 本地执行路径，确保能随时回退；
- runner 的流式 stdout/stderr、取消、超时和最终结果必须保持现有工具调用的用户体验。

这不是把整个 daemon 搬入 WSL2。后者会使 discovery、更新、桌面生命周期和跨系统
配置都变成问题；本方案只将最需要 Linux 语义的能力移出。

## 2. 现状与根因

当前调用路径为：

```text
模型 tool_use
  -> deepx-msglp::AgentState::rebind_store
  -> deepx_tools::execution::execute_with_context
  -> 授权、工作区校验、ToolManager
  -> in-process exec/file handler
  -> Agent2Ui 进度与 ToolResult
```

关键实现位于：

- `crates/deepx-msglp/src/state/agent.rs`：把 `MessageStore` 工具请求绑定到
  `deepx_tools::execution::execute_with_context`；
- `crates/deepx-tools/src/execution.rs`：构造调用、授权、会话校验、执行与审计；
- `crates/deepx-tools/src/manager.rs`：工具注册、allowlist、取消和统计；
- `crates/deepx-tools/src/exec.rs`：进程执行及 stdout/stderr 进度流；
- `crates/deepx-proto/src/agent_protocol.rs`：`ToolExecDelta`、结果及 UI 协议。

**根因：**工具执行器依赖了 daemon 进程内的全局运行时和本机 Windows 文件/进程语义，
因此不能通过给现有函数增加一个 WSL 地址安全地跨系统调用。

## 3. 架构边界

```text
Electron renderer
  <-> Electron main <-> deepx-daemon (Windows；控制面)
                           |
                           | 受管 stdio JSON-RPC（每用户、每工作区）
                           v
                    wsl.exe -d <distro> -- deepx-tool-runner --stdio
                           |
                           v
                    WSL2 workspace（数据面）
```

### 3.1 控制面必须留在 daemon

以下内容不迁移，且不得由 runner 自行决定：

- 模型请求、tool schema、tool call ID 和 tool result 回灌；
- 会话所有权、前端审批、权限等级、Plan/Code 模式；
- 审计记录、会话 JSONL、UI 事件排序和重连；
- `ask_user`、`todo/task`、`skills`、`memory` 等 UI 或持久状态工具；
- 工具策略选择和 fallback（本地或 WSL）。

### 3.2 可迁移工具的顺序

| 阶段 | 工具 | 原因 |
| --- | --- | --- |
| P1 | `exec` | Linux shell/包管理是 WSL 的直接价值；不涉及跨系统写文件。 |
| P2 | `read`、`list`、`search`、`process_inspect` | 只读，可验证路径映射和大输出行为。 |
| P3 | `patch`（后续再考虑 create/delete） | 已落地的严格 unified diff 工具；它以 `read` hash 为基线，适合跨系统验证与审计。 |
| 不迁移 | `ask_user`、`todo/task`、`skills`、`memory` | 本质上不是 OS worker 工作。 |

P1 不得顺手迁移文件修改工具。

## 4. 传输和协议设计

### 4.1 为什么使用受管 stdio，而非 TCP/HTTP

daemon 通过 `wsl.exe` 启动 runner 并持有它的 stdin/stdout/stderr。优点：

- 不需要暴露 WSL 端口、端口发现文件或长期 bearer token；
- 仅 daemon 可向 child stdin 写入，攻击面显著小于本机 TCP 服务；
- 可复用父进程退出时终止 child 的生命周期；
- 适合单工作区、双向流式进度和取消。

不能使用“一次命令、一段文本输出”的 CLI 接口：它无法可靠区分进度、最终结果、协议
错误和取消确认。

### 4.2 新的内部协议（v1）

新建 workspace crate `crates/deepx-tool-proto`，只承载 runner IPC，不能复用或改变
Electron <-> daemon 的 `CONTROL_PROTOCOL_VERSION`。初版协议版本独立为
`TOOL_RUNNER_PROTOCOL_VERSION = 1`。

消息采用 UTF-8、长度前缀帧（4-byte big-endian length + JSON），上限 1 MiB；工具
输出不塞入单个请求帧，而通过 `progress` 分片传送。所有消息均带 `request_id`；执行相关
消息带不可复用的 `call_id`。

```text
Daemon -> Runner
  hello       { protocol_version, runner_nonce, workspace, requested_capabilities }
  execute     { request_id, call_id, tool:"exec", args, authorization }
  cancel      { request_id, call_id, reason }
  shutdown    { request_id }

Runner -> Daemon
  hello_ack   { protocol_version, runner_version, capabilities, workspace_identity }
  progress    { call_id, stream:"stdout"|"stderr", seq, chunk }
  result      { request_id, call_id, success, content, truncated, exit_code, files_affected }
  cancelled   { request_id, call_id }
  error       { request_id?, code, safe_message }
```

协议约束：

- `seq` 对同一 `call_id` 单调递增；daemon 只接受连续分片，缺失或重复即失败并停止该调用；
- `result` 只允许一次；其后到达的 progress 必须丢弃并计入诊断；
- runner 仅支持 hello 协商出的 capability，未知字段忽略，未知消息类型失败关闭；
- daemon 与 runner 的协议版本不兼容时 fail closed，并按配置回退到本地工具或向用户报错；
- runner 不记录完整参数、命令、token 或 output 到日志；错误文本必须脱敏且有长度上限。

未来改变任何已发送字段时，按 `deepx-dev-audit` API policy 走 additive/negotiated
兼容，而不是静默改写 v1 语义。

### 4.3 授权模型

daemon 先沿用现有 `deepx_tools::authorization::admit` 做用户可见的权限决策。仅在授权成功
后才向 runner 发送 execute。runner 仍必须独立执行防御性校验，不可将 `authorized=true`
视为安全证明。

`authorization` 是短期、一次性 capability，最少包含：

- `call_id`、工具名、规范化参数哈希、会话 ID；
- WSL workspace canonical root、允许资源根、权限等级；
- 过期时间和 runner nonce。

P1 中 capability 仅在受管私有 stdio 上传输，runner 必须核对 call ID、参数哈希、workspace
和过期时间。不要把 capability 写入环境变量、命令行、审计、会话消息或临时文件。P2/P3
开始前需要决定是否以会话内随机密钥 MAC 该 capability；若 runner 会被复用或可被非父
进程连接，MAC 是强制项。

## 5. 工作区与进程语义

### 5.1 工作区映射

配置中保存用户明确选择的 WSL distro 与 WSL workspace，例如：

```toml
[tool_runner]
mode = "wsl"
distro = "Ubuntu"
workspace = "/mnt/f/DeepX"
```

不要由字符串替换把 `F:\DeepX` 猜成 `/mnt/f/DeepX`。首次启用时由 daemon 调用受限的
runner `hello`，让 runner 返回 canonical Linux root 和文件系统 identity；Windows 侧也
canonicalize 用户工作区。二者必须与用户确认的映射一致才启用。

对 P1，runner 将该路径作为唯一 working directory；拒绝 `..` 逃逸、绝对路径绕过、
符号链接逃逸和不在 root 内的 `cwd`。P3 文件写入还必须处理：

- Windows reparse point 与 Linux symlink 的差异；
- 大小写差异、换行符和文件权限；
- 临时文件和 rename 原子性的卷边界；
- 返回给 Electron 的路径必须映射回 Windows canonical path。

### 5.2 取消、超时、断线

- daemon 生成每个调用的 deadline，并将剩余时间交给 runner；runner 不得无限执行；
- `cancel` 后 runner 要终止进程树，排空 pipe，并在规定时间内回复 `cancelled` 或 `result`；
- runner 崩溃、EOF、协议损坏或超时时，daemon 将当前 call 标记失败，发送一个结构化
  ToolResult，不能重放写操作；
- P1 `exec` 可以由用户/模型显式重试；P3 写操作绝不自动重试；
- daemon 退出时关闭 stdin 并在有限等待后终止 `wsl.exe` child；不管理用户自己启动的 WSL
  进程。

## 6. 实施阶段

### 阶段 A：设计冻结与契约测试

1. 在 `crates/deepx-tool-proto` 定义消息、错误码、帧编码和版本常量。
2. 编写 JSON round-trip、frame size、未知字段、版本拒绝、序列号和单终态测试。
3. 记录 IPC 是新内部协议；不改变现有 daemon WebSocket 协议，也不升级其版本。

**完成条件：**消息 schema、状态机和 mixed-version 回退语义经评审确认。

### 阶段 B：runner 骨架

1. 新建 `apps/deepx-tool-runner` 或 `crates/deepx-tool-runner` 二进制 crate，并加入 workspace。
2. 实现 `--stdio`，严格只把 protocol 写 stdout；运行日志仅写 stderr 或受控本地日志。
3. 实现 hello、健康检查、workspace canonicalization、shutdown 及结构化错误。
4. 将 runner 打进开发和 Windows 安装包可访问的位置；先不默认启用。

**完成条件：**Windows 可通过 `wsl.exe -d <distro> -- <runner> --stdio` 启动，并完成 hello
后正常退出；任何错误不会污染 stdout 协议。

### 阶段 C：daemon runner client 与 `exec` 垂直切片

1. 在 `deepx-tools` 抽象 `ToolExecutionBackend`：`Local` 与 `WslRunner` 两种实现；默认
   `Local`，不得改变现有用户行为。
2. 将已有授权/审计留在调用端；将 `exec` handler 的进程执行部分搬入 runner 侧。
3. daemon client 处理 spawn、hello、request correlation、progress 转 `Agent2Ui::ToolExecDelta`、
   result 转现有 ToolResult、超时和 cancel。
4. 加受控配置入口；仅开发者显式开启 WSL 模式，配置缺失、runner 不可用或版本不兼容时回退
   本地 `exec` 并给出可诊断状态。

**完成条件：**同一 `exec` 调用在 Local 与 WSL 模式下都有正确 stdout/stderr、最终 exit code、
取消和 UI 结果；模型上下文中的 ToolResult 格式不变。

### 阶段 D：试用与只读工具

仅在真实 WSL 开发工作区灰度启用。稳定后迁移只读工具，验证路径映射、中文 UTF-8、巨大
目录、权限拒绝与进程巡查。写工具仍保留本地。

### 阶段 E：写入工具（单独评审）

文件编辑需要单独的安全设计审查、双端路径一致性测试和恢复方案；未完成前禁止默认迁移。
不要迁移历史 `edit`、`edit_block`、`write` 语义。`patch` 已作为 additive 工具落地：它只修改一个
既有文本文件，要求 workspace 相对路径、严格 `--- a/<path>` / `+++ b/<path>` unified diff、完整
`read` hash 与精确 hunk context；它拒绝 stale 基线、路径逃逸和链接解析到工作区外的目标。WSL 阶段应
复用该契约，并采用“runner 写入 + daemon 回读校验 hash/diff”的两阶段结果确认；失败时让用户看到
不确定状态而非自动重试。

## 7. 验收与测试矩阵

| 类别 | 必测内容 | 通过标准 |
| --- | --- | --- |
| 协议单测 | 编解码、1 MiB 限制、未知字段、版本不匹配、重复 result、乱序 seq | fail closed；不 panic、不泄露 payload。 |
| runner 单测 | workspace canonicalization、路径逃逸、deadline、取消、进程树终止 | 根外路径/软链接逃逸均拒绝。 |
| daemon client 测试 | hello、spawn 失败、EOF、timeout、progress 合并、回退 Local | 每个调用恰有一个最终 ToolResult。 |
| 工具契约测试 | `exec` 成功、非零退出、stdout/stderr、UTF-8 分片、超大输出截断 | 保持现有 LLM 截断与 UI streaming 语义。 |
| 安全测试 | 审批拒绝、过期 capability、call/args hash 不符、跨 workspace、日志扫描 | runner 不执行；日志/审计无完整命令敏感值或 capability。 |
| 集成测试 | Windows daemon + WSL distro + `/mnt/f` 工作区 | hello、exec、cancel、断线后的会话恢复均通过。 |
| 回归测试 | 未配置 WSL、WSL 未安装、distro 不存在、runner 版本不匹配 | 默认本地工具不退化。 |
| 打包测试 | 开发、安装包、更新后 runner 发现与版本协商 | 不会误启动或替换用户自管 WSL 程序。 |

最低命令验证：受影响 crate 的 `cargo test` 与 `cargo check`，桌面端 `pnpm --dir
apps/desktop typecheck`、聚焦状态/UI 测试，以及 Windows+WSL 手工 smoke。开始 P3 前须增加
写文件的恢复、路径安全和断电/中断测试。

## 8. 非目标、风险与回滚

首轮不做：整个 daemon 的 WSL 化、远端 TCP runner、多机器 worker、全部工具迁移、自动
执行未审批的高风险命令、跨文件系统自动同步。

主要风险是跨系统路径与文件安全，而非 IPC 本身。对策是 P1 只做 `exec`、默认关闭、每次
worker 启动进行版本/工作区协商，并且保留 Local executor。

回滚开关是 `tool_runner.mode = "local"`；该开关只影响后续工具调用，不修改会话消息格式
或 daemon 控制协议。WSL runner 故障时，已经开始的调用报告失败且不自动重放；后续调用
可由用户选择 Local 模式重试。

### Patch 工具演进

首代 `patch` 保持单一、扁平 schema：`path`、`patch`、`expected_hash` 和可选 `dry_run`。
系统提示负责教所有模型使用 `read -> patch(dry_run=true) -> 检查结果 -> patch(dry_run=false)`；
这比扩充多个工具 schema 更适合尚未专门训练过该工具的模型。

将来若需要显式状态机，仍只扩展同一个工具为 `patch(action: "preview" | "apply", ...)`，并让
旧的 `dry_run` 作为兼容读取路径。不要创建 `patch_edit`、`patch_commit` 等一级工具；其中
`apply` 表示落盘，不能与 Git 的 `commit` 混淆。

## 9. 交接清单

- [ ] 实现者先为阶段 A 写失败测试，再写协议代码。
- [ ] 每新增字段都标明 producer、consumer、可选性、默认与 mixed-version 行为。
- [ ] 不把 token、capability、完整工具参数或输出写入命令行、日志、审计和会话。
- [ ] 不修改现有 `CONTROL_PROTOCOL_VERSION`。
- [ ] 不删除或弱化 `deepx-tools` 的本地授权与路径校验。
- [ ] P1 只包含 `exec`，PR 不得夹带文件写入迁移。
- [ ] 提交前执行本计划第 7 节适用的验证并记录未覆盖项。
