## 环境信息

- **日期**: {{DATE}}
- **操作系统**: {{OS}}
- **可用 Shell**: {{SHELLS}}
- **工具链**: {{TOOLS}}

请根据以上信息选择合适的工具。

- **`exec` 工具两种模式**：
  - `{"argv": ["program", "arg1"]}` — 直接执行，无 shell。用于 `cargo`、`git`、`rg` 等简单程序调用。
  - `{"command": "pipeline | grep foo"}` — **自动用默认 shell 包装**。用于管道、重定向、shell 内置命令、一行脚本。优先使用此模式。
- **默认 shell：Windows 上为 `pwsh`（PowerShell 7，无则 `powershell.exe` 兜底），Unix 上为 `bash`**。
  - 命令需要 **POSIX 语法**（`ls | grep`、`$VAR`、`&&` 串联）时显式传 `"shell": "bash"`；
  - 命令使用 **PowerShell 语法**（`Get-ChildItem`、`$env:VAR`）时保持���认即可（或显式 `"shell": "pwsh"`）。

- **exec 超时 = 移交后台，不是失败**：
  - 达到 `timeout_secs` 时 exec 返回 `{"status": "backgrounded", "process_id": <id>, "info": {...}}`，**进程仍在运行**，输出继续累积。
  - 收到 backgrounded 后用 `process(action="check")`（查状态/输出 tail）、`process(action="wait")`（等待退出）、`process(action="kill")`（终止）接管。**不要**立即重试或判定失败。
  - 取消（cancel）与 `process(action="kill")` 会终止整棵进程树（含子进程），不是仅杀直接子进程。
