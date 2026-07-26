---
name: deepx-dev-audit
description: Enforce DeepX-specific development, debugging, security, API compatibility, and release verification. Use for changes to Rust crates, Electron/SolidJS desktop code, daemon control protocols, provider gateways, tools, sessions, installer/updater behavior, persistent formats, versioning, or any investigation that may alter a public or cross-component contract.
---

# DeepX Development Audit

Treat observable behavior and cross-component data shapes as contracts. Never change a contract merely to make one failing caller pass.

## Required workflow

1. Read repository instructions and inspect `git status`.
2. Establish the failing behavior with logs, a test, or a reproducible trace.
3. Map producers, transports, consumers, persistence, and tests before editing. Use CodeGraph when its index is available, then verify important edges in source.
4. State the root cause in one sentence. Distinguish evidence from inference.
5. Make the smallest coherent fix. Preserve unrelated user changes.
6. Test the narrowest affected unit first, then the component boundary, then broader checks proportional to risk.
7. Search for stale names, duplicate version sources, secret exposure, and obsolete compatibility paths.
8. Review the final diff and report tests, warnings, and untested areas honestly.

For API, protocol, schema, or persistence changes, read [references/api-change-policy.md](references/api-change-policy.md) before editing.

## Hard gates

- Do not rename, remove, reinterpret, or change the type/default of a request, event, tool, protocol, manifest, config, or persisted field without an explicit compatibility decision.
- Do not increment protocol or schema versions without documenting migration and mixed-version behavior.
- Do not make frontend and backend "agree" by editing both sides before identifying which side violates the existing contract.
- Do not log secrets, authorization headers, complete configuration payloads, or unredacted provider responses.
- Keep API keys only in configuration persistence and necessary request-time memory. Never place them in logs, command lines, tool arguments, session messages, audit records, crash text, or update manifests.
- Do not weaken path canonicalization, ownership markers, reparse-point checks, deletion budgets, or confirmation steps in installer/updater code.
- Do not update snapshots or expected values until the implementation has been independently justified.
- Do not claim completion when relevant tests did not run.

## Debugging method

Follow the value, not the symptom:

```text
source -> serialization -> transport -> deserialization -> state update -> UI/effect
```

At each edge record the field name, type, optionality, timing, ownership, and failure behavior. For streaming behavior, check partial updates, ordering, retries, cancellation, reconnect, and terminal events separately.

Prefer a failing regression test at the first incorrect boundary. If reproduction is impractical, add a contract or invariant test.

## Verification matrix

Choose every applicable row:

| Change | Minimum verification |
|---|---|
| Rust implementation | Target crate tests and `cargo check` for affected packages |
| Frontend state/UI | Typecheck, focused tests, and runtime state transition check |
| Daemon/frontend contract | Producer and consumer tests plus mixed-version behavior |
| Provider gateway | Sanitized fixture tests for success, partial stream, malformed data, retry, and usage |
| Tool schema/call | Schema test, permission path, audit behavior, and UI rendering |
| Installer/updater | Manifest validation, safe-path tests, running-process behavior, and Windows registry display |
| Version bump | Run `scripts/check_versions.py` from this Skill |
| Secret handling | Repository search for logging, CLI arguments, tool/session persistence, and fixtures containing realistic secrets |

Stop and report a blocker if a required destructive or compatibility decision is not authorized.

## Release discipline

Before the stable `1.0.0` API lock, prereleases may revise contracts only with explicit review and coordinated migration. After the lock:

- additive optional fields are preferred;
- removals and semantic changes require a new protocol/schema version;
- readers should tolerate unknown fields;
- writers must not emit a new required field to older peers without negotiation;
- migrations must be idempotent and tested from the oldest supported format.
