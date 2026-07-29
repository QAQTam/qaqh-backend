---
name: deepx-dev-audit
description: Enforce DeepX-specific development, debugging, security, API compatibility, and release verification. Use for changes to Rust crates, Electron/SolidJS desktop code, daemon control protocols, provider gateways, tools, sessions, external process or shell execution, capability detection, startup/readiness loops, worker orchestration, installer/updater behavior, persistent formats, versioning, or any investigation that may alter a public or cross-component contract.
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

## External process and readiness safety

Treat capability discovery as observation, not execution.

- Never launch a program merely to determine whether it exists on a synchronous startup, request, event-loop, or tool-dispatch path. Inspect `PATH`, filesystem metadata, the registry, or an authoritative API instead.
- If execution is the only reliable probe, move it off the critical path; disable profiles and startup scripts; bound it with a short timeout and cancellation; cache the result; and make failure fall back safely.
- Apply timeout and cancellation to the total operation: resolution, capability checks, process spawn, execution, pipe draining, worker joins, and teardown. Do not start the timeout only after preflight work.
- Do not call blocking process or filesystem APIs inside async `select` branches, actor loops, IPC dispatchers, UI event handlers, or readiness initialization. Use a bounded worker and keep the loop able to process cancellation and shutdown.
- Prefer argv/direct execution. When accepting shell command strings, make the shell dialect explicit; never choose Bash, PowerShell, or cmd solely because it is installed.
- Treat every `wait`, `join`, channel send, pipe read, and child-process status call as potentially unbounded. Identify who releases it and what happens if that party stalls or dies.
- Emit readiness only after the component can consume work. Distinguish `accepted`, `queued`, `started`, and `completed` in traces so queued input cannot be mistaken for a frontend retry requirement.
- Model concurrency amplification before approving probes or startup work: calculate subprocesses, threads, memory, and waits for multiple sessions and parallel tool calls.

For any new external-process discovery or execution path, require tests that:

1. Place a candidate on `PATH` that would create a marker if executed, and assert discovery leaves no marker.
2. Cover missing, non-executable, and non-zero-exit candidates without selecting a broken fallback.
3. Verify the advertised timeout and cancellation bound the complete wall-clock operation.
4. Exercise concurrent cold starts or parallel calls at the component boundary.
5. Verify the selected command dialect on every supported platform.

## Hard gates

- Do not rename, remove, reinterpret, or change the type/default of a request, event, tool, protocol, manifest, config, or persisted field without an explicit compatibility decision.
- Do not increment protocol or schema versions without documenting migration and mixed-version behavior.
- Do not make frontend and backend "agree" by editing both sides before identifying which side violates the existing contract.
- Do not merge executable capability probes based on unbounded `status`, `output`, `wait`, `join`, or equivalent calls.
- Do not claim a timeout or cancellation guarantee when preflight, shell detection, worker joins, or pipe cleanup occur outside that guarantee.
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

When logs stop before the suspected loop, bracket synchronous initialization stages and inspect the blank interval. Do not add more logs only inside a loop that may not have started. Log event variants, lengths, identifiers, and elapsed time—not message bodies, tool arguments, credentials, or provider payloads.

## Verification matrix

Choose every applicable row:

| Change | Minimum verification |
|---|---|
| Rust implementation | Target crate tests and `cargo check` for affected packages |
| Frontend state/UI | Typecheck, focused tests, and runtime state transition check |
| Daemon/frontend contract | Producer and consumer tests plus mixed-version behavior |
| Provider gateway | Sanitized fixture tests for success, partial stream, malformed data, retry, and usage |
| Tool schema/call | Schema test, permission path, audit behavior, and UI rendering |
| External process or shell | Side-effect-free discovery test, total timeout/cancel test, dialect test, and concurrent cold-start test |
| Startup/event loop | Readiness trace, queued-input transition test, shutdown/cancel responsiveness, and multi-instance stress |
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
