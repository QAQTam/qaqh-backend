#!/usr/bin/env python3
import json
import re
import sys
from pathlib import Path


def fail(message: str) -> None:
    print(f"ERROR: {message}", file=sys.stderr)
    raise SystemExit(1)


root = Path(sys.argv[1] if len(sys.argv) > 1 else ".").resolve()
expected = (root / "version.txt").read_text(encoding="utf-8").strip()

cargo = (root / "Cargo.toml").read_text(encoding="utf-8")
match = re.search(r"\[workspace\.package\][\s\S]*?version\s*=\s*\"([^\"]+)\"", cargo)
if not match:
    fail("workspace package version is missing")

values = {
    "version.txt": expected,
    "Cargo.toml": match.group(1),
    "package.json": json.loads((root / "package.json").read_text(encoding="utf-8"))["version"],
    "apps/desktop/package.json": json.loads(
        (root / "apps/desktop/package.json").read_text(encoding="utf-8")
    )["version"],
    "apps/desktop/deepx-backend.lock.json": json.loads(
        (root / "apps/desktop/deepx-backend.lock.json").read_text(encoding="utf-8")
    )["version"],
}

for source, value in values.items():
    if value != expected:
        fail(f"{source} has {value!r}; expected {expected!r}")

lock = json.loads((root / "apps/desktop/deepx-backend.lock.json").read_text(encoding="utf-8"))
if f"/download/v{expected}/" not in lock["release_manifest_url"]:
    fail("backend release manifest URL does not match version.txt")

tui_justfile = (root / "apps/deepx-tui/justfile").read_text(encoding="utf-8")
if f'_version := "{expected}"' not in tui_justfile:
    fail("apps/deepx-tui/justfile version does not match version.txt")

vector_cargo = (root / "crates/deepx-vector/Cargo.toml").read_text(encoding="utf-8")
if "version.workspace = true" not in vector_cargo:
    fail("deepx-vector must inherit the workspace release version")

installer_main = (root / "apps/installer/src/main.rs").read_text(encoding="utf-8")
installer_install = (root / "apps/installer/src/install.rs").read_text(encoding="utf-8")
if 'write_uninstall_registry(&self.config.target_path, env!("CARGO_PKG_VERSION"))' not in installer_main:
    fail("installer must register the Cargo package version with Windows")
if 'set_value("DisplayVersion", version)' not in installer_install:
    fail("Windows uninstall registration must write DisplayVersion")

print(f"OK: all DeepX release versions are {expected}")
