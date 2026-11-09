#!/usr/bin/env python3
"""LTS API-surface freeze guard.

Regenerates the SDK contract export at HEAD and compares it against the golden
snapshot in docs/api-surface/<name>/:

- every artifact byte-identical, except manifest.json where the per-commit
  fields (source_revision / source_dirty) are excluded before comparing —
  contract_hash itself covers only artifact hashes and is revision-independent;
- the daemon HTTP route table (parsed from axum_server sources) unchanged;
- the qaqh-client facade `pub use` list unchanged.

Exit 0 = surface unchanged. Non-zero = API drift; on the LTS branch such a
change must be rejected: revert the implementation, never the snapshot.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
VOLATILE_MANIFEST_FIELDS = ("source_revision", "source_dirty")

ROUTE_CHAINED = re.compile(r'\.route\(\s*"([^"]+)"\s*,\s*(?:get|post|put|delete|patch)\b')
ROUTE_ARM = re.compile(r'\(\s*"([A-Z]+)"\s*,\s*"([^"]+)"\s*\)\s*=>')


def git(*args: str) -> str:
    return subprocess.check_output(["git", *args], cwd=ROOT, text=True).strip()


def tracked_dirty() -> bool:
    """Untracked files cannot influence compiled artifacts; tracked ones can."""
    lines = git("status", "--porcelain").splitlines()
    return any(not line.startswith("??") for line in lines)


def export_fresh(stage: Path) -> None:
    env = os.environ.copy()
    env.update(
        QAQH_SOURCE_REVISION=git("rev-parse", "HEAD"),
        QAQH_SOURCE_DIRTY="1" if tracked_dirty() else "0",
    )
    subprocess.run(
        ["cargo", "run", "-p", "qaqh-contract-export", "--",
         "export", "--out", str(stage), "--release"],
        cwd=ROOT, env=env, check=True,
    )


def route_lines() -> list[str]:
    server = ROOT / "crates" / "qaqh-daemon" / "src" / "axum_server"
    routes: set[str] = set()
    for source in server.rglob("*.rs"):
        text = source.read_text(encoding="utf-8")
        for path in ROUTE_CHAINED.findall(text):
            routes.add(f"* {path}")
        for method, path in ROUTE_ARM.findall(text):
            routes.add(f"{method} {path}")
    return sorted(routes)


def facade_lines() -> list[str]:
    src = (ROOT / "crates" / "qaqh-client" / "src" / "lib.rs").read_text(encoding="utf-8")
    statements = re.findall(r"pub use[^;]+;", src, flags=re.S)
    return sorted(" ".join(statement.split()) for statement in statements)


def compare_file_set(golden: Path, stage: Path) -> list[str]:
    problems: list[str] = []
    # snapshots/ holds route & facade baselines consumed below, not export artifacts.
    def export_files(root: Path) -> list[str]:
        return sorted(
            p.relative_to(root).as_posix()
            for p in root.rglob("*")
            if p.is_file() and not p.relative_to(root).as_posix().startswith("snapshots/")
        )
    golden_files = export_files(golden)
    fresh_files = export_files(stage)
    for path in golden_files:
        if path not in fresh_files:
            problems.append(f"artifact missing from fresh export: {path}")
    for path in fresh_files:
        if path not in golden_files:
            problems.append(f"artifact added on this branch: {path}")
    for path in golden_files:
        if path == "manifest.json" or path not in fresh_files:
            continue
        if (golden / path).read_bytes() != (stage / path).read_bytes():
            problems.append(f"artifact differs: {path}")
    return problems


def compare_manifest(golden: Path, stage: Path) -> list[str]:
    golden_manifest = json.loads((golden / "manifest.json").read_text(encoding="utf-8"))
    fresh_manifest = json.loads((stage / "manifest.json").read_text(encoding="utf-8"))
    for volatile in VOLATILE_MANIFEST_FIELDS:
        golden_manifest.pop(volatile, None)
        fresh_manifest.pop(volatile, None)
    if golden_manifest != fresh_manifest:
        return ["manifest.json differs (excluding source_revision/source_dirty)"]
    return []


def compare_snapshot(golden: Path, fresh: str, name: str) -> list[str]:
    expected = (golden / "snapshots" / f"{name}.txt").read_text(encoding="utf-8")
    if expected.strip() != "\n".join(fresh).strip():
        return [f"{name} surface differs from snapshot (see docs/api-surface snapshots/{name}.txt)"]
    return []


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--against",
        default="docs/api-surface/v3.0",
        help="golden snapshot directory relative to the repository root",
    )
    options = parser.parse_args()

    golden = ROOT / options.against
    if not golden.is_dir():
        raise SystemExit(f"golden snapshot directory not found: {golden}")

    if tracked_dirty():
        print("refusing: tracked working-tree changes would make the export ambiguous", file=sys.stderr)
        return 2

    problems: list[str] = []
    with tempfile.TemporaryDirectory(prefix="qaqh-lts-guard-") as temp:
        stage = Path(temp) / "export"
        export_fresh(stage)
        problems += compare_file_set(golden, stage)
        problems += compare_manifest(golden, stage)
        problems += compare_snapshot(golden, route_lines(), "routes")
        problems += compare_snapshot(golden, facade_lines(), "facade")

    if problems:
        print("LTS API-surface guard: FAILED — reject this change on the LTS branch.", file=sys.stderr)
        for problem in problems:
            print(f"  - {problem}", file=sys.stderr)
        print("Freeze policy: docs/lts-v3.0-maintenance.md §1. Revert the implementation, never the snapshot.", file=sys.stderr)
        return 1
    print(f"LTS API surface unchanged (baseline {options.against}).")
    return 0


if __name__ == "__main__":
    sys.exit(main())
