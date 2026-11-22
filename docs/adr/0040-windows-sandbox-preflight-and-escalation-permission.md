# ADR-0040 Windows 沙箱分步自检协议、执行会话接线与提权授权闭环(contract-refreeze)

状态: `已采纳并实施` · 决策日期 2026-11-22 · 决策人 仓库所有者(三项拍板:服务面 `sandbox.preflight.start/status` 轮询驱动启动自检动画 / `exec_command` 增 `with_escalated_permissions` + `justification` 提权摘要 / `tool.permission.respond` 增 `scope` 支持 `once|session|persistent` 本对话默认授权)

## 背景

1. **生产执行面与 Windows 沙箱(`sbx-win`)断路**:
   - 唯一注册进生产 `ToolManager` 的命令工具是 `exec_command`(`crates/qaqh-workspace/src/registration.rs:47`),其执行入口 `ExecCommandTool::run`(`crates/qaqh-process-tools/src/exec_session/tool.rs:258`)调用 `qaqh_sandbox::prepare_pty_launch` 后落到 `ExecSessionManager::start_platform`(`crates/qaqh-process-tools/src/exec_session/manager.rs:277`)。
   - `prepare_pty_launch` 在非 Linux 平台直接退回 `SandboxBackend::None`(`crates/qaqh-sandbox/src/lib.rs:340`),`start_platform` 直接走 `qaqh_pty::spawn_pty` / `spawn_pipe_split` 普通进程派生;真正接线了 `sbx-win` 的 `direct_exec_inner` → `sbx_bypass::sbx_exec` 只挂在未注册的旧 `ExecTool` 上,导致 Windows 生产环境 `exec_command` 100% 裸跑。
   - 更致命的是:当宿主机启用了 ProjFS 时,`SandboxBackend::Auto` 在 `resolve_windows_backend`(`crates/qaqh-sandbox/src/sbx_map.rs:52`)中返回 `Some(WindowsRedirect)`,使审批层 `exec_backend_enforces_write_isolation` 返回 `true` 并免弹窗自动放行,但执行层 `exec_command` 却未进 `sbx-win`,造成“审批以为进沙箱、执行实际裸奔”的 P0 安全失配。
2. **沙箱开启却退化为每次 `exec_command` 都弹审批(因果倒置)**:
   - `SandboxSpec::workspace_write` 默认 `backend = SandboxBackend::Auto`,在未开启 ProjFS 的 Windows 机器上 `resolve_windows_backend` 返回 `Ok(None)`,未回落到 `WindowsToken`,导致 `exec_backend_enforces_write_isolation` 为 `false`;且 Linux 下该函数硬编码 `false`(`crates/qaqh-sandbox/src/lib.rs:134`)。
   - Windows 默认 shell `pwsh` 在 `classify_exec_command`(`crates/qaqh-permission/src/command_class.rs:730`)中不做只读文法分析,非黑名单命令恒为 `Unclassified`,而在默认档位 `WorkspaceWrite`(档位 2)下 `Unclassified` 仍走 `AskUser` 弹窗。
3. **无启动沙箱自检进度协议**:
   - `SandboxCapabilities::detect()` 在 Windows 上静态硬编码 `filesystem_write_isolation: true`(`crates/qaqh-sandbox/src/capability.rs:104`),`sbx_win::acceptance::run_all` 从未接入 daemon,前端无法在启动时展示分步沙箱自检动画,也无法向用户提示开启 `Client-ProjFS` 或非管理员运行建议。
4. **缺少模型主动提权(`with_escalated_permissions` + `justification`)与“本对话默认授权”(`scope = session`)**:
   - 当前 `exec_command` 无提权请求参数,且权限弹窗只支持单次 `(call_id, execution_id, args_hash)` 消耗;
   - `trust_folder` 对 `Exec/Net` 无效,且在 `session_loop.rs:1455` 中错误地把会话自身的 `self.cwd`(`workspace_root`)加入内存 `trusted_dirs`,既未绑定实际外部目标目录也未落盘。

## 决策

1. **RingingV2 沙箱分步自检服务面(`sandbox.preflight.start` / `sandbox.preflight.status`)**:
   - 在 `POST /ringing/v2/service/{method}` 新增 `sandbox.preflight.start`(Write,admin_only=false)与 `sandbox.preflight.status`(Read,admin_only=false),与 `siwc.sign_in.start/status` 同构,不新增 HTTP 路由。
   - 自检引擎在后台按序执行 5 个真实探针步骤并实时更新进度(`pending | running | passed | warning | failed`)、耗时与 `remediation`(用户开启/修复指引):
     1. `token_elevation`:检测当前进程是否管理员提权(`is_elevated()`);提权下标记 `warning` 并提示以普通用户权限运行以避免管理员 Owner 旁路 DACL;
     2. `restricted_token`:实测 `CreateRestrictedToken`(`WRITE_RESTRICTED`) + Capability SID + 私有桌面创建;
     3. `dacl_write_isolation`:在隔离临时目录实测授权根目录允许新建/写入、未授权文件被内核 DACL 拒绝写入;
     4. `deny_carveout`:实测 `.git` 保护路径显式 Deny ACE 拦截写入且读取透传;
     5. `projfs_redirect`:检测 `Client-ProjFS`(`projectedfslib.dll`)与转正开关;可用时实测 ProjFS 视图写入与 merge,未启用时标记 `warning` 并返回 `remediation = "DISM /Online /Enable-Feature /FeatureName:Client-ProjFS /NoRestart"`(引导用户开启最强 RedirectPlane,同时明确当前已自动启用 TokenPlane 内核写隔离)。
   - 自检结论写入进程级缓存,作为 `SandboxCapabilities::detect()` 与审批层 `exec_backend_enforces_write_isolation` 的真实依据。
2. **打通 `exec_command` → `ExecSessionManager` 的 Windows `sbx-win` 执行链**:
   - `resolve_windows_backend` 对 `SandboxBackend::Auto` 在 ProjFS 可用时解析为 `WindowsRedirect`,在 ProjFS 不可用时**正式回落为 `WindowsToken`**(不再返回 `None`),Linux 侧 `exec_backend_enforces_write_isolation` 同步按 `LinuxBubblewrap | LinuxLandlockSeccomp` 如实返回 `true`。
   - `sbx_win::spawn` 增配可保持 stdin 写端打开的交互式管道派生 `spawn_interactive` 与 stdio 句柄提取;
   - `ExecCommandRequest` 与 `ExecSessionStart` 显式携带 `sandbox_spec: Option<SandboxSpec>`;Windows 下当 `sandbox_spec.enabled == true` 且解析到 `WindowsToken | WindowsRedirect` 时,`ExecSessionManager::start_platform` 走 `ProcessControl::WindowsSbx`(受限令牌 + 私有桌面 + Job Object 进程树管控 + 工作区 `.git` 自动 Deny carveout + 实时 stdout/stderr 流式读取与 `write_stdin` 交互 + 退出时 ProjFS `turn.merge()` 与 DACL 拒绝导流反馈)。
3. **`exec_command` 提权请求参数(`with_escalated_permissions` + `justification`)与默认沙箱免审**:
   - `ExecCommandArgs` 新增 `with_escalated_permissions: Option<bool>` 与 `justification: Option<String>`。
   - **沙箱开启且写隔离生效时的准入准则**:
     - 在 `WorkspaceWrite`(档位 2)与 `SandboxRun`(档位 4)下,只要 `with_escalated_permissions != Some(true)` 且未命中高危红线(`ExecCommandClass::Risky` / `exec_hits_self_administration_paths`),**不论 `bash` 还是 `pwsh` 均默认在沙箱内自动放行(`GrantKind::SandboxClassified`)**,不再逐条命令弹窗!
     - 当命令在沙箱内因越权写或网络限制失败时,导流文案引导模型以 `with_escalated_permissions: true` 附带 `justification`(提权摘要)重新调用;
     - 当 `with_escalated_permissions == Some(true)` 时,无论当前处于 `ReadOnly`、`WorkspaceWrite` 还是 `SandboxRun`,均强制触发审批弹窗(`PermissionDecision::AskUser`),把模型的 `justification` 注入 `PermissionRequestDetails.justification` 供前端审批卡直接展示;用户批准后,该次执行以脱离沙箱(`SandboxSpec::disabled()`,且 `NetworkPolicy::Allow`)运行。
     - 当一条普通(未显式传 `with_escalated_permissions`)但需审批的命令(如联网 `git fetch` 或高危命令)经用户弹窗批准后,执行时将 `sandbox_spec.network` 升级为 `NetworkPolicy::Allow`,消除“用户批准后仍被沙箱断网杀死”的死锁。
4. **“本对话默认授权”(`PermissionGrantScope`: `once | session | persistent`)与 `trust_folder` 修复**:
   - 新增契约枚举 `PermissionGrantScope { Once, Session, Persistent }`;`RingingCommand::ToolPermissionRespond` 新增 `scope: Option<PermissionGrantScope>`(兼容 `trust_folder: bool`:`scope = None` 时 `trust_folder = true` 映射为 `Persistent`,否则为 `Once`)。
   - `InteractionDecision` 新增 `ApprovedForSession`;权限交互卡片 `InteractionView.choices` 默认下发三项:`允许本次`(`approved`)、`本对话默认允许`(`approved_for_session`)、`拒绝`(`rejected`)。
   - `WorkspaceToolPort` 维护会话级授权表 `SessionApprovalStore`:当用户选择 `Session`(`ApprovedForSession`)时,按当前审批的类别/动作在**本会话生命周期内**记住授权(如本会话允许 `exec_command` 常规/提权或命令前缀、允许 `Net` 工具、允许目标外部目录读写),后续同类调用在本对话内自动放行,跨会话不共享。
   - 修复 `trust_folder`(`Persistent`)误绑 `self.cwd` 的 Bug:提取 `details.paths` 中落在 `workspace_root` 之外的目标路径父目录,同时写入会话 `port.trust_dir(dir)` 与全局持久化 `qaqh_workspace::trust_folder(&dir)`。

## 被否选项

1. **新增独立 HTTP 路由承载沙箱自检**:违反最少路由原则且需要额外维护路由鉴权表;复用 `POST /ringing/v2/service/{method}` 的 `sandbox.preflight.start/status` 与 `siwc` 完全同构。
2. **继续在 `WorkspaceWrite` 下要求 `pwsh` 命令逐条审批**:违背“开启沙箱即由内核兜底写边界”的初衷,导致 Windows 用户开启沙箱反而承受双倍弹窗摩擦。
3. **仅靠前端勾选 `trust_folder` 充当本对话默认授权**:`trust_folder` 是跨会话持久目录信任,语义与“本对话内允许执行/提权”完全不同,混用会导致跨会话越权或对 `Exec/Net` 失效。

## 后果

- 触及 RingingV2 命令面(`RingingCommand::ToolPermissionRespond`)、会话读模型(`PermissionRequestDetails`、`RingingV2PendingPermissionView`、`InteractionDecision`)与服务面(`sandbox.preflight.start/status`),按宪法 #15 归类为 **`[contract-refreeze]`**:
  - `COMMAND_CONTRACT_REVISION`: `ringing-commands-10` → `ringing-commands-11`
  - `SESSION_CONTRACT_REVISION`: `ringing-session-5` → `ringing-session-6`
  - 重新运行 `qaqh-contract-export` 刷新 `docs/api-surface/v4.2` 金标准并通过 `check --against docs/api-surface/v4.2`。

## 落地清单

- [x] `crates/sbx-win`: 抽取分步自检探针与 `spawn_interactive` 交互管道支持
- [x] `crates/qaqh-sandbox`: 实现异步分步自检状态机 `preflight` + 修复 `Auto` 回落 `WindowsToken` 与 Linux `exec_backend_enforces_write_isolation`
- [x] `crates/qaqh-ringing-contract` & `crates/qaqh-ringing` & `crates/qaqh-runtime`: 扩展 `PermissionRequestDetails.justification`、`PermissionGrantScope`、`InteractionDecision::ApprovedForSession` 与 `sandbox.preflight.start/status`
- [x] `crates/qaqh-process-tools`: `ExecCommandArgs` 增 `with_escalated_permissions`/`justification`,Windows `ExecSessionManager` 接入 `sbx-win`
- [x] `crates/qaqh-permission` & `crates/qaqh-workspace` & `crates/qaqh-agent-runtime`: 沙箱生效时常规 `exec_command` 免审、提权弹窗透传 `justification`、会话级 `SessionApprovalStore` 与 `trust_folder` 外部目录修复
- [x] `tools/qaqh-contract-export`: 刷新 `docs/api-surface/v4.2` 金标准并通过 `check --against` 与定向测试
