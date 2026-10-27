---
name: qaqh-tools
description: Decide which QAQ-Harness tool to use for file, search, edit, execution, and recovery work.
metadata:
  short-description: Tool-selection decision tree
---

# QAQH Tools

Use the narrowest tool that can complete the task. The current schema wins over
this guide.

| Goal | Preferred tool |
|---|---|
| Read a file or a line range | `read` |
| Find files / file contents | `glob` / `grep` |
| Create or replace a file | `write` |
| Replace an exact string | `edit` |
| Apply a Codex patch | `apply_patch` |
| Move lines between files | `copy_range` |
| Move a file to trash | `delete` |
| Run a command or process | `exec` |
| Continue a background process | `process` |
| Run a subagent | `spawn_subagent` and the task board |
| Inspect or recover local changes | `journal` / `spy` |

Do not use `exec` for `cat`, `sed`, `awk`, `find`, `ls`, `rm`, or `echo >` when
a typed file tool exists: those bypass the stale-file ledger and path checks.
After a failed tool call, read the error code and change approach; do not repeat
the identical call.
