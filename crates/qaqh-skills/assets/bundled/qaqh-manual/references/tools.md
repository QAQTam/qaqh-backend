# Tool Reference

Use the narrowest tool that can complete the task. The current schema wins over
this table.

| Goal | Preferred tool | Notes |
|---|---|---|
| Read a file or range | `read` | Lines are `L{n}: content`; strip the prefix for edits. |
| Find files | `glob` | Gitignore-aware; narrow the pattern if truncated. |
| Search contents | `grep` | Uses ripgrep; case-insensitive by default. |
| Resolve a symbol | `lsp` | Prefer over grep when a server is attached. |
| Create/replace | `write` | Returns a receipt; stale files must be re-read. |
| Exact replacement | `edit` | `old_string` must occur once unless `replace_all`. |
| Codex patch | `apply_patch` | First context hit; partial application possible. |
| Move lines | `copy_range` | Uses exact whole-line anchors. |
| Delete | `delete` | Moves to `.qaqh/trash/`, not unlink. |
| Run a command | `exec` | Do not use for typed file operations. |
| Continue a process | `process` | `check` / `wait` / `write` / `kill`; no list. |
| Delegate | `spawn_subagent` | Include all context; no polling. |
| Inspect changes | `journal` / `spy` | Inspect before undo/restore. |

## File tools

- `read` caps output and returns a continuation. Pass `if_hash` to get a
  not-modified result instead of a second full read.
- `write` and `delete` fail with `stale_file` when the file changed since your
  last read. Re-read, then retry.
- `edit` uses exact-match as its guard. Read the reported diff on failure
  instead of guessing.
- `delete` is recoverable through `.qaqh/trash/`.

## Exec and process

- Use `shell: "bash"` only when POSIX syntax is required.
- The environment is minimal; pass necessary variables explicitly.
- `cwd` defaults to the workspace root.
- A timeout backgrounds the process with an id; use `process` to check, wait,
  write, or kill it.
- Use `background_after_secs` for long jobs; never append `&`.
- Do not use `exec` for `cat`, `sed`, `awk`, `find`, `ls`, `rm`, or `echo >`
  when a typed file tool exists.

## Recovery

`journal` covers typed file-tool revisions; `spy` covers workspace snapshots
and exec-created changes. Inspect before restoring. `restore` with `prune` can
delete newer files and must not be used as a blind fix.
