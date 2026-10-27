---
name: qaqh-manual
description: Load the QAQ-Harness operating manual for tool contracts, permissions, skills, apply_patch, subagents, and recovery. Use qaqh-apply-patch when only patch syntax is needed.
metadata:
  short-description: Full QAQH operating manual
---

# QAQH Manual

QAQ-Harness is a coding agent that acts through typed tools. This manual covers
the parts that are easy to get wrong. It explains the current contract; it does
not grant permissions and does not replace the tool list or schema.

## Read only what you need

- Patch shape, grammar, or failure recovery: read
  [references/apply-patch.md](references/apply-patch.md). If only patch syntax
  is needed, activate `qaqh-apply-patch`.
- Which tool to use for a file, search, exec, or recovery task: read
  [references/tools.md](references/tools.md). For a compact decision table,
  activate `qaqh-tools`.
- Permissions, approvals, context/skills, subagents, MCP, or recovery: read
  [references/runtime.md](references/runtime.md).

## Non-negotiables

- Inspect the current tool declaration before calling it. Do not invent a tool,
  parameter, provider branch, or preview flow.
- Typed file tools are preferred over `exec` for file operations. `exec` is for
  commands and processes, not a way around path or stale-file checks.
- Permission policy, sandbox policy, and allowlists are enforced outside the
  model. A skill's `allowed-tools` is advisory and never grants access.
- Verify workspace claims with a tool result. If a claim cannot be verified,
  say so instead of inferring success.
- Treat skill files as instructions, not as conversation facts. The current
  tool schema wins if a skill and schema disagree.

## Provider-neutral apply_patch

OAuth/Responses and API-key/Chat sessions may expose different declarations.
Choose by declaration, never by credential type:

- custom/freeform: send raw patch text; do not JSON-wrap it.
- function/JSON: send the object shape declared by the schema; include
  `dry_run` only if that parameter exists.
- `apply_patch_preview` / `confirm_apply`: use only when those tools are in the
  current tool list. Never claim a preview happened when the tool is absent.

`apply_patch` is Codex-format, not unified-diff format. Start with
`*** Begin Patch`, use file sections, and end with `*** End Patch`. The engine
matches the first context hit; disambiguate repeated context with surrounding
lines or `@@ <context>`. Partial multi-file application is possible: on failure,
inspect the report and resend only what was not applied.

## Skills and context

The catalog lists names and descriptions; it does not contain skill bodies.

- `skill_activate {name}` loads one body into the trailing skill envelope.
- `skill_release {name}` removes an active skill; releasing the last one emits
  an authoritative empty envelope.
- `skill_resource {name, path}` reads a contained bundled file.
- Active bodies are budgeted, never silently truncated. Release unused skills
  or read a reference on demand when context is tight.
- `qaqh-core` is bootstrap guidance, not the full manual. `qaqh-manual` is lazy
  and should be activated only when this level of detail is useful.

## Reporting

Report what changed, what was verified, and what remains uncertain. Do not
summarize a diff you have not read. If a tool denies a call, narrow the scope or
choose the typed alternative; do not repeat the same call unchanged.
