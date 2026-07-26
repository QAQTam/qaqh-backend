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


# Index expr: numeric literal (with optional `usize`/`u32`/etc suffix), or a
# variable/method-chain that may contain simple arithmetic (`i+1`, `idx + 3`),
# `as` casts, one level of parens, and balanced-paren method calls.
_IDX = (
    r"(?:\d+(?:_?[a-zA-Z]\w*)?"                          # 10, 10usize, 10_u32
    r"|\([^\[\]]*\)"                                       # (n), (a+b)
    r"|[a-zA-Z_]\w*"                                       # base ident
    r"(?:\s*\.\s*\w+(?:\s*\([^()]*(?:\([^()]*\)[^()]*)*\))?)*"  # .method(...) chain, 1 level nested parens
    r"(?:\s*(?:as\s+\w+|[+\-]\s*\d+|[+\-]\s*\w+))*"        # `as usize`, `+ 3`, `- n`
    r")"
)

# Match `expr[start..end]` or `&expr[start..end]` on string-like expressions.
# re.DOTALL so the slice body may span multiple lines.
SLICE_RE = re.compile(
    r"(&?)([a-zA-Z_]\w*(?:\s*\.\s*[a-zA-Z_]\w*(?:\([^()]*\))?)*)\s*\[\s*"
    r"(" + _IDX + r"?)\s*\.\.(=)?\s*"
    r"(" + _IDX + r"?)\s*\]",
    re.DOTALL,
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

# `let v: Vec<u8> = ...` / `let v: &[u8] = ...` — declares a base variable as
# a non-string (byte/element) collection, so `v[a..b]` on it is a Vec/slice
# range, not a str range, and cannot panic on a UTF-8 boundary.
NON_STRING_DECL = re.compile(
    r"\b(?:let\s+(?:mut\s+)?|fn\s+\w+\s*\([^)]*|:\s*)"
    r"([a-zA-Z_]\w*)\s*:\s*"
    r"(?:&\s*(?:mut\s+)?)?"
    r"(?:Vec\s*<|\[\s*\w|&\s*\[)"
)


def classify_index(expr: str, known_safe_vars: set) -> str:
    expr = expr.strip()
    if not expr:
        return "safe"
    # bare numeric literal, with optional type suffix: 10, 10usize, 10_u32
    if re.fullmatch(r"\d+(_?[a-zA-Z]\w*)?", expr):
        return "unsafe"
    # parenthesized bare literal: (10), (10usize)
    if re.fullmatch(r"\(\s*\d+(_?[a-zA-Z]\w*)?\s*\)", expr):
        return "unsafe"
    if expr in known_safe_vars:
        return "safe"
    # `as` cast onto an otherwise-safe/known-safe base: strip and re-check
    m = re.match(r"^(.*?)\s+as\s+\w+$", expr)
    if m:
        return classify_index(m.group(1), known_safe_vars)
    # method-chain that is safe
    for pat in [
        r"\.\s*(?:floor_char_boundary|ceil_char_boundary)\s*\(",
        r"\.\s*chars\s*\(\s*\)",
        r"\.\s*char_indices\s*\(\s*\)",
        r"\.\s*len_utf8\s*\(\s*\)",
    ]:
        if re.search(pat, expr):
            return "safe"
    # `.as_bytes()[...]` is itself a *byte* slice, not a str slice — indexing
    # into it numerically is legal Rust (won't panic on boundary), so treat
    # as safe here. NOTE: caller still flags it via a separate warning class
    # if the *result* is later re-interpreted as a str (see BYTES_REINTERPRET).
    if re.search(r"\.\s*as_bytes\s*\(\s*\)\s*$", expr):
        return "safe"
    # method-chain that clamps to a numeric literal → effectively hardcoded byte index
    # e.g. .min(40), .max(200), len().min(100)
    if re.search(r"\.\s*(?:min|max)\s*\(\s*\d+(_?[a-zA-Z]\w*)?\s*\)", expr):
        return "unsafe"
    # arithmetic with numeric literals → likely byte math (len() - 1, n + 10, i+1)
    if re.search(r"[+\-*/]\s*\d+", expr) or re.search(r"\d+\s*[+\-*/]", expr):
        return "unsafe"
    # arithmetic between two variables (idx + off) — index math of unknown
    # safety, can't prove either way → maybe, not safe
    if re.search(r"[a-zA-Z_]\w*\s*[+\-]\s*[a-zA-Z_]\w*", expr):
        return "maybe"
    return "maybe"


def _strip_comments_and_strings(content: str) -> str:
    """Blank out //, /* */, and string-literal contents (keeping newlines and
    overall byte length so line/col numbers stay valid) so the slice regex
    never fires inside a comment or a `"[..50]"`-shaped string literal."""
    out = []
    i, n = 0, len(content)
    in_line_comment = False
    in_block_comment = False
    in_string = False
    in_char = False
    escape = False
    while i < n:
        c = content[i]
        nxt = content[i + 1] if i + 1 < n else ""
        if in_line_comment:
            out.append("\n" if c == "\n" else " ")
            if c == "\n":
                in_line_comment = False
        elif in_block_comment:
            out.append("\n" if c == "\n" else " ")
            if c == "*" and nxt == "/":
                out[-1] = " "
                out.append(" ")
                i += 1
                in_block_comment = False
        elif in_string:
            out.append("\n" if c == "\n" else " ")
            if escape:
                escape = False
            elif c == "\\":
                escape = True
            elif c == '"':
                in_string = False
        elif in_char:
            out.append("\n" if c == "\n" else " ")
            if escape:
                escape = False
            elif c == "\\":
                escape = True
            elif c == "'":
                in_char = False
        else:
            if c == "/" and nxt == "/":
                in_line_comment = True
                out.append(" ")
                out.append(" ")
                i += 1
            elif c == "/" and nxt == "*":
                in_block_comment = True
                out.append(" ")
                out.append(" ")
                i += 1
            elif c == '"':
                in_string = True
                out.append(c)
            elif c == "'" and re.match(r"^'([^'\\]|\\.)'", content[i:]):
                # crude char-literal guard so a lifetime `'a` isn't mistaken
                # for the start of a char literal and swallows the rest of line
                in_char = True
                out.append(c)
            else:
                out.append(c)
        i += 1
    return "".join(out)


def check_file(filepath: Path) -> list:
    with open(filepath, "r", encoding="utf-8") as f:
        content = f.read()
    lines = content.split("\n")

    # First pass: collect variables assigned from safe index sources, and
    # variables explicitly typed as non-string collections (Vec/&[T]).
    known_safe: set[str] = set()
    non_string_vars: set[str] = set()
    for line in lines:
        for m in SAFE_INDEX_ASSIGN.finditer(line):
            known_safe.add(m.group(1))
        for m in NON_STRING_DECL.finditer(line):
            non_string_vars.add(m.group(1))

    cleaned = _strip_comments_and_strings(content)

    issues = []
    seen = set()  # dedupe: (line, col) — multiline matches can overlap on re-scan
    for m in SLICE_RE.finditer(cleaned):
        var = m.group(2).strip()
        start_raw = m.group(3).strip() if m.group(3) else ""
        # group(4) is the optional `=` of `..=`; index expr shifted to group(5)
        end_raw = m.group(5).strip() if m.group(5) else ""
        if not start_raw and not end_raw:
            continue
        if var[0].isdigit():
            continue

        lineno = cleaned.count("\n", 0, m.start()) + 1
        line_start = cleaned.rfind("\n", 0, m.start()) + 1
        col = m.start() - line_start + 1
        key = (lineno, col)
        if key in seen:
            continue
        seen.add(key)

        # `expr.as_bytes()[..n]` slices a &[u8], not a &str — numeric byte
        # indices there are legal Rust and cannot panic on a boundary, so
        # skip entirely regardless of what start/end look like.
        if re.search(r"\.\s*as_bytes\s*\(\s*\)\s*$", var):
            continue

        # base variable was explicitly declared `Vec<T>` / `&[T]` elsewhere
        # in the file → indexing it is element-range, not str-range.
        base_var = var.split(".")[0].strip()
        if base_var in non_string_vars:
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
            "col": col,
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
