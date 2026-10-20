#!/usr/bin/env python3
"""Read-only target DAG, Cargo dependency and documentation file-link checks."""

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path
from urllib.parse import unquote, urlsplit


ROOT = Path(__file__).resolve().parents[2]
MATRIX = Path("new-tech/docs/v2-reset/dependencies.json")


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def load_matrix(path):
    return json.loads(path.read_text(encoding="utf-8-sig"), object_pairs_hook=unique_object)


def string_list(value, label, errors):
    if not isinstance(value, list) or any(
        not isinstance(item, str) or not item.strip() for item in value
    ):
        errors.append(f"{label}: expected an array of nonempty strings")
        return []
    if len(set(value)) != len(value):
        errors.append(f"{label}: duplicate entries")
    return value


def validate_matrix(data):
    errors = []
    if not isinstance(data, dict):
        return ["matrix: expected an object"]
    if type(data.get("schema_version")) is not int or data["schema_version"] != 1:
        errors.append("schema_version: expected 1")
    crates = data.get("crates")
    if not isinstance(crates, dict) or not crates:
        return errors + ["crates: expected a nonempty object"]
    graph = {}
    for name, rule in crates.items():
        if not isinstance(rule, dict):
            errors.append(f"{name}: expected an object")
            continue
        if not isinstance(rule.get("owner"), str) or not rule["owner"].strip():
            errors.append(f"{name}: missing owner")
        string_list(rule.get("modules"), f"{name}.modules", errors)
        allowed = string_list(
            rule.get("allowed_local_dependencies"), f"{name}.allowed_local_dependencies", errors
        )
        string_list(
            rule.get("forbidden_external_dependencies"),
            f"{name}.forbidden_external_dependencies", errors,
        )
        graph[name] = allowed
        for dependency in allowed:
            if dependency not in crates:
                errors.append(f"{name}: unknown target dependency {dependency}")

    visiting, visited = [], set()

    def visit(name):
        if name in visiting:
            cycle = visiting[visiting.index(name):] + [name]
            errors.append("dependency cycle: " + " -> ".join(cycle))
            return
        if name in visited:
            return
        visiting.append(name)
        for dependency in graph.get(name, []):
            if dependency in graph:
                visit(dependency)
        visiting.pop()
        visited.add(name)

    for name in sorted(graph):
        visit(name)
    mappings = data.get("current_to_target")
    if not isinstance(mappings, dict):
        errors.append("current_to_target: expected an object")
    else:
        for source, targets in mappings.items():
            targets = string_list(targets, f"current_to_target.{source}", errors)
            if not targets:
                errors.append(f"current_to_target.{source}: missing destination")
            for target in targets:
                if target not in crates:
                    errors.append(f"current_to_target.{source}: unknown target {target}")
    pending = data.get("pending_integrations", {})
    if not isinstance(pending, dict):
        errors.append("pending_integrations: expected an object")
    else:
        for name, proposal in pending.items():
            if not isinstance(proposal, dict) or proposal.get("included_in_target_graph") is not False:
                errors.append(f"pending {name}: must explicitly remain outside the confirmed graph")
            if name in crates:
                errors.append(f"pending {name}: also present in the confirmed graph")
    return errors


def cargo_workspace(root):
    # --locked/--offline forbid lockfile updates and dependency downloads.
    result = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked", "--offline"],
        cwd=root, capture_output=True, text=True, encoding="utf-8", timeout=60,
    )
    if result.returncode:
        raise ValueError("cargo metadata failed: " + result.stderr.strip())
    return json.loads(result.stdout)


def compare_workspace(data, metadata):
    members = set(metadata["workspace_members"])
    packages = [p for p in metadata["packages"] if p["id"] in members]
    actual = {p["name"] for p in packages}
    targets = data["crates"]
    errors = [f"missing target crate: {name}" for name in sorted(set(targets) - actual)]
    errors.extend(f"non-target workspace crate: {name}" for name in sorted(actual - set(targets)))
    # All declared direct deps are checked, including optional/build/dev and every
    # target condition. Cargo resolves package renames and workspace inheritance.
    for package in sorted(packages, key=lambda p: p["name"]):
        name = package["name"]
        if name not in targets:
            continue
        rule = targets[name]
        for dependency in package["dependencies"]:
            dep_name = dependency["name"]
            kind = dependency.get("kind") or "normal"
            condition = dependency.get("target") or "all targets"
            label = f"{name} -> {dep_name} [{kind}; {condition}]"
            if dependency.get("path") is not None:
                if dep_name not in rule["allowed_local_dependencies"]:
                    errors.append(f"disallowed local dependency: {label}")
            elif dep_name in rule["forbidden_external_dependencies"]:
                errors.append(f"forbidden direct external dependency: {label}")
    return errors, len(packages)


def check_links(root):
    errors, checked = [], 0
    docs = root / "new-tech/docs"
    if not docs.is_dir():
        return ["new-tech/docs: directory missing"], 0
    pattern = re.compile(r"\[[^\]\n]*\]\(([^)\n]+)\)")
    for path in sorted(docs.rglob("*.md")):
        fence = None
        for line_number, line in enumerate(path.read_text(encoding="utf-8-sig").splitlines(), 1):
            marker = re.match(r"^\s*(`{3,}|~{3,})", line)
            if marker:
                token = marker.group(1)
                if fence is None:
                    fence = token
                elif token[0] == fence[0] and len(token) >= len(fence):
                    fence = None
                continue
            if fence is not None:
                continue
            for match in pattern.finditer(line):
                reference = match.group(1).strip().strip("<>")
                parsed = urlsplit(reference)
                if parsed.scheme or parsed.netloc or not parsed.path:
                    continue
                checked += 1
                target = (path.parent / unquote(parsed.path)).resolve()
                label = f"{path.relative_to(root).as_posix()}:{line_number}: {reference}"
                if not target.is_relative_to(root):
                    errors.append("link outside repository: " + label)
                elif not target.exists():
                    errors.append("missing link target: " + label)
    return errors, checked


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("validate", "check", "links"))
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--matrix", type=Path, default=MATRIX)
    args = parser.parse_args(argv)
    root = args.root.resolve()
    try:
        if args.command == "links":
            errors, count = check_links(root)
            summary = f"links: {count} local file links checked"
        else:
            data = load_matrix(root / args.matrix)
            errors = validate_matrix(data)
            if errors:
                summary = "target matrix invalid"
            elif args.command == "validate":
                summary = f"validate: {len(data['crates'])} target crates, DAG and mappings valid"
            else:
                errors, count = compare_workspace(data, cargo_workspace(root))
                summary = f"check: {count} actual / {len(data['crates'])} target crates"
    except (OSError, ValueError, KeyError, TypeError, subprocess.TimeoutExpired) as error:
        print(f"audit input/tool error: {error}", file=sys.stderr)
        return 2
    print(summary)
    for error in errors:
        print("  " + error)
    print(f"{'FAIL' if errors else 'PASS'}: {len(errors)} findings")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
