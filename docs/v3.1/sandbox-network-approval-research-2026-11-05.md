# QAQHarness 沙箱联网审批调研报告

日期：2026-11-05（Asia/Shanghai）  
状态：调研完成；架构建议待实现，不代表现有能力  
范围：backend 沙箱、工具授权、执行会话、RingingV2 审批面，以及本地 Codex 的对照实现

## 1. 结论与建议

backend 已有工具级网络审批，但缺少“沙箱内实际联网请求 → 目标审批 → 有限授权 → 请求继续”的完整闭环。当前问题不能只靠增加一个“允许联网”按钮解决：策略表达、网络出口、授权生命周期和 RingingV2 命令面需要一起设计。

建议借鉴 Codex 受管网络模式的核心架构：

1. 将文件系统隔离、网络访问策略、审批策略分开表达。
2. 引入受管网络出口，未知目标触发审批，已有授权直接通行。
3. 将网络审批定义为 RingingV2 的一等协议语义，由会话 owner 管理事实与决策。
4. 将授权绑定到明确的目标与生命周期，区分单次、会话、持久规则。
5. 保持文件系统限制；批准联网不得隐含关闭沙箱或扩大写入范围。

推荐日常开发使用 Managed 网络模式，保留 Offline 和 Unrestricted 作为显式选择。具体默认值属于本报告的产品建议，并非已实现行为。

## 2. 调研基线与方法

### 2.1 源码基线

| 项目 | 本地目录 | 调研时 HEAD |
| --- | --- | --- |
| backend | `/home/qaqtamsy/Projects/qaqh-backend` | `d126a5e51a03e6639b2387e1578cd5af0292df0f` |
| Codex | `/home/qaqtamsy/Projects/codex` | `db3c44414c2cd4ae33adaf4fa33382cb3dde6bd5` |

结论依据本地工作区源码，而非仅依据提交快照。backend 调研时存在未提交修改，其中包括 `crates/qaqh-process-tools/src/exec_session/tool.rs`；本文描述的是当时磁盘上的实现，不保证上述 HEAD 的纯净检出具有完全相同的执行链路。调研没有覆盖或回退这些修改。

### 2.2 方法与限制

- backend 使用已有 CodeGraph 索引定位符号并读取当前源码；Codex 目录未发现 `.codegraph/`，使用文本检索与源码阅读。
- 使用 Context7 查询 `/openai/codex` 的网络代理与审批资料，并阅读官方安全说明；源码事实以本地实现为准。
- 未运行构建、全局测试、联网探针或跨平台实测；本文是静态架构调研，不是隔离有效性的运行时认证。
- 文中的文件行号是调研定位点，后续修改可能使其漂移；应同时按符号名定位。

调研时资源：8 个 CPU 核心，内存总量约 7.7 GiB、可用约 3.5 GiB，Swap 已用约 3.2–3.3 GiB，磁盘剩余约 59 GiB。因此本次只读源码并编写 Markdown，没有触发构建。

## 3. backend 现状

### 3.1 网络策略是二值开关

`qaqh-policy` 的 `NetworkPolicy` 只有 `Deny` 与 `Allow`。`SandboxSpec::workspace_write` 默认开启沙箱、允许工作区写入、拒绝网络。

`SandboxSpec::for_permission` 的行为是：

| 条件 | 沙箱 | 网络策略 |
| --- | --- | --- |
| `sandbox_enabled = false` | 关闭 | Allow |
| 沙箱开启，`SkipPermissions` | 开启 | Allow |
| 沙箱开启，其他权限档位 | 开启 | Deny |

因此，“开启沙箱就一定断网”不是全部事实；准确说法是普通权限档位在沙箱内默认拒绝网络，跳过审批档位同时获得网络 Allow。审批档位与网络能力存在耦合。

证据：[NetworkPolicy、SandboxSpec 及其构造方法](../../crates/qaqh-policy/src/lib.rs)，定位 `NetworkPolicy:214`、`workspace_write:245`、`for_permission:275`。

### 3.2 工具审批与网络能力并不是同一个授权

`needs_permission` 对 Read、Write、Exec、Net 等工具类别作决策。普通工作区写入可以自动批准，Exec/Net 一般需要确认，`SkipPermissions` 对普通工具自动批准；此外还有敏感路径等独立规则。

这个决策回答的是“是否允许调用工具”，没有返回目标 host、协议、端口或网络授权期限。执行审批通过后，不会因此自动产生一个网络目标授权。

证据：[needs_permission](../../crates/qaqh-permission/src/permission.rs)，定位 `436`、`521`、`538`。

必须区分两种场景：

- `web_fetch` 等 Net 工具：已有调用前审批，但它是否受同一网络出口约束，需要单独梳理其宿主 HTTP 执行路径。
- `exec_command` 中运行 `cargo fetch`、`git fetch`、脚本或任意联网程序：工具类别是 Exec，程序实际访问哪些目标不是工具类别所能表达的。

不能把“存在 ToolCategory::Net”当成“存在沙箱内目标级联网审批”。

### 3.3 执行链路确实消费沙箱策略

当前 `ExecCommandTool::run` 构造 shell argv 后调用 `qaqh_sandbox::prepare_pty_launch`，将 `context.sandbox_spec()` 交给沙箱层，再将改写后的 `launch.argv` 交给执行宿主。

链路如下：

```text
工具授权 / ToolCallContext
  → ExecCommandTool::run
  → prepare_pty_launch(argv, cwd, sandbox_spec)
  → ExecSessionHost::exec_command
  → ExecSessionManager::start
  → spawn_pty / spawn_pipe_split
```

`ExecSessionStart` 本身没有携带 `SandboxSpec`，但不能据此断言沙箱被绕过：沙箱包装已在上游写入 argv。后续改造必须保持这个事实清晰，避免审批层与实际启动策略分叉。

证据：

- [ExecCommandTool::run](../../crates/qaqh-process-tools/src/exec_session/tool.rs)，定位 `165`、`210`、`225`。
- [SessionExecHost::exec_command](../../crates/qaqh-agent-runtime/src/tool_job.rs)，定位 `106`。
- [ExecSessionManager::start_platform](../../crates/qaqh-process-tools/src/exec_session/manager.rs)，定位 `257`。

### 3.4 Linux 的拒绝发生在底层执行边界

Landlock 路径在 Deny 时声明网络访问限制；seccomp 路径同时拦截创建 `AF_INET`、`AF_INET6`、`AF_PACKET` socket，返回 `EPERM`。bubblewrap 路径默认使用 `--unshare-all`，只有 Allow 才加 `--share-net`。

这些机制能形成网络拒绝，但不会自动产生携带目标地址的应用层审批。特别是创建 socket 时尚不能获得完整的 HTTP 目标语义。因此，无法仅靠解析 stderr 或补一个 UI 按钮实现稳定的请求级授权。

已经启动的进程也不能通过修改 Rust 中的 `SandboxSpec` 撤销其已安装的 seccomp 限制。要实现批准后继续请求，应在启动时建立可审批的受控出口。

证据：[apply_landlock / apply_seccomp](../../crates/qaqh-sandbox/src/linux.rs)，定位 `132`、`170`；[configure_bubblewrap](../../crates/qaqh-sandbox/src/lib.rs)，定位 `509`。

### 3.5 RingingV2 目前表达的是工具权限审批

当前 device 审批类型为 `ToolPermission / Ask / Plan`。工具审批响应映射为 `RingingCommand::ToolPermissionRespond`，核心字段是 `tool_call_id`、`approved`、`trust_folder`。

这套结构可以承载现有工具确认和目录信任，但没有为网络目标授权定义独立、明确的类型与决策范围。不能借用 `trust_folder` 表达信任域名，也不应把网络批准伪装成 Ask 的自由文本答案。

现有 pending 投影使用可选的 `pending_permission` 和 `pending_interaction`，网络请求可能在同一个执行中并发出现，需要评估多 pending 的表示能力。

证据：[审批 DTO](../../crates/qaqh-ringing/src/v2/types.rs)，定位 `RingingV2PendingApprovals:529`、`RingingV2DeviceApprovalKind:541`；[command_for](../../crates/qaqh-daemon/src/axum_server/axum_impl/challenge.rs)，定位 `210`。

### 3.6 平台差异必须暴露

Windows 沙箱分支明确记录：token plane 强制写入限制，但网络策略没有被强制执行。其他非 Linux 分支与部分后端降级路径也不能仅凭策略声明就宣称网络已隔离。

需要区分“请求的策略”和“实际后端能力”。本报告不对各操作系统作运行时保证。

证据：[wrap_command 及能力处理](../../crates/qaqh-sandbox/src/lib.rs)，定位 `144`、`185`、`197`。

## 4. Codex 的设计

### 4.1 适用前提：受管代理模式

本节描述启用相应网络配置、执行归属与审批策略后的受管网络路径。Codex 同时存在离线、直接联网与沙箱提权等路径，不能把受管代理行为推广到所有配置。

`begin_network_approval` 只有在受管网络生效、工具提供相应 spec 等条件满足时才注册审批。`handle_inline_policy_request` 还检查 permission profile、活动上下文与审批策略；例如 Never 不允许进入这条交互审批流程。

证据：本地 Codex `codex-rs/core/src/tools/network_approval.rs`，定位 `allows_network_approval_flow:199`、`handle_inline_policy_request:717`、`begin_network_approval:1185`。

### 4.2 操作系统出口限制与代理策略配合

Codex 的 Linux bubblewrap 网络模式包含 `FullAccess / Isolated / ProxyOnly`。ProxyOnly 使用独立网络命名空间，由 helper 建立代理路由通道。

因此，受控代理不是仅向程序设置 `HTTP_PROXY` 的约定：操作系统边界也需要约束绕过代理的直接出网。HTTPS、HTTP 与 SOCKS 等流量在相应代理路径上接受策略判定。

证据：本地 Codex `codex-rs/linux-sandbox/src/bwrap.rs:108`、`codex-rs/linux-sandbox/src/linux_run_main.rs:196`。

### 4.3 实际请求驱动审批，绑定执行归属

`HostApprovalKey` 包含：

```text
environment_id + lowercase(host) + protocol + port
```

待审批键在此基础上再包含 turn 与 execution 标识。执行注册保存工具名称、命令、执行环境、取消信号等信息，网络请求可归属到具体执行；同执行同目标的等待请求可以合并。

目标级授权能让用户判断“哪条命令要访问哪里”，也避免一个命令的单次授权被其他并发命令直接借用。

证据：本地 Codex `codex-rs/core/src/tools/network_approval.rs`，定位 `HostApprovalKey:140`、`PendingHostApprovalKey:148`、`ActiveNetworkApprovalCall`、`get_or_create_pending_approval:591`。

### 4.4 授权生命周期分层

| 决策 | 本地实现含义 | 注意事项 |
| --- | --- | --- |
| Approved | 产生 AllowOnce 决策 | 不写入会话批准集合；并发等待请求可能共享当前待审批决策，不能机械解释为恰好一个 TCP socket |
| ApprovedForSession | 写入 session approved hosts | 缓存键包含环境、host、协议、端口 |
| NetworkPolicyAmendment | 修改运行时策略并尝试持久化网络规则 | 与会话缓存粒度不同；修改对象主要表达 host/action，并结合审批上下文持久化 |
| 拒绝、取消或超时等 | 拒绝待审批请求并处理执行结果 | 结束等待，避免请求无限悬挂 |

不能把会话端点键、持久域名规则和命令前缀批准视为同一种授权。持久规则也不应被称为无条件、永不撤销的永久授权。

证据：本地 Codex `codex-rs/core/src/tools/network_approval.rs:984`、`codex-rs/protocol/src/approvals.rs:183`、`codex-rs/core/src/session/mod.rs:3053`。

### 4.5 规则边界与更细粒度

代理支持域名 allow/deny，显式 deny 优先；策略 decider 主要用于处理允许列表未命中，不能任意覆盖明确拒绝和本地网络限制。

本地 README 还描述 limited 模式：只允许 GET、HEAD、OPTIONS；HTTPS 如需方法级控制，需要 MITM 等解密检查机制。普通 CONNECT 隧道只能提供目标级控制，不能据此宣称支持 URL 路径或 HTTP 方法审批。

因此第一阶段适合借鉴端点级审批，不建议同时引入证书、HTTPS 解密和路径级策略。

证据：本地 Codex `codex-rs/network-proxy/README.md`，定位 “Understand blocks / debugging”“Policy hook”“Security notes”。

## 5. 差距汇总

| 能力 | backend 当前状态 | Codex 受管路径 | 建议 |
| --- | --- | --- | --- |
| 网络模式 | Allow / Deny | 隔离、受管出口、直接联网等路径 | 增加 Managed |
| 目标表达 | 沙箱 spec 无目标规则 | host、协议、端口及环境归属 | 定义规范化端点 |
| 审批触发 | 工具调用前 | 实际请求可触发 | 保留工具授权，新增出口审批 |
| 授权期限 | 工具批准、目录信任 | 单次、会话、持久规则 | 使用独立网络 grant |
| 执行关联 | 有 call / exec session，但未形成网络授权闭环 | 网络请求归属 execution | 贯通同一身份链 |
| 等待恢复 | 底层直接拒绝网络 | 请求可等待决策 | 批准后继续连接建立 |
| 并发审批 | 当前 pending 投影偏单槽 | 去重、排队、取消 | pending 列表与有界队列 |
| 强制执行 | Linux 有拒绝路径，平台能力不齐 | 受管代理配合平台执行边界 | 如实报告有效能力 |

## 6. 推荐架构

### 6.1 网络模式与审批策略独立

以下为建议的新模型，不是现有 API：

| 网络模式 | 行为 |
| --- | --- |
| Offline | 禁止外网；不自动请求扩权，切换模式需明确策略变更 |
| Managed | 只能经受管出口；匹配允许规则直接通行，未知目标按审批策略处理 |
| Unrestricted | 网络目的地不经受管过滤；文件系统沙箱仍可开启 |

审批策略独立决定未知请求是询问还是直接拒绝。跳过工具审批不应隐含开启不受限网络；如果希望“自动允许受管网络目标”，也应由网络策略明确表达。

### 6.2 模块职责

```text
沙箱进程 / 后代进程
  → 操作系统网络边界（只允许到受管出口）
  → 网络出口代理：获取实际目标，执行目标规则
  → NetworkAuthorizationService：请求归属、grant、等待与取消
  → 会话 owner：保存审批事实并发布 RingingV2 事件
  → RingingV2 客户端：显示目标与授权范围，提交规范命令
  → owner 接受决策 → 授权服务应用有效授权 → 代理继续或拒绝请求
```

代理只执行网络策略，不自行生成一套业务审批协议；agentloop 不拥有独立审批状态机。RingingV2 命令面是权威入口，设备 challenge 只是对规范审批的受限投影。

长时间网络等待不得占住 owner 的串行处理通道，否则审批应答本身可能无法被处理。等待应在独立执行任务中，通过明确的消息与取消信号关联 owner。

### 6.3 grant 与规则的作用域

建议端点键至少包含 `host + protocol + port`，再绑定会话和有效执行环境。环境标识使用项目现有或正式定义的身份，不凭空添加无消费者字段。

- 单次授权：限定当前 execution 与当前待审批批次；批次定义必须进入协议说明。
- 会话授权：仅当前会话、当前环境、当前端点可复用，默认不跨会话共享。
- 持久规则：单独表达存储范围、匹配方式和撤销入口；首版建议精确端点匹配。
- 子代理：不得因父会话存在授权就隐式继承全部网络权限；继承关系需要正式规则。

域名大小写、尾点、IP 字面量、IPv6、IDNA、默认端口与规则匹配必须采用统一规范化结果。目标重定向后重新判定；授权原域名不代表授权所有跳转目的地。

明确拒绝和执行环境上限优先于普通批准。一次批准不应替模型放开私网、localhost 或云元数据目标。

### 6.4 操作系统边界

Linux Managed 不能继续使用“全禁 IP socket”的原样策略，也不能直接切成 Allow。需要建立“允许代理通路、禁止任意直连”的真实执行能力。

具体采用 namespace + 代理桥接、socket broker 或其他机制，应在小规模技术验证后确定；本报告推荐能力形态，不把尚未验证的机制写成实现承诺。

仅修改代理环境变量不满足强制隔离。自带网络栈、不读取代理变量的工具，以及 SSH、UDP 等协议，需要声明支持范围；未支持的连接应明确失败，不得悄悄直连。

Windows 和其他平台也必须遵循同一能力契约：无法强制 Managed 时报告不支持，不能只写一条警告后继续以 Managed 名义启动。

## 7. RingingV2 协议演进建议

本节名称是设计草案，实际命名和 wire schema 应在协议实现时统一确定。

### 7.1 网络审批请求

建议定义结构化 NetworkAccess 审批，至少包含：

| 字段组 | 内容 |
| --- | --- |
| 身份 | 审批 ID、session、来源 call、execution、有效环境 |
| 目标 | 规范化 host、协议、端口 |
| 原因 | 未命中允许规则等明确原因，来源工具与有界命令摘要 |
| 生命周期 | 创建时间、过期时间、当前状态、可选决策 |
| 展示边界 | 可展示字段白名单；不携带认证头、请求正文、密钥或含凭据 URL |

一个 execution 可以同时等待多个目标，不能只靠唯一的 tool_call_id 识别网络审批。pending 应能够返回多个审批，且和事件流中的审批 ID 一致。

### 7.2 决策命令与结果

建议网络响应使用专门的规范命令，决策可表达 `allow_once / allow_session / allow_rule / deny`。实际可用选项由请求中的 allowed decisions 决定；客户端不能提交任意更宽目标来扩权。

流程应复用现有会话归属、lease、设备 challenge 和命令校验边界，同时明确：

1. owner 接受命令，不等同于出口授权已经生效。
2. 授权生效、持久化成功、请求继续、连接失败是不同结果。
3. 审批 ID 只能解决对应 pending；过期、重放和跨会话决策必须有明确结果。
4. 事件与 bootstrap 能恢复“待审批”和“已解决”的显示状态，但 daemon 重启不能复活已丢失的 socket 或执行进程。
5. 若运行时授权已生效、规则持久化失败，应分别报告，不能宣称持久化成功，也不能假装此前授权从未发生。

网络 grant 应由后端基于 pending 目标生成，不能由模型或半可信 device 自行构造。不得在 agentloop 内解析自由文本回复来替代规范决策。

### 7.3 生命周期

```text
网络请求到达
  ├─ 已允许 → 建立连接
  ├─ 明确拒绝 / 不可审批 → 返回结构化拒绝
  └─ 可审批 → Pending
                  ├─ 批准 → 授权生效 → 继续连接建立
                  ├─ 拒绝 → 拒绝请求
                  ├─ 到期 → 到期结果
                  └─ 执行退出 / 用户取消 / 请求断开 → 取消并回收
```

继续连接建立不保证应用请求最终成功。TLS、DNS 或远端服务仍可能失败；此类错误不得错误归类为审批失败。

用户可能等待较久，应独立定义审批等待期限、工具执行期限与客户端网络超时的关系。如果程序已超时退出，迟到的单次批准不能重新启动整条命令；重试必须是新的执行行为。

## 8. 用户体验与观测

推荐卡片示例：

```text
cargo fetch 请求访问 index.crates.io:443（HTTPS）
原因：该目标尚未获得允许

[允许本次] [本会话允许] [保存允许规则] [拒绝]
```

“允许本次”应有明确作用域说明；保存规则时展示规则的实际范围。新目标和重定向目的地独立判定，相同 execution 的相同目标等待请求合并，避免下载多个包时重复弹出同一审批。

观测记录建议包含审批 ID、session/call/execution、目标、策略来源、授权范围、等待耗时、最终决策、实际生效状态和错误类别。不要默认记录请求正文、认证头或完整含查询参数的 URL。

受管出口服务可共享监听资源，执行归属必须独立。限制等待队列与保留事件规模，避免为每个连接启动进程或创建一套代理；现有资源条件不适合无界并发和高容量抓包。

## 9. 落地顺序与验收

### 阶段一：先确定协议与执行边界

- 明确网络模式、端点规范化、授权范围和策略优先级。
- 扩展 RingingV2 类型、命令、事件、pending 与 device challenge 投影。
- 确定唯一的会话 owner 审批状态机以及异步网络等待方式。
- 验证 Linux 受控出口可达、直连不可达；未证明之前不对外宣称 Managed 生效。

### 阶段二：打通最小可用闭环

- 优先实现 Linux 上明确支持的 HTTP/HTTPS 代理路径。
- 支持未知目标审批、单次与会话批准、拒绝、取消、到期及同目标去重。
- 将来源执行身份贯穿普通管道、PTY 和后代进程。
- 同步客户端卡片与错误展示，确保批准后等待中的连接能继续。

### 阶段三：持久规则与平台扩展

- 增加规则保存、加载、撤销和策略刷新，区分运行时生效与落盘结果。
- 梳理宿主 Net 工具是否复用同一授权服务，明确不属于子进程沙箱的路径。
- 按实际需求增加 SOCKS、SSH、UDP、localhost 与跨平台执行支持。
- 方法级、路径级与 HTTPS MITM 作为独立议题，不塞进首版。

### 最小验收矩阵

下表是实现阶段的针对性验收建议，本次没有执行这些测试。只覆盖关键边界，避免每次修改都运行全仓测试。

| 场景 | 必须满足的结果 |
| --- | --- |
| 工作区写入 + 目标联网批准 | 联网成功，外部写入仍受限 |
| 未知 HTTPS 目标 | 出现结构化审批，而非仅有 EPERM |
| 单次批准后的另一 execution | 不能复用前一次单次批准 |
| 会话批准后的相同端点 | 当前会话内可复用，其他会话不能借用 |
| 同 execution 同目标并发 | 合并等待；所有等待者最终被处理 |
| 新域名、协议或端口 | 按新目标判定，不扩大原授权 |
| 显式 deny、私网限制 | 普通批准不能越过策略上限 |
| 取消、超时、执行退出 | 回收 pending，迟到响应不复活执行 |
| 忽略代理变量并尝试直连 | 被操作系统边界阻断 |
| PTY 与 pipe | 消费一致的网络策略与执行归属 |
| 持久化失败 | 准确区分当前授权和保存失败 |
| 后端不支持网络强制隔离 | 明确报告能力不足，不冒充 Managed |

阶段二可先用小型本地目标与代理验证关键行为，再选一个真实包管理命令做集成检查；跨平台检查只在对应平台实现发生变化后开展。

## 10. 不采用的方案与待决问题

### 10.1 不采用

- 批准联网后把整个沙箱关闭：权限范围过大。
- 将 Deny 直接改成 Allow 作为目标审批实现：不具备目标粒度。
- 通过 stderr 中的 EPERM、超时或 DNS 文案猜测目标：缺少可靠归属与语义。
- 只设置代理环境变量：不能限制绕过代理的程序。
- 在 agentloop 增加私有审批回调，再转换成 Ringing 展示：会产生第二套权威状态。
- 复用目录信任、Ask 自由文本或任意 payload 作为网络授权：协议语义不明确。
- 复制 Codex 全部 guardian、MITM 和多环境设施：超出本项目首版必要范围。

### 10.2 实现前需要确定

1. 首版支持的网络协议与操作系统范围。
2. 单次授权具体覆盖当前请求组还是当前 execution 的目标访问；必须有唯一语义。
3. 会话授权在会话恢复、daemon 重启及规则撤销后的行为。
4. 持久规则放置于用户、项目还是配置 profile，以及谁有权修改。
5. 长连接与撤销：规则变更是否仅影响新连接，是否需要主动关闭已有连接。
6. 子代理网络授权的继承与收回规则。
7. 工具超时、审批超时及用户等待之间的计时关系。

以上属于明确的产品与协议决策，不应通过兼容分支或隐式回退掩盖。

## 11. 参考资料

### backend 源码

- [沙箱与策略类型](../../crates/qaqh-policy/src/lib.rs)
- [工具权限引擎](../../crates/qaqh-permission/src/permission.rs)
- [沙箱启动与平台能力](../../crates/qaqh-sandbox/src/lib.rs)
- [Linux Landlock / seccomp](../../crates/qaqh-sandbox/src/linux.rs)
- [工具执行入口](../../crates/qaqh-process-tools/src/exec_session/tool.rs)
- [执行会话管理器](../../crates/qaqh-process-tools/src/exec_session/manager.rs)
- [运行时执行宿主](../../crates/qaqh-agent-runtime/src/tool_job.rs)
- [RingingV2 审批 DTO](../../crates/qaqh-ringing/src/v2/types.rs)
- [设备 challenge 与规范命令映射](../../crates/qaqh-daemon/src/axum_server/axum_impl/challenge.rs)

### Codex 本地源码

根目录：`/home/qaqtamsy/Projects/codex`。相对该目录的主要文件：

- `codex-rs/core/src/tools/network_approval.rs`：请求归属、等待去重、会话 grant 与审批决策。
- `codex-rs/core/src/tools/orchestrator.rs`：执行审批、沙箱与受管网络的协调。
- `codex-rs/core/src/session/mod.rs`：`persist_network_policy_amendment`。
- `codex-rs/protocol/src/approvals.rs`：网络协议类型、审批上下文及规则修改对象。
- `codex-rs/linux-sandbox/src/bwrap.rs`：`BwrapNetworkMode`。
- `codex-rs/linux-sandbox/src/linux_run_main.rs`：代理路由接入。
- `codex-rs/network-proxy/README.md`：网络代理配置、规则语义与能力边界。

### 官方资料

- [OpenAI：Agent approvals & security](https://learn.chatgpt.com/docs/agent-approvals-security)：沙箱边界与审批策略的区别。
- [OpenAI Codex 网络代理 README](https://github.com/openai/codex/blob/db3c44414c2cd4ae33adaf4fa33382cb3dde6bd5/codex-rs/network-proxy/README.md)：对应本地 HEAD 的源码文档地址；具体引用以本地已读内容为准。

本文的三个关键判断分别是：backend 缺少目标级联网授权闭环；Codex 的受管网络实现提供可参考的粒度；QAQHarness 应以 RingingV2 为权威完成这条链路。本文没有声称这些建议已经实现或验证。
