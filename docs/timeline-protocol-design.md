# Native timeline protocol

## Decision

The desktop transcript will move to a native timeline contract. `Agent2Ui`,
`RoundData`, and the legacy projector are not adapters or fallback inputs for
this contract. The older `RoundBlock` concept is only a semantic precedent:
the replacement types live in `deepx-domain::timeline` and never import
`deepx-proto`.

## Invariants

1. A single appender assigns `timeline_seq`, monotonically within a daemon
   epoch and session seed. It is the only sequence that defines transcript
   order across reasoning, text, and tools.
2. `block_order` is assigned once when a block opens. Tool status and output
   update the same block; they cannot move it in the transcript.
3. `fragment_seq` is contiguous within a reasoning/text block. A gap or a
   duplicate is a recovery error, never an instruction to concatenate in
   arrival order.
4. A Markdown block is rendered as Markdown only after `block_sealed`.
   Streaming content may be accumulated or presented as plain text first.
5. Snapshots are materialized state (`TimelineSnapshot`), carrying a
   watermark. Reconnect replays entries strictly after that watermark.

## Native records

The new domain types are:

```text
TimelineEntry { timeline_seq, turn_id, round_num?, event }
TimelineEvent { turn_opened | block_opened | text_delta | tool_updated | tool_progress |
                block_sealed | round_sealed | turn_sealed }
TimelineSnapshot { watermark, turns[] }
```

`TimelineBlock` carries stable `block_id`, `block_order`, kind, state, text,
and optional structured tool state. The renderer consumes only timeline
entries/snapshots for transcript layout. Control and telemetry can remain
separate streams because neither is allowed to determine transcript order.

## Capability audit: legacy versus Timeline

`Timeline` is deliberately a **transcript protocol**, not a replacement name
for every historical `Agent2Ui` message. Treating it as a wholesale wire
replacement would make the following losses silent. The audit below is the
cutover gate; a `yes` means the native record already carries the information
needed by the renderer, not merely that an older projection could recreate it.

| User-visible capability | Agent2Ui / current Ringing | Timeline core | Cutover disposition |
| --- | --- | --- | --- |
| Ordered reasoning, tool and answer blocks | `RoundBlock` preserves a block list, but the three Ringing channels do not have one shared cursor | **Yes**: `timeline_seq` plus immutable `block_order` | Timeline is the authority. |
| Lossless streamed text and reconnect recovery | Per-channel reliable journal/snapshot; cross-channel order is undefined | **Yes in core**: contiguous fragments, watermark snapshot and replay | Add the v3 persisted journal/transport before enabling UI. |
| Markdown completion boundary | Implied by `RoundCompleted`/turn terminal and can be ambiguous across channels | **Yes**: `BlockSealed` is explicit | Renderer must parse Markdown only at seal. |
| Tool name and lifecycle | Prepared/running/finished/failed | **Yes for executed tools**: direct producers write args, progress patches, output, failure and permission reference into the stable block | Native activity/audit remains separate. |
| Tool notices, audit and code-change receipts | Separate tool events | **No** | Keep them in a native auxiliary activity stream or model them as Timeline annotations; do not discard them. |
| Turn completion, cancellation, failure and usage | Conversation events carry terminal state, error and usage | **Partial**: completion, stream failure and cancellation are native; usage is not yet native-complete | Add native usage records before cutover. |
| Compact/retry/provider-tool status | Conversation events | **No** | Remain in the native control/telemetry plane; they cannot order transcript blocks. |
| Permission / ask / plan interactions | Control and tool events | **No by design** | Stay in a native control/interaction protocol, with references to `turn_id` only. |
| Session, dashboard, skills and command receipts | Agent2Ui and Ringing control | **No by design** | Keep the existing native command/control protocol; Timeline must not become a catch-all bus. |

Therefore the current implementations are **not capability-equivalent**. The
safe direct cutover scope is the rendered transcript only, after the remaining
`Partial` rows are completed. It does not permit a compatibility adapter from
`Agent2Ui`, nor does it require duplicating non-transcript controls into
Timeline.

## Transport cutover

The production cutover adds one authoritative `timeline` SSE stream and its
own reliable journal. It is not implemented by merging the current
`conversation` and `tool` streams in Electron:

```text
engine source -> TimelineAppender -> timeline journal/snapshot -> timeline SSE -> renderer
```

The current conversation/tool events remain only while their producers are
being replaced. No `Agent2Ui -> TimelineEntry` bridge is permitted. The
cutover is complete only after all model deltas and tool lifecycle producers
emit native timeline records, at which point `Agent2Ui` and
`legacy_projector` can be removed together.

## First implementation slice

This change introduces the isolated domain model and `TimelineAppender`.
The appender is mutable by design: a future transport actor owns it as the
single writer, so worker threads must enqueue timeline intents rather than
allocate their own counters. It validates block sealing and fragment order,
keeps a replay journal, and materializes recovery snapshots.

## Required next slices

1. Complete native transcript records for tool arguments/progress/result/error,
   permission reference, turn terminal status/error and usage. This closes
   every `Partial` row in the audit without importing `Agent2Ui` types.
2. Move provider-stream and tool-engine output construction to timeline
   intents, including contiguous block IDs for interleaved reasoning/text/tool
   output.
3. Add a persisted timeline journal and the Ringing V1 per-session endpoints
   `/ringing/v1/sessions/{seed}/timeline` and
   `/ringing/v1/sessions/{seed}/timeline/events`.
4. Replace the Electron transcript store with a gap-aware timeline reducer;
   text deltas render while the block is open and `block_sealed` marks its
   final lifecycle state. The presentation conversion
   is from Timeline's own materialized model, never from `Agent2Ui` events.
5. Delete `Agent2Ui`, `legacy_projector`, and their old event/reducer paths in
   the same protocol-removal change.

## Verification requirements

- A sequence reasoning -> tool -> text retains that exact block order after
  live delivery, reconnect, and snapshot recovery.
- A missing or reordered text fragment is rejected and triggers recovery.
- A sealed text block is immutable; open text blocks remain incrementally
  renderable as their deltas arrive.
- Mixed streams cannot change transcript order because there is only one
  timeline cursor.
