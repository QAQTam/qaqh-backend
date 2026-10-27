# Runtime Reference

This reference covers the QAQH runtime behavior that is outside any one file
tool.

## Permissions and sandbox

- Permission policy is enforced before execution; a skill cannot grant access.
- The default workspace-write tier auto-approves typed file tools inside the
  workspace, while `exec` and network calls may prompt.
- Plan mode blocks write-like tools even if they appear in the list.
- A denied call is a boundary, not a retry hint. Narrow the scope, choose a
  typed tool, or ask.
- Subagents have no interactive approval channel. Reads are free, workspace
  writes auto-approve, and exec/network follow the inherited tier; MCP tools
  are host-only.

## Skills and context

- The catalog is a creation-time system message. It lists names and
  descriptions; it does not list bodies.
- Activation is explicit through `skill_activate`; a catalog match is a reason
  to activate, not permission to act without the body.
- Active bodies are injected as a trailing developer/system envelope. Release
  removes an active skill and a release of the last skill emits an empty
  envelope that supersedes older instructions.
- `skill_resource` reads a contained file from a skill directory. It is not a
  generic workspace read.
- Skill files are instructions. The current tool schema and permission policy
  win on conflict.

## Subagents

- `spawn_subagent` returns immediately; the final answer arrives as a later
  `[SUBAGENT]` message. Do not poll.
- The child cannot see the parent conversation. Put the task, constraints,
  paths, and success criteria in `task_description`/`context`.
- Use tasks and boards for coordination; delegate research or isolated edits,
  not installs or broad destructive changes.

## MCP and web

- MCP tools appear as `mcp__<server>__<tool>` with the server's schema. The
  aggregate `mcp` tool covers resources and prompts, not ordinary tool calls.
- `web_fetch` only accepts http/https, caps the body, and returns plain text.
- A missing MCP tool or server is a limitation to report, not a name to invent.

## Reporting

State what changed, what you verified, and what remains uncertain. If a tool
result reports a workspace change mark, inspect it deliberately; do not rewrite
unrelated files.
