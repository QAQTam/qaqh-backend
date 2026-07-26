#!/usr/bin/env python3
"""
check-cjk-split.py — Scan Rust source for unsafe byte-level string slicing
that could panic on multi-byte CJK (Chinese/Japanese/Korean) text.
"""

import re
import sys
import json
from pathlib import Path
from collections import defaultdict

def find_rust_files(root: Path):
    if root.is_file():
        if root.suffix == ".rs":
            yield root
        return
    for path in root.rglob("*.rs"):
        if "target" in path.parts:
            continue
        yield path

# 索引表达式正则（支持跨行 \s*）
_IDX = (
    r"(?:\d+(?:_?[a-zA-Z]\w*)?"                          
    r"|\([^\[\]]*\)"                                       
    r"|[a-zA-Z_]\w*"                                       
    r"(?:\s*\.\s*\w+(?:\s*\([^()]*(?:\([^()]*\)[^()]*)*\))?)*"  
    r"(?:\s*(?:as\s+\w+|[+\-]\s*\d+|[+\-]\s*\w+))*"        
    r")"
)

SLICE_RE = re.compile(
    r"(&?)([a-zA-Z_]\w*(?:\s*\.\s*[a-zA-Z_]\w*(?:\([^()]*\))?)*)\s*\[\s*"
    r"(" + _IDX + r"?)\s*\.\.(=)?\s*"
    r"(" + _IDX + r"?)\s*\]",
    re.DOTALL,
)

# 增强版：支持跨行追踪安全赋值
SAFE_INDEX_ASSIGN = re.compile(
    r"\b([a-zA-Z_]\w*)\s*=\s*(?:"
    r"[a-zA-Z_]\w*(?:\s*\.\s*[a-zA-Z_]\w*(?:\([^()]*\))?)*\s*\.\s*(?:find|rfind|split_once|split)\s*\([^)]*\)"
    r"|[a-zA-Z_]\w*(?:\s*\.\s*[a-zA-Z_]\w*(?:\([^()]*\))?)*\s*\.\s*floor_char_boundary\s*\([^)]*\)"
    r")",
    re.DOTALL
)

# 增强版：支持隐式/显式非字符串集合声明（新增 vec! 及字节流字面量识别）
NON_STRING_DECL = re.compile(
    r"(?:\b(?:let\s+(?:mut\s+)?|fn\s+\w+\s*\()|:\s*)"
    r"([a-zA-Z_]\w*)\s*(?:"
    r":\s*(?:&\s*(?:mut\s+)?)?(?:Vec\s*<|\[\s*\w|&\s*\[)"
    r"|\s*=\s*vec\s*!|b\".*?\"|b'.*?'"
    r")",
    re.DOTALL
)

# 常见字节流/缓冲区命名启发式规则（规避结构体字段无法推导类型的误报）
BYTE_HEURISTICS = [
    r"\.as_bytes\s*\(\s*\)",
    r"\.as_mut_bytes\s*\(\s*\)",
    r"\bbuf\b", r"\bbuffer\b", r"\bbytes\b", r"\bpayload\b", r"\bdata\b", 
    r"_buf\b", r"_bytes\b", r"_u8\b", r"\braw\b"
]

def _clean_rust_code(content: str) -> str:
    """
    状态机清洗函数：
    1. 完美擦除 // 与 /* */（支持 Rust 特有的嵌套块注释 /* /* */ */）
    2. 擦除常规字符串 "..." 与原生字符串 r"...", r#"..."#, r##"..."##
    3. 保留换行符与字符绝对位置，确保行列定位完全精准。
    """
    out = []
    i, n = 0, len(content)
    
    while i < n:
        # 1. 处理嵌套块注释
        if i + 1 < n and content[i:i+2] == "/*":
            depth = 1
            out.extend([" ", " "])
            i += 2
            while i < n and depth > 0:
                if i + 1 < n and content[i:i+2] == "/*":
                    depth += 1
                    out.extend([" ", " "])
                    i += 2
                elif i + 1 < n and content[i:i+2] == "*/":
                    depth -= 1
                    out.extend([" ", " "])
                    i += 2
                else:
                    out.append("\n" if content[i] == "\n" else " ")
                    i += 1
            continue
        
        # 2. 处理行注释
        if i + 1 < n and content[i:i+2] == "//":
            out.extend([" ", " "])
            i += 2
            while i < n and content[i] != "\n":
                out.append(" ")
                i += 1
            continue
        
        # 3. 处理 Rust 原生字符串 (Raw Strings)
        if content[i] == 'r' and i + 1 < n and (content[i+1] == '"' or content[i+1] == '#'):
            j = i + 1
            pounds = 0
            while j < n and content[j] == '#':
                pounds += 1
                j += 1
            if j < n and content[j] == '"':
                start_idx = i
                i = j + 1
                out.extend([" "] * (i - start_idx))
                closing_pattern = '"' + '#' * pounds
                while i < n:
                    if content[i:i+len(closing_pattern)] == closing_pattern:
                        out.extend([" "] * len(closing_pattern))
                        i += len(closing_pattern)
                        break
                    else:
                        out.append("\n" if content[i] == "\n" else " ")
                        i += 1
                continue

        # 4. 处理普通字符串
        if content[i] == '"':
            out.append('"')
            i += 1
            while i < n:
                if content[i] == '"':
                    out.append('"')
                    i += 1
                    break
                elif content[i] == '\\':
                    out.extend([" ", " "])
                    i += 2
                else:
                    out.append("\n" if content[i] == "\n" else " ")
                    i += 1
            continue

        # 5. 处理字符字面量（避免生命周期标记如 'a 干扰）
        if content[i] == "'":
            match_char = re.match(r"^'([^'\\]|\\.)'", content[i:])
            if match_char:
                length = match_char.end()
                out.extend([" "] * length)
                i += length
                continue

        out.append(content[i])
        i += 1
        
    return "".join(out)

def classify_index(expr: str, known_safe_vars: set) -> str:
    expr = expr.strip()
    if not expr:
        return "safe"
    if re.fullmatch(r"\d+(_?[a-zA-Z]\w*)?", expr):
        return "unsafe"
    if re.fullmatch(r"\(\s*\d+(_?[a-zA-Z]\w*)?\s*\)", expr):
        return "unsafe"
    if expr in known_safe_vars:
        return "safe"
    
    m = re.match(r"^(.*?)\s+as\s+\w+$", expr)
    if m:
        return classify_index(m.group(1), known_safe_vars)
        
    for pat in [
        r"\.\s*(?:floor_char_boundary|ceil_char_boundary)\s*\(",
        r"\.\s*chars\s*\(\s*\)",
        r"\.\s*char_indices\s*\(\s*\)",
        r"\.\s*len_utf8\s*\(\s*\)",
    ]:
        if re.search(pat, expr):
            return "safe"
            
    if re.search(r"\.\s*as_bytes\s*\(\s*\)\s*$", expr):
        return "safe"
    if re.search(r"\.\s*(?:min|max)\s*\(\s*\d+(_?[a-zA-Z]\w*)?\s*\)", expr):
        return "unsafe"
    if re.search(r"[+\-*/]\s*\d+", expr) or re.search(r"\d+\s*[+\-*/]", expr):
        return "unsafe"
    if re.search(r"[a-zA-Z_]\w*\s*[+\-]\s*[a-zA-Z_]\w*", expr):
        return "maybe"
    return "maybe"

def check_file(filepath: Path) -> list:
    with open(filepath, "r", encoding="utf-8") as f:
        content = f.read()

    cleaned = _clean_rust_code(content)
    
    # 核心修复：基于花括号位置计算局部作用域界限
    stack = []
    block_map = {}  # 记录每个 '{' 对应的 '}' 绝对位置
    for idx, char in enumerate(cleaned):
        if char == '{':
            stack.append(idx)
        elif char == '}':
            if stack:
                start_brace = stack.pop()
                block_map[start_brace] = idx

    def get_scope_end(pos):
        """获取当前位置所在闭合代码块的结束位置"""
        innermost_end = len(cleaned)
        innermost_start = -1
        for start_brace, end_brace in block_map.items():
            if start_brace <= pos <= end_brace:
                if start_brace > innermost_start:
                    innermost_start = start_brace
                    innermost_end = end_brace
        return innermost_end

    # 变量作用域存储库: (var_name, start_pos, end_pos, type)
    variable_registry = []

    # 全文跨行扫描安全变量赋值
    for m in SAFE_INDEX_ASSIGN.finditer(cleaned):
        vname = m.group(1).strip()
        variable_registry.append((vname, m.start(), get_scope_end(m.start()), "safe"))

    # 全文跨行扫描非字符串声明
    for m in NON_STRING_DECL.finditer(cleaned):
        vname = m.group(1).strip()
        variable_registry.append((vname, m.start(), get_scope_end(m.start()), "non_string"))

    issues = []
    seen = set()
    
    for m in SLICE_RE.finditer(cleaned):
        var = m.group(2).strip()
        start_raw = m.group(3).strip() if m.group(3) else ""
        end_raw = m.group(5).strip() if m.group(5) else ""
        if not start_raw and not end_raw:
            continue
        if var[0].isdigit():
            continue

        slice_pos = m.start()
        lineno = cleaned.count("\n", 0, slice_pos) + 1
        line_start = cleaned.rfind("\n", 0, slice_pos) + 1
        col = slice_pos - line_start + 1
        
        key = (lineno, col)
        if key in seen:
            continue
        seen.add(key)

        # 引入启发式规则：跳过显式带有 byte/buf 等命名的结构体属性字段
        if any(re.search(h, var, re.IGNORECASE) for h in BYTE_HEURISTICS):
            continue

        # 动态提取当前切片位置有效的变量快照（彻底解决全局污染 Bug）
        active_safe = set()
        active_non_string = set()
        for vname, s_pos, e_pos, vtype in variable_registry:
            if s_pos <= slice_pos <= e_pos:
                if vtype == "safe":
                    active_safe.add(vname)
                else:
                    active_non_string.add(vname)

        base_var = var.split(".")[0].strip()
        if base_var in active_non_string:
            continue

        s_cls = classify_index(start_raw, active_safe)
        e_cls = classify_index(end_raw, active_safe)
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

    if check_mode and unsafe:
        return 1
    return 0

if __name__ == "__main__":
    sys.exit(main())