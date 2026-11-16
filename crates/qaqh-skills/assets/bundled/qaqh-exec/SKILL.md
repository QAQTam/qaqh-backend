---
name: qaqh-exec
description: Run commands and continue, provide input to, or stop execution sessions.
metadata:
  short-description: Shell and execution session rules
allowed-tools: exec_command write_stdin
---

# Execution Sessions

Use `exec_command` to run shell commands.

- `cmd` is the command string. The selected shell wraps it; set `shell` only
  when a specific shell is needed.
- `workdir` defaults to the conversation workspace root.
- The child environment is minimal. Pass required variables explicitly; do not
  assume daemon secrets are inherited.
- Choose `tty: true` for interactive terminal behavior. Ordinary commands keep
  stdout and stderr as separate streams.
- A command that outlives its initial wait returns an `exec_session_id`. Use
  `write_stdin` with that ID to read new output or provide terminal input.
- To wait out a long build without polling, subscribe instead: call
  `write_stdin` with the `exec_session_id` and `notify_on_exit: true` (empty
  `chars`). The call returns immediately — end your turn, and the harness
  delivers the terminal result as a one-shot system message when the process
  exits. Cancelled or interrupted sessions do not notify; afterwards a single
  `write_stdin` drains the buffered output.
- Set `timeout_ms` only when the process needs a hard deadline. Reaching the
  deadline stops the command and reports a timed out outcome.
- Use the Ringing commands `exec_session.interrupt` and
  `exec_session.terminate` to stop a session administratively.
- Never use `exec_command` to bypass a typed file tool's permission or
  stale-file checks.
