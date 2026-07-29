#!/usr/bin/env python3
"""输出 DeepX 的 Node.js / Cargo 依赖审计摘要，避免大段 CLI 输出污染上下文。"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import tomllib
import urllib.error
import urllib.parse
import urllib.request
from collections import defaultdict
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
DESKTOP = ROOT / "apps" / "desktop"
USER_AGENT = "deepx-dependency-audit/1.0"


def http_json(url: str) -> dict[str, Any]:
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)


def npm_latest(name: str) -> str | None:
    encoded = urllib.parse.quote(name, safe="")
    try:
        data = http_json(f"https://registry.npmjs.org/{encoded}")
        return data.get("dist-tags", {}).get("latest")
    except (OSError, urllib.error.URLError, json.JSONDecodeError):
        return None


def crate_latest(name: str) -> str | None:
    encoded = urllib.parse.quote(name, safe="")
    try:
        data = http_json(f"https://crates.io/api/v1/crates/{encoded}")
        crate = data.get("crate", {})
        return crate.get("max_stable_version") or crate.get("newest_version")
    except (OSError, urllib.error.URLError, json.JSONDecodeError):
        return None


def parse_pnpm_lock(path: Path) -> dict[str, list[str]]:
    versions: dict[str, set[str]] = defaultdict(set)
    in_packages = False
    package_line = re.compile(r"^  (['\"]?)(.+)\1:$")
    version_start = re.compile(r"^(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)")

    for line in path.read_text(encoding="utf-8").splitlines():
        if line == "packages:":
            in_packages = True
            continue
        if in_packages and line and not line.startswith(" "):
            break
        if not in_packages:
            continue
        match = package_line.match(line)
        if not match:
            continue
        key = match.group(2)
        split_at = key.rfind("@")
        if split_at <= 0:
            continue
        name, raw_version = key[:split_at], key[split_at + 1 :]
        version_match = version_start.match(raw_version)
        if version_match:
            versions[name].add(version_match.group(1))

    return {name: sorted(items) for name, items in sorted(versions.items())}


def node_audit(with_latest: bool) -> dict[str, Any]:
    package = json.loads((DESKTOP / "package.json").read_text(encoding="utf-8"))
    direct: list[dict[str, Any]] = []
    for group in ("dependencies", "devDependencies", "peerDependencies", "optionalDependencies"):
        for name, spec in sorted(package.get(group, {}).items()):
            direct.append(
                {
                    "name": name,
                    "group": group,
                    "declared": spec,
                    "latest": npm_latest(name) if with_latest else None,
                }
            )

    locked = parse_pnpm_lock(DESKTOP / "pnpm-lock.yaml")
    duplicates = [
        {"name": name, "versions": versions}
        for name, versions in locked.items()
        if len(versions) > 1
    ]
    return {
        "package_manager": package.get("packageManager"),
        "direct_count": len(direct),
        "locked_package_count": len(locked),
        "direct": direct,
        "locked_duplicates": duplicates,
    }


def cargo_metadata() -> dict[str, Any]:
    result = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
        encoding="utf-8",
    )
    return json.loads(result.stdout)


def cargo_audit(with_latest: bool) -> dict[str, Any]:
    metadata = cargo_metadata()
    workspace_ids = set(metadata["workspace_members"])
    packages = [p for p in metadata["packages"] if p["id"] in workspace_ids]
    direct_by_name: dict[str, dict[str, Any]] = {}

    for package in packages:
        manifest = str(Path(package["manifest_path"]).relative_to(ROOT)).replace("\\", "/")
        for dependency in package["dependencies"]:
            if dependency.get("source") is None:
                continue
            name = dependency["name"]
            entry = direct_by_name.setdefault(
                name,
                {"name": name, "requirements": set(), "manifests": set(), "latest": None},
            )
            entry["requirements"].add(dependency["req"])
            entry["manifests"].add(manifest)

    direct: list[dict[str, Any]] = []
    for name, entry in sorted(direct_by_name.items()):
        direct.append(
            {
                "name": name,
                "requirements": sorted(entry["requirements"]),
                "manifests": sorted(entry["manifests"]),
                "latest": crate_latest(name) if with_latest else None,
            }
        )

    lock_data = tomllib.loads((ROOT / "Cargo.lock").read_text(encoding="utf-8"))
    locked: dict[str, set[str]] = defaultdict(set)
    for package in lock_data.get("package", []):
        if package.get("source", "").startswith("registry+"):
            locked[package["name"]].add(package["version"])
    duplicates = [
        {"name": name, "versions": sorted(versions)}
        for name, versions in sorted(locked.items())
        if len(versions) > 1
    ]
    requirement_conflicts = [
        {"name": item["name"], "requirements": item["requirements"]}
        for item in direct
        if len(item["requirements"]) > 1
    ]
    return {
        "workspace_package_count": len(packages),
        "direct_registry_count": len(direct),
        "locked_registry_count": len(locked),
        "direct": direct,
        "direct_requirement_conflicts": requirement_conflicts,
        "locked_duplicates": duplicates,
    }


def compact(report: dict[str, Any]) -> dict[str, Any]:
    node = report["node"]
    cargo = report["cargo"]
    return {
        "node": {
            "package_manager": node["package_manager"],
            "direct_count": node["direct_count"],
            "locked_package_count": node["locked_package_count"],
            "outdated_direct": [
                item
                for item in node["direct"]
                if item["latest"] and item["latest"] not in item["declared"]
            ],
            "locked_duplicate_count": len(node["locked_duplicates"]),
            "locked_duplicate_names": [item["name"] for item in node["locked_duplicates"]],
        },
        "cargo": {
            "workspace_package_count": cargo["workspace_package_count"],
            "direct_registry_count": cargo["direct_registry_count"],
            "locked_registry_count": cargo["locked_registry_count"],
            "direct_requirement_conflicts": cargo["direct_requirement_conflicts"],
            "locked_duplicate_count": len(cargo["locked_duplicates"]),
            "locked_duplicate_names": [item["name"] for item in cargo["locked_duplicates"]],
            "outdated_direct": [
                item
                for item in cargo["direct"]
                if item["latest"]
                and all(item["latest"] not in requirement for requirement in item["requirements"])
            ],
        },
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--no-latest", action="store_true", help="不访问 npm/crates.io")
    parser.add_argument("--full", action="store_true", help="输出完整直接依赖列表")
    parser.add_argument("--output", type=Path, help="同时写入 JSON 文件")
    args = parser.parse_args()

    report = {
        "node": node_audit(not args.no_latest),
        "cargo": cargo_audit(not args.no_latest),
    }
    output = report if args.full else compact(report)
    text = json.dumps(output, ensure_ascii=False, indent=2)
    if args.output:
        args.output.write_text(text + "\n", encoding="utf-8")
    print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
