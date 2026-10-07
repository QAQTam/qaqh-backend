# Windows 沙箱与写拦截体系 Spec(双平面:TokenPlane + RedirectPlane)

> **2026-10-03:本文档权威版本已迁移至独立仓 `E:\win-sandbox-rs\docs\spec\`,本文件为历史快照,不再更新。**
> 锚点裁决(受限令牌 + capability-SID,Low-IL 方案作废)见该仓 `docs/adr/0001-token-plane-anchor.md`。

> 状态:v1 待复核稿(2026-10-03)。依据当日代码实证,见 §0。
> 用途:交付前由独立模型逐条复核,重点攻击 §10 待验证声明(V1–V12)与 §4 冻结契约的自洽性。
> 关联:`plan-workspace-diff-injection.md`(spy 事实来源)、Linux 侧
> `crates/qaqh-sandbox/src/linux.rs`(Landlock+seccomp helper,本 spec 的架构参照)。

## 0. 实证起点(代码依据,复核时请核对仍然成立)

1. Windows 当前**无强制后端**:`qaqh-sandbox/src/lib.rs` `wrap_command` 在非 Linux
   分支 fail-open 并 `warn_spec_unenforced` —— 本 spec 把该分支替换为真实后端。
2. exec 的唯一 spawn 点:`qaqh-workspace/src/exec/direct.rs` `direct_exec_inner`,
   经 `wrap_command(&mut cmd, argv, cwd, spec)` 包装 `std::process::Command`,
   随后 `env_clear()` + `minimal_child_env()` + `CREATE_NO_WINDOW`。
3. `SandboxSpec`(`qaqh-policy/src/lib.rs:175`)已有 `writable_roots`,默认
   `workspace_write(workspace_root)` = 整个 workspace 可写 —— **这正是 exec 内
   `sed -i` 旁路 per-file 权限的位置**。
4. `SandboxBackend` 枚举(`qaqh-policy/src/lib.rs:158`)serde 序列化,新增变体
   属于向前兼容(`snake_case` rename),但消费方序列化稳定性需排查(见 §8)。
5. spy 已具备:`Session::scan / changes_since / report_since / undo / restore`,
   CAS blob + journal + manifests 存工作区外;procmon 级命令归因在 lib.rs 注释中
   标注为 M3 搁置 —— 本 spec M2 的事件流将其补齐。
6. daemon 自身永不降权(Linux 侧既定原则),Windows 侧沿用:降权只发生在
   exec 子进程。

## 1. 目标 / 非目标

**目标**

- G1:exec 期间的文件**写**(创建/覆盖/追加/删除/rename/截断)在**发生时刻**
  受权限裁决:未授权的写失败,授权的写进入 pending 区或预批准位置。
- G2:每次 exec **零提权**(不弹 UAC、不要求 daemon 以管理员运行)。
- G3:用户态实现为主;唯一依赖内核的部分是 Windows 自带的
  完整性等级 / AppContainer / ACL 机制,不加载任何驱动。
- G4:与 Linux 侧同构:同一 `SandboxSpec` 语义、同一能力上报模型、
  spy/审批/journal 复用。
- G5:`discard` 毫秒级;`capability.rs` 的 Windows 档位从降级变为真实上报。

**非目标**

- N1:敌对代码防护(raw syscall 主动逃逸、进程注入对抗)。信任模型与 Linux 侧
  一致:agent 半可信。硬隔离走 VM 层,另立项。
- N2:读取拦截(与 Linux 侧对齐:读取不隔离,exfiltration 是摩擦不是边界)。
- N3:注册表虚拟化在 M1/M2 不做,推 M3(§7.2)。
- N4:网络隔离 M1 不做;AppContainer 变体(M3 评估)顺带获得,不作为里程碑承诺。

## 2. 威胁模型与信任边界

```
┌─ daemon(Medium IL,用户态,无管理员)────────────────────────────┐
│  authorize_call(含 write_paths 审批)→ spawn shim → merge/restore │
└──────────────┬───────────────────────────────────────────────────┘
               │ CREATE_SUSPENDED + 降权令牌 + 注入(自有子进程,免特权)
┌──────────────▼───────────────────────────────────────────────────┐
│  exec 子进程树(Low IL = 平面一;ntdll hook = 平面二)              │
│   写 workspace/用户可写区 → 内核 IL 检查拒绝(hook 被绕过也兜底)    │
│   写系统区(HKLM/Program Files/裸卷)→ 非提权令牌被 ACL 拒绝(白嫖) │
└───────────────────────────────────────────────────────────────────┘
```

- **平面一(令牌平面)**:内核强制、路径形态无关(8.3 短名/symlink/大小写
  花样不影响,内核查的是文件对象 label)。负责"未授权的写必然失败"。
- **平面二(重定向平面)**:用户态协作式。负责"授权的写落在 pending 区、
  读侧合并视图、结构化拒绝理由"。可被敌意绕过,被平面一兜住。
- 已知残余(与 Linux 同病,如实记录):`schtasks` 创建用户级任务可在沙箱
  进程树之外延迟执行写入;hook 层对直接 syscall 无约束(靠平面一)。

## 3. 总体架构与分期

```
        exec 工具调用
             │
   authorize_call ── write_paths[] 逐文件审批(复用 ApprovalRegistry)
             │
   spawn shim(qaqh-sandbox-win,M1 起)
     1. 复制令牌降 IL ──────────► 平面一(M1)
     2. scratch 布局 + 环境重指 + Job Object
     3. CREATE_SUSPENDED 注入 sbx-hook ──► 平面二(M2)
     4. Resume
             │
   upper/<exec> ──(枚举合并/读写合并)── workspace 只读基底(M2)
             │
   turn 边界:spy 精确 diff upper → 审批 → merge 落盘 / discard
```

- **M1(纯平面一,无 hook)**:deny-steer。未声明路径写入 = 内核 EACCES;
  已审批 `write_paths` 通过**临时降 label** 允许就地写(§5.4)。
- **M2(平面二)**:pending-overlay。授权写入重定向进 upper,枚举合并,
  deny 带结构化理由码。
- **M3**:注册表虚拟化、merge 工具打磨、WinFsp 对比评估、AppContainer 变体。

## 4. 冻结契约(数据模型)

### 4.1 `qaqh-policy` 扩展

```rust
pub enum SandboxBackend {
    Auto,
    LinuxBubblewrap,
    LinuxLandlockSeccomp,
    ProcessHardening,
    None,
    WindowsLowIl,        // M1:平面一
    WindowsHookOverlay,  // M2:平面一 + 平面二
}

pub struct SandboxSpec {
    pub enabled: bool,
    pub backend: SandboxBackend,
    pub writable_roots: Vec<PathBuf>,   // 构建类目录白名单(自动放行,不问询)
    pub writable_files: Vec<PathBuf>,   // 新增:文件粒度授权(write_paths 审批产物;
                                       //   Linux 侧 Landlock per-file 规则共用此字段)
    pub network: NetworkPolicy,
    pub max_open_files: Option<u64>,
}
```

- `writable_files` 语义跨平台:**这些文件本次 exec 可写**(M1 = 临时降 label;
  M2 = 重定向或放行),其余 `writable_roots` 之外的工作区/用户区写入一律拒绝。
- `max_open_files` 在 Windows 无对应 Job 限额,**保持未实现并如实上报**,
  不得静默假装生效。

### 4.2 裁决模型(平面二内部,平面一无此层)

```rust
pub enum Verdict { Pass, Redirect { upper: PathBuf }, Deny { reason: Reason } }
pub enum Reason { ProtectedPath, OutsideScope, DaemonTimeout, PathUnresolved }
```

裁决顺序(纯函数,可单测):

```
1. 规范化路径(§6.3);规范化失败 → Deny(PathUnresolved)
2. 非写语义 open(纯读)→ Pass            // 读不拦,与 Linux 对齐
3. 命中 writable_roots  → Pass(auto_allow)
4. 命中 writable_files  → Redirect(M2)/ 已降 label(M1 天然放行)
5. workspace 内其余路径 → Deny(ProtectedPath)+ 事件
6. workspace 外用户可写区(%USERPROFILE% 等)→ Deny(ProtectedPath)+ 事件
7. 系统区 → Pass + 事件                    // ACL 自己会拒,返回 OS 原生错误
```

### 4.3 scratch 布局与会话状态

```
%LOCALAPPDATA%\qaqh\sbx\<session_id>\
  exec\<exec_id>\upper\      # M2:pending 写入镜像(标 Low IL)
  exec\<exec_id>\tmp\        # TMP/TEMP 重指目标(标 Low IL)
  home\                      # HOME/USERPROFILE 重指 + junction 农场(§5.5)
  journal.jsonl              # 裁决事件流(spy + 归因消费,跨 exec 追加)
  pending-labels.json        # M1:被临时降 label 的文件清单(恢复用,§5.4)
```

- scratch 根由 daemon(Medium IL,自有文件持完全控制权)创建并逐目录设
  Low label;`discard` = 删除 `exec\<exec_id>` + 关闭句柄,毫秒级。
- `journal.jsonl` 事件 schema:

```json
{"ts":"...","session":"...","exec":"...","pid":1234,
 "event":"verdict","op":"open_write|rename_dest|link_dest|delete",
 "path":"...","verdict":"redirect|deny|pass|auto_allow",
 "reason":"protected_path|outside_scope|daemon_timeout|path_unresolved",
 "cache_hit":false}
```

## 5. M1:TokenPlane(令牌平面)

### 5.1 模块与 crate

```
crates/qaqh-sandbox-win/        # 平面一 + spawn shim + (M2)注入器
  src/lib.rs        # 公共 API:spawn / detect / WinSbxProcess
  src/token.rs      # 令牌降级
  src/scratch.rs    # scratch 布局与 label
  src/env_shim.rs   # 环境重指表
  src/spawn.rs      # CreateProcessAsUserW + 管道装配 + Job Object
crates/qaqh-sbx-nt/            # NT 原型/结构/路径规范化(纯 Rust 可单测)
crates/qaqh-sbx-hook/          # M2:cdylib hook 引擎(§6)
```

依赖:`windows`(features:Win32_Security, Win32_System_Threading,
Win32_System_JobObjects, Win32_Storage_FileSystem, Win32_Foundation)。
NT 原型(`NtCreateFile` 等签名、`OBJECT_ATTRIBUTES/IO_STATUS_BLOCK`)自行
vendored 进 `qaqh-sbx-nt`(windows-rs 覆盖不全;复核点 V9 附清单)。

### 5.2 令牌降级序列(`token.rs`)

```
1. OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE|TOKEN_QUERY)
2. DuplicateTokenEx(.., SecurityImpersonation, TokenPrimary) → hLow
3. AllocateAndInitializeSid(S-1-16-4096)          // SECURITY_MANDATORY_LOW_RID
4. SetTokenInformation(hLow, TokenIntegrityLevel,
       TOKEN_MANDATORY_LABEL{ Label: SID+SE_GROUP_INTEGRITY })
5. CreateProcessAsUserW(hLow, ..., CREATE_SUSPENDED|CREATE_NO_WINDOW|
       CREATE_UNICODE_ENVIRONMENT, env_block, cwd, &si, &pi)
```

- 关键性质:对自有令牌**降权**不需要特权;`CreateProcessAsUser` 使用
  自身令牌的副本被认为免 `SeAssignPrimaryTokenPrivilege`(Chrome broker
  先例,复核点 V2)。
- stdio 管道句柄:`SetHandleInformation(HANDLE_FLAG_INHERIT)` 后随
  `bInheritHandles=TRUE` 传入(管道对象无 IL 约束,复核点 V11)。

### 5.3 scratch 目录 Low label(`scratch.rs`)

```
SDDL: "S:(ML;;NW;;;LW)"   // SYSTEM_MANDATORY_LABEL, no-write-up, Low
ConvertStringSecurityDescriptorToSecurityDescriptorW →
SetNamedSecurityInfoW(path, SE_FILE_OBJECT, LABEL_SECURITY_INFORMATION)
```

在 scratch 根与各子目录创建时设置一次,子文件自动继承。**免提权依据:
daemon 是创建者,默认 DACL 授予完全控制(含所需写权限),完整性 label
的写入按 WRITE_OWNER 特例走** —— 此句为复核点 V1,实证前不得视为定论。

### 5.4 write_paths 的 M1 语义:临时降 label

- spawn 前:对 `writable_files` 中每个文件
  `SetNamedSecurityInfoW(file, LABEL_SECURITY_INFORMATION, Low)`,
  追加进 `pending-labels.json`;
- exec 结束(含超时/取消路径,`finally` 语义):恢复 Medium label;
- daemon 启动时扫描 `pending-labels.json`,恢复上次崩溃残留(幂等)。
- 就地写是**审批在前**的(授权发生在 exec 之前),语义满足 G1;
  内容变更仍由 spy 现有 diff 捕获。

### 5.5 环境重指表(`env_shim.rs`,M1 生效)

| 变量 | 重指到 | 理由/代价 |
|---|---|---|
| `TMP` `TEMP` | `scratch\exec\<id>\tmp` | 否则 Low IL 连临时文件都写不了 |
| `CARGO_HOME` | `scratch\home\.cargo`(真实目录,config 以 junction 只读挂真实文件) | registry 缓存冷启动,首次构建重新下载;不重指则 cargo 写 `~/.cargo` 被 IL 拒 |
| `npm_config_cache` | `scratch\home\npm-cache` | 同上 |
| `PIP_CACHE_DIR` `XDG_CACHE_HOME` | `scratch\home\cache` | 同上 |
| `GOCACHE` `GOMODCACHE` | `scratch\home\go` | 同上 |
| `HOME` `USERPROFILE` | `scratch\home`(junction 农场:预置 `.gitconfig` 等已知配置文件只读链接) | 无枚举合并时读不到原 home 的其余内容;已知配置外的读取失败列为 M1 已知降级 |

- junction 读侧依据:Low IL 进程**读** Medium IL 文件不受阻(no-write-up
  only,复核点 V7)。
- 表必须随实证扩充;`minimal_child_env()` 现有白名单保持,重指在其后应用,
  模型显式 `env` 最后覆盖(与 direct.rs 现有顺序一致)。

### 5.6 Job Object 与 kill 语义

- `CREATE_SUSPENDED` 后、Resume 前将进程挂入 Job:
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_JOB_MEMORY`
  (内存限额取 spec 可配置项,默认不设)。
- 取消/超时:CloseHandle(Job) 即杀整树 —— 替代 Linux 侧 `process_group(0)`
  + SIGKILL 的 Windows 等价物。

### 5.7 deny-steer 反馈注入

- M1 无 hook,拒绝错误来自子进程自身(stderr 出现
  `Access is denied` / `拒绝访问`,或 exit code 5 相关)。
- exec 结果后处理(direct.rs 现有输出包装处):当
  `sandbox_backend == WindowsLowIl` 且退出码非 0 且 stderr 命中拒绝特征,
  追加治理提示行,冻结文案:

```
[qaqh-sandbox] 写入受保护路径被拒绝(内核完整性策略)。
工作区文件的修改请改用 edit/write 工具(走权限审批),或在 exec 的
write_paths 中声明目标文件后再执行。
```

### 5.8 能力上报(`capability.rs` Windows 分支)

```rust
Platform::Windows => SandboxCapabilities {
    platform: Platform::Windows,
    backend: SandboxBackend::WindowsLowIl,
    landlock: false, seccomp: false, bubblewrap: false,
    filesystem_write_isolation: true,   // M1 起为真(IL 内核强制)
    network_isolation: false,
    process_hardening: true,            // Job Object
    detail: "low-integrity token plane" / "hook overlay plane"(M2)
}
```

### 5.9 M1 验收标准

- A1:非提权 daemon 下,`cmd /c echo x > workspace\protected.txt` 失败
  (EACCES),journal/审计含记录,反馈文案注入。
- A2:`write_paths=["protected.txt"]` 审批通过后同一命令成功;exec 结束后
  文件 label 恢复 Medium;daemon 崩溃重启后 label 恢复。
- A3:`cmd /c cargo build`(workspace 内)在重指环境下完成,构建产物
  落在 `writable_roots`(workspace 内 target/ —— M1 默认
  `writable_roots=[workspace]` 时整体放行;收紧策略的行为见 §8.4)。
- A4:discard 路径:删除 scratch exec 目录 < 50ms。
- A5:capability 上报与实际行为一致(降级路径仍响亮警告,沿用 H1 原则)。

## 6. M2:RedirectPlane(hook 引擎,pending-overlay)

### 6.1 注入时序(spawn shim 内)

```
1. CreateProcessAsUserW(CREATE_SUSPENDED)          // §5.2
2. 依目标进程机器位选 DLL(x64/x86;ARM64 列为 M3,复核点 V12)
3. VirtualAllocEx + WriteProcessMemory(UTF-16 DLL 路径)
4. CreateRemoteThread(LoadLibraryW),等待完成
5. DLL DllMain:仅记录模块句柄(loader lock 纪律)
6. DLL 内部启动 init 线程:读 QAQH_SBX_CONFIG(管道名 + 策略快照路径)
   → 安装 retour hook → 连接 named pipe → 就绪事件
7. 父进程 ResumeThread
```

- 配置经环境变量传递是**顾问性**的:目标进程篡改自身环境只影响体验,
  不影响强制力(强制 = 管道裁决 + 平面一)。
- DllMain 纪律:不调用 LoadLibrary/COM/不连管道;全部延迟到 init 线程。
- `panic = "abort"`(cdylib profile);hook 体内禁止任何可 panic 路径,
  错误一律转为 `Deny(DaemonTimeout/PathUnresolved)` 或透传原语义。

### 6.2 hook 清单(冻结:四个,不做第五个)

| hook | 拦截点 | 裁决 | 依据 |
|---|---|---|---|
| `NtCreateFile` `NtOpenFile` | 写语义 open:DesiredAccess 含 `FILE_WRITE_DATA\|FILE_APPEND_DATA\|DELETE\|WRITE_DAC\|WRITE_OWNER`,或 Disposition ∈ `FILE_CREATE/FILE_OVERWRITE/FILE_OVERWRITE_IF/FILE_SUPERSEDE` | §4.2 顺序 | 句柄访问权在 open 时定型,`NtWriteFile`/内存映射/`O_TRUNC` 全被上游覆盖,无需单独 hook |
| `NtSetInformationFile` | `FileRenameInformation[Ex]`/`FileLinkInformation`:**目标路径**裁决;源已被 open 闸门覆盖 | 同上 | 否则"upper 内文件 rename 盖掉受保护文件"成为唯一漏点 |
| `NtQueryDirectoryFile` | 目录枚举 | 合并:upper 条目优先,按名去重,滤 whiteout | 类 overlayfs merged view |
| `NtCreateUserProcess` | 子进程创建 | 挂起态递归注入(同 §6.1 步骤 3-6) | 进程树传播;cmd `start`、批处理递归均经此 |

**重定向实现**:换写 `OBJECT_ATTRIBUTES.ObjectName` 指向栈上构造的
upper 路径(该结构在调用期间同步消费,返回前恢复原值),再调 retour
trampoline 原函数。

### 6.3 路径规范化(`qaqh-sbx-nt`,纯函数,属性测试)

```
输入形态: C:\a\b │ \\?\C:\a\b │ \??\C:\a\b │ \Device\HarddiskVolumeX\a\b │ 正斜杠
步骤: 前缀归一 → 卷符号映射(QueryDosDeviceW 缓存)→ 正斜杠折反斜杠
      → 大小写折叠(OrdinalIgnoreCase)→ 去尾分隔符/尾点尾空格
      → 8.3 短名(~+数字):对父目录 GetFinalPathNameByHandle 解析;
         解析失败 → Deny(PathUnresolved),平面一兜底
UNC/网络路径:workspace 外 → Pass
```

### 6.4 copy-up / whiteout / 合并视图

- **copy-up(懒)**:首次写语义 open 只存在于 lower 的文件 →
  `CopyFileW`(含 ADS、属性)拷至 `upper\<relpath>.qaqh-tmp` → rename 定稿
  → 重定向。per-file 互斥防并发双拷。
- **whiteout**:删除/改名消失 = 在 upper 写
  `.qaqh-whiteout.<原名>`(目录用 `.qaqh-whiteout-dir.<名>`);枚举合并时
  过滤;merge 时执行真实删除。
- **枚举合并**:`NtQueryDirectoryFile` 是按句柄多次调用遍历的模型,hook
  维护 `HashMap<HANDLE, MergeSession>`(upper 快照 + 游标),
  `RestartScan` 重置;需逐类处理
  `FileDirectoryInformation / FileFullDirectoryInformation /
   FileBothDirectoryInformation / FileIdBothDirectoryInformation /
   FileNamesInformation / FileIdExtdDirectoryInformation` 的内存布局,
  以及 `ReturnSingleEntry / RestartScan / IndexSpecified` 标志组合。
  **此为 M2 最大工作量项(约占一半)**。
- **merge 工具(daemon,Medium IL)**:upper → 真实树 copy、whiteout →
  删除、M2 事件流中的 rename 对优先还原为 rename;合并后 label 提回
  Medium。turn 边界触发,审批流逐文件/逐块批准。

### 6.5 策略来源与 IPC

```
管道: \\.\pipe\qaqh-sbx-<session>-<exec_id>   (JSON lines)
C→S: {"id":n,"kind":"query","op":"open|rename_dest|link_dest",
      "path":"<规范化后>","access":"write|delete"}
S→C: {"id":n,"verdict":"redirect","upper":"..."} |
      {"id":n,"verdict":"deny","reason":"protected_path"} |
      {"id":n,"verdict":"pass"}
超时 250ms → Deny(DaemonTimeout) + 事件;连接失败 → 全量 Deny(硬失败)
```

- 决策缓存:per(规范化路径前缀 × access),TTL 30s;`writable_roots`
  命中根本不进管道(构建命令的 open 风暴不能打到守护进程,复核点 V5)。
- 事件流:所有裁决(含 cache_hit)写 journal.jsonl → spy 与 M3 命令归因
  的输入。

### 6.6 M2 验收标准

- A6:`sed -i` 对未声明文件"看似成功"(exit 0),实际落在 upper;spy
  `changes_since` 精确列出;deny 的写入返回 `STATUS_ACCESS_DENIED`
  且 stderr 含 §5.7 文案。
- A7:枚举合并:`dir` / PowerShell `Get-ChildItem` / `git status` 在
  merged view 下一致(upper 新文件可见、被删文件不可见、无重名)。
- A8:进程树传播:`cmd /c start /b node -e "fs.writeFileSync(...)"` 被
  同一裁决覆盖。
- A9:绕过兜底:测试进程用直接 syscall(内联 `syscall` 指令)写工作区文件
  → 平面一拒绝(EACCES),journal 无 redirect 记录但 IL 拒绝可观测。
- A10:merge/roundtrip:upper + whiteout + rename 混合场景,merge 后
  与期望真实树逐字节一致,label 恢复 Medium。

## 7. M3(概述,不冻结)

1. **注册表虚拟化**:`NtCreateKey/NtOpenKey/NtSetValueKey` 写语义
   (`KEY_SET_VALUE|KEY_CREATE_SUB_KEY|DELETE`)重定向至
   `HKCU\Software\qaqh-sbx\<id>\mirror\`,flush 按审批合并。
2. **AppContainer 变体**:第二档令牌平面(capability SID 细粒度 + 网络
   隔离 = Windows 侧 seccomp 断网等价物),免提权创建 profile 的
   工序与兼容性先实证。
3. **WinFsp 评估**:若枚举合并正确性成本超预期,WinFsp(一次性安装
   服务,此后 per-exec 零提权)提供用户态 FUSE 级 merged view;
   store 格式(upper/whiteout/journal)设计为与 hook 引擎共享,
   切换 = 换引擎不换体系。评估标准:正确性、路径身份(`W:\`)、
   oplock 语义、性能四项打分表,另立 ADR。

## 8. 集成清单(逐文件)

| 位置 | 变更 | 阶段 |
|---|---|---|
| `qaqh-policy/src/lib.rs` | `SandboxBackend` 两变体;`SandboxSpec.writable_files` | M1 |
| `qaqh-sandbox/src/lib.rs` `wrap_command` | 非 Linux 分支:Windows 走 `qaqh-sandbox-win::spawn`;返回形态变化(见下) | M1 |
| `qaqh-sandbox/src/capability.rs` | Windows `detect()` 分支(§5.8) | M1 |
| 新 crate `qaqh-sandbox-win` / `qaqh-sbx-nt` / `qaqh-sbx-hook` | 见 §5.1 | M1/M2 |
| `qaqh-workspace/src/exec/direct.rs` `direct_exec_inner` | spawn 分支:Windows 自定义 spawn 替代 std Command(令牌/挂起/管道);引入 `SbxChild` 抽象对齐现有 std Child 使用面(wait/try_wait/kill/id/stdin/stdout/stderr,以 pipe.rs 实际调用为准,开工前枚举冻结) | M1 |
| `qaqh-workspace/src/exec/direct.rs` 输出包装 | §5.7 反馈注入 | M1 |
| `qaqh-workspace/src/authorization.rs` + exec schema(`exec/register.rs`) | `write_paths` 参数接入现有审批流(per-file,ApprovalRegistry);Linux 侧同步受益(Landlock per-file 规则) | M1 |
| `qaqh-spy` | journal.jsonl 消费(归因);M2 起 `changes_since` 增读 upper 精确源 | M2 |
| `SandboxSpec` 序列化消费方排查 | backend 新变体的 serde 兼容性(配置文件/审计/前端) | M1 前置 |

注:spawn 形态变化是 M1 最大集成风险 —— `wrap_command` 现签名 mutating
`std::process::Command`,Windows 令牌路径无法装进该抽象,必须改为
返回自管 spawn 结果,`direct.rs` 消费面同步收缩(见复核点 V10)。

## 9. 测试计划

- **单测**:路径规范化属性测试(形态矩阵 × 大小写 × 短名)、裁决纯函数
  全分支、scratch label SDDL 构造、store 格式 roundtrip。
- **集成(Windows runner)**:A1–A10 对应用例;PowerShell/cmd/Git Bash
  sed 双 shell 覆盖;崩溃恢复(label 残留扫描)。
- **兼容性矩阵**:cargo/npm/pip/go/git/robocopy/7z 在重指环境 +
  deny-steer 下的行为逐一过(预期项写入 §5.5 表)。
- **CI**:windows-latest 跑用户态全量;M2 注入冒烟
  (cmd/powershell/node/python/git);IL 断言用例需防 runner 权限漂移。
- **非目标断言**:确认全程无 UAC 弹窗(自动化以非管理员账户跑通即证明)。

## 10. 待验证声明(复核清单 —— 请逐条攻击)

- **V1 免提权设置 integrity label**:依据"创建者持完全控制权 +
  label 写入走 WRITE_OWNER 特例"。请验证:非管理员进程对自建目录
  `SetNamedSecurityInfoW(LABEL_SECURITY_INFORMATION)` 是否确无
  `ACCESS_SYSTEM_SECURITY`/SeSecurityPrivilege 要求。
- **V2 CreateProcessAsUser 免 SeAssignPrimaryTokenPrivilege**:自有
  primary token 副本降级后启动。Chrome broker 先例,请给出权威依据或
  反例(替代路径:STARTUPINFOEX 的 PROC_THREAD_ATTRIBUTE_TOKEN,需确认
  可用版本)。
- **V3 Low IL 兼容性影响面**:UIPI(window 消息)、COM 激活、MSI、
  安装器类命令;给出豁免清单与"按命令降档"的触发条件。
- **V4 NtCreateUserProcess hook 的传播覆盖率**:`cmd start`、
  explorer 委托、WScript/COM 外壳启动是否全部经 hook;不经 hook 的
  逃逸路径枚举(平面一兜底但要列明)。
- **V5 裁决延迟**:open 风暴(cargo 全量构建)下,缓存命中率与管道
  P99 延迟;确认 writable_roots 短路策略足够。
- **V6 枚举合并会话状态**:同句柄并发查询、句柄复用(关→开同值)时
  `HashMap<HANDLE,...>` 的回收正确性。
- **V7 junction 读侧 IL 语义**:Low IL 进程经 junction 读 Medium IL
  文件确认为放行(no-read-up 才受限,默认策略 no-write-up)。
- **V8 8.3 短名与路径规范化盲区**:除 `~` 数字形态外的绕过面;
  `GetFinalPathNameByHandle` 对父目录解析的失败模式。
- **V9 windows-rs/NT 原型缺口**:给出完整缺口清单(OBJECT_ATTRIBUTES、
  UNICODE_STRING、IO_STATUS_BLOCK、四个 Nt 函数、
  TOKEN_MANDATORY_LABEL 是否需 ntapi crate 或手写)。
- **V10 spawn 抽象重构风险**:`direct.rs/pipe.rs` 对 std Child 的完整
  使用面;`wrap_command` 签名变更的波及(调用点、测试、后台任务路径)。
- **V11 stdio 句柄继承**:Low IL 子进程对父进程创建的管道句柄读写
  无 IL 阻碍(DACL 为默认everyone?)—— 实证。
- **V12 ARM64 Windows**:x64 hook DLL 无法注入 ARM64 进程(异构
  模拟除外);列出检测与降级(拒绝执行或回退纯平面一)策略。

## 11. 工作量与里程碑

| 阶段 | 内容 | 估时(单人) |
|---|---|---|
| M1 | qaqh-sbx-nt 骨架 + token/scratch/env_shim/spawn + SbxChild 重构 + write_paths 审批 + 反馈注入 + capability | 1.5–2 周 |
| M2 | hook 引擎四 hook + 枚举合并 + copy-up/whiteout + 管道裁决 + merge 工具 | 3–5 周 |
| M3 | 注册表虚拟化 + AppContainer 变体 + WinFsp 评估 ADR | 2–3 周 |

## 12. 附录

- **参考实现**:Sandboxie-Plus(路径重写/枚举合并的用户态完整参考)、
  wcifs/bindflt(语义对照,逆向教材)、Microsoft agent-governance-toolkit
  (中间件层定位,已确认与本诉求无交集,仅哲学借鉴)。
- **设计原则出处**:Linux 侧 `qaqh-sandbox` 的"短命 helper 承载强制、
  daemon 永不降权、降级必须响亮(H1)"三条原则,Windows 侧全程沿用;
  双平面分工 = "协作层负责丝滑,内核层负责不出事"。
- **术语**:平面一 = TokenPlane(令牌平面);平面二 = RedirectPlane
  (重定向平面);deny-steer = 拒绝并导流回受管控工具;pending-overlay =
  写入挂起待审。
