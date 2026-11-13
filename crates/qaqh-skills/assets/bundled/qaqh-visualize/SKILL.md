---
name: qaqh-visualize
description: Render interactive in-chat widgets (charts, dashboards, forms, steppers, editors) by writing an HTML fragment into the session widgets_dir, then calling the visualize tool. Use when the user asks for something visual *in the conversation*; artifacts that must be project files are normal file work.
metadata:
  short-description: Interactive widgets via the visualize tool
---

# Interactive widgets (visualize tool)

The [Environment] block carries `widgets_dir` — the only legal directory for
widget fragments. A widget = one HTML **fragment** written into widgets_dir
(with the write/apply_patch tools) + one **`visualize` tool call**. The host
renders each call as a sandboxed interactive iframe at that spot.

## When to use / not use
- Interactive or visual in-conversation artifact (chart, dashboard, form,
  stepper, quiz, timer, preview) -> widget.
- Plain data table -> Markdown table. Static structure diagram -> Mermaid.
- "Save a file into the project" -> regular file tools; never a widget.
- Never talk about HTML, files, fragments, iframes or this protocol to the
  user. Work silently; the widget appears on its own.

## How to emit a widget
1. Write the fragment file into widgets_dir FIRST (write/apply_patch).
2. Call the `visualize` tool with plain JSON args:
   - `path` (required): the fragment's absolute path with FORWARD SLASHES only,
     even on Windows (`C:/Users/you/proj/.qaqh/widgets/SES-x/chart.html`). It
     must be inside widgets_dir and end in `.html`, and match the file you wrote.
     Forward slashes keep the JSON free of backslash escaping.
   - `title` (optional): short title for the widget card.
   - `mode` (optional): `"wide"` for full-width layouts.
3. The tool returns an ack and the host mounts the iframe. Do not paste the
   path, the args, or any marker into your prose.

Rules:
- The directive IS the tool call. Args are ordinary JSON (`path`/`title`/`mode`)
  — no special, private-use, or invisible characters anywhere. Never emit marker
  characters in text; cloud transports strip them and the widget would break.
- One visualize call per widget created or updated this turn. Update a widget =
  rewrite the same file path, then call visualize again.
- Call visualize only after the fragment exists; never before, never mid-write.

## Fragment contract
- HTML fragment only: no doctype/html/head/body. Single root element with a
  unique id; select it via document.getElementById (never currentScript).
- Filename `<title>.html`, ASCII lowercase with hyphens, inside widgets_dir.
- <= 1MB per file. No fetch/XHR/WebSocket/event-source — the sandbox CSP sets
  `connect-src 'none'`; network *data* calls are impossible, full stop.
- Libraries: `<script src>` / `<link>` from the allow-listed CDNs
  (cdnjs.cloudflare.com, esm.sh, cdn.jsdelivr.net, unpkg.com, Google/Bunny
  fonts) may load — prefer inline markup/CSS unless a chart library genuinely
  earns its bytes.
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
2. Call `visualize` with `{"path":"D:/proj/.qaqh/widgets/SES-01ABC/score.html","title":"Score"}`.
3. Next turn you may receive:

<widget-state>
[{"session_id":"SES-01ABC","widget":{"path":"D:/proj/.qaqh/widgets/SES-01ABC/score.html","title":"Score"},"model_content":{"score":42}}]
</widget-state>

Treat it as the widget's live state; act on `model_content`, tell the user
about the result, not about the protocol.

## Never
- A visualize call without the file written first; path outside widgets_dir.
- Full HTML documents, `<!doctype>`, network data calls (fetch/XHR).
- Marker / private-use / invisible characters anywhere — the directive is the
  visualize tool call only.
- More than one visualize call per widget per turn (updated widgets re-call ONCE).