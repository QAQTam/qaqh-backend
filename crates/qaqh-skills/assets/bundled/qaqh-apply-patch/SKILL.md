---
name: qaqh-apply-patch
description: Apply Codex-style patches with apply_patch, including custom/freeform and JSON/function surfaces.
metadata:
  short-description: Apply Codex-style patches safely
allowed-tools: read apply_patch
---

# Apply Patch

Use this skill when a task requires editing files and the current tool surface
contains `apply_patch`. Choose the input shape from the declaration, not from a
provider name or credential type.

## Input shape

| Current declaration | Input |
|---|---|
| custom / freeform | Raw patch text. Do not JSON-wrap it. |
| function / JSON | The JSON object declared by the schema. |

If the schema has a `dry_run` parameter, use it only for a genuine preview.
If `apply_patch_preview` or `confirm_apply` is not in the current tool list, do
not call it and do not claim a preview was performed.

## Patch format

```text
*** Begin Patch
*** Update File: path/to/file
@@
 old line
-removed line
+added line
*** Add File: path/to/new_file
+new content
*** Delete File: path/to/old_file
*** End Patch
```

Rules:

- The first matching context wins. Add surrounding lines or an `@@` anchor
  when a context block is repeated.
- Use exact file text, not line-number prefixes.
- Add files with `*** Add File`; delete with `*** Delete File`.
- Keep unchanged context lines in the same order.
- A failed hunk is not permission to rewrite the whole file. Re-read the
  relevant region and retry with a narrower patch.
- Do not add an empty update hunk or a marker-only hunk.

## After applying

Verify the result with a read or an assertion when practical. If the tool
reports a partial application, inspect the reported paths before continuing.
