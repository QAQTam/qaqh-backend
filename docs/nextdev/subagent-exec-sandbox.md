# B2：子代理 exec 沙箱化 — 设计文档

> 状态：**设计稿（待实施）** — 前置：方案 B1（子代理权限沙箱，自动批准/自动拒绝）已落地
> 依赖代码：`crates/deepx-workspace/src/exec.rs`（工具 handler）、`backend.rs`（HttpExecuteRequest）、`authorization.rs`（B1 沙箱标志）
> 关联：`docs/nextdev/subagent` 系列（B1 实施记录见 git log）

## 1. 背景

B1 落地后，子代理的权限行为是：

| 操作 | B1 行为 |
|---|---|
| workspace 内文件读写/编辑 | 自动批准（等价 Level 3 语义） |
| 跨 workspace 文件访问 | 自动拒绝（`PermissionDenied`） |
| **exec / 网络** | **自动拒绝**（防逃逸，但子代理无法跑测试/构建） |

B1 拒绝 exec 是**安全优先的取舍**：exec 天然可逃逸路径级检查（`cd ..`、绝对路径引用、环境变量、孙进程）。B2 的目标是在**不放开逃逸能力**的前提下，让子代理能安全执行 workspace 内的受限命令（测试、构建、lint、git 只读等）。

## 2. 现状（执行路径）

```
子代理 worker（host）
  ├─ prepare_req → authorization::admit（B1 沙箱已在此层拒绝 exec）  ← 需要放开 exec 的"拒绝"
  └─ HttpToolExecutionBackend ──HTTP──▶ serve 进程（deepx-workspace serve）
                                          └─ exec handler（exec.rs:572 Command::new spawn）
                                       或回退：worker 进程内 LocalToolExecutionBackend 直接跑 handler
```

关键事实：
- **exec 的 handler 是共享代码**（`exec.rs`），无论从 serve 进程还是 worker 进程调用都是同一实现 → **沙箱改造集中在 exec handler 层即可双端生效**；
- `exec.rs:884-902` 已有 workspace 路径解析（相对 cwd 基于 workspace root 解析）——弱沙箱可复用；
- serve 端请求已携带 `workspace`（`HttpExecuteRequest`）——沙箱根已知。

## 3. 目标与非目标

**目标**：
1. 子代理可执行 **workspace 内**的受限命令（`cargo test`、`python -m pytest`、`git status` 等）；
2. 任何尝试访问 workspace 外资源（文件/网络/环境）的命令被**可靠拦截或无法成功**；
3. 子代理 exec 不产生新的审批弹窗（延续 B1：无人工审批通道）；
4. 主代理 exec 行为**完全不变**。

**非目标**：
- 完全的内核级隔离（不引入虚拟机/容器运行时）；
- 任意命令白名单 DSL（B2.1 只做硬性路径/环境限制，白名单留作 B2.4 可选）；
- 网络白名单化（沙箱内默认禁网，后续单独决策）；
- 修改主代理的权限模型。

## 4. 威胁模型（exec 逃逸路径清单）

| # | 逃逸路径 | 示例 | B2 拦截层 |
|---|---|---|---|
| T1 | 参数直接引用 workspace 外 | `cat ~/.deepx/config.toml`、`type C:\Windows\...` | 弱沙箱参数扫描（B2.1） |
| T2 | 目录穿越 | `cd .. && cat ../secret`、`..\..\` | cwd 规范化 + 启动后 cwd 校验（B2.1） |
| T3 | 环境变量泄漏 | `echo $HOME`、`env` | 环境最小化（B2.1） |
| T4 | 符号链接/junction 指向外部 | workspace 内 `link -> ~/.deepx` | canonicalize 检查 + OS 沙箱兜底（B2.2/2.3） |
| T5 | 孙进程逃逸（命令后台化/派生） | `python -c "import subprocess..."`、`start` | Job Object 树终止（B2.2）/ Landlock 进程树（B2.3） |
| T6 | 网络外联 | `curl https://...` | AppContainer 网络能力剥离（B2.2）/ 弱沙箱禁网命令扫描（B2.1 尽力） |
| T7 | shell 包装绕过 | `sh -c 'evil'`、`cmd /c ...` | 弱沙箱：禁止 shell 包装 + argv 直传（B2.1） |

**结论**：B2.1 弱沙箱可堵住 T1/T2/T3/T7 的大多数；**T4/T5/T6 只有 OS 级沙箱（B2.2/2.3）能可靠拦截**——因此分层实施，弱沙箱先行（快），OS 沙箱兜底（稳）。

## 5. 方案设计

### 5.1 架构位置与标志传递

```
admit() 沙箱判定（authorization.rs，B1）
  └─ exec 的 Denied → 改为 "放行但标记沙箱执行"
       ↓ 传递路径：
  engine_tool → ToolCallCtx 增加字段？→ exec handler 读沙箱标志
```

两条传递路径（择一，推荐 A）：

- **路径 A（推荐）：进程内标志 + handler 读取**。B1 已有 `authorization::is_subagent_sandbox()` 进程级标志。exec handler 里直接读该标志即可——**worker 进程（LocalBackend 回退）天然正确**；但 serve 进程是共享的（主/子代理共用），**serve 端不能靠进程标志**，必须走请求字段。
- **路径 B：请求字段**。`HttpExecuteRequest` 增加 `sandbox: bool`（由 host worker 的 `is_subagent_sandbox()` 填充），serve 端 exec handler 从请求上下文读取。**serve 端唯一可靠来源**。

**结论**：两端都要——worker 进程内（LocalBackend）读 `is_subagent_sandbox()`；serve 进程走 `HttpExecuteRequest.sandbox` 字段。统一在 exec handler 内部归一为 `sandbox: bool` 参数（handler 不知道自己在哪个进程，由调用方注入）。

### 5.2 分层实现

#### B2.1 弱沙箱（跨平台，先做，预计 1-2 天）

`exec.rs` handler 沙箱模式下的强制规则：

1. **cwd 锁定**：忽略调用方 `cwd` 参数（或强制规范化到 workspace 内），`Command::current_dir(workspace_root)`；启动后立即读 `/proc/self/cwd`（Linux）或 `GetCurrentDirectory`（Windows）校验，漂移即 kill；
2. **argv 参数扫描**：遍历参数，拒绝：
   - 绝对路径指向 workspace 外（`starts_with(workspace_root)` 之外且不在显式例外列表）；
   - 含目录穿越模式的参数（`..` 段解析后仍在 workspace 外）；
   - shell 元字符组合（`;` `&&` `|` `>` `$(...)` 反引号——**仅当命令经 shell 包装时**；直接 argv 执行的命令不扫 shell 元字符，因为无 shell 解释）；
3. **禁止 shell 包装**：沙箱下 `shell=true` 参数强制关闭（直接 `argv` 执行）；拒绝 `sh -c`/`cmd /c`/`powershell -Command` 作为 argv[0] 的常见包装形态；
4. **环境最小化**：清理 `HOME`/`USERPROFILE`/`APPDATA`/`PATH` 中指向 workspace 外的关键项（PATH 仅保留系统必需目录 + workspace 内工具），移除 secret 类变量（`*KEY*`/`*TOKEN*`/`*SECRET*`/`*PASSWORD*`）；
5. **stdin 关闭**：沙箱命令无交互输入；
6. **超时强制**：沿用现有 timeout 机制，超时 kill 进程树。

**已知局限**（记录在案，由 B2.2/2.3 兜底）：参数扫描可被编码绕过（base64/hex 命令、unicode 变体）；`python -c "open('/etc/passwd')"` 这类"参数内嵌代码"扫描不可靠——**弱沙箱是尽力而为，不承诺防 T4/T5/T6**。

#### B2.2 Windows 沙箱（AppContainer + Job Object，预计 3-5 天）

- **AppContainer**（`CreateAppContainer`/`CreateProcess` + `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`）：
  - 文件系统：只授予 `{workspace}`（含子目录）+ `{temp}` + 显式例外目录的 ACL；`~/.deepx`、系统目录、其他用户目录**默认不可见**（DACL 拒绝）→ 堵 T1/T2/T4；
  - 网络：AppContainer 默认无网络能力（不添加 `InternetClient` 能力）→ 堵 T6；
  - 写保护虚拟化：可选（未来临时文件策略）。
- **Job Object**（`CreateJobObject` + `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` + `JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK` 不设置）：
  - 进程树统一归属 job，父进程死亡/超时 → 整树终止 → 堵 T5；
  - 限制桌面访问（可选）。
- 实现载体：`exec.rs` 的 Windows spawn 分支（`cfg(target_os = "windows")`），需要新增一个 `sandbox_windows.rs` 模块（Job + AppContainer 生命周期管理）。

#### B2.3 Linux/macOS 沙箱（Landlock / seccomp，预计 3-5 天）

- **Landlock**（Linux ≥ 5.13，`landlock_create_ruleset` syscall）：规则集 `AccessFS` 只允许 `{workspace}` + 例外目录 + 只读系统库路径；进程树（含子进程）继承规则 → 堵 T1/T2/T4/T5（进程树内强制）；
- **seccomp 兜底**（可选）：禁用 `execve` 到 workspace 外路径不可行（seccomp 不做路径判断），Landlock 是正确工具；seccomp 只用于禁 `ptrace` 等自举逃逸；
- macOS：无 Landlock，降级为 B2.1 弱沙箱 + 文档记录局限（macOS 不作为首期目标）。
- 内核不支持 Landlock（< 5.13 或容器内无权限）→ 自动降级 B2.1 + 日志警告。

### 5.3 例外与降级策略

| 场景 | 策略 |
|---|---|
| 内核/OS 不支持（Linux 无 Landlock、macOS） | 自动降级 B2.1 弱沙箱，`log::warn` 记录"弱沙箱模式" |
| workspace 内需要访问工具链（cargo/node 在 workspace 外） | 例外目录列表：`PATH` 解析出的可执行文件所在目录 + `{workspace}`；可配置项 `subagent.sandbox_extra_dirs` |
| 子代理任务需要临时文件 | `{temp}/{sub_seed}` 独立子目录授予，命令 `TMPDIR` 指向它 |
| 主代理 | 永不进入沙箱分支，行为零变化（标志在 subagent worker 进程才有） |

### 5.4 网络策略（待决策，本期默认禁网）

B2.2 AppContainer 天然禁网；B2.1/B2.3 下网络仍可能（`curl` 直连）——参数扫描无法可靠拦截。**默认沙箱禁网**；如后续需要（如 `cargo publish --dry-run` 类），再单独设计网络白名单（域名级代理或 AppContainer 网络能力定向授予）。

## 6. 实现路径

| 阶段 | 内容 | 改动点 | 预估 |
|---|---|---|---|
| **B2.1** | 弱沙箱：cwd 锁定 + 参数扫描 + 禁 shell 包装 + 环境最小化 + stdin 关闭 | `exec.rs` handler 沙箱分支；`HttpExecuteRequest.sandbox` 字段（backend.rs + serve.rs）；admit 对 exec 改为"沙箱放行"而非 Denied | 1-2 天 |
| **B2.2** | Windows：AppContainer + Job Object | `exec.rs` Windows spawn 分支 + 新 `sandbox_windows.rs` | 3-5 天 |
| **B2.3** | Linux：Landlock 规则集 + 降级 | `sandbox_linux.rs` + Landlock FFI（或 `libc` 封装） | 3-5 天 |
| B2.4 | （可选）命令白名单：`subagent.sandbox_allowed_commands` 配置 | 配置 + 校验 | 待定 |

**B2.1 落地前置**：B1 的 `admit` 需要从"exec → Denied"改为"exec → 标记沙箱放行"。注意保留跨 workspace 文件操作的拒绝（那是 B1 语义，与 B2 无关）；exec 的沙箱判定交给 exec handler 的运行时强制，而非 admit 的静态拒绝。

## 7. 验收矩阵

| # | 场景 | 期望 |
|---|---|---|
| A1 | 子代理 `cargo test`（workspace 内，工具链在例外目录） | ✅ 成功 |
| A2 | 子代理 `git status`（workspace 内） | ✅ 成功 |
| A3 | 子代理 `cat ~/.deepx/config.toml`（绝对路径外部） | ❌ 拒绝（B2.1 扫描 / B2.2 ACL / B2.3 Landlock） |
| A4 | 子代理 `cd .. && cat ../secret.txt` | ❌ 拒绝（cwd 校验 + 参数扫描） |
| A5 | 子代理 `curl https://x` | ❌ 拒绝（B2.2 禁网；B2.1 尽力，记录局限） |
| A6 | 子代理后台派生孙进程后自身退出 | 整树终止，无残留（Job Object / Landlock） |
| A7 | 主代理任意 exec | 行为与现在完全一致（不进沙箱分支） |
| A8 | Linux 无 Landlock 内核 | 自动降级弱沙箱 + 日志警告，不崩溃 |

## 8. 决策记录（待评审）

1. **exec 沙箱 vs 白名单**：先做沙箱（硬性限制），白名单（B2.4）作为配置增强——沙箱保证"默认安全"，白名单提供"精细放行"；
2. **网络默认禁**：B1 已禁 Net 类别；exec 内嵌网络命令由沙箱兜底，本期不开放；
3. **例外目录可配置**：`subagent.sandbox_extra_dirs`（默认空），避免硬编码工具链路径；
4. **B2.1 的局限是否可接受**：编码绕过（base64）在 B2.1 下可绕过参数扫描——若产品上线要求强保证，B2.1 不应单独发布，需 B2.2/2.3 至少一项配套。

## 9. 不做的事（明确排除）

- 容器/VM 级隔离（WSL2、Docker、Windows Sandbox）——重、启动慢，不符合"子代理秒级拉起"；
- 修改主代理 exec 语义；
- 为子代理恢复人工审批弹窗（B1 决策：无审批通道）；
- macOS 首期支持（无 Landlock，仅弱沙箱，文档记录）。
