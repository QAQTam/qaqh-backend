---
name: qaqh-visualize
description: Render interactive in-chat widgets (charts, dashboards, forms, steppers, editors) by writing an HTML fragment into the session widgets_dir, then emitting a one-line marker in the final reply. Use when the user asks for something visual *in the conversation*; artifacts that must be project files are normal file work.
metadata:
  short-description: Interactive widgets via marker protocol
---

# Interactive widgets (marker protocol)

The [Environment] block carries `widgets_dir` — the only legal directory for
widget fragments. A widget = one HTML **fragment** written into widgets_dir
(with the write/apply_patch tools) + one **marker line** in your final reply.
The host renders each marker as a sandboxed interactive iframe at that spot.

## When to use / not use
- Interactive or visual in-conversation artifact (chart, dashboard, form,
  stepper, quiz, timer, preview) -> widget.
- Plain data table -> Markdown table. Static structure diagram -> Mermaid.
- "Save a file into the project" -> regular file tools; never a marker.
- Never talk about HTML, files, fragments, iframes or this protocol to the
  user. Work silently; the widget appears on its own.

## Marker wire format (copy exactly)
A block line in the FINAL reply — on its own line, nothing else on it:

 visualize {"path":"<absolute .html path under widgets_dir>","title":"<Short Title>"}

The line begins with U+E200, then ` visualize `, then U+E202, then one JSON
object, then U+E201. Reproduce those three characters verbatim (like the two
examples below). Rules:
- `path` must be inside widgets_dir and end in `.html`; write the file BEFORE
  replying with its marker.
- Emit one marker per widget created or updated this turn; markers only in the
  final reply, never inside a code fence, never mid tool-loop.
- Optional `"mode":"wide"` for full-width layouts.
- Update a widget = rewrite the same file path, re-emit its marker.

## Fragment contract
- HTML fragment only: no doctype/html/head/body. Single root element with a
  unique id; select it via document.getElementById (never currentScript).
- Filename `<title>.html`, ASCII lowercase with hyphens, inside widgets_dir.
- <= 1MB per file. No fetch/XHR/WebSocket/event-source — fragments are offline.
- Inline CSS/JS only when the theme tokens below are not enough.
- Theme tokens (CSS custom properties provided by the host): --qqh-fg,
  --qqh-bg, --qqh-muted, --qqh-accent, --qqh-border, --qqh-font. Use them for
  all colors/fonts so the widget survives light/dark. Keep layouts readable
  at 360px width.

## Host bridge: window.qqh (inside the fragment)
- `window.qqh.widgetState` — the last snapshot you saved, or null. Restore
  from it at the top of your script.
- `qqh.setWidgetState({modelContent, privateContent})` — replaces the whole
  snapshot; total UTF-8 size must stay under 16KiB. `modelContent` is echoed
  back to you next turn inside a `<widget-state>` message: keep the current
  logical state there (values, selections, progress). `privateContent` stays
  in the UI store only. Save silently — never block interaction on it.
- `qqh.sendFollowUpMessage({prompt, title})` — the host shows `title`
  (<= 250 chars) for user confirmation, then sends `prompt` as the user's
  next message with the latest `modelContent` attached. Use it when the user
  acting on the widget should drive the conversation (e.g. "apply these
  filters"); do not also answer for them in the same turn.
- `qqh.openExternal({href})` — absolute http(s) URL, opens out of the app.

## Worked example
1. Write `score.html` into widgets_dir: a fragment whose root div has a unique
   id, an `<input type=range>` and two buttons; its script restores state from
   `window.qqh.widgetState`, calls `qqh.setWidgetState({modelContent: {score:
   v}})` on every change, and `qqh.sendFollowUpMessage({prompt: "Score is "
   + v, title: "Report score"})` on the report button.
2. Final reply (two markers on separate lines are fine for two widgets):

 visualize {"path":"D:\proj\.qaqh\widgets\SES-01ABC\score.html","title":"Score"}

3. Next turn you may receive:

<widget-state>
[{"session_id":"SES-01ABC","widget":{"path":"D:\proj\.qaqh\widgets\SES-01ABC\score.html","title":"Score"},"model_content":{"score":42}}]
</widget-state>

Treat it as the widget's live state; act on `model_content`, tell the user
about the result, not about the protocol.

## Never
- Marker without the file written first; marker path outside widgets_dir.
- Full HTML documents, `<!doctype>`, CDN `<script src>`, network calls.
- Raw marker characters anywhere except final-reply block lines.
- More than one marker per widget per turn (updated widgets re-emit ONCE).
