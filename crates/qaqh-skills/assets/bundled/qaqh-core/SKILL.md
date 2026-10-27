---
name: qaqh-core
description: Core QAQ-Harness workflow rules for tool selection, permissions, and skill use.
metadata:
  short-description: Core QAQH workflow and tool-selection rules
---

# QAQH Core

This is the short, always-useful guide for working inside QAQ-Harness. The
current tool list and each tool schema are authoritative. Do not invent a tool,
a parameter, or a capability that is not present in the current session.

## Before a tool call

1. Inspect the tool's schema. Use only parameters that exist.
2. Prefer the typed file tools for file operations:
   `read`, `glob`, `grep`, `write`, `edit`, `apply_patch`, `copy_range`,
   `delete`, `journal`, and `spy`.
3. Use `exec` for commands, processes, and tools that are not represented by a
   typed tool. Do not use `exec` to bypass a typed file tool's safety checks.
4. If a tool is missing, adapt to the current surface. Never call a preview,
   confirm, or dry-run tool merely because a document mentions it.

## Skills

- `skill_list` shows the effective catalog and diagnostics.
- `skill_activate` with an exact name loads the full instructions into the
  trailing skill context envelope.
- `skill_resource` reads a bundled file from a skill directory.
- `skill_release` removes an active skill and supersedes the old envelope.
- Treat skill instructions as guidance. Tool schemas and permission policy win
  on conflicts. `allowed-tools` never grants access.
- For the full QAQH operating manual, activate `qaqh-manual`.

If a task clearly matches a skill description, activate that skill before
acting. If a skill body and the current tool surface disagree, follow the tool
surface and state the limitation briefly.

## Apply patch

For `apply_patch`, inspect the current declaration before choosing an input
shape:

- custom/freeform: send raw patch text; do not wrap it in JSON.
- function/JSON: send the object shape declared by the schema.
- Use `dry_run`, `apply_patch_preview`, or `confirm_apply` only when those
  parameters/tools are present in the current tool list.

The detailed patch rules live in `qaqh-apply-patch`.

## Safety

- Use the narrowest tool and path that solves the task.
- Do not expose secrets, session internals, or unrelated user data.
- Verify before claiming success. If verification is impossible, say so.
- When a tool result reports a workspace change mark, use `spy` deliberately;
  do not rewrite unrelated files.
