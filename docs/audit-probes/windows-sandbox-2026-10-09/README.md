# Windows sandbox diagnostic probe

Companion report: [Windows AppContainer / ProjFS audit](../../analysis-windows-appcontainer-projfs-2026-10-09.md).

Windows-only, non-elevated; ProjFS must already be available. This does not enable Windows features or change production code.

```powershell
cargo run --manifest-path docs/audit-probes/windows-sandbox-2026-10-09/Cargo.toml --target-dir target/audit-20261009
```

The probe uses randomly named profiles and temporary canaries. It compares standard AppContainer launch before/after actual profile creation, native/helper capability counts, the public ExecTool under Auto and explicit Windows backends, production-shaped scratch ACL inheritance, and verbose-command pipe backpressure. It intentionally observes current failures rather than asserting a fixed implementation.

Profiles and temporary files are cleaned on normal completion. If interrupted, inspect the specific `qaqh-audit-*` or `qaqh-acl-audit-*` directory from that run; do not delete unrelated temporary directories. `results.txt` is the captured output of the audit run, not a golden snapshot.
