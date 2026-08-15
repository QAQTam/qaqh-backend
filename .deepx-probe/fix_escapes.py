# Fix over-escaped backslashes in file_edit_v2.rs (char-count based, no ambiguity):
# 1. description line: 8 backslashes + quote -> 2 backslashes + quote (JSON string literal needs \" -> \\")
# 2. R23 test: r"let (\\w+) = (\\d+)" -> r"let (\w+) = (\d+)"
p = 'crates/deepx-workspace/src/file_edit_v2.rs'
with open(p, 'r', encoding='utf-8', newline='') as f:
    lines = f.readlines()

BS = '\\'  # single backslash char
fixed_any = False

for i, ln in enumerate(lines):
    if 'sed s/// semantics' in ln and 'kind' in ln:
        # 8 backslashes + quote -> 2 backslashes + quote (only in the replace_inline segment;
        # the rest of the line already uses 2-backslash + quote which must not be touched)
        fixed = ln.replace(BS * 8 + '"', BS * 2 + '"')
        if fixed != ln:
            lines[i] = fixed
            print('fixed desc line', i + 1)
            fixed_any = True
        break

for i, ln in enumerate(lines):
    if 'let (' in ln and 'w+)' in ln and 'regex_with_captures' in ln:
        fixed = ln.replace(BS * 4 + 'w', BS * 2 + 'w').replace(BS * 4 + 'd', BS * 2 + 'd')
        if fixed != ln:
            lines[i] = fixed
            print('fixed regex line', i + 1)
            fixed_any = True
        break

with open(p, 'w', encoding='utf-8', newline='') as f:
    f.writelines(lines)
print('DONE' if fixed_any else 'NOTHING FIXED')
