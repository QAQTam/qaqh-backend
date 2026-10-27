---
name: qaqh-subagent
description: Delegate research or isolated work to a subagent and coordinate through the task board.
metadata:
  short-description: Subagent and task-board coordination
---

# Subagents

Use a subagent for independent research or isolated work that can proceed
without the parent's conversation. Put everything it needs into
`task_description` and `context`; it cannot see the parent transcript.

- `spawn_subagent` returns immediately. Do not poll or loop on `wait_agent`.
  The final answer arrives later as a `[SUBAGENT]` message.
- `steer_agent` / `interject_agent` merge into a running turn; `send_message`
  queues; `followup_task` queues and starts a turn.
- Use `task_*` and `board_*` for multi-agent coordination.
- Delegate reading and research, not builds, installs, or destructive changes.
- Give the child a narrow, verifiable deliverable and its own success criteria.
