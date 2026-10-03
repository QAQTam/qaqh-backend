# Windows 沙箱交叉评审:Sandboxie-Plus × Codex × AGT × 本仓 v1 spec

> **2026-10-03:本文档权威版本已迁移至独立仓 `E:\win-sandbox-rs\docs\spec\`,本文件为历史快照,不再更新。**
> Spike 已裁决:cap-SID 锚点成立(见该仓 `docs/adr/0001` 与 `sbx selftest`)。

> 状态:交叉评审稿(2026-10-03),针对 `windows-sandbox-write-interception-v1.md` 的复核与修订建议。
> 输入:① 本仓 v1 spec 及代码实证;② `E:\myXCode`(OpenAI Codex 源码,`codex-rs` 工作区,
> 重点 `windows-sandbox-rs` / `sandboxing` / `mxc-sandbox`);③ `E:\agent-governance-toolkit`
> (微软 AGT);④ Sandboxie-Plus 官方隔离机制文档 + 2025 年逃逸 CVE 公开分析
> (https://sandboxie-plus.com/sandboxie/isolationmechanism ;
>  https://depthfirst.com/research/alpc-you-later-cve-2025-64721-sandbox-escape-smashing-the-heap-over-ipc)。
> Codex 侧结论均有源码路径可核对(工作树快照,行号引用见文中)。

## 0. TL;DR(五条)

1. **总架构不用改**。v1 的双平面(令牌强制 + 用户态 hook 虚拟化)与 Sandboxie 的
   "SbieDll 用户态虚拟化 + 内核侧强制兜底" 分层哲学同构,Sandboxie 二十余年演化
   验证了这个分层值得做;Codex 则证明"无 hook 时纯令牌单平面 + fail-closed"是社区
   最强实践。我们与 Codex 的真正差异化正是平面二(CoW/pending-overlay)。
2. **核心修订:M1 强制锚点从「Low IL + 临时降 label(§5.4)」换成
   「restricted token(WRITE_RESTRICTED)+ capability-SID DACL」**(Codex 同款,
   生产级验证)。收益:删除 `pending-labels.json` 崩溃恢复状态机、绕开复核点 V1、
   per-file 授权变成"加一条 allow ACE"、跨工作区横向隔离免费获得。
   代价:需要管理 SID 生命周期与 ACE 卫生(幂等写入,无需恢复)。
3. **V2、V11 直接被 Codex 生产代码证实**,无需再找权威文档;同时新增一组
   Codex 已趟平的坑(lpDesktop 缺失 → PowerShell `STATUS_DLL_INIT_FAILED`、
   `\\.\NUL` 需要放行、deny carveout 目标必须预创建、只保留 SeChangeNotifyPrivilege),
   应原样吸收进 M1 实现清单。
4. **Sandboxie CVE-2025-64721(2025-12 披露)给 §6.5 管道协议划了硬约束**:
   broker(daemon)是全系统最高价值目标,子进程发来的任何消息按敌意输入处理。
   Rust 类型化解析天然避开该 CVE 的成因(手工指针算术),但仍需消息上限、
   字段交叉校验、每子进程管道实例配额。
5. **AGT 与本诉求无机制交集**(证实 spec 附录判断),只借三条设计:
   审批绑定动作身份(防 TOCTOU)、审计日志哈希链、结构化"不可用原因"上报。

---

## 1. 四方对照总表

| 维度 | Sandboxie-Plus | Codex Windows | 微软 AGT | qaqh v1 spec(M1/M2) |
|---|---|---|---|---|
| 强制层 | 内核驱动 SbieDrv(路径检查 + 令牌兜底) | 内核访问检查:restricted token(WRITE_RESTRICTED)+ DACL ACE | 无(OS 层全委托外部) | 平面一:令牌(Low IL,拟改为 restricted token) |
| 用户态虚拟化层 | SbieDll(ntdll hook,file/registry 虚拟化、枚举合并) | **无**(明确不做 hook/注入) | 仅 Python 进程内软沙箱(自我声明非安全边界) | 平面二:sbx-hook 四 hook + upper overlay(M2) |
| 需要特权 | 安装期管理员(装驱动/服务) | legacy 零提权;elevated 一次性 UAC provisioning(建账号/防火墙/WFP) | 无 | 零提权(G2) |
| 写白名单锚点 | 沙箱副本路径(重定向即"写") | capability SID ↔ 文件 allow/deny ACE | — | M1:目标文件临时降 label;M2:重定向 |
| CoW/写重定向 | 有(核心能力,"完美谎言") | **无**(做不了就 fail-closed) | 无 | M2 pending-overlay(有,这是差异化点) |
| 读隔离 | 可配置(closed path 内核拒读) | legacy 全盘读(WRITE_RESTRICTED 先天限制);elevated 后端 deny-ACL/glob 快照 | — | 不做(与 Linux 对齐,N2) |
| 网络 | 可选限制 | legacy=env steering(明示非强制);elevated=防火墙 COM + WFP 持久过滤器;MXC 原生 | — | M1 不做(N4);M3 AppContainer/评估 |
| 兼容性策略 | 极重度 workaround(SbieSvc 修协议) | fail-closed 矩阵:后端表达不了的 policy 拒绝执行,绝不弱化 | — | §5.7 deny-steer + H1 响亮降级 |
| 部署/安装 | 驱动 + 服务,常驻 | legacy 零安装;elevated 一次性 setup | pip/npm 包 | 零安装(免驱动、免 UAC) |
| 威胁模型 | 敌意代码(IE 浏览器安装器时代起源) | agent 半可信,同我们 | 非安全边界(应用层治理) | agent 半可信(N1) |

---

## 2. Sandboxie-Plus:分层哲学的祖师爷,内核路线我们不采用

### 2.1 组件与职责(官方文档 + 仓库结构确认)

- `Sandboxie/core/drv`(SbieDrv.sys):内核驱动。钩住 ntdll 大部分 syscall 的内核侧,
  裁决"沙箱外无写、closed path 无读";同时向进程注入 SbieDll。
- `Sandboxie/core/svc`(SbieSvc):SYSTEM 权限常驻服务,做 broker —— 沙箱内进程
  无法直接完成的协议操作(COM、命名管道等)由它代理;**它就是最高价值攻击面**。
- `Sandboxie/core/dll`(SbieDll):注入每个沙箱进程,**file/registry 虚拟化在用户态实现**
  (官方文档原话),合并真实系统与沙箱副本(copy location)数据并重定向访问;
  被不当地绕过时结果"an access denied error"(驱动兜底)。
- 令牌:沙箱进程以"几乎不能访问任何东西的受限令牌"起步(用到约六个未文档化内核
  符号),再由 SbieDll 逐项"修复"出可用性。

### 2.2 为什么不采用其内核路线

驱动签名/WHQL、安装面、内核维护成本,以及我们的 N1(敌对代码防护非目标)共同决定:
**SbieDrv 对应的能力位我们用"令牌进内核访问检查"替代**——强度低一档(只能挡"用户本就
无权/受限 SID 未获授权"的写,不能做任意路径规则),但对"agent 半可信"足够,
且零安装。这条取舍应写进 spec §3 的架构注记,防止后续有人把"上驱动"当选项重新提出。

### 2.3 直接可搬的三条经验

1. **"完美谎言"语义**:重定向后向应用返回成功码,上层无感。对应我们的 M2 redirect
   语义与 A6 验收(`sed -i` "看似成功"落 upper)。Sandboxie 证明这是唯一能撑住真实
   软件生态的语义——拒绝式(让 sed 失败)只对 deny-steer 场景成立。
2. **枚举合并是边角案之源**:SbieDll 在目录枚举合并上积累了二十年的修复史,
   印证 §6.4 "约占 M2 一半工作量" 的估时不是悲观而是现实;WinFsp 备选项(§7.3)
   保留合理性增加。
3. **CVE-2025-64721 的教训**(见 §4):SbieIniServer::RC4Crypt 对消息总长做了校验、
   却独立信任客户端的 `value_len` 字段,整数回绕 + OOB → SYSTEM。我们 §6.5 的
   JSON-lines 管道必须按"子进程是敌意的"来设计输入校验。

---

## 3. Codex:restricted-token 路线的完整生产参照(本次最重要输入)

### 3.1 机制还原(概要)

`codex-rs/windows-sandbox-rs`:**无驱动、无 hook、无注入、无 AppContainer**(AppContainer
仅存在于微软 MXC 后端内部,且 `mxc-sandbox/src/lib.rs` L90 明确排除其 fallback)。

- 令牌:`CreateRestrictedToken`,flags = `DISABLE_MAX_PRIVILEGE | LUA_TOKEN |
  WRITE_RESTRICTED`;restricting SIDs 序列 = capability SIDs → extra restricting →
  logon SID → Everyone(S-1-1-0)(`src/token.rs::create_token_with_caps_from` L468-525)。
- **WRITE_RESTRICTED 语义即我们的授权模型**:限制 SID **只参与写访问检查**
  (读走原始用户权限,天然全盘可读——与我们的 N2 完全一致)。
- 授权锚点:随机生成 `S-1-5-21-...` capability SID(持久化 `~/.codex/cap_sid`,
  per-CWD / per-write-root),spawn 前对可写根挂 allow ACE(WRITE_ALLOW_MASK、
  SET_ACCESS、容器+对象继承,`src/acl.rs` L746-755);deny carveout(`.git`、
  `.codex`、`.agents`、`.aws` 等)挂 deny ACE;`\\.\NUL` 特意放行
  (`acl.rs::allow_null_device` L1091)。
- spawn:`CreateProcessAsUserW` + `STARTUPINFOEXW` + ProcThreadAttributeList
  (job 句柄、句柄继承列表、desktop)+ `CREATE_NO_WINDOW`(`src/process.rs` L94-193)。
- policy 分辨:`sandboxing/src/windows.rs::resolve_windows_{restricted_token,elevated}_filesystem_overrides`
  —— 后端表达不了的 policy 一律 `refusing to run unsandboxed`(fail-closed)。
- 网络:legacy = 纯 env steering(代理指 127.0.0.1:9、denybin 假 ssh、
  `CARGO_NET_OFFLINE` 等,`src/env.rs::apply_no_network_to_env` L126-177,明示非强制);
  elevated = 一次性 UAC provisioning(专用低权账号 + 防火墙 COM + WFP 持久过滤器)。

### 3.2 为什么 cap-SID 锚点优于 Low-IL + 临时降 label(核心论证)

| | v1 方案:Low IL + 降 label(§5.2/§5.4) | 建议方案:restricted token + cap-SID ACE |
|---|---|---|
| 授权动作 | 改目标文件完整性 label(有状态) | 对目标加 allow ACE(无状态、幂等) |
| 崩溃恢复 | `pending-labels.json` + 启动扫描恢复 | 不需要(ACE 留置无害) |
| 免提权依据 | V1(未证:label 写入是否需 ACCESS_SYSTEM_SECURITY) | owner 隐式 WRITE_DAC,成熟无争议 |
| 未授权写被拒 | no-write-up(内核 label 检查) | restricting SID 不在 DACL(内核 DACL 检查) |
| per-file 粒度 | 逐文件降 label,逐文件恢复 | 逐文件加 ACE;未命中文件天然拒 |
| 跨工作区横向隔离 | 无(所有 Low 子进程同质) | **per-workspace SID:A 工作区令牌写不动 B 工作区** |
| spawn 兼容 | UIPI/COM 影响(V3 待证) | Codex 全量生产验证;已知坑有现成解法 |
| 对用户文件的可见修改 | label 变化(执行后还原) | DACL 追加 ACE(留置;可提供清理命令) |

**M2 兼容性论证**(有人会问:换了锚点,M2"绕过 hook 也写不进真实工作区"还成立吗?):
成立。M2 语义下令牌只对 `writable_roots`(构建目录,如 target/)与 scratch/upper 发
allow ACE;受保护工作区**不发**——hook 被绕过时内核 DACL 检查照样拒绝
(`verdict=deny` 与内核拒绝双保险)。v1 的 Low-IL 论证平移过来逐条成立,只是
"no-write-up" 换成 "no-cap-ACE"。

**SID 生命周期**(新增设计点,codex 经验):SID 按 workspace 持久化
(建议 `%LOCALAPPDATA%\qaqh\sbx\cap_sids.json`,键 = spy 同款
`sha256(工作区绝对路径)[..16]`),ACE 幂等追加、**不做执行后清理**。
绝不能 per-exec 换 SID——否则旧 ACE 变成无法回收的垃圾散落在用户文件上。
同一工作区内,后续 exec 复用 SID,ACE 已在即零操作。

### 3.3 Codex 证实/新增的工程事实(直接吸收进 M1 清单)

1. **V2 证实**:`CreateProcessAsUserW` + 自有令牌的受限副本,免
   `SeAssignPrimaryTokenPrivilege`,legacy 后端全量用户在跑。无需 PROC_THREAD_ATTRIBUTE_TOKEN 备选。
2. **V11 证实**:管道句柄 `STARTF_USESTDHANDLES` + ProcThreadAttributeList
   `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` 精确继承列表,受限令牌下工作正常。
3. **lpDesktop 必须设置**(process.rs L127-130 注释):否则 PowerShell 等
   `STATUS_DLL_INIT_FAILED`。我们或设 lpDesktop 为默认桌面,或按 codex 建
   私有桌面(`desktop.rs`,带缓存)。**建议 M1 直接建私有桌面**,顺带获得
   窗口消息隔离(UIPI 的近似替代,部分覆盖 V3)。
4. **`\\.\NUL` 要放行**:受限写检查会打到设备对象 DACL;`2>NUL` 这类重定向
   不放行就坏。spawn 前对 NUL 加 cap-SID ACE(设备对象用 SetKernelObjectSecurity)。
5. **deny carveout 目标必须预创建**(spawn_prep.rs L280-287 注释):
   "carveouts must exist before the command starts so the sandbox cannot create
   them under a writable parent first" —— 防"子进程先创建再继承父 ACL"的 TOCTOU。
   我们对 `.git` 默认 deny(见下)时同理。
6. **特权只保留 `SeChangeNotifyPrivilege`**(目录遍历需要);其余全裁。
7. **DefaultDACL 收紧**:OWNER RIGHTS ACE(S-1-3-4)压制 owner 隐式 WRITE_DAC,
   令子进程创建的对象 DACL 收敛到 logon SID(`token.rs` 尾部)。
8. **fail-closed 分辨矩阵**:policy 超出后端表达力 → 拒绝执行、错误回给模型。
   我们 `resolve_backend` 应移植同样的"可强制范围判定",而不是降级执行(H1 同源)。
9. **拒绝识别启发式**:`is_likely_sandbox_denied`(denial.rs L13-42,exit code +
   关键词)与我们的 `classify_denial` 同构;Windows 关键词表加
   "Access is denied / 拒绝访问 / exit 5"。
10. **已知兼容性问题有遥测**:WindowsApps 商店应用别名在受限令牌下打不开
    (专门打点 `codex.windows_sandbox.createprocessasuserw_failed`)——
    进我们的兼容性矩阵与错误上报。
11. **deny-read/glob 是快照语义**(spawn 前展开,之后新建文件不自动覆盖)
    ——与我们"读取不隔离"无冲突,但写 spec 时别许诺运行期动态 glob。

### 3.4 v1 spec 仍优于 codex 的点(保留理由)

- **CoW/pending-overlay(M2)**:codex 明确放弃了这类能力,遇到"必须落真实路径"
  的场景只能 fail-closed;我们的 hook 平面补的正是这块。
- **env 重指表(§5.5)**:codex 不做重指,缓存冷启动靠用户加 writable root;
  我们的 scratch + junction 农场在"免安装 + 半可信 agent 自助"场景下体验更好。
  (注意:cap-SID 锚点下重指表依然必要——TEMP/caches 的 DACL 只有用户 SID,
  没有 cap ACE,子进程照样写不了;§5.5 全表保留。)
- **审计/journal 事件流**:codex 无 per-op 裁决事件(无 hook 自然没有);
  我们 M2 的 journal.jsonl 是 spy 归因与命令归因(M3)的输入,是独有价值。

---

## 4. CVE-2025-64721 → §6.5 管道协议硬约束(逐条修订)

该 CVE:SbieSvc(ALPC broker)的 RC4Crypt handler 只校验消息总长,独立信任
客户端 `value_len`,32 位整数回绕 → 4GB memcpy → 堆毁 → 桌面堆 ROP → SYSTEM。
对我们的直接映射:

1. **daemon 是 broker,按最高价值目标对待**:沙箱内所有强制力的"放行"都出自它,
   它被打穿 = 全部作废。它必须只做"解析 → 裁决 → 回包",**绝不代执行子进程指定的
   任何路径/命令动作**(v1 已隐含,写成显式不变式)。
2. **消息硬上限**(如 4KiB/条)+ serde strict(`deny_unknown_fields`、path 字段
   长度上限、枚举封闭)——Rust 无手工指针算术,该 CVE 成因类别天然规避,
   但 JSON 解析仍有资源放大面(深嵌套/超长字符串)。
3. **每子进程独立管道实例 + 全局实例数上限**;未决 query 数上限,超限即
   `Deny(DaemonTimeout)` 并断链。防 open 风暴打穿 daemon(V5 的姐妹项)。
4. **裁决缓存**(§6.5 已有)保持:命中不进管道;`writable_roots` 前缀短路在
   hook 侧完成,不产生管道流量。
5. 超时 250ms → Deny、连接失败 → 全量 Deny(v1 已冻结,保留;与 AGT 的
   "无策略时 fail-closed"原则互证)。

---

## 5. AGT:证实无交集,借三条设计

AGT 全仓无 integrity level / Job Object / restricted token / AppContainer / hook 的
任何实现代码;`LIMITATIONS.md` "What AGT Is Not" 明确排除 OS 级隔离;其 Python
进程内软沙箱自我声明 "not a true security boundary"。spec 附录"中间件层定位,
已确认与本诉求无交集"判断**证实**。可借鉴:

1. **审批绑定动作身份(liftable-deny + enforced_identity)**:`write_paths` 审批
   不应只记路径,应记 `{path, content_sha256(审批时刻)}`;spawn 前重导出校验,
   不匹配则拒绝(防审批与执行之间文件被换)。与 spy `undo` 的
   "先校验 after sha 再回滚"同一防 TOCTOU 哲学,成本近零。
2. **审计日志哈希链**:spy `journal.jsonl` 与 sandbox `journal.jsonl` 每行追加
   `prev_hash`,形成 Merkle 链(AGT 审计模块同款)。改动历史即断链可检,
   实现成本一行状态。
3. **结构化不可用上报**:`capability.rs::detect()` 增加
   `available: bool + unavailable_reason`(仿 AGT SandboxProvider 的
   `is_available/unavailable_reason`),替换目前 `detail` 字符串里
   "not wired yet" 这种自由文本;审批 UI 的 WARNING 位
   (`permission.rs:175` 已有)消费同一结构。

---

## 6. 对 v1 spec 的逐条修订清单

| § | 修订 | 依据 |
|---|---|---|
| §3 架构图 | 平面一改名 "TokenPlane(restricted-token)",注记"对标 Sandboxie SbieDrv 的能力位,强度低一档,由 N1 决定不上驱动" | §2.2 |
| §4.1 | `SandboxBackend::WindowsLowIl` → `WindowsRestrictedToken`;`WindowsHookOverlay` 保留。**若坚持旧名,内部锚点仍应换**,但名实不符,建议趁未接线改名 | §3.2 |
| §4.1 | 新增 `deny_write_paths: Vec<PathBuf>`(默认 `workspace\.git/**`);`writable_files` 语义 = "spawn 前对该路径加 cap-SID allow ACE(文件必须已存在或由 daemon 预创建)" | §3.3-5 |
| §5.2 | 令牌构造整体替换为 `CreateRestrictedToken` 序列(保留 §5.2 的步骤骨架,替换 3-4 步:不再 SetTokenInformation Low IL,改为 restricting SIDs + DefaultDACL + 单特权) | §3.1/3.3 |
| §5.3 | scratch 目录**不再需要 Low label**;改为创建时对 scratch 根挂 cap-SID allow ACE(容器+对象继承)。V1 复核点整体作废 | §3.2 |
| §5.4 | **整节删除**(临时降 label + pending-labels.json + 崩溃恢复)。替代:writable_files → ACE 追加(幂等,无恢复);SID 按 workspace 持久化 | §3.2 |
| §5.5 | env 重指表全表保留(cap-SID 下 TEMP/caches 依旧不可写);补 `denybin` 式网络 steering 为可选(M1 网络 = 明示非强制的 env steering,吸收 codex `apply_no_network_to_env`) | §3.4 |
| §5.6 | Job Object 保留;补:私有桌面 + ProcThreadAttributeList(继承句柄列表)进 spawn 清单 | §3.3-3 |
| §5.7 | 反馈文案保留;`classify_denial` Windows 关键词表按 codex 扩充 | §3.3-9 |
| §5.8 | capability Windows 分支改报 `WindowsRestrictedToken`;加 `available/unavailable_reason` 结构化字段 | §5 |
| §5.9 | A2 改写:审批后写入成功,**不再断言 label 恢复**(改为断言 ACE 已在/可清理);A1 内核拒绝来源表述改 DACL | §3.2 |
| §6.5 | §4 的五条硬约束并入;daemon 不变式显式化 | §4 |
| §6.2 | hook 清单不变;补注:cap-SID 锚点下"M2 hook 被绕过 → 内核 DACL 拒",与 v1 的 IL 论证等价 | §3.2 |
| §7 M3 | 增列 **MXC(微软 mxc)评估**:codex `mxc-sandbox` 已接入 `BaseContainerRunner`(零安装路径),AGT 亦引用;与 AppContainer/WinFsp 并列打分 | §3.1 |
| §8 | 集成清单增补(见 §7) | — |
| §10 | V1 作废;V2/V11 关闭;V3 改写为 restricted-token 兼容面;V5 增管道实例配额;其余照旧 | — |

## 7. 集成面补充(本仓代码勘察新事实,补进 §8/§10)

1. **`wrap_command` 全仓仅 2 个调用点**(`exec/direct.rs:100` 与一个 bwrap 集成测试),
   SbxChild 重构波及面收敛:对 `std::process::Child` 的真实使用 =
   direct.rs 6 处(stdin.take/write_all/kill/wait/stdout.take/stderr.take)+
   `process_registry.rs` 3 处(attach_child 里 `id()`、try_wait、kill 路径
   `taskkill /T /F` + wait)。pipe.rs 对 Child 零依赖(纯 `Read` 泛型)。
   **SbxChild trait 建议冻结为**:`id / try_wait / kill / wait / take_stdin_writer /
   take_stdout / take_stderr`;Windows kill 路径在沙箱子进程上应改为
   Job CloseHandle(process_registry.rs:702-721 是替换点),taskkill 仅作
   非-沙箱进程兜底。
2. **穷举 match 同步点**:`resolve_backend`(qaqh-sandbox/lib.rs:230-260)、
   `classify_denial`(lib.rs:328-331)、capability.rs detect 分支 —— 新增
   SandboxBackend 变体必须同时改这三处(编译器会强制,但提前知道省一轮)。
3. **`SandboxCapabilities::detect()` 是 `ToolIntent.sandbox_spec_hash` 的输入**
   (`qaqh-runtime/src/agent/tool_runtime.rs:498-503`):Windows detect 输出改变后,
   Windows intent hash 全变。属预期一次性漂移,记 changelog 即可,不算破坏。
4. **exec 加 `write_paths` 的接线清单**:`exec/handler.rs` ExecArgs(deny_unknown_fields,
   L29-48)+ `exec_schema()`(L437-477)+ authorization.rs
   `permission::extract_target_paths`(exec 现只取 cwd)+ conflict.rs:13
   `file_write_paths`(同批工具写冲突分组,天然衔接)。审批复用
   `ApprovalRegistry`(challenge TTL 默认 120s,单次性靠类型)。
5. **审批 UI 警告位已就绪**:`permission.rs:175` 在沙箱未强制时追加 WARNING 文案,
   Windows 落地后此分支自动消失,审计语义(`GrantKind::SandboxAuto` 亦已存在)无需改动。
6. **spy 交互**:cap-SID ACE / label 变更不影响 spy(基于 mtime+size+sha 扫描,
   ACL 变化不触发);M2 起 `changes_since` 读 upper 为事实源(spec §8 已列)。
7. **序列化消费方排查结论**:SandboxSpec/SandboxBackend 目前不出现在
   wire/UI/审计 v2(全仓 grep 零命中),新增变体无前端波及——spec §8 该项
   "排查"可关闭,只余 daemon 测试 `sandbox_helper.rs` 的 JSON 形状。

## 8. 结论与下一步

1. 双平面总架构、M2 hook 设计、§4.2 裁决纯函数、§4.3 scratch 布局、§9 测试计划
   **维持**;M1 锚点按 §6 清单切换为 restricted-token + cap-SID。
2. 修订量集中在 §5.2-§5.4 与 §4.1/§5.8,估时不变(M1 1.5-2 周内含 SbxChild 重构,
   codex 参照使 spawn/ACL 两块的不确定性显著下降)。
3. 先做一次 30 分钟的 spike 验证唯一剩余的机制未知数(原 V1 的替代项):
   非提权进程对**用户所有但非本进程创建**的工作区文件 `SetNamedSecurityInfoW`
   (DACL 追加 ACE)是否全部成功(owner WRITE_DAC 依据成熟,预期通过;
   失败则回退 v1 Low-IL 方案,§5.4 作为 Plan B 保留在本评审文档,不删)。

### 参考链接

- Sandboxie 隔离机制(官方):https://sandboxie-plus.com/sandboxie/isolationmechanism
- Sandboxie CVE-2025-64721 分析(DepthFirst, 2025-12):
  https://depthfirst.com/research/alpc-you-later-cve-2025-64721-sandbox-escape-smashing-the-heap-over-ipc
- Sandboxie 组件源位:`Sandboxie/core/{drv,svc,dll}`(https://github.com/sandboxie-plus/Sandboxie)
- Codex Windows 沙箱:`E:\myXCode\codex-rs\windows-sandbox-rs`(token/acl/spawn_prep/process)、
  `codex-rs\sandboxing\src\windows.rs`(fail-closed 分辨)、`codex-rs\core\README.md` L65-93(Windows 支持面)
- AGT:`E:\agent-governance-toolkit`(`policy-engine/spec/SPECIFICATION.md`、
  `docs/LIMITATIONS.md`、`agent-mesh/src/agentmesh/governance/audit.py`)
