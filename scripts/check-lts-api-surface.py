#!/usr/bin/env python3
"""LTS API-surface freeze guard.

Regenerates the SDK contract export at HEAD and compares it against the golden
snapshot in docs/api-surface/<name>/:

- every artifact byte-identical, except manifest.json where the per-commit
  fields (source_revision / source_dirty) are excluded before comparing —
  contract_hash itself covers only artifact hashes and is revision-independent;
- the HTTP route table unchanged. Since ADR-0003 the route set is read from the
  two *data tables* that actually drive `build_router`
  (`qaqh_ringing::RINGING_V2_HTTP_ROUTES` and `axum_impl::CONTROL_PLANE_ROUTES`),
  never re-derived from builder source with a regex; a table that does not parse
  is an error, so the guard fails closed instead of silently under-collecting;
- the axum_server source tree contains no literal-path `.route("…")` (nor any
  `.nest(` / `.merge(`): the tables are the only place a route may be declared;
- the qaqh-client facade `pub use` list unchanged.

Exit 0 = surface unchanged. Non-zero = API drift; on the LTS branch such a
change must be rejected: revert the implementation, never the snapshot.
Snapshots are machine-written only — use --rebaseline (and on LTS that is a
refreeze, see docs/adr/0007.md §决策 2).
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

HTTP_METHODS = ("GET", "POST", "PUT", "DELETE", "PATCH")

# 路由表的真源：两张数据表，而非任何 .route() 调用点。
ROUTE_TABLES = (
    ("crates/qaqh-ringing/src/v2/mod.rs", "RINGING_V2_HTTP_ROUTES"),
    ("crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs", "CONTROL_PLANE_ROUTES"),
)

# 登记处 lint 的作用域：daemon 的 HTTP 层。字面量路径一律禁止，见 ADR-0003 决策 2b。
ROUTE_SOURCE_DIR = ROOT / "crates" / "qaqh-daemon" / "src" / "axum_server"
BANNED_ROUTE_WRITES = (
    re.compile(r'\.route\(\s*"'),
    re.compile(r'\.nest\('),
    re.compile(r'\.merge\('),
)

def git(*args: str) -> str:
    return subprocess.check_output(["git", *args], cwd=ROOT, text=True).strip()


def tracked_dirty() -> bool:
    """Untracked files cannot influence compiled artifacts; tracked ones can."""
    lines = git("status", "--porcelain").splitlines()
    return any(not line.startswith("??") for line in lines)


def _strip_comments(source: str) -> str:
    """Blank out // and /// lines so the registry lint talks about code only.

    Lines are blanked, never dropped: the lint reports `file:line`, and a shifted
    line number would make the guard itself a source of misinformation.
    """
    return "\n".join(
        "" if line.lstrip().startswith("//") else line for line in source.splitlines()
    )


def route_table(rel_path: str, const_name: str) -> list[str]:
    """Parse one `pub const NAME: &[(&str, &str)] = &[ ("METHOD", "/path"), … ];`.

    Fail closed: a missing const, an unterminated body, or any row that is not
    exactly a ("METHOD", "/path") literal raises. Silent under-collection is the
    failure mode this replaces.
    """
    source = (ROOT / rel_path).read_text(encoding="utf-8")
    marker = f"const {const_name}"
    if marker not in source:
        raise SystemExit(f"{const_name} not found in {rel_path}: the route table must exist as data")
    body = source.split(marker, 1)[1]
    if "= &[" not in body:
        raise SystemExit(f"{const_name} in {rel_path} is not a `&[(&str, &str)]` literal table")
    body = body.split("= &[", 1)[1].split("];", 1)[0]
    rows: list[str] = []
    for raw in body.splitlines():
        line = raw.strip()
        if not line or line.startswith("//"):
            continue
        match = re.fullmatch(r'\("(GET|POST|PUT|DELETE|PATCH)",\s*"([^"]+)"\),?', line)
        if match is None:
            raise SystemExit(
                f"{const_name} in {rel_path} has an unparsable row: {line!r} "
                f"(rows must be exactly (\"METHOD\", \"/literal/path\"),)"
            )
        rows.append(f"{match.group(1)} {match.group(2)}")
    if not rows:
        raise SystemExit(f"{const_name} in {rel_path} is empty: refusing to treat that as 'no routes'")
    return rows


def route_lines() -> list[str]:
    routes: set[str] = set()
    for rel_path, const_name in ROUTE_TABLES:
        routes.update(route_table(rel_path, const_name))
    return sorted(routes)


def literal_route_writes() -> list[str]:
    """Any literal-path .route("…") / .nest( / .merge( in the daemon HTTP tree."""
    offenders: list[str] = []
    for source in sorted(ROUTE_SOURCE_DIR.rglob("*.rs")):
        text = _strip_comments(source.read_text(encoding="utf-8"))
        for number, line in enumerate(text.splitlines(), start=1):
            if any(pattern.search(line) for pattern in BANNED_ROUTE_WRITES):
                offenders.append(f"{source.relative_to(ROOT).as_posix()}:{number}: {line.strip()}")
    return offenders


def facade_lines() -> list[str]:
    src = (ROOT / "crates" / "qaqh-client" / "src" / "lib.rs").read_text(encoding="utf-8")
    statements = re.findall(r"pub use[^;]+;", src, flags=re.S)
    return sorted(" ".join(statement.split()) for statement in statements)


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


def compare_snapshot(golden: Path, fresh: list[str], name: str) -> list[str]:
    path = golden / "snapshots" / f"{name}.txt"
    if not path.exists():
        return [f"snapshot baseline missing: {path.relative_to(ROOT).as_posix()} (--rebaseline to write it)"]
    expected = path.read_text(encoding="utf-8")
    if expected.strip() != "\n".join(fresh).strip():
        return [f"{name} surface differs from snapshot (see docs/api-surface snapshots/{name}.txt)"]
    return []


def rebaseline(golden: Path, routes: list[str], facade: list[str]) -> None:
    """Machine-write the snapshot baselines (never hand-edit them)."""
    (golden / "snapshots").mkdir(parents=True, exist_ok=True)
    for name, lines in (("routes", routes), ("facade", facade)):
        path = golden / "snapshots" / f"{name}.txt"
        path.write_text("\n".join(lines) + "\n", encoding="utf-8", newline="\n")
        print(f"rebaselined {path.relative_to(ROOT).as_posix()} ({len(lines)} lines)")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--against",
        default="docs/api-surface/v3.0",
        help="golden snapshot directory relative to the repository root",
    )
    parser.add_argument(
        "--rebaseline",
        action="store_true",
        help="rewrite snapshots/routes.txt and snapshots/facade.txt from the tables and "
             "the qaqh-client facade, then exit. On an LTS branch this is a refreeze "
             "(docs/adr/0007.md §决策 2), not a fix.",
    )
    parser.add_argument(
        "--skip-export",
        action="store_true",
        help="check only the route registry, the literal-route lint and the facade list "
             "(no cargo build; for a fast local look at the source-of-truth drift)",
    )
    options = parser.parse_args()

    golden = ROOT / options.against
    if not golden.is_dir():
        raise SystemExit(f"golden snapshot directory not found: {golden}")

    routes, facade = route_lines(), facade_lines()

    if options.rebaseline:
        rebaseline(golden, routes, facade)
        return 0

    problems: list[str] = []

    # 登记处 lint 先跑:它不看快照,只看"路由是否只能写在表里"。
    offenders = literal_route_writes()
    if offenders:
        problems.append(
            "routes must be declared in RINGING_V2_HTTP_ROUTES / CONTROL_PLANE_ROUTES, "
            "not with a literal path at a call site:"
        )
        problems.extend(f"  {offender}" for offender in offenders)
    problems.extend(compare_snapshot(golden, routes, "routes"))
    problems.extend(compare_snapshot(golden, facade, "facade"))

    if not options.skip_export:
        if tracked_dirty():
            print("refusing: tracked working-tree changes would make the export ambiguous", file=sys.stderr)
            return 2
        with tempfile.TemporaryDirectory(prefix="qaqh-lts-guard-") as temp:
            stage = Path(temp) / "export"
            export_fresh(stage)
            problems += compare_file_set(golden, stage)
            problems += compare_manifest(golden, stage)

    if problems:
        print("LTS API-surface guard: FAILED — reject this change on the LTS branch.", file=sys.stderr)
        for problem in problems:
            print(f"  - {problem}", file=sys.stderr)
        print("Freeze policy: docs/lts-v3.0-maintenance.md §1. Revert the implementation, never the snapshot.")
        print("Snapshots are machine-written: --rebaseline (on LTS that means a refreeze).")
        return 1
    print(f"LTS API surface unchanged (baseline {options.against}, {len(routes)} routes).")
    return 0


if __name__ == "__main__":
    sys.exit(main())
