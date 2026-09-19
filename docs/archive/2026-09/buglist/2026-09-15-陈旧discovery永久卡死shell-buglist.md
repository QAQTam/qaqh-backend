# buglist（2026-09-15）— 陈旧 `daemon.json` 永久卡死 shell（非 Windows 判活恒真）

> 登记规则：一行一个缺陷；**详情进 `docs/report/`**，本文件只做索引与状态跟踪。
> 状态口径：`open` / `fixed（工作区，待提交）` / `fixed @{commit}` / `verified` / `wontfix`。
>
> 本条目**未单开 report**（证据自足并逐条内联于下）。
> 发现来源：用户真机报告——`qaqh-tui` 报 `连接 daemon 失败 / Connection refused`，
> 且「无法拉起同文件夹下的 daemon」。
> 姊妹条目：[`2026-09-15-daemon启动期工具探测可挂起-buglist.md`](./2026-09-15-daemon启动期工具探测可挂起-buglist.md)

## 缺陷

| ID | 状态 | 项 |
|---|---|---|
| BUG-2026-09-15-03 | `fixed @9556aec` | 非 Windows 的 `process_is_running` 是**恒返回 `true` 的 stub**：daemon 非正常死亡后遗留的 `daemon.json` 让「pid 判活过滤」全部放行 → 客户端拿死端点去连 → `Connection refused`，**永不回退到拉起 daemon**。Linux/macOS 上每个 shell 都得手工删文件才恢复 |
| BUG-2026-09-15-04 | `fixed @572f36a` | `spawn_daemon_detached`（Unix）未 `setsid`/未建新进程组，daemon 与 shell 同进程组 → shell 异常退出时 daemon 被一并收走。**这是 03 的实际触发路径**：它不断制造陈旧记录，03 则把记录变成永久砖 |

## 事实与证据（03）

报错现场（用户原文）：

```
Error: 连接 daemon 失败
Caused by:
    0: reqwest error: error sending request for url (http://127.0.0.1:33041/ringing/v1/clients/open)
    ...
    3: tcp connect error
    4: Connection refused (os error 111)
```

| 位置 | 事实 |
|---|---|
| `qaqh-client/src/discovery.rs`（修复前） | `#[cfg(not(windows))] pub fn process_is_running(_pid: u32) -> bool { true }` |
| `discovery.rs:ensure_daemon_running` | `if let Ok(d) = read_discovery() && process_is_running(d.pid) { return Ok(d) }` ← 恒真即**直接返回陈旧记录**，不 spawn |
| `client.rs::wait_for_daemon` | `read_discovery().ok().filter(\|d\| process_is_running(d.pid))` ← 同上 |
| `client.rs::connect_async` | 同一过滤器 → 使用死端点 → `session.open()` 在 `/ringing/v1/clients/open` 上 `Connection refused` |

现场核验（事故当时的 `~/.config/qaqh/daemon.json`，Linux 数据根是
`$XDG_CONFIG_HOME/qaqh` 即 `~/.config/qaqh`，**不是** `~/.qaqh`）：

```
endpoint: http://127.0.0.1:33041   ← 与报错端口一致
pid: 19683                          ← ps 查无此进程
可执行: /home/qaqtamsy/Projects/12/qaqh-daemon
```

`ss -ltn` 确认 33041 无人监听；`daemon.lock` 里也是同一个死 pid。

**为什么这个环很容易撞上**：见 04——daemon 与 shell 同进程组，关终端/异常退出即
带走 daemon，留下陈旧记录；旧代码又把该记录当权威。两者相接就是「关一次终端，
之后每个 shell 都连不上，且都不知道为什么」。

## 处置（03，`9556aec`）

1. 非 Windows 的 `process_is_running` 委托给 `qaqh_types::platform::process_is_running`
   （Unix 走 `kill -0`）。Windows 保留原 Win32 直调版本（避免 `tasklist` 子进程延迟）。
2. 新增 `discovery_is_live = pid 存活 **且** 端点连得上`，替换三处裸 pid 检查。
   第二道实证挡住 pid 判活的两处漏网：**pid 复用**、**daemon 活着但没在听**。
   探测是本地回环一次 TCP（≤300ms），只在「已有 discovery 记录」的连接路径上发生；
   **判不了就返回 `true`**——宁可放行，也不误判健康 daemon 为陈旧而重复拉起。
3. `lock_holder_alive` 两平台同构（此前非 Windows 恒 `false`，daemon 冷启动窗口内
   会被重复 spawn）。顺带给 `qaqh_types` 的 `kill -0` 加 `stderr(null)`：判死是
   正常路径，不该把「没有那个进程」漏进调用方 stderr。

**真机前后对比**（隔离 data root，人为造 pid 已死 + 端口无人听的记录，TUI 与
daemon 同目录）：

| | 结果 |
|---|---|
| 修复前（用户现用二进制） | 退出码 1，`连接 daemon 失败`，**记录 pid 不变**（根本没去拉起） |
| 修复后 | 退出码 124（跑满），相位 `◌ connecting → ● ready`，记录 pid 换成新 daemon |

回归锁 5 条（`qaqh-client` `discovery::tests`）。破坏验证：把非 Windows 判活退回
`true` → 恰好 `process_is_running_is_false_for_a_reaped_child` 与
`discovery_is_dead_when_pid_is_gone_even_if_endpoint_listens` 两条红，而
`discovery_is_dead_when_endpoint_refuses` 仍绿（端点探测独立兜住了那一层）。

验证边界：**未在 Windows 上编译核验**（本机无该 target）；Windows 分支改动仅为
保留原实现，未触碰。

## 处置（04，`572f36a`）

Unix 分支加 `process_group(0)`（std 自带，无需 libc）；两处 spawn 合并为唯一出口
`discovery::spawn_daemon_process`（此前 `discovery::spawn_daemon_detached` 与
`client::spawn_detached` 各一份，本次实测就吃了亏：先只改了 discovery 那份，
daemon 仍随 shell 死，才发现 client 那份也在生效路径上）；「脱离配置」抽成
`configure_detached(&mut Command)` 以便测试。

**真机前后对比**（同一探针）：

| | daemon PGID | shell 收尾后 |
|---|---|---|
| 修复前 | 99007（= TUI 的组） | **死**，`daemon.json` 残留 |
| 修复后 | 112135 = 自身 pid | **存活** |

回归锁 `detached_spawn_lands_in_its_own_process_group`（Linux 读 `/proc/<pid>/stat`
的 pgrp）。破坏验证：注掉 `process_group(0)` → 红。
边界：未 `setsid`（仍在原会话，但已不在前台进程组，实测足以存活）；Windows/macOS 未验。

## 04 的证据与建议（原始记录，保留）

`discovery.rs::spawn_daemon_detached`（Unix 分支）只设了 null stdio，**没有
`setsid`、也没有独立进程组**；Windows 分支设了 `CREATE_NEW_PROCESS_GROUP`，
但 Unix 侧没有对应物——函数名叫 `detached` 而实际并未脱离。

实测：`qaqh-daemon.log`

```
[1789466465] INFO  qaqh_subagent::host: [SUBAGENT] in-process subagent host installed
[1789466500] INFO  qaqh_daemon::server: [daemon] termination signal received — entering graceful shutdown
```

6465 → 6500 = **35s**，正是 harness `timeout 35` 掐掉 TUI 的时刻。daemon 随 TUI
一起收信号。

建议：Unix 分支加 `std::os::unix::process::CommandExt::process_group(0)`
（std 自带，无需引入 libc），使 daemon 脱离 shell 的前台进程组，免于终端关闭时
的 SIGHUP/SIGTERM。设计意图上 daemon 本就该跨 shell 复用（`daemon.lock` 单实例锁、
`stop_if_idle` 控制端点都指向这一点），现在的行为与设计相反。

**注**：03 修好后，04 不再造成「永久砖」，只会造成「每个 shell 重新拉一个
daemon」（浪费但可自愈）。故 04 优先级低于 03。
