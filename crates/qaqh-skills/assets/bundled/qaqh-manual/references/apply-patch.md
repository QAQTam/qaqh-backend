# Apply Patch Reference

Use this reference when the task involves `apply_patch`. The live tool schema is
still authoritative.

## Input shape

| Declaration | What to send |
|---|---|
| custom / freeform | Raw patch text. Do not wrap it in JSON. |
| function / JSON | The object declared by the schema, normally `{ "patch": "..." }`. |

If the schema has `dry_run`, use it only for a real preview. If
`apply_patch_preview` or `confirm_apply` is not in the current tool list, do not
call it and do not claim a preview was performed.

## Grammar

```text
*** Begin Patch
*** Update File: path/to/file
@@
 context line
-removed line
+added line
@@ optional context anchor
 context line
+added line
*** Add File: path/to/new_file
+new content
*** Delete File: path/to/old_file
*** End Patch
```

Rules:

- Begin with `*** Begin Patch` and end with `*** End Patch`.
- Update, add, and delete sections are separate file hunks.
- An update hunk starts with `@@`; an optional context line may follow it.
- `+` adds, `-` removes, and a leading space keeps context.
- `*** End of File` may anchor a hunk at the end of a file.
- The first matching context wins. Add surrounding lines or `@@ <context>` when
  the context is repeated.
- Use exact file text, not `L{n}:` prefixes from `read`.
- Do not create empty update hunks or marker-only hunks.

## Update example

```text
*** Begin Patch
*** Update File: src/lib.rs
@@ pub fn answer
-    41
+    42
*** End Patch
```

## Add example

```text
*** Begin Patch
*** Add File: notes/todo.md
+# TODO
+- verify the patch
*** End Patch
```

## Failure handling

- `not_found`: the context does not match. Re-read the region and retry with a
  narrower, exact hunk.
- `ambiguous_match`: add surrounding context or `@@ <context>`.
- Partial application: the result identifies applied and unapplied paths. Do
  not resend the whole patch; resend only the remainder.
- `dry_run` overwrite warning: use `*** Update File` or `*** Delete File`
  deliberately; do not silently replace an existing file.

After applying, verify with `read`, an assertion, or a targeted command. Do not
re-read a file only to restate a successful receipt.
