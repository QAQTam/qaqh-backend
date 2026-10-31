#!/usr/bin/env python3
"""Compile current registry/notification sources against existing Rust artifacts."""

import pathlib
import subprocess
import tempfile


root = pathlib.Path(__file__).resolve().parents[2]
deps = root / "target/debug/deps"
command = [
    "rustc",
    "--edition",
    "2024",
    str(pathlib.Path(__file__).with_name("registry_wait_probe.rs")),
    "-L",
    f"dependency={deps}",
]
for name in ("libc", "log", "serde_json"):
    artifacts = list(deps.glob(f"lib{name}-*.rlib"))
    if not artifacts:
        raise SystemExit(f"Missing existing {name} rlib under {deps}; no Cargo build started.")
    artifact = max(artifacts, key=lambda path: path.stat().st_mtime_ns)
    command.extend(["--extern", f"{name}={artifact}"])

with tempfile.TemporaryDirectory(prefix="qaqh-tool-wait-audit-") as directory:
    binary = pathlib.Path(directory) / "registry-wait-audit"
    subprocess.run([*command, "-o", str(binary)], cwd=root, check=True, timeout=30)
    subprocess.run([str(binary)], cwd=root, check=True, timeout=8)
