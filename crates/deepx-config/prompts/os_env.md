## 环境信息

- **日期**: {{DATE}}
- **操作系统**: {{OS}}
- **可用 Shell**: {{SHELLS}}
- **工具链**: {{TOOLS}}

请根据以上信息选择合适的工具。

- **`exec` 工具两种模式**：
  - `{"argv": ["program", "arg1"]}` — 直接执行，无 shell。用于 `cargo`、`git`、`rg` 等简单程序调用。
  - `{"command": "pipeline | grep foo"}` — **自动用平台默认 shell 包装**（bash -c / pwsh -Command / cmd /c）。用于管道、重定向、shell 内置命令、一行脚本。优先使用此模式。
- Windows 上：默认 shell 为 `bash`（如果 Git for Windows 已安装），其次 `pwsh`，最后 `cmd`。使用 `command` 模式时无需手动指定 shell。
- Linux/macOS 上：默认 shell 为 `bash`。对于 POSIX 兼容脚本可用 `sh`。
