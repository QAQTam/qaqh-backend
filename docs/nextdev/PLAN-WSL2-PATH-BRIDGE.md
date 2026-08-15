# PLAN：WSL2 模式工具执行路径桥接（缺口记录 + 修复方案）

> 状态：**核心路径桥接已实施**（2026-08-15 落地：serve 侧 Windows→/mnt 转换 + resolve + 授权边界，单测 7 用例 + WSL 端到端验证通过）；**exec 性能权衡待定**（见 §5）
> 日期：2026-08-15
> 范围：`deepx-workspace`（backend.rs / serve.rs / workspace.rs / lib.rs）、`deepx-runtime`（workspace_supervisor.rs）
> 关联：[PLAN-WORKSPACE-INPROCESS.md](./PLAN-WORKSPACE-INPROCESS.md)（阶段 3.2 WSL2 模式）、[PLAN-WORKSPACE-CAPABILITY.md](./PLAN-WORKSPACE-CAPABILITY.md)
> 证据基准：本节所有 file:line 均经直接代码阅读验证（2026-08-15）。

---

## 0. 结论

**WSL2 模式的工具执行链缺少「Windows workspace 路径 → WSL `/mnt/...` 路径」的转换。** serve 能在 WSL2 里拉起来（probe + `wsl.exe` + localhost 反射 endpoint 都通），但**工具真正执行时路径解析会错**：相对路径 join 到不存在的 Windows 根、绝对 Windows 路径在 Linux 侧无效、exec 的 cwd 是失败的 workspace。等于"serve 活着、但文件操作在错误的空间"。

**这是 WSL2 模式跑通前的硬性缺口**——不是可选项，是不做工具就全错。当前 Local 模式不受影响（同 OS 进程内/本地 serve）。

## 1. 现象 / 症状

切换到 `[workspace] mode = wsl` 并重启 daemon 后：
- `workspace.status` 显示 wsl、endpoint 正常（serve 拉起来了）；
- 但 read/write/edit/grep 对相对路径解析失败或落到错误目录；
- exec 的 cwd 不指向预期工作区；
- 授权边界（workspace 范围判断）基于 Windows 路径在 Linux 侧失效。

## 2. 现状与代码证据

| # | 缺陷 | 证据 |
|---|---|---|
| 1 | **`to_wsl_path` 不在工具执行链上** | `to_wsl_path`（workspace_supervisor.rs:463）唯一**生产调用点是 `install_wsl`**（L721，把仓库路径转给 WSL cargo build 编译 deepx-workspace）。工具执行从未调用它 |
| 2 | **worker 把 Windows 路径原样传给 WSL serve** | `backend.rs` `HttpExecuteRequest.workspace = host_workspace.to_string_lossy()`——worker 在 Windows 上，`host_workspace` 是 Windows 路径（如 `F:\DeepX\ws`），未转 `/mnt/f/...` 就发出 |
| 3 | **WSL serve 在 Linux 里用 Windows 路径 → cd 失败仅 warn** | serve.rs:395 `set_process_workspace(&job.request.workspace)` → workspace.rs:80-85 `std::env::set_current_dir(path)` 在 Linux 侧 cd `F:\DeepX\ws` 必失败（只 warn），但 `CURRENT_WORKSPACE` 仍被设成 Windows 路径 |
| 4 | **`resolve_workspace_path` 不转绝对路径** | lib.rs:218-235：绝对路径直接 `as-is` 返回（L224-225），不转 `/mnt/...`；相对路径用 Windows 根 join 也错 |
| 5 | **授权边界同样错** | `authorized_resources` / workspace 边界判断基于 workspace 路径，Linux 侧拿 Windows 路径判定 → 边界失效或误拒 |

## 3. 影响面

- **L1 工具**（read / write / edit_file_v2 / grep / exec）：路径参数解析、exec cwd、相对路径根 全错。
- **授权/沙箱**：workspace 范围判定错位（越权或误拒）。
- **文件账本**：`file_state` 以路径为 key，Windows 路径 vs WSL 路径不一致 → 账本对不上。

## 4. 修复方向（实施时参考）

在 **WSL serve 收到 workspace 的那一侧**做转换，覆盖：

1. `set_process_workspace` 收到 Windows 路径 → 转 `/mnt/{drive}/...`（复用/泛化 `to_wsl_path`，处理 `\\?\` 前缀、盘符大小写、UNC 拒绝）。
2. `resolve_workspace_path` 对绝对 Windows 路径也转 `/mnt/...`。
3. `authorized_resources` / workspace 边界在 WSL 侧统一用 `/mnt/...` 判定。
4. exec 的 cwd / 路径参数同理。

**语义决策（实施前需拍板）**：
- **A. 桥接映射**：Windows 工作区经 `/mnt/{drive}/...` 暴露——worker 的 Windows 路径与 WSL 侧一一对应，agent 操作的是同一份 Windows 代码（`/mnt/c` 性能损耗、inotify 受限）。
- **B. WSL 原生工作区**：在 WSL 侧建独立 Linux 工作区（`~` 或 `/home`），worker 传 WSL 路径——原生 Linux 语义、性能好，但 worker（Windows）路径与 WSL 路径分离，跨侧文件不一致。

推荐按 **A**（与现有 `to_wsl_path` / `install_wsl` 的 `/mnt` 约定一致，最小心智负担），但 A 的 `/mnt` 边界在 WSL 下是 9P 文件系统，git/cargo 性能与文件监听（inotify）受限——若目标是"WSL 原生干活"，B 更合适。

## 5. 性能权衡（2026-08-15 评估：A 方案的已知代价 → B 的论据）

微软官方文档（[WSL 跨文件系统文件存储与性能](https://learn.microsoft.com/zh-cn/windows/wsl/filesystems#file-storage-and-performance-across-file-systems)）明确建议"避免跨文件系统使用文件"：Linux 侧工具链应把文件放 **WSL 原生 ext4**（`/home/user/...`），`/mnt/<drive>` 是 DrvFs/9P 挂载，性能差（尤其大量小文件 I/O）。

对工具执行链的两类 I/O，代价不同：

| 操作 | 9P 跨文件系统代价 |
|---|---|
| 工具文件读写（read/write/edit/glob/apply_patch） | 单文件小批量，每操作几~几十 ms，可接受 |
| exec（cargo/git/node 等大量小文件 I/O） | 明显变慢（数倍），是主要痛点 |

**结论**：
- 方案 A 打通了**正确性**（serve 在 /mnt 上能正常干活，2026-08-15 端到端验证通过），但 exec 性能是已知代价——适合"先跑通"，不适合"WSL 里长期大量编译"。
- 微软文档是方案 B（WSL 原生工作区）的**硬论据**：若用户主要用 Linux 工具链且工作区可落在 WSL 侧（`~`），B 性能明显更优；代价是 Windows 工作区与 WSL 工作区分裂，需镜像/同步决策。
- 中间形态：A 下 exec 默认仍走 Windows 进程内（L1 本来如此），仅确需 WSL 环境时才走 serve，把跨文件系统 exec 降到最低。

**（端到端实测）**：WSL 内 `cargo build -p deepx-workspace` 在 /mnt/f 上 1m11s 完成——正确但慢于原生；作为一次性验证可接受。

### 参照：Codex 路径零转换（2026-08-15 实地调研 F:\codex）

OpenAI Codex 的 exec-server **不做 Windows↔/mnt 转换**：`PathUri` 原样透传，由"目标 OS 里跑的 exec-server"按本地语义解释（remote_file_system_path_uri_tests.rs:47-96 证明 Windows 盘符/UNC URI 原样发送）。唯一 WSL 路径转换在 CLI 层 `normalize_for_wsl`（仅当进程已运行于 WSL 内时转 `/mnt`）。

**启示**：A 方案的 /mnt 转换是"serve 主动适配 Windows 下发路径"；Codex 是"客户端按目标环境构造 PathUri、serve 零转换"——后者更简洁、对"多执行环境（local/wsl/remote）"更可扩展。**收敛方向**（可选，非当前必需）：worker 侧按 serve 环境构造 URI，serve 侧移除 /mnt 转换，仅保留"远端解释"。路径语义决策见 §6。

## 6. WSL 端就绪：Rust 工具链 + install_wsl（2026-08-15 实操记录）

WSL 端要能跑 serve，需先就绪两件事：**Rust 工具链** + **`deepx-workspace` 二进制**。

### 6.1 前置：WSL 内装 Rust（rsproxy 镜像）

WSL 里 `command -v cargo` 缺失时（`install_wsl` 会因此失败并提示 rustup），用 rsproxy 加速安装：

```bash
export RUSTUP_DIST_SERVER=https://rsproxy.cn
export RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
```

cargo 依赖镜像（`~/.cargo/config.toml`）：

```toml
[source.crates-io]
replace-with = "rsproxy-sparse"

[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"

[net]
git-fetch-with-cli = true
```

验证：`rustc --version` / `cargo --version`（本机实测 rustc/cargo 1.97.1，与 Windows 工具链同版本）。

> rsproxy = 字节跳动 Rust 工具链 + crates 索引加速镜像。`rsproxy.cn` 根路径是文档页，安装脚本走官方 `sh.rustup.rs`，仅把 toolchain 下载源指向镜像。

### 6.2 install_wsl：在 WSL 里从源码构建 serve（workspace_supervisor.rs:705）

不是放二进制，而是 WSL 内 `cargo build`（平台不同，Windows exe 不能在 WSL 跑）：

| 步骤 | 命令 | 作用 |
|---|---|---|
| 前置 | 仅 Windows | 非 Windows 报错 |
| 1 | `repo_root` → `to_wsl_path` → `/mnt/f/DeepX` | Windows 仓库路径转 WSL |
| 2 | `command -v deepx-workspace` | 幂等：已装即返回成功 |
| 3 | `command -v cargo` | 缺 Rust 工具链 → 报错 + rustup 提示 |
| 4 | `test -f {wsl_repo}/Cargo.toml` | 确认 /mnt 可访问源码 |
| 5 | `tar` 从 /mnt 拷源码到 `~/.deepx-workspace-src` | 拷到 WSL 原生 ext4（排除 .git/target/node_modules 等；**保留旧 target 供增量**） |
| 6 | `cargo build --release -p deepx-workspace` | WSL 原生 ext4 编译（避开跨文件系统慢） |
| 7 | `install ~/.local/bin/` + PATH 写 `~/.bashrc` | 装到 WSL，`command -v` 可达 |
| 8 | `~/.local/bin/deepx-workspace list` | 全路径验证 |

关键设计：**构建已用 WSL 原生 ext4**（tar 拷贝，非直接 /mnt 编译），只有 serve 运行时工作区在 /mnt（A 方案）。这是后续 ext4 工作区（Codex 形态）的现成样板。

### 6.3 前端切 WSL → 拉起 serve 链路（workspace_supervisor.rs）

```
前端 set mode=wsl → 重启 daemon
  → WorkspaceSupervisor::start(wsl)          (supervisor.rs:85)
  → probe:  wsl.exe -e bash -lc "command -v deepx-workspace"
  → spawn:  wsl.exe -e bash -lc "DEEPX_WORKSPACE_TOKEN='$token' deepx-workspace serve --port 0"  (L246)
  → 读 stdout "DEEPX_WORKSPACE_READY <host>:<port>" 拿端口
  → GET /health 确认 ready
  → endpoint+token 注入 worker (MODE=wsl → Http backend)
  → Windows 经 127.0.0.1:port 访问（WSL2 localhost 转发）
```

- 桥梁 = `wsl.exe`；连接 = WSL2 localhost 转发；前置 = WSL 已装 serve（install_wsl），未装/不可用 → 自动回退 local serve（降级记录）。
- 现状：前端切换**需重启 daemon** 生效（`backend.restart` 未实现，UI 有提示）；补 `backend.restart` 可改即时切换。

## 7. 决策记录

| 决策 | 结论 |
|---|---|
| 实施优先级 | **暂缓**（2026-08-15 拍板：已知缺口，WSL2 为未来可选场景，不阻塞当前 Local 主线） |
| 路径语义 | **A 已实施**（serve 侧 /mnt 桥接，2026-08-15）；**收敛方向** = Codex 式"远端解释 PathUri 零转换"（worker 按目标环境构造 URI，serve 移除 /mnt 转换，见 §参照小节）；B 保留为演进选项（§5 性能权衡） |
| 落地时机 | 与 PLAN-WORKSPACE-INPROCESS 阶段 3.2（WSL2 模式完善）同批 |
| WSL 端就绪 | rsproxy 装 Rust + install_wsl 从源码构建（实测 rustc/cargo 1.97.1，见 §6） |

## 8. 变更记录

| 日期 | 改动 | 触发 |
|---|---|---|
| 2026-08-15 | 初稿：记录 WSL2 路径桥接缺口 + 修复方向 | 盘点 L1 工具执行归属时发现路径转换未接入工具链 |
| 2026-08-15 | **落地核心路径桥接**：serve 侧 Windows→/mnt 转换（新 `wsl_path.rs`，字符串解析）+ `set_process_workspace` / `resolve_workspace_path` / `permission::resolve_target_path` 三处统一；单测 7 用例 + WSL 端到端三断言（exec pwd / 相对 read / 绝对 read）通过；补 §5 性能权衡 | 用户拍板 A 方案开工 |
| 2026-08-15 | 补参照：OpenAI Codex 路径零转换（PathUri 透传、远端解释），路径语义收敛方向定为 Codex 式 | 实地调研 F:\codex |
| 2026-08-15 | 补 §6 WSL 端就绪（rsproxy 装 Rust + install_wsl 机制 + 前端拉起链路） | 实操记录 |