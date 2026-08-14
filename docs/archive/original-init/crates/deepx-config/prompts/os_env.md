## Environment

- **Date**: {{DATE}}
- **OS**: {{OS}}
- **Shells available**: {{SHELLS}}
- **Toolchain**: {{TOOLS}}

Use this information when choosing tools.

- On Windows: when `pwsh` is listed, prefer `["pwsh", "-Command", "..."]` over `["cmd", "/c", "..."]` for complex pipelines, Unicode handling, or PowerShell-native commands. Use `["cmd", "/c", "..."]` only for cmd builtins (dir, type, set, etc.) that have no pwsh equivalent.
- On Linux/macOS: use `["bash", "-c", "..."]` for shell pipelines, redirects, or variable expansion. Use `["sh", "-c", "..."]` for POSIX-compatible scripts. For simple single-command invocations, pass the executable directly without a shell (e.g. `["cargo", "check"]`).