# Windows AppContainer / ProjFS redirect / 审批接线审计

日期：2026-10-09。范围：当前 `E:\qaqh-backend` 工作树；未修改生产实现。

## 1. 结论

**已观察到的 AppContainer 标准启动路径 `ERROR_FILE_NOT_FOUND`，主要根因是 profile 生命周期实现错误，而不是 Windows 11 API 路径失效。**

同一宿主、同一个容器 SID、同一镜像、同一个 `CreateProcessW + PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` 调用，只改变“是否真正调用 CreateAppContainerProfile”，启动结果就由 `0x80070002` 变为成功。现有 `ensure_profile` 把 SID 派生成功错误地当作 profile 存在。

另外有两个实际写隔离漏洞：**Auto 后端解析结果没有接入执行路径**；**ProjFS 视图所谓 protected DACL 重建仍保留 scratch 继承来的 capability 写授权**。二者不能用 AppContainer 未转正或 Windows 版本变化解释。

**审批接线未完成**：当前有 exec 执行前审批、有执行后 overlay diff/merge 事件，但没有 pending overlay → 用户确认 → 对指定删除执行 merge 的闭环。新增/修改自动合并，删除跳过并撤销 tombstone；函数返回后视图被销毁。提示“需要确认”不是已实现审批。

不以一次宿主实验证明所有 Windows build 无兼容性差异；但本次已明确推翻代码中“26300 上标准 SECURITY_CAPABILITIES 路径恒失败”的泛化归因。

## 2. 实验环境与证据

- 当前用户非提权：探针 `elevated=false`。
- 注册表 `DisplayVersion=26H2`，`CurrentBuildNumber=26300`，`UBR=9550`。`ProductName` 仍报告 `Windows 10 Pro`，不靠该旧字段判断 Windows 世代；本报告绑定精确 build。
- ProjFS 在本机可用，相关验收实际执行，没有走“缺功能跳过”。
- 诊断探针：[源码](audit-probes/windows-sandbox-2026-10-09/src/main.rs)、[依赖锁定](audit-probes/windows-sandbox-2026-10-09/Cargo.lock)、[实测输出](audit-probes/windows-sandbox-2026-10-09/results.txt)。
- 探针只创建随机命名的临时目录、canary 与 AppContainer profile；正常结束会删除本次 profile 和临时文件。不操作真实业务文件。

复现：

```powershell
cargo run --manifest-path docs/audit-probes/windows-sandbox-2026-10-09/Cargo.toml --target-dir target/audit-20261009
```

关键结果：

```text
derive nonexistent name: OK
standard before profile: ERR 0x80070002
standard after ensure_profile: ERR 0x80070002
actual CreateAppContainerProfile: OK (ensure_profile had NOT created it)
standard after actual create: OK exit=0
capabilities requested=2 native_count=2 helper_count=1
exec backend=Auto outside_written=true exit=0
exec backend=WindowsToken outside_written=false exit=1
exec backend=WindowsRedirect outside_written=false exit=1
scratch inherited cap + protected view: unauthorized_written=true exit=0
verbose baseline exit=0 bytes=124000 elapsed_ms=117
verbose sbx stalled_before_wait=true captured_bytes=4092 exit=0
```

## 3. 实际调用链

### 3.1 生产 exec

```text
ToolCallContext.sandbox_spec（workspace_write 默认 Auto）
  → authorization::admit_with_context
      enabled + 平台 filesystem_write_isolation → 命令分类自动准入
  → ExecTool::run / handler::run_exec
  → direct_exec_sandboxed / direct_exec_inner
      显式 WindowsToken / WindowsRedirect → sbx_exec
      Auto → wrap_command → 返回 WindowsToken / WindowsRedirect + policy JSON
           → 返回值未触发 sbx_exec → 普通 Command::spawn
```

显式 sbx 路径：

```text
map_policy（固定 Token；AC 不接生产）
  → workspace SID + WRITE_RESTRICTED token
  → inplace ACL + scratch capability inheritable allow
  → redirect::prepare_turn（可选）
  → private desktop + CreateProcessAsUserW + Job
  → try_wait 轮询（此时没有 stdout/stderr 读线程）
  → child.wait 才启动管道读取
  → notifications + diff（只记录数量）
  → merge(ConfirmDeletions)
  → Drop view + 清 scratch → 返回 ExecOutput
```

### 3.2 AppContainer

生产 `sbx_map::map_policy` 固定 `IsolationKind::Token`；`sbx_exec` 也固定构造 restricted token。AC 目前是 sbx 库、CLI 与集成测试中的实验路径，不是生产 redirect 的底层。

CLI 显式拒绝 `AppContainer × redirect` 组合，见 [win.rs](../crates/sbx-cli/src/win.rs) 的 redirect 前置检查。因此不能把“生产 redirect 出错”自动归类成“AppContainer 出错”，也不能把 Token+ProjFS 通过验收当作 AC+ProjFS 已实证。

## 4. 已复现问题

### F1 / P0：Auto 后端报告隔离但实际普通 spawn

位置：[direct.rs:99](../crates/qaqh-process-tools/src/exec/direct.rs#L99)、[lib.rs:126](../crates/qaqh-sandbox/src/lib.rs#L126)、[authorization.rs:417](../crates/qaqh-workspace/src/authorization.rs#L417)。

`direct_exec_inner` 的 sbx 旁路只匹配输入 spec 的显式 Windows 后端。Auto 后续虽然通过 `wrap_command` 得到 resolved backend 和 `sbx_policy_json`，但该 JSON 没有消费，命令也没有被改造成 Windows helper，最终仍 `cmd.spawn()`。

对同一空工作区、同一普通用户私有临时目录中的工作区外 canary：Auto 写入成功，显式 Token/Redirect 返回 access denied。这里没有 Everyone 可写目录等豁免条件干扰。

**影响**：授权层按平台级 detect 报告 `filesystem_write_isolation=true`，不是按该次实际启动路径判定。SandboxRun 可以在此错误前提下自动执行不可分类命令。即使显式后端自身有效，也不能据此宣布默认模式安全。

修复方向：先统一解析该次 launch plan，再按 resolved backend 分派执行。授权与执行共享同一已解析计划，不能只相信平台能力。`enabled=true, backend=None`、退出开关、缺 workspace 等降级路径也必须纳入判定。

### F2 / P0：protected 视图 ACL 没清旧授权，逐目标白名单失效

位置：[acl.rs:345](../crates/sbx-win/src/acl.rs#L345)、[redirect.rs:145](../crates/sbx-win/src/redirect.rs#L145)、[sbx_bypass.rs:240](../crates/qaqh-process-tools/src/exec/sbx_bypass.rs#L240)。

注释声称“protected 重建后根 DACL 仅含 user ACE”，但 `add_ace_obj` 仍把 `old_dacl` 传给 `SetEntriesInAclW`。protected 标志不是对既有 ACE 的清空操作。

生产先给 scratch 挂 `(OI)(CI)` capability write allow，再在 scratch 下建 view。该布局下调用 `prepare_turn(store, view, &[])`，没有任何 view 可写授权，restricted 子进程仍成功创建 `unauthorized.txt`。

现有 redirect 测试的 view 与 store 是独立临时路径，并未复制 sbx_exec 的 scratch 继承来源，所以选择性授权测试通过不能覆盖这个漏洞。

**影响**：本应只允许指定目标的视图，实际继承了可写出口。merge 不重新校验 writable/deny/批准路径，因而会信任被错误放行的 upper。

修复方向：protected 路径真正从空 ACL 构造明确的父进程 user 管理权限；不带入旧 capability ACE；检查根及已物化子对象的继承传播。用生产 scratch 布局验收“无授权”“单文件授权”“deny carveout”。

### F3 / P1：ensure_profile 不创建 profile，标准 AC 启动被误报成系统问题

位置：[appcontainer.rs:48](../crates/sbx-win/src/appcontainer.rs#L48)。错误归因注释位于 [token.rs:114](../crates/sbx-win/src/token.rs#L114)、[spawn.rs:38](../crates/sbx-win/src/spawn.rs#L38)。

当前流程先 Derive，成功即返回，只有 Derive 失败才 Create。实测不存在的随机名称也能 Derive 成功。因此正常首次调用不会创建 profile。

探针先派生随机 SID，标准启动报 2；调用当前 ensure 后仍报 2；再真正 Create profile，标准启动立刻成功。保留同一 SID、同一绝对 cmd.exe 路径、同一 system32 cwd、同一属性列表，排除了镜像丢失、环境路径等变量。

Microsoft 的实现示例是先 Create，只有 `ERROR_ALREADY_EXISTS` 才 Derive；标准创建路径明确使用 SECURITY_CAPABILITIES。来源：[Launching an AppContainer](https://learn.microsoft.com/en-us/windows/win32/secauthz/implementing-an-appcontainer)。

**裁决**：本例是初始化逻辑错误产生的误导性错误码。lowbox+CreateProcessAsUser 的成功只能证明替代路径能运行，不能证明标准 API 失效。用 native token 路径绕过 profile 初始化，也不自动证明对象命名空间、profile/注册表环境与标准启动完全等价。

修复方向：Create → 仅 already-exists 时 Derive → 其他错误透传；检查 profile 存储确已创建，并以标准属性启动做验收，而不仅比较两次 SID 相等。

### F4 / P1：能力查询只返回第一个 SID，形成假缺能力诊断

位置：[token.rs:236](../crates/sbx-win/src/token.rs#L236)。

`TOKEN_GROUPS.Groups` 在绑定中是 `[SID_AND_ATTRIBUTES; 1]`，其原生语义为尾随变长数组。`tg.Groups.iter().take(GroupCount)` 不会扩展数组长度，至多读一项。

实测申请 internetClient + privateNetworkClientServer：原生 GroupCount=2，helper_count=1。这不是内核没授能力，而是查询代码漏读。`token_groups` 也共用该 helper。

修复方向：验证返回缓冲区长度、偏移与 GroupCount 后，按尾随数组构造 slice；增加 0/1/多能力验收。来源：[TOKEN_GROUPS](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-token_groups)。

### F5 / P1：管道排空太晚，大输出产生假超时

位置：[sbx_bypass.rs:360](../crates/qaqh-process-tools/src/exec/sbx_bypass.rs#L360)、[spawn.rs:85](../crates/sbx-win/src/spawn.rs#L85)。

sbx_exec 先只用 try_wait 等进程退出，之后 `Child::wait` 才启动 stdout/stderr 读取。匿名管道容量有限，子进程写满后阻塞，当然不会退出。

相同 cmd for 循环：普通 output 调用 117ms 完成、输出124000字节；sbx 轮询1秒仍不退出，kill_tree 后仅收获4092字节。最终 exit 还可能是0，所以不能单看 exit 判定成功。

这属于执行生命周期错误，不是 AppContainer/ProjFS 或命令本身失效。

修复方向：spawn 成功就立即排空双管道；同时做有界捕获、取消、进程树与 EOF 收敛，复用 direct 路径已有的成熟 pipe pump。不能仅靠增加 timeout。

## 5. 审批与 redirect 的实现缺口（静态确认）

### F6 / P1：删除审批没有 pending 状态，也没有回放入口

位置：[sbx_bypass.rs:420](../crates/qaqh-process-tools/src/exec/sbx_bypass.rs#L420)、[projfs.rs:800](../crates/sbx-win/src/projfs.rs#L800)、[approval.rs](../crates/qaqh-policy/src/approval.rs)。

实际行为：

1. 取 notifications 只写 journal；PRE_DELETE/PRE_RENAME 回调恒 `S_OK`，不是审批或 veto。
2. diff 只对外写 count，没有保存逐条路径/内容快照/批准对象。
3. merge 自动应用 New/Modified。
4. Deleted 在 ConfirmDeletions 下跳过，尝试撤销 tombstone，只有计数和 stderr 警告。
5. 随后 drop turn 并清 scratch，没有持久 pending overlay。
6. `AllowDeletions` 的实际调用只出现在 sbx 测试，生产没有调用它的审批消费路径。

所以“用户后续确认，再重放同一删除”目前没有可恢复的载体。一次 exec 返回 exit=0 时，真实删除仍可能未发生。rename 可表现为新名称自动落盘、旧名称因删除需确认仍留存，即退化成复制。

不能说整个项目没有审批系统：现有工具执行前的 PermissionChallenge / ApprovalRegistry 已有；缺的是 overlay 生命周期与它的连接。

建议接线（不在本次研究中实施）：

```text
exec 完成 → 冻结 overlay/精确 diff/基底指纹
          → 存 PendingOverlay + ApprovalRegistry challenge
          → 用户批准指定项 → 校验基底与批准范围 → merge 指定变更
          → 拒绝/取消 → discard
          → 写入明确终态，再销毁 overlay
```

不要用一次 `AllowDeletions` 全局开关代替逐项批准；批准集合与实际 merge 集合必须一致。

### F7 / P1：失败/取消后的 merge 及 merge 失败结果不诚实

`timed_out`、`cancelled` 或命令非零退出后仍走相同 `turn.merge()`。这意味着取消不等于丢弃。merge 失败只附加 stderr，最终 ExecOutput 仍使用子进程 exit code，可能 `completed + exit_code=0`，而工作区已经部分落盘。

`turn.diff().unwrap_or_default()` 又把 diff 错误写成零变更。merge 会再次 diff，因此不代表必然吞掉所有失败，但第一次审计事件确实不诚实。

需要区分 process outcome、overlay outcome、approval outcome；事务状态不能隐藏在普通 stderr 中。取消后是否保留/丢弃也应成为明确语义。

### F8 / P1：restore 计数器不是并发互斥

位置：[spy_tool.rs:180](../crates/qaqh-workspace/src/spy_tool.rs#L180)、[redirect_guard.rs](../crates/qaqh-process-tools/src/exec/redirect_guard.rs)。

restore 先 load 计数为0，再执行 restore；期间另一个 exec 可 acquire 并进入 turn。计数器没有阻止 restore 与 merge 同时发生。反过来，guard 在 prepare_turn 完成之后才 acquire；也是保护窗口缺口。

计数是进程全局的，会因另一个工作区的 turn 拒绝本工作区 restore，却无法解决同一工作区并行 turn/文件工具写入/merge 的冲突。

修复方向：按规范化 workspace 的读写锁或统一事务 gate；开始准备到 merge/discard 全程占用。并行 overlay 对同一基底还需要冲突检测，不能只依赖同步 merge。

## 6. 其他实现问题与证据边界

以下为源码/API 契约确认，未在本次进行完整攻击或跨系统复现：

| 项目 | 证据 / 影响 | 判断 |
|---|---|---|
| SID 释放函数错误 | appcontainer.rs:63 使用 LocalFree；Create/Derive 文档均要求 FreeSid | API 使用错误，不能用当前未崩溃证明正确 |
| private desktop 关闭函数错误 | desktop.rs:28 把 HDESK 转 HANDLE 后 CloseHandle；应该 CloseDesktop | 长寿命 daemon 资源泄漏/资源耗尽风险 |
| FULL 权限过宽 | FULL_MASK 包含 WRITE_DAC / WRITE_OWNER / DELETE；deny write 掩码不覆盖这些 | 应重新证明 carveout 不可被改 ACL/删除重命名绕过，不把 FULL 当兼容性万能解 |
| ACL 升级幂等判定错误 | add_ace_with_mask 只用 mode 的窄掩码判 has_ace；已有 narrow allow 会阻止升级 FULL | 授权存在但实际权限不足，可能表现为“内核新版本异常” |
| ProjFS hydration 缓冲不对齐 | projfs.rs:403 使用 Vec<u8>；未使用 PrjAllocateAlignedBuffer，也不查询/校验对齐 | 普通 buffered 小文件通过不覆盖 unbuffered I/O；存在契约违例 |
| merge 仅检查叶子 reparse | store_is_reparse(rel) 不检查祖先；copy/create_dir_all 按路径运行 | 祖先 junction、基底并发替换有边界穿透风险；尚未做端到端攻击复现 |
| 路径 normalize 非 canonicalize | sbx-nt normalize 不消解中间 `..`；workspace_rel 只是规范化字符串前缀 | 不能将此结果视为最终对象归属证明；需结合组件检查/句柄校验 |
| ProjFS available 只是装载检测 | 只检查 DLL 和符号，缓存一次；没有实际 View::start 探测 | DLL 存在不等于该工作区/文件系统/驱动可用；所有失败归“未启用功能”不精确 |
| 缺失全生命周期 RAII | spawn 早期 ?、AssignJob 失败可能泄漏句柄/悬挂进程；Child.wait 后 Drop 未关 job | 需要全路径资源所有权审计；不是仅成功路径回归即可 |
| 全局能力造成假警告 | lib.rs:69 因 network=false 就宣称全部无强制/full user；permission.rs:211 同类文案 | 显式 Token 路径有写强制、无网络强制，不能合并成“全部没有” |
| deny-steer 是启发式 | feedback.rs 的非零 exit + denied 关键词不能区分 OS ACL/MIC/杀毒/沙箱 | “疑似被拒”不等于已确认内核策略命中，不应自动扩权重试 |

对应官方来源：

- [CreateAppContainerProfile（FreeSid）](https://learn.microsoft.com/en-us/windows/win32/api/userenv/nf-userenv-createappcontainerprofile)。
- [DeriveAppContainerSidFromAppContainerName（FreeSid）](https://learn.microsoft.com/en-us/windows/win32/api/userenv/nf-userenv-deriveappcontainersidfromappcontainername)。
- [CloseDesktop](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-closedesktop)。
- [PrjWriteFileData 对齐要求](https://learn.microsoft.com/en-us/windows/win32/api/projectedfslib/nf-projectedfslib-prjwritefiledata)。
- [SetNamedSecurityInfoW 权限与继承规则](https://learn.microsoft.com/en-us/windows/win32/api/aclapi/nf-aclapi-setnamedsecurityinfow)。

### “Low 标签导致既有文件无法写”也缺证据

`tests/appcontainer.rs` 的 existing.txt 并没有加入 writable_files，测试其写失败只能证明未授权文件被拒，不能证明授权的既有文件因 Low 标签无法改写。当前 sbx-win 源码也没有设置 Low mandatory label 的实现。这是测试解释与实际测试条件不一致。

真正区分 DACL 与 MIC 时，应记录 TokenIsAppContainer、TokenIntegrityLevel、对象 SACL、desired access，并比较授权前后以及新建/既有文件。FULL DACL 权限不能跨越 MIC 的 no-write-up。来源：[Mandatory Integrity Control](https://learn.microsoft.com/en-us/windows/win32/secauthz/mandatory-integrity-control)。

空 capability 测试仅检查令牌中缺 internetClient，没有联网验收；应把它称为令牌级证据，而不是已证明所有网络面与 broker 通道封闭。

## 7. 验收结果及为何现有测试没抓住

执行了：

```powershell
cargo test -p sbx-win --test appcontainer -- --nocapture --test-threads=1
cargo test -p sbx-win --test projfs --test redirect -- --nocapture --test-threads=1
cargo test -p sbx-win --lib --test spike -- --nocapture --test-threads=1
```

总计35项通过：AppContainer 3，ProjFS 5，redirect 5，lib 21，spike 1。这不是“系统坏了”的证据，也不是“生产接线正确”的证据。

缺失覆盖：

1. AC 验收只跑 native lowbox，不跑真正 profile + 标准属性启动；profile 幂等测试只比较 SID。
2. capabilities 测试仅一个 SID，抓不到变长数组截断。
3. redirect 测试缺生产 scratch inherited allow 布局。
4. sbx 库验收绕过 qaqh 的 Auto → resolved backend → exec 分派链。
5. 输出量小，管道未写满。
6. 删除测试有手动再次删除/AllowDeletions 调用，但没有真实审批交互和跨返回生命周期。

## 8. 修复优先级

1. **先堵 P0**：Auto 实际执行分派、protected ACL 真重建；在这些前提修正前，不把平台级 filesystem_write_isolation 当自动审批的充分依据。
2. **修正 AC 根因与诊断**：profile Create-first、FreeSid、多 SID 查询、撤回“Windows 11 标准 API 恒失效”的注释和结论。
3. **修复生命周期**：spawn 即读管道、资源 RAII、Job/Desktop 正确关闭、取消/merge 状态诚实。
4. **完成审批接线**：持久 pending overlay、精确批准集合、基底冲突检测、merge/discard 明确终态。
5. **再谈 AC 转正**：标准/lowbox 对照矩阵、实际 shell/toolchain 可读根、网络实测、AC×ProjFS 合成验收，以及多 build 对比。

归因总表：

| 现象 | 本次归因 |
|---|---|
| SECURITY_CAPABILITIES 启动报 file not found | 已复现 profile 初始化错误；不是本 build API 失效 |
| 生产没有 AC 网络隔离 | 显式设计为实验后端，未接生产 |
| 默认 Auto 写出工作区 | 实现接线错误 |
| redirect 无授权仍能写 | protected ACL 实现错误 |
| 删除“等待批准”但无法真正批准 | 审批生命周期未实现 |
| 有能力却诊断缺能力 | 变长数组读取错误 |
| 大输出命令超时/输出少 | 管道生命周期错误 |
| 宣称所有沙箱都无强制 | 能力分项被合并后的诊断错误 |

本次产物为研究报告和独立诊断探针，不包含生产修复，也没有提交或发布。

清理记录：正式探针正常结束并清理自己的临时文件；较早一次实验在 ProjFS 停止后的清理阶段失败，留下 `C:\Users\tsy3m\AppData\Local\Temp\qaqh-acl-audit-18608-c8d49a89`。后续限定到该目录的清理命令被自动审批审查拒绝，工具只返回 `blocked by policy`，未提供更具体原因；未绕过该拒绝继续删除。
