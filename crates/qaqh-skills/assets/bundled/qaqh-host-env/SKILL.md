---
name: qaqh-host-env
description: Diagnose and fix missing environment variables or PATH entries for exec_command children under QAQHarness (tools like bun, node/npm/pnpm via fnm, or anything a version manager installs), and apply the supported config [exec.env] hot-reload injection layer. Use when a command fails with command-not-found or "no such file" inside the agent even though the user's interactive shell has the tool.
metadata:
  short-description: QAQHarness exec env/PATH missing — diagnosis and [exec.env] injection
---

# QAQHarness 环境变量缺失的排查与注入

症状：用户终端里好好的工具，在 agent 的 exec 里 `command not found`。
这几乎从不是工具没装，而是**环境分层的必然结果**——按本技能三步定位，再用
受支持的注入口修复。

## 1. 三层环境模型（先定位缺在哪层）

1. **daemon 进程**：QAQHarness 的 daemon 通常由 systemd user 服务拉起
   （`/proc/<pid>/environ` 可自证），**不经过 `/etc/profile`、`.bashrc`、
   `.bash_profile`**。用户在 rc 文件里 export 的一切对 daemon 不存在。
2. **exec 子进程**：每次 spawn 做 `env_clear` + 最小白名单
   （`PATH/HOME/LANG/LC_ALL/TERM/USER/LOGNAME/TMPDIR/PATHEXT` 等系统级键），
   再强制注入防挂死变量（`PAGER/GIT_PAGER/GH_PAGER=cat`、`GIT_TERMINAL_PROMPT=0`）。
   白名单外的 daemon 变量到不了子进程——这是防凭据泄漏的设计，勿绕过。
3. **`[exec.env]` 覆盖层**：`~/.config/qaqh/config.toml` 的用户授权注入面，
   正式修复走这里。

自查命令：`env | cut -d= -f1 | sort`（我在拿什么）；
`pgrep -f qaqh-daemon` 后读其 environ（daemon 有什么）。缺的键若不在白名单，
改 daemon 环境也没用，必须走第 2 节的 overlay。

## 2. 正式解：`[exec.env]` 覆盖层契约

```toml
[exec]
[exec.env]
MY_TOOL_HOME = "/home/<user>/.mytool"
PATH = "/home/<user>/.mytool/bin:/usr/local/bin:/usr/bin:/bin:/usr/local/sbin"
```

- **热重载约 1.5s**，仅新 spawn 的子进程可见；长驻 exec_session/MCP 不回填。
  **绝不为环境变量重启 daemon**（会杀光所有会话）。
- **全量替换语义**：overlay 里删键，下次 spawn 即消失。
- 值插值**只支持 `${secret:name}`**（引用 secrets.toml，启动校验）；
  **无 `$HOME`/`$PATH` 展开**，同名键（尤其 PATH）是整体替换，必须写完整值。
- 组装次序：白名单 < overlay < 工具显式 env 参数 < harness 强制注入。
  沙箱例外：scratch 重指的 `HOME/TMP/TEMP` 压过 overlay（沙箱权威）。
- 改的是宿主级 `~/.config/qaqh/config.toml`，不是仓库文件。

注入后验证模板：`sleep 2.5` → 新 exec 里 `which <tool> && <tool> --version`。

## 3. 版本管理器与 shim 类工具

fnm/nvm/pyenv 的多 shell 动态目录**绝不可写进 PATH**（每 shell 一次性，实测
有弃尸目录）；corepack 锁死单版本会破坏逐项目自适应。通用原则：注入
**稳定别名目录**、把版本选择留给 shim 本身；确认 shim 无交互提示（否则会
挂住 exec 会话）。bun/fnm/corepack 的已实测具体姿势见
[references/host-examples.md](references/host-examples.md)——需要配这三类工具时读它。

## 4. 兜底

应急一次性：`bash -ic '命令'`（借用户 rc 的环境）或直接绝对路径。
`systemctl --user set-environment` / `~/.config/environment.d/` 可修 daemon
环境本身，但要重启 daemon 才传导，代价大；`~/.local/bin` 不保证在任何
默认 PATH 里，勿假设 mkdir 即生效。
