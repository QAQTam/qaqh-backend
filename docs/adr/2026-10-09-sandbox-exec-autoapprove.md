# 沙箱内 exec 分级自动放行与写拒绝重试

日期：2026-10-09。状态：设计采用（owner 已授权），待实施；不是实现完成声明。

## 授权与范围

用户在当前设计会话授权本切片作为独立功能切片推进，不扩展架构收敛（clean）的清理范围，与 [AskUser Form ADR](2026-10-08-ask-user-form.md) 同类。

**不解冻 wire。** 本切片对 Ringing v2 的全部改动限于既有 permission-challenge/审批载荷上新增带 `#[serde(default)]` 的可选字段（I14 允许项），不改既有事件、命令语义，不新增路由或交互形态。若实施中发现需要新交互词汇或新路由，停止实施并回到本 ADR 重新授权。

影响面：`qaqh-permission`（分类器）、`qaqh-workspace`（授权/执行复核）、`qaqh-process-tools`（exec 拒绝检测）、`qaqh-sandbox`（能力查询）、`qaqh-ringing`/`qaqh-types`（可选字段）。实施在独立分支进行，不混入 clean 净删基线。

## 背景与问题

Windows TokenPlane 的执行强制是分项的：文件写由内核强制（WRITE_RESTRICTED 受限令牌 + cap-SID DACL），读透传，网络未强制（`qaqh-sandbox/src/capability.rs`）。而 workspace-write 权限档下 exec 不分读写一律弹审批，读工具自动放行 —— 高频只读命令（`rg`、`git log`、`fd`）的审批摩擦与安全收益不成比例。

前置修复已完成：授权快照与执行复核的资源提取不再依赖线程局部工作区状态（`extract_target_paths_in` 显式 base），相对路径授权不再错位。

业界参照（Microsoft agent-governance-toolkit，MIT）：有界 shell 分词器 + 动态片段不可判定 + fail-closed 白名单/拒绝模式，验证了静态分类的可行边界；但其宿主无内核沙箱，必须执行前完成全部裁决，pwsh 只能做删除/secret 特判。qaqh 拥有写入时刻的内核强制，判不了的命令可以放行运行、由 DACL 在写入发生时拦截 —— 这是本设计与其根本差异。

## 决策

**1. 能力分项判据。** exec 自动放行条件 = 沙箱启用 且 `filesystem_write_isolation == true` 且 命令分类为只读。不要求 `network_isolation`（Windows 上恒为 false，不以此阻塞切片）；网络面由 deny 模式与后续 AppContainer 里程碑兜底（独立立项，不在本切片）。

**2. 只读分类器（阶段 1）。**

- bash/sh 侧：移植 AGT 式有界分词器（POSIX 文法）。含控制符、重定向、未闭合语法、动态替换片段（`$var`/`$(...)`/反引号）的命令一律拒判；argv[0] 白名单（rg/grep/fd/ls/cat/git 只读子命令/type 等，初始集在实施 PR 定稿并用测试锁定）。
- pwsh/cmd 侧：不做文法分词。只做 deny 特判 —— 删除命令及 flag 解析、secret 读、下载管道执行、云 metadata 端点（吸收 AGT 规则集，补 `Invoke-WebRequest`/`iex (iwr ...)` 形态）；未命中的命令不弹窗，走决策 3 进沙箱运行。
- fail-closed：拒判 = 不自动放行；沙箱能力查询失败视为不可自动放行。
- 分类器只决定摩擦，不是安全边界：误判的兜底是 DACL 与 deny 模式。

**3. 写拒绝 → 询问 → 重试闭环（阶段 2）。**

- exec 结果中识别沙箱写拒绝信号（exit code / stderr / 结构化回报；实施首日先实测确认信号可靠性，不可靠则阶段 2 缓行，阶段 1 独立交付）。
- 检测到拒绝 → 产出 PermissionChallenge（paths = 被拒路径，consequence 强制披露"命令已部分执行"），复用既有 ask/审批挂起-恢复管线；用户批准后扩展写路径、重新准入执行。`AuthorizedToolCall` 单次授权语义不变。
- 不新增第三种交互词汇，不用 TTL 充当终态，每条命令保持终态（架构不变量）。

**4. 审计与告知。** 自动放行与沙箱写拒绝均落工具审计账（沿用现有 kind，扩展走 `#[serde(default)]` 字段）；审批对话框维持 H1 义务 —— 沙箱未强制网络必须明示。

**5. 沙箱优先档 SandboxRun（owner 2026-10-09 追加授权，含 wire 档位值 4）。**

- `PermissionLevel` 新增 `SandboxRun = 4`（wire 语义新增，owner 于 2026-10-09 设计会话授权；旧客户端不识别该值，降级保持现档，能力协商安全）。单调性例外：数值比 SkipPermissions 大，但网络审批不放行。
- 行为：exec 全部自动放行（只读分类命中与不可判定形态一致对待——分类器在本档降级为摩擦优化器，不是裁决者）；越权写由 DACL 在发生时刻内核拦截，拒绝经 v2a 导流文案（含疑似目标路径）回模型。
- 保持审批的例外：Risky deny 形态（递归删除、secret 读、下载管道执行、代码执行、环境倾倒、云 metadata 端点）、独立下载/联网命令（网络未强制前唯一 fail-closed 边界）、Net 类工具、会话敏感路径命令文本。
- 生效前置：沙箱启用且 `filesystem_write_isolation == true`；平台无写强制时本档自动退化为 workspace-write 行为（fail-closed）。

## 理由

- 静态判 Read 与 DACL 构成双层：第一层降摩擦，第二层（内核写强制）才是安全边界。pwsh 文法（backtick 转义、`$()`、PS7 `&&`、splatting、`-Command` 内嵌脚本）使静态判定不可靠，故 pwsh 侧取"沙箱优先"而非"分类优先"。
- 复用 PermissionChallenge 单次授权闭环，不造第二套交互账本（单一事实源）。
- cmd 无文法支持，同样走沙箱优先 + deny 模式。

## 后果与边界

- 客户端审批 UI 需理解"沙箱拦截写入"扩展语义；旧客户端缺省降级为现状（弹窗），能力协商缺失时安全。
- 风险与对策：分类器误判 → 保守白名单 + 测试锁定；拒绝信号不可靠 → 实测前置、阶段拆分交付；部分执行副作用 → 审批文案强制披露。
- 验收：仓库三件套（fmt/clippy/test）+ 探针 —— 沙箱内 `rg` 自动放行不弹窗；伪造越权写入被拒并触发询问；deny 模式命中仍弹窗；相对路径资源授权回归不复发。
- 明确不做：AppContainer 转正（独立里程碑，前置：lowbox 通道稳定性实证、`readable_roots` 自动推导、ACL 崩溃窗口审计）；网络强制；cmd 文法分词。
