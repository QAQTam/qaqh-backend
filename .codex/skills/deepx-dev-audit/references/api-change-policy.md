# DeepX API Change Policy

## Contract inventory

Audit all applicable surfaces before changing a field or behavior:

- daemon WebSocket and HTTP control messages;
- Electron preload IPC and renderer request/event payloads;
- provider request, stream event, usage, and error normalization;
- tool names, JSON schemas, results, permission categories, and audit metadata;
- `config.toml`, SQLite mirrors, session JSONL, manifests, catalogs, install state, and registry values;
- command-line arguments and environment variables;
- frontend stores and persisted browser state.

## Change classification

- **Internal:** no observable or persisted effect outside one module. Focused tests suffice.
- **Additive compatible:** optional field/event with a safe default; older readers continue working.
- **Behavioral compatible:** same shape, corrected semantics. Add a regression test and document the old failure.
- **Breaking:** removal, rename, type/default/meaning change, new required field, ordering requirement, or persistence incompatibility.

Treat uncertainty as breaking until proven otherwise.

## Breaking-change procedure

1. Identify every producer and consumer.
2. Decide minimum supported frontend, backend, installer, and stored-data versions.
3. Select negotiation, dual-read/single-write migration, or explicit version rejection.
4. Define rollback behavior.
5. Add old↔new compatibility tests and migration tests.
6. Increment the relevant protocol or schema version.
7. Record the decision in code next to the version constant or migration.

Never silently repurpose an existing field.

## Security review

Search both names and flows. A secret may leak without containing `api_key` in the destination code.

Check:

- structured and formatted logs;
- errors containing request/debug objects;
- process command lines and environment inheritance;
- tool arguments, messages, JSONL and audit data;
- temporary/outbox files and database mirrors;
- analytics, crash reporting and update metadata;
- test fixtures committed to Git.

Use obviously synthetic values in tests. Redact by default and log only presence when operationally necessary.
