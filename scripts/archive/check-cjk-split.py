"""
check-cjk-split.py — Scan Rust source for unsafe byte-level string slicing
that could panic on multi-byte CJK (Chinese/Japanese/Korean) text.

Usage:
    python scripts/check-cjk-split.py [path]            # default: show UNSAFE only
    python scripts/check-cjk-split.py [path] --all      # show MAYBE too (noisier)
    python scripts/check-cjk-split.py [path] --ci       # GitHub Actions annotations
    python scripts/check-cjk-split.py [path] --json     # JSON output (all levels)
    python scripts/check-cjk-split.py [path] --check    # exit 1 if ANY UNSAFE found

Classification:
    UNSAFE  – raw numeric literal slice `s[..50]`, `s[1..]` -> almost certainly a bug
    MAYBE   – variable-indexed slice `s[..n]` -> safe if n came from find() on ASCII
"""

import re
import sys
import json
from pathlib import Path
from collections import defaultdict


def find_rust_files(root: Path):
    for path in root.rglob("*.rs"):
        if "target" in path.parts:
            continue
        yield path


# Match `expr[start..end]` or `&expr[start..end]` on string-like expressions
SLICE_RE = re.compile(
    r"""(&?)([a-zA-Z_]\w*(?:\s*\.\s*[a-zA-Z_]\w*)*)\s*\[\s*"""
    r"""((?:\d+|[a-zA-Z_]\w*(?:\s*\.\s*\w+)*\s*(?:\([^)]*\))?)?)\s*\.\.\s*"""
    r"""((?:\d+|[a-zA-Z_]\w*(?:\s*\.\s*\w+)*\s*(?:\([^)]*\))?)?)\s*\]"""
)

# Lines that produce "safe" indices (assign from find/split on ASCII)
SAFE_INDEX_ASSIGN = re.compile(
    r"\b([a-zA-Z_]\w*)\s*=\s*(?:"
    r"[a-zA-Z_]\w*\s*\.\s*"
    r"(?:find|rfind|split_once|split)\s*\([^)]*\)|"
    r"[a-zA-Z_]\w*\s*\.\s*"
    r"floor_char_boundary\s*\("
    r")"
)


def classify_index(expr: str, known_safe_vars: set) -> str:
    expr = expr.strip()
    if not expr:
        return "safe"
    if re.fullmatch(r"\d+", expr):
        return "unsafe"
    if expr in known_safe_vars:
        return "safe"
    # method-chain that is safe
    for pat in [
        r"\.\s*(?:floor_char_boundary|ceil_char_boundary)\s*\(",
        r"\.\s*chars\s*\(\s*\)",
        r"\.\s*char_indices\s*\(\s*\)",
        r"\.\s*len_utf8\s*\(\s*\)",
        r"\.\s*as_bytes\s*\(\s*\)",
    ]:
        if re.search(pat, expr):
            return "safe"
    # method-chain that clamps to a numeric literal → effectively hardcoded byte index
    # e.g. .min(40), .max(200), len().min(100)
    if re.search(r"\.\s*(?:min|max)\s*\(\s*\d+\s*\)", expr):
        return "unsafe"
    # arithmetic with numeric literals → likely byte math (len() - 1, n + 10)
    if re.search(r"[+\-*/]\s*\d+", expr) or re.search(r"\d+\s*[+\-*/]", expr):
        return "unsafe"
    return "maybe"


def check_file(filepath: Path) -> list:
    with open(filepath, "r", encoding="utf-8") as f:
        content = f.read()
    lines = content.split("\n")

    # First pass: collect variables assigned from safe index sources
    known_safe: set[str] = set()
    for line in lines:
        for m in SAFE_INDEX_ASSIGN.finditer(line):
            known_safe.add(m.group(1))

    issues = []
    in_block = False
    for lineno, line in enumerate(lines, 1):
        s = line.strip()
        if s.startswith("//") or s.startswith("#"):
            continue
        if s.startswith("/*"):
            in_block = True
        if in_block:
            if "*/" in s:
                in_block = False
            continue

        for m in SLICE_RE.finditer(line):
            var = m.group(2).strip()
            start_raw = m.group(3).strip() if m.group(3) else ""
            end_raw = m.group(4).strip() if m.group(4) else ""
            if not start_raw and not end_raw:
                continue
            if var[0].isdigit():
                continue

            s_cls = classify_index(start_raw, known_safe)
            e_cls = classify_index(end_raw, known_safe)
            if s_cls == "safe" and e_cls == "safe":
                continue

            severity = "UNSAFE" if (s_cls == "unsafe" or e_cls == "unsafe") else "MAYBE"
            prefix = "&" if m.group(1) else ""
            range_str = f"{start_raw}..{end_raw}" if start_raw else f"..{end_raw}"

            issues.append({
                "file": str(filepath),
                "line": lineno,
                "col": m.start() + 1,
                "severity": severity,
                "code": f"{prefix}{var}[{range_str}]",
                "start_raw": start_raw,
                "end_raw": end_raw,
            })
    return issues


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    flags = set(a for a in sys.argv[1:] if a.startswith("--"))
    root = Path(args[0]) if args else Path.cwd()
    show_all = "--all" in flags
    ci_mode = "--ci" in flags
    json_mode = "--json" in flags
    check_mode = "--check" in flags

    all_issues = []
    for fpath in find_rust_files(root):
        all_issues.extend(check_file(fpath))

    unsafe = [i for i in all_issues if i["severity"] == "UNSAFE"]
    maybes = [i for i in all_issues if i["severity"] == "MAYBE"]

    if json_mode:
        print(json.dumps(all_issues, indent=2, ensure_ascii=False))
    elif ci_mode:
        for i in unsafe + maybes:
            sev = "error" if i["severity"] == "UNSAFE" else "warning"
            print(f"::{sev} file={i['file']},line={i['line']},col={i['col']}::{i['code']}")
    else:
        if not unsafe and not (show_all and maybes):
            print("[OK] No CJK-unsafe string slicing detected.")
            return 0

        if unsafe:
            print(f"\nUNSAFE ({len(unsafe)}) -- raw byte offsets that WILL panic on CJK:\n")
            for i in unsafe:
                print(f"  {i['file']}:{i['line']}:{i['col']}  {i['code']}")

        if show_all and maybes:
            by_file = defaultdict(list)
            for i in maybes:
                by_file[i["file"]].append(i)
            print(f"\nMAYBE ({len(maybes)}) -- variable-indexed slices, review recommended:\n")
            for fpath, items in sorted(by_file.items()):
                print(f"  {fpath}:")
                for i in items:
                    print(f"    L{i['line']:>4}:{i['col']:<4}  {i['code']}")
        elif maybes and not show_all:
            by_file = defaultdict(list)
            for i in maybes:
                by_file[i["file"]].append(i)
            print(f"\nMAYBE ({len(maybes)}) -- use --all to see details; per-file summary:")
            for fpath, items in sorted(by_file.items()):
                print(f"  {fpath}: {len(items)} slice(s)")

        print(
            "\nTips:"
            "\n  Unsafe: use `&text[..text.floor_char_boundary(n)]` or `text.chars().take(n).collect()`"
            "\n  Maybe:  likely safe if index came from find(\"ascii\") or split on ASCII"
        )

    if check_mode and unsafe:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
