---
name: qaqh-exec
description: Run commands, choose shells, and manage background processes with exec and process.
metadata:
  short-description: Shell, background, and process rules
allowed-tools: exec process
---

# Exec and Process

Use `exec` for commands that are not covered by a typed tool.

- The session shell wraps the command; pass `shell: "bash"` only when POSIX
  syntax is required.
- Under bash/zsh/sh, extra `args` arrive as `$1`, `$2`, `$@`; under pwsh they
  arrive in `$args`.
- The child environment is minimal. Pass required variables explicitly; do not
  assume daemon secrets are inherited.
- `cwd` defaults to the workspace root.
- Output is merged stdout/stderr, ANSI-stripped, and folded to the configured
  limit. Filter inside the command (`rg -n`, `--json`, `head`, `-q`) instead of
  rerunning a large command.
- A timeout does not kill the process; it becomes `backgrounded` with a
  `process_id`. Use `process {action: check|wait|write|kill, id}`. There is no
  `list` action.
- For servers and long jobs, use `background_after_secs`; never append `&`.
- Never use `exec` to bypass a typed file tool's permission or stale-file
  checks.
