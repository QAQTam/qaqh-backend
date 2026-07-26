---
name: deepx-self-audit
description: Audit DeepX development and debugging changes across Rust backend, Electron frontend, daemon protocols, provider gateways, tools, sessions, installer/updater, persistent data, and release versions. Activate before diagnosing cross-component bugs, changing APIs or schemas, handling secrets, modifying update or deletion logic, or declaring a fix complete.
---

# DeepX Self Audit

Protect contracts and user data before optimizing implementation.

## Investigate before editing

1. Inspect repository state and preserve unrelated changes.
2. Reproduce the symptom or establish concrete evidence.
3. Trace the complete path:

```text
producer -> serialization -> transport -> deserialization -> state -> UI/effect
```

4. Use CodeGraph for candidate callers and consumers when available. Verify every critical edge in source.
5. State the root cause before changing code.
6. Fix the first incorrect boundary instead of making every layer match an accidental shape.

Read [references/contract-and-security.md](references/contract-and-security.md) for API, persistence, credential, installer, updater, or deletion changes.

## Mandatory gates

- Treat requests, events, tool schemas, IPC, protocols, manifests, configuration and persisted records as contracts.
- Do not rename, remove, reinterpret, or change field types/defaults without classifying compatibility.
- Do not change both producer and consumer until the violating side is identified.
- Do not place API keys or authorization values in logs, command lines, tool arguments, sessions, audit records, errors, manifests, or crash output.
- Do not weaken canonical path, ownership marker, reparse-point, deletion budget, process shutdown, or confirmation protections.
- Do not update snapshots merely to make tests pass.
- Do not declare success without running relevant verification.

## Verify by risk

- Rust: run focused tests, then checks for affected crates.
- Frontend: run typecheck, focused state/UI tests, and verify incremental event timing.
- Cross-component API: test producer, consumer, missing fields, unknown fields, reconnect, cancellation and mixed versions.
- Provider streaming: test partial chunks, malformed events, retries, usage updates and terminal events using sanitized fixtures.
- Tools: test schema, permissions, execution, audit output and frontend rendering.
- Installer/updater: test manifests, rollback, running processes, registry values, safe deletion and component-only updates.
- Release: verify all DeepX components inherit the intended version.

Report commands, results, warnings and untested areas. Stop when a breaking or destructive decision needs user authorization.
