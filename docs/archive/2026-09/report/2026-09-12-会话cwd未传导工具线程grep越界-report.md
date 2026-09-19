# 会话cwd未传导工具线程grep越界（2026-09-12）

## 0. 元信息

| 项 | 值 |
|---|---|
| 报告日期 | 2026-09-12（UTC+8） |
| 分析对象 | 后端 `D:\project\QAQ-Harness` @ `4e03a88`（v1.0.1，工作区干净）；前端 `D:\project\qaqh-winui-app`（HEAD 未参与结论，仅核对链路）；引入 commit `dae42c7`（2026-09-11） |
| 触发方式 | 用户报告：「前端设置 cwd/工作目录，后端 grep 仍然无法收到」；续问「哪个 commit 带入这个 bug」 |
| 执行者 | QAQ-Harness 调试会话（AI 助手） |
| 结论 | **P1 功能阻塞**。前端 → daemon 的 cwd 协议链路完好；断点在后端内部：`dae42c7` 让 actor 上下文跳过物理 cd 后，「会话 cwd」只存在于 actor 线程 TLS，而工具（grep/glob/read/exec）运行在派生线程，`current_workspace()` 回退到恒空的进程全局 → 路径解析锚定 daemon 进程 cwd。引入 commit：**`dae42c745518e25fae7c9a5ecbe229d71f9581ee`**（2026-09-11 13:01 +0800）。 |

## 1. 结论摘要

| ID | 严重度 | 状态 | 类型 | 位置 | 影响（一句话） |
|---|---|---|---|---|---|
| BUG-2026-09-12-05 | **P1** | **已修复（工作区待提交，回归 2/2 通过）** | 功能阻塞 / 路径解析 | `crates/qaqh-workspace/src/grep_tool.rs:225,263-266`；根因 `lib.rs:282-291`、`workspace.rs:41-52`、`runtime.rs:162-198` | 相对路径 / 跨目录 `paths` 的 grep 以 daemon 进程 cwd 为根：全部误报 `resolves outside the workspace` 或搜错目录；glob、相对路径 read 同根因 |
| （同根因·面2） | P2 | **已修复（同一提交）** | 静默漂移 | `crates/qaqh-workspace/src/exec/handler.rs:249-259` | exec 缺省 cwd 兜底读到空工作区 → 子进程静默继承 daemon 进程 cwd，`pwd`/相对路径产物落错目录，无任何报错 |
| （同根因·面3） | P2 | **已修复（同一提交）** | 授权基准错位 | `crates/qaqh-workspace/src/authorization.rs:380-390`、`crates/qaqh-runtime/src/agent/state/lifecycle.rs:201-205,260-263,292-295,322-325` | 授权工作区快照读进程全局（恒空）；lifecycle 初始化 `SkillContextManager` 同样读全局 → skills 工作区错误 |

## 2. 分析方法与证据链

1. 前端链路核对（qaqh-winui-app）：rg 定位 `cwd` 相关代码点 → 逐点核对 `workspace.set` / `SessionCreate.cwd` 发送路径与显示回读路径。**结论：前端无责（E2）**。
2. 后端执行链逐行读通：`grep_tool.rs` → `lib.rs::current_workspace`（TLS 优先、全局回退）→ `workspace.rs::set_process_workspace`（actor 上下文跳 cd）→ `runtime.rs::ActorToolScope`（capture 字段清单不含 workspace）→ `engine_tool.rs:626-645` / `turn_lap/admit.rs:112-143`（工具在派生线程执行 + scope 恢复）。**E2**。
3. git 考古：`git log -S` 逐符号追溯（`ActorToolScope`、`ACTOR_WORKSPACE`、`effective_workspace_root`、`in_actor_context`、`set_current_dir`、`set_process_workspace`），`git show` 核对关键 diff 与 Initial commit 形态。**E2，关键 diff 原文见 §3.2**。
4. 现场旁证（E1 现象 / E3 机制归因，见 §4）：调试会话内相对路径 grep 反复报 `RESOURCE_MISMATCH`（`execution.rs:59` 的授权/执行资源比对失败），与「admit 线程按 TLS 解析、执行线程按进程 cwd 解析」的错位模型一致；运行中 daemon 的构建 commit 未核实，机制归因列为 E3。

## 3. 发现 D-1：会话 cwd 未传导到工具线程，grep 以进程 cwd 为界

### 3.1 现象

- 前端设置工作目录后，grep 传相对 `paths`（如 `["crates/qaqh-workspace/src"]`）报错：`grep: path … resolves outside the workspace — search is workspace-bounded`。
- grep 传仓库外绝对路径时边界行为"看起来对"、传仓库内相对路径时反而失败——与"前端没把 cwd 送到后端"的直觉完全相反。
- read/exec 因模型惯用绝对路径而长期"正常"，掩盖了同一缺陷。

### 3.2 根因（E2：逐行调用链 + diff 归属）

数据流断点（`4e03a88` 行号）：

```
[写入侧] workspace.set(service.rs:430-445) → meta.cwd 持久化 ✅
          AgentReloadConfig → engine_session.rs:82 → lifecycle.rs:11-18
          → set_process_workspace (workspace.rs:41-52)
             ├─ crate::set_workspace(&path)
             │    └─ lib.rs:390-397：actor 上下文 → 只写本线程 TLS（ACTOR_WORKSPACE）
             │       非 actor 上下文 → 写进程全局 CURRENT_WORKSPACE
             └─ in_actor_context → return   ← dae42c7 新增：跳过物理 cd
[读取侧] 派生工具线程（无 TLS）：
          grep_tool.rs:225 → current_workspace() (lib.rs:282-291)
             ├─ ACTOR_WORKSPACE = None（派生线程）
             └─ CURRENT_WORKSPACE = ""（daemon 从不写全局）
          → 回退 "." → std::env::current_dir() = daemon 启动目录
          → grep_tool.rs:263-266 边界判定锚定错误根
```

关键事实：

1. **工具不在 actor 线程执行**：`engine_tool.rs:626-645`、`turn_lap/admit.rs:112-143` 将工具派发到 `std::thread::Builder::spawn` 的 OS 线程，靠 `ActorToolScope::capture()/install()` 搬运 per-actor 状态（`runtime.rs:155-198`）。该结构自 Initial commit `d5ba44c` 起就**不含 workspace 字段**（只有 runtime/manager/mode/sandbox，后加 policy）。
2. **`dae42c7` 的 diff（决定性证据，原文节选）**：

```diff
 pub fn set_process_workspace(path: &str) {
     let path = crate::wsl_path::platform_workspace_path(path);
+    let in_actor_context = crate::is_actor_context();
     crate::set_workspace(&path);
+    if in_actor_context {
+        return;
+    }
     if let Err(error) = std::env::set_current_dir(&path) {
```

   该 commit 的测试注释自述前提：「Path resolution never depends on the process cwd while an actor context is active」——该前提**只在 actor 线程成立**，对派生工具线程不成立。
3. **历史形态核对**：`d5ba44c`（2026-08-22）时 `set_process_workspace` 无条件物理 cd，单 worker 进程模型下进程 cwd = 会话 cwd，工具线程读 `"."` 兜底仍落对目录 → 无 bug；`81a83da`（08-31，PR-3-3 cwd 宿主注入）把注入点移到 actor 线程但 cd 仍在 → 无 bug（隐患已埋）；`dae42c7`（09-11 13:01）移除 cd → bug 生效。`b6e1d96`/`4e03a88`（09-12）对 `workspace.rs` 零改动（`-S` 命中来自删除的 docs），排除。
4. **dae42c7 自身已知工具线程不继承 TLS**：其父提交（`dae42c7~1`）的工作区记忆 `.workbuddy/memory/MEMORY.md:33` 明确记载「工具执行在 actor 派生的 OS 线程上，不继承 thread-local，必须靠 `ActorToolScope::capture()/install()` 搬运」——搬运清单漏了 workspace。
5. **测试缺口**：`dae42c7` 报告 958 通过 / 2 失败（均为已知环境敏感 exec 测试）。无任何用例覆盖「actor 设 cwd → 派生线程相对路径 grep」组合。

### 3.3 影响面

- `grep`：相对/跨目录 `paths` 必然失败或搜错根（P1 显性报错）。
- `glob`（`file_glob.rs:40`）、相对路径 `read`（`resolve_workspace_path`，`lib.rs:403-422`）、`write/edit` 相对路径：同根因，锚定 daemon 进程 cwd。
- `exec`：缺省 cwd 静默漂移（§4-2，无报错，最阴险）。
- 授权基准：`authorize_call` 的 `effective_workspace_root()` 读进程全局（`authorization.rs:380-390`），与 admit 时按 TLS 解析的资源在两线程上可能不一致 → 触发 `execution.rs:39-49`（WORKSPACE_MISMATCH）与 `:58-60`（RESOURCE_MISMATCH）防御性拒绝。
- 兼容面：独立 serve / CLI 进程（非 actor）不受影响；WSL 后端模式若依赖路径归一化同理受影响（未验证，见 §5）。

### 3.4 复现（E2 + E1 旁证）

代码级复现步骤（E2）：

1. 构建含 `dae42c7` 的 daemon，前端创建会话并 `workspace.set` 到目录 A（非 daemon 启动目录）。
2. 对话中让模型执行 `grep`，`paths=["任意相对目录"]`。
3. 期望：按 A 解析；实际：按 daemon 进程 cwd 解析 → `resolves outside the workspace` 或结果为空。

真机旁证（E1 现象，机制归因 E3）：本调试会话（运行中 daemon，含该缺陷代次构建）内，绝对路径 glob/grep 正常，相对路径 grep 三连报 `[ERROR] RESOURCE_MISMATCH`（原文：`grep: path "crates/qaqh-workspace/src" …`→`[ERROR] Resource mismatch — tool invocation targets different resources than authorized`）。与「admit 线程（有 TLS）按 A 解析资源、执行线程（无 TLS）按进程 cwd 重解析 → 两份资源不相等 → `execution.rs:58-60` 拒绝」的模型吻合。

### 3.5 修复建议（最小 diff）

1. **治本**：`runtime.rs::ActorToolScope` 增加 `workspace: Option<String>` 字段；`capture()` 追加 `workspace: Some(crate::current_workspace())`；`install()` 写入并在 `Drop` 恢复（写入通道：`lib.rs` 增加一对 `push/pop_thread_workspace` 或将 `ACTOR_WORKSPACE` 暴露 `pub(crate)`）。一处修复同时校正 grep/glob/read/exec/授权基准。
2. `authorization.rs::effective_workspace_root()` 改用 TLS 优先的 `crate::current_workspace()`。
3. `lifecycle.rs` 四处 `qaqh_workspace::CURRENT_WORKSPACE.read()` 改为 `qaqh_workspace::current_workspace()`。
4. 加固（另列，不阻断）：`grep` 增加 `cwd` 参数（语义同 exec：经 `resolve_workspace_path` 提升，仍受边界约束），更新工具 schema 描述引导相对路径场景显式传参；代价是 schema 变更需前端/提示词同步。

明确**不建议 revert `dae42c7`**：跳 cd 的动机（多 actor 进程 cwd 互踩）正当，且该 commit 携带大量存储架构改动。

### 3.6 验收清单

```
# 1) 新增回归测试（建议名）：
cargo test -p qaqh-workspace --lib tool_thread_sees_actor_workspace
#    场景：set_actor_context(A, seed) → 派生线程上 current_workspace()==A，
#    且 grep 相对路径命中 A 下文件、不报 outside。

# 2) 既有回归（确认不破坏 dae42c7 的防漂移语义）：
cargo test -p qaqh-workspace --lib set_process_workspace_skips_cwd_inside_actor_context
cargo test -p qaqh-workspace
cargo test -p qaqh-runtime --lib

# 3) clippy 无新增告警：
cargo clippy -p qaqh-workspace -p qaqh-runtime --all-targets

# 4) 端到端（人工）：前端设 cwd=项目 B → 相对路径 grep 命中 → 切第二会话工作区 C →
#    B/C 两会话并发 grep 结果互不串扰（多 actor 隔离语义保持）。
```

## 4. 次要观察

| # | 位置 | 观察 | 级别 | 证据 |
|---|---|---|---|---|
| 1 | `exec/handler.rs:249-259` | 缺省 cwd 兜底 `current_workspace()` 在工具线程读空 → 子进程继承 daemon 进程 cwd，静默落错目录；dae42c7 的 commit message 自称「exec cwd 兜底锚定会话工作区，杜绝漂移」，实际兜底读到的仍是空工作区，与宣称相反 | P2 | E2（代码链）；静默性意味着无错误日志可查 |
| 2 | `authorization.rs:380-390` | `effective_workspace_root()` 直接读 `CURRENT_WORKSPACE`，绕过 TLS 优先语义；与 admit/execute 双线程解析错位共同构成 RESOURCE_MISMATCH/WORKSPACE_MISMATCH 拒工具通道 | P2 | E2（代码链）+ E1 现象（§3.4） |
| 3 | `lifecycle.rs:201-205,260-263,292-295,322-325` | 已调用 TLS 优先的 `load_session_workspace` 后，紧接读进程全局初始化 `agent.skills` → daemon 中 skills 工作区恒为空根 | P2 | E2（代码链） |
| 4 | `runtime.rs:155-172` | `ActorToolScope` 文档注释声明搬运清单（context/manager/mode/sandbox/fold-policy），漏列 workspace——结构性遗漏而非笔误，修复时建议同步补注释 | P3 | E2 |
| 5 | 调试会话内建 `grep` 对 `paths` 的若干调用报 `RESOURCE_MISMATCH`、对仓库外绝对路径报 `outside the workspace` | 后者为正确行为（路径确在会话工作区外），不作为本 bug 证据；前者纳入观察 #2 | — | E1（原文粘贴于会话记录） |

## 5. 不确定性与未验证假设

1. **运行中 daemon 的构建 commit 未核实**（§3.4 旁证的机制归因因此为 E3）：`version.txt=1.0.1` 与 `4e03a88` 对应，但未取得运行进程的构建信息。
2. **WSL 后端模式**下的影响未验证（`wsl_path::platform_workspace_path` 归一化与 TLS 缺失的交互）。
3. **未实测修复 diff**：§3.5 方案未经编译/测试验证，仅到代码链论证为止；`push/pop_thread_workspace` 的具体形态（新增 API vs 暴露 `pub(crate)`）留待实现者定夺。
4. 前端仓库 `qaqh-winui-app` HEAD commit 未记录（仅核对了工作区代码链路）。


> **修复附注（2026-09-12 17:37）**：§3.5 方案已落地为工作区改动（lib.rs push/pop_thread_workspace、runtime.rs ActorToolScope.workspace、authorization.rs、lifecycle.rs、qaqh-subagent lib.rs 继承源）。验证：新增回归 `tool_thread_workspace` 2/2 通过；`cargo test -p qaqh-workspace` 284 过/2 败（均为 dae42c7 message 登记的既有环境敏感失败）；`cargo test -p qaqh-runtime --lib` 174/174；`cargo test -p qaqh-subagent` 3/3；`cargo clippy` 三 crate 无新增告警；`cargo check -p qaqh-daemon` 通过。dae42c7 防漂移语义测试 `set_process_workspace_skips_cwd_inside_actor_context` 保持通过。

## 6. 产物与复现物清单

| 路径 | 类型 | 是否落盘 | 说明 |
|---|---|---|---|
| `docs/archive/2026-09/report/2026-09-12-会话cwd未传导工具线程grep越界-report.md` | 本报告 | ✅ | 即本文件 |
| — | 代码改动 | 无 | 纯分析，未修改任何源码 |
| 会话内调试命令记录 | 过程物 | 会话日志 | git -S/show 命令原文见附录 B |

## 7. 后续工作与建议排期

| 优先级 | 事项 | 责任面 |
|---|---|---|
| P1 | 落地 §3.5-1/2/3（ActorToolScope + 授权基准 + lifecycle），附 §3.6 回归测试 | workspace/runtime 维护者 |
| P2 | grep `cwd` 参数加固 + 工具 schema/提示词同步 | workspace 工具面 |
| P2 | 排查既有数据影响：dae42c7 之后产生会话中 exec 相对路径产物的落盘位置核对（运维） | 运维/用户确认 |
| P3 | `ActorToolScope` 文档注释补全（观察 #4） | 随 P1 顺带 |

## 附录 A：环境快照

- OS：Microsoft Windows 10.0.26300.9539（x64）；Shell：pwsh 7
- git 2.55.0.windows.5；rustc 1.98.1（48a229cea 2026-09-01）；cargo 1.98.1
- 分析用工具：内置 read/glob、内置 grep（受限，部分调用返回 RESOURCE_MISMATCH，见观察 #5）、pwsh 下 rg / git
- 行号基准：后端 `4e03a88`（2026-09-12，v1.0.1）

## 附录 B：复现命令（可原样复制）

```powershell
# 1) 确认 grep 边界检查与读取点在 Initial commit 即存在（行号与 HEAD 一致）
git -C D:\project\QAQ-Harness grep -n "workspace-bounded" d5ba44c -- crates/qaqh-workspace/src/grep_tool.rs
git -C D:\project\QAQ-Harness grep -n "current_workspace\(\)" d5ba44c -- crates/qaqh-workspace/src/grep_tool.rs

# 2) 确认 Initial commit 的 set_process_workspace 无条件物理 cd（无 in_actor_context 守卫）
git -C D:\project\QAQ-Harness show d5ba44c:crates/qaqh-workspace/src/workspace.rs

# 3) 定位"跳过 cd"的引入 commit（唯一命中即 dae42c7）
git -C D:\project\QAQ-Harness log -S "in_actor_context" --format="%h %ad %s" --date=short
git -C D:\project\QAQ-Harness show dae42c7 -- crates/qaqh-workspace/src/workspace.rs

# 4) 排除 09-12 两个 commit（对 workspace.rs 零改动）
git -C D:\project\QAQ-Harness show b6e1d96 -- crates/qaqh-workspace/src/workspace.rs
git -C D:\project\QAQ-Harness show 4e03a88 -- crates/qaqh-workspace/src/workspace.rs

# 5) 确认工具在派生线程执行且 ActorToolScope 自始不含 workspace
git -C D:\project\QAQ-Harness grep -n "ActorToolScope::capture" d5ba44c
git -C D:\project\QAQ-Harness show d5ba44c:crates/qaqh-workspace/src/runtime.rs
```
