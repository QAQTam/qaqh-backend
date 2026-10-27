---
name: qaqh-mcp
description: Use MCP tools, resources, and prompts exposed through the aggregate mcp tool.
metadata:
  short-description: MCP tool and resource usage
---

# MCP

Connected MCP servers expose model-facing tools as `mcp__<server>__<tool>`.
Use those names directly when the tool is present.

The aggregate `mcp` tool covers server resources and prompts; it does not proxy
ordinary MCP tool calls.

- Inspect the server/tool schema before calling.
- A missing server or tool is not a reason to invent a name; report it and use
  a local fallback.
- MCP resources may be large; read narrowly and summarize only what the task
  needs.
- Do not expose secrets, tokens, or session-internal paths in tool arguments.
