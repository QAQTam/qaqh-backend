# Contract and Security Review

## Compatibility classification

- Internal: invisible outside one module and not persisted.
- Additive: optional data with a safe default that old readers ignore.
- Behavioral: shape is stable but semantics are corrected.
- Breaking: rename, removal, type/default/meaning change, new required data, ordering dependency, or storage incompatibility.

Treat uncertain changes as breaking.

For a breaking change, identify all producers and consumers, define supported mixed versions, select negotiation or migration behavior, test rollback, then increment the relevant protocol or schema version.

## Credential boundary

Credentials may exist only in configuration persistence and request-time memory. Audit:

- formatted and structured logging;
- errors containing request or debug objects;
- process arguments and inherited environment;
- tool calls and conversation persistence;
- temporary files, database mirrors and audit trails;
- analytics, updates and crash reporting;
- committed fixtures.

Use synthetic test values. Log credential presence only when operationally necessary.

## Destructive operations

Resolve and canonicalize exact targets before deletion. Require an application-owned marker bound to the canonical root and current user. Reject drive roots, protected ancestors, symlinks, junctions and reparse points. Revalidate immediately before deletion and enforce entry/byte budgets.

Workspace `.deepx` content is user-project data. Never delete it as part of global uninstall unless the user separately selects exact workspace roots.
