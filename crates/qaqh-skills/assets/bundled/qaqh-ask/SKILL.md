---
name: qaqh-ask
description: Ask the user focused clarification questions using the strict ASK v1 format.
metadata:
  short-description: Ask the user safely
---

# Ask the User

Use `ask` only when a missing user preference or decision blocks useful progress.
Do not use it for permission approval, plan review, facts already in context, or
questions you can resolve safely yourself.

The tool accepts exactly one JSON field, `input`, containing a complete `ASK v1`
document. The text is parsed strictly. If formatting is wrong, the call is not
shown to the user: the model receives a retryable `ASK_FORMAT_ERROR` and must
resubmit the complete corrected document.

## ASK v1 grammar

```text
ASK v1
Question: Which environment should we deploy to?
Options:
- Staging
- Production
Allow custom: no

Question: Are there any constraints I should consider?
```

Rules:

- Start with the exact version header `ASK v1`.
- Each question block starts with one `Question: ` line; question text must be
  non-empty and fit on one line.
- Separate question blocks with a blank line.
- An optional `Options:` line is followed by one or more `- option` lines.
  Options must be non-empty and unique. Preserve their order.
- An optional `Allow custom: yes` or `Allow custom: no` line follows options.
  Omission means `yes`. `no` requires one or more options.
- No IDs are supplied; the backend assigns `q1`, `q2`, … in order.
- Do not add Markdown fences, JSON, commentary, unknown fields, or trailing
  material. The entire tool input is the document.
- Limits: 16 KiB total, at most 10 questions, 10 options per question,
  1,000 characters per question, and 200 per option.

## Asking well

- Ask the smallest number of independent, decision-relevant questions.
- Make each question answerable without relying on another question's answer.
- Use short, mutually exclusive options when the decision space is known.
- Set `Allow custom: no` only when the listed options cover every valid answer.
- Omit options or use `Allow custom: yes` when a genuinely open answer is useful.
- Do not expose secrets or ask the user to repair tool syntax.
