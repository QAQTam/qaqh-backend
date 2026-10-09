# Release memory monitor

`qaqh-memwatch` is a small, independent runtime library used by `qaqh-runtime`
and the daemon service API. It is compiled in release builds; it does not
replace the global allocator, capture prompt/tool contents, or write diagnostic
files. Collection is disabled until explicitly started.

## API

The same snapshot is reachable two ways.

### Push (preferred): admin frames on the session SSE

An **admin** subscription to the existing per-session stream
`GET /ringing/v2/sessions/{seed}/timeline/events` additionally receives
`diagnostics.memory` frames carrying the snapshot JSON, one per second while a
probe window is open. Non-admin subscribers never receive these frames (no
ticker is even started for them). No second transport or endpoint is added; the
diagnostics ride the stream the client already holds. Frames are incremental on
`phases` exactly like the polling contract below.

### Pull: admin service methods over the existing endpoint

```text
POST /ringing/v2/service/diagnostics.memory.start
POST /ringing/v2/service/diagnostics.memory.snapshot
POST /ringing/v2/service/diagnostics.memory.stop
```

The request still needs a valid admin bearer token and the normal client lease
header required by the service surface. The methods accept an empty JSON object.
Call `start` before a measured conversation and `stop` after the run; `start`
resets the phase window. `stop` leaves the last snapshot readable. The in-memory
phase ring keeps the newest 4096 entries and reports `dropped_phase_samples` if
it wraps.

The first snapshot may omit `after_sequence` to obtain the current ring;
subsequent snapshots should pass the prior response's `latest_phase_sequence`
and consume only the new `phases`. `phase_gap=true` means the cursor fell behind
the bounded ring and the consumer should rebase from the returned current
snapshot/high-water fields. The push frames use the same cursor discipline, so
a client can combine a pull bootstrap with push deltas.

## Classification

Every row carries a stable classification so a client can bucket sources
without parsing name prefixes:

- `components[].group` — `ComponentGroup` (`ringing`, `agent_registry`,
  `service`, `transport`, `workspace`, `other`), derived from the name prefix.
- `phases[].kind` — `PhaseKind` (`session_resume`, `context_build`,
  `token_preflight`, `provider_request`, `compact`, `request_log`, `other`),
  derived from the phase label at record time.
- `sessions[].last_phase_kind` — the [`PhaseKind`] of the session's last phase.


Example first and incremental request bodies:

```json
{}
```

```json
{"after_sequence": 28}
```

## What is sampled

- Process metrics: resident working set and private committed bytes on Windows
  (`windows.psapi`); RSS, private pages and virtual size on Linux (`procfs`).
  Unsupported platforms return `null` counters plus a source/error marker.
- Lifecycle phases: session resume, context build, token-preflight copy and
  serialization, provider request begin/end, and compact prompt build.
- Per-session estimates: message/turn/block counts, text/image payload bytes,
  `MessageStore` owned-capacity estimate (including pending persistence copies),
  context payload size and token-preflight JSON size.
- Component gauges: active registry workers/maps, timeline sessions/turns/
  blocks/replay journal, content-store entries and loaded bodies, V2 projection
  sessions/overlays, image registry metadata, MCP/LSP connections, and service
  task/board stores.

Payload estimates traverse structures and use string/vector capacities; they do
not include allocator metadata, fragmentation, shared allocations, or opaque
library internals. `estimate_json_bytes` is the token-preflight serialization
after inline images were replaced by placeholders; it is **not** the exact HTTP
request body size. Provider request phases bracket SDK construction/networking,
but exact transport-buffer size and allocation call stacks remain outside this
first probe.

`agent_aux_heap_estimate_bytes` is a lower-bound estimate around tool schemas,
prefix hashes, and inline `AgentState` storage; it does not recursively walk
skills, `TurnEngine`, or `ToolEngine` internals. The V2 projection gauge exposes
resident session/overlay counts and a shell estimate, not a byte-exact walk of
every projection row. Those are good next instrumentation points if the first
long run shows process memory growing without a matching MessageStore/timeline
increase.

Some component gauges intentionally overlap: `ringing.timeline.appender` is an
aggregate footprint, while `ringing.timeline.replay_journal` and
`ringing.timeline.structure` break out parts of it. Do not sum every component
row as though they were disjoint. `heap_estimate_bytes: null` means that
subsystem exposes counts but no reliable heap estimate.

The API is admin-only because snapshots include process-wide and per-session
identifiers. It never includes conversation text, tool output, image bytes,
API keys, or raw provider responses.
