# Protocol replacement inventory

This is the deletion ledger for the `Agent2Ui` replacement. A legacy event is
not carried forward merely because it exists; every user-visible datum has one
native owner and one transport class.

## Native owners

| Data | New owner | Transport | Why |
| --- | --- | --- | --- |
| user prompt, reasoning/text fragments, tool position and terminal transcript state | `TimelineIntent` → `TimelineAppender` | Ringing V1 reliable timeline journal and one cursor | The renderer needs one causal order, not three channel-local orders. |
| tool arguments, running state, output tail, failure and permission reference | `TimelineTool` | timeline update to the block's stable id | Updates retain a position instead of creating unrelated tool-card and text races. |
| permissions, ask-user, plan review and command receipts | native control event/command | control stream keyed by interaction/command id | They are state machines, not transcript ordering records. |
| session activity, dashboard, skills, compact progress, token/accounting metadata | native control/activity snapshot | replaceable control/activity stream | They can be replaced by revision and must not advance transcript order. |
| audit trail and code-change counters | activity record | bounded activity snapshot/event stream | They have their own retention and privacy policy. |

## Legacy deletion groups

1. **Transcript projection group**: `Agent2Ui::{TurnStart, RoundDelta,
   RoundComplete, ToolCallPreview, ToolResults, ToolExecDelta, ExecProgress,
   TurnEnd, Done, Cancelled}` and `legacy_projector` transcript branches. Delete
   after Ringing V1 timeline SSE and the Electron timeline reducer are the only desktop
   transcript source.
2. **Control projection group**: legacy `Ready`, session lifecycle, ask/plan,
   permission, dashboard, skills and error variants. Delete only after the
   native control plane has its own Electron store and command receipt UI.
3. **Replay group**: `EventBus` Agent2Ui projection and `session.replay_events`.
   Delete after persisted Ringing V1 timeline/control snapshots cover resume and load
   more turns.

## Explicit non-goals

- Do not add `Agent2Ui -> TimelineIntent` conversion.
- Do not combine control and transcript events into one lossy catch-all event.
- Do not use a renderer-side merge of separate `conversation` and `tool` streams.
- Do not silently accept a pre-Ringing V1 client. Version negotiation
  must reject it, so upgrades roll forward or roll back as complete units.

## Current migration evidence

The native producer command is `TimelineIntent`; its worker frame is
`Ringing_timeline_intent_v1`. The runtime's `RingingHub::publish_timeline` owns the
single `TimelineAppender`. Model and UI-tool producers now write turn open,
reasoning/text deltas, tool preparation/running/progress/result/permission,
block/round seals, and the normal completion terminal directly. Electron now
fetches `/ringing/v1/sessions/{seed}/timeline` and consumes one per-session
SSE cursor; its reducer rejects gaps and renders incoming text deltas before a
block seal.

This is still a staged cutover: Ringing V1 control stores supply interactions,
dashboard and accounting to the presentation fallback, and error/cancelled
model terminal paths plus native usage records remain to be completed before
the transcript projection group is deleted.
