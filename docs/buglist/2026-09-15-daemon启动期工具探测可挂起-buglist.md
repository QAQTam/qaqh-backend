# buglist（2026-09-15）— daemon 启动期工具探测可无限挂起

> 登记规则：一行一个缺陷；**详情进 `docs/report/`**，本文件只做索引与状态跟踪。
> 状态口径：`open` / `fixed（工作区，待提交）` / `fixed @{commit}` / `verified` / `wontfix`。
>
> 本条目**未单开 report**（缺陷小、证据自足并逐条内联于下）。
> 发现来源：TUI 侧 T-01 阶段二迁移时，端到端验证跑不起来，逐层归因到此处。
> 姊妹条目：[`2026-09-15-频道流epoch未归零-buglist.md`](./2026-09-15-频道流epoch未归零-buglist.md)

## 缺陷

| ID | 状态 | 项 |
|---|---|---|
| BUG-2026-09-15-02 | `open` | 启动期工具探测用 `Command::…output()` 且**无超时**：被探测程序的后代进程若持续持有管道写端，EOF 永不到达，`.output()` 永久阻塞 → **daemon 永远起不来**（不报错、不退出、无日志） |

## 事实与证据

调用链（全部 `crates/qaqh-runtime/src/registry.rs`）：

| 位置 | 事实 |
|---|---|
| `crates/qaqh-daemon/src/main.rs:59`、`:84` | daemon 启动期调用 `qaqh_runtime::detect_os_info()` |
| `registry.rs:58-105` | `detect_os_info()` 同步执行；先 `uname -a`（非 Windows），再逐个探测工具 |
| `registry.rs:79-86` | 探测表：`git` / **`cargo`** / `node` / `python`,`python3` / `rustc` / `pnpm`，参数均为 `--version` |
| `registry.rs:90` | `if let Ok(output) = background_command(program).args(args).output()` ← **关键行** |
| `registry.rs:107-118` | `background_command()` 就是裸的 `Command::new(program)`（仅 Windows 加 `CREATE_NO_WINDOW`）：**无 env、无 cwd、无 stdin 重定向、更无超时** |

**为什么无超时就会永久阻塞**：`.output()` 的契约是「等子进程退出 **且** 读空 stdout/stderr 管道到 EOF」。只要有任何后代进程继承并持有该管道的写端，EOF 就永不出现——**即便 `cargo --version` 本身早已退出**。由于它在 daemon 开始服务之前同步执行，整个启动被卡死。

**同类问题本仓已识别过**：`crates/qaqh-workspace/src/process_registry.rs:354` 的注释原文——

> 管道 EOF——孙进程可能持有管道写端导致 EOF 永不到达（如 cargo test …）

即该失败模式在 exec 路径上已有认知与处理，**但启动探测这里未加同款保护**。

## 复现

```bash
cd ~/Projects/qaqh-backend && cargo build -p qaqh-daemon

# 隔离 data root 起一次，观察是否产出 daemon.json
T=$(mktemp -d); mkdir -p "$T/.qaqh"
QAQH_DATA_DIR="$T/.qaqh" ./target/debug/qaqh-daemon run </dev/null >/dev/null 2>&1 &
sleep 30
ls "$T/.qaqh/daemon.json"          # 复现时：不存在
tail -2 "$T/.qaqh/qaqh-daemon.log" # 停在 exec shell bootstrap: bash

# 卡在哪：进程处于 do_wait（等子进程）
PID=$(ps -eo pid,comm | awk '$2=="qaqh-daemon"{print $1}')
cat /proc/$PID/wchan                # 复现时：do_wait
for c in $(pgrep -P $PID); do tr '\0' ' ' < /proc/$c/cmdline; echo; done
                                    # 复现时：cargo --version
```

**实测记录（2026-09-15）**：

- 现象：`cargo --version` 子进程停在 `futex_do_wait`，daemon 停在 `do_wait`；日志停在
  `qaqh_runtime::registry: [runtime] exec shell bootstrap: bash`，无 stderr，进程不退（`timeout` 杀掉时 exit 124）。
- 已排除的变量（**在同一台机器上逐项试过，均复现**）：空数据根 / 真实数据根的副本 /
  有无 `config.toml` / `HOME` 取真实值或临时值 / `stdin` 是否指向 `/dev/null`。
- **`cargo --version` 单独在 shell 里跑是秒回（exit 0）**——所以挂的不是 cargo 本身，而是
  「`.output()` 等管道 EOF」这一调用方式与当时 shim 行为的组合。
- 时好时坏：同机 11:26 起过一次正常运行的 daemon（日志显示它越过了该探测，继续到
  journal/timeline/tool-manager/subagent 初始化）；16:40 起同版本二进制即复现挂起。
- **与在改的 workspace 重构无关**：用 HEAD 干净构建（不含任何未提交改动）同样复现；
  且 `qaqh-daemon` 不依赖 `qaqh-client`，故该重构与本条目无因果。

## 建议

给探测加超时并回收子进程（对照 `qaqh-client/src/session.rs` 中 `OPEN_TIMEOUT_SECS` 一类做法），
或改为不依赖管道 EOF 的探测方式（例如显式 `kill` 子进程、或只取退出码不读管道）。
探测结果只是给提示词用的工具快照（`crate::agent::prompt::TOOLS_INFO`），**探测失败应当降级而非阻塞启动**。
