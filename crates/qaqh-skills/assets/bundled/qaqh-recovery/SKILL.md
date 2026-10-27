---
name: qaqh-recovery
description: Inspect and recover local workspace changes with journal and spy.
metadata:
  short-description: Journal and spy recovery
---

# Recovery

Use `journal` to inspect typed file-tool revisions:

- `journal {action: "journal"}` lists change ids.
- `journal {action: "undo", id: "..."}` reverts one change.
- `journal {action: "replay", id: "...", path: "..."}` replays a revision.
- `journal {action: "cat", id: "..."}` prints a historical blob (bounded).

Use `spy` for workspace-level snapshots and exec-created changes:

- `spy {action: "journal"}` lists marks.
- `spy {action: "undo", id: "..."}` reverts one change.
- `spy {action: "restore", mark: "...", prune: true}` rewinds to a mark; `prune`
  also deletes files newer than the mark.
- `spy {action: "cat", ...}` prints a historical blob.

Inspect before restoring. A restore can remove unrelated recent work; never use
it as a blind fix for a failed edit.
