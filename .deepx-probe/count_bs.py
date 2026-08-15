p = 'crates/deepx-workspace/src/file_edit_v2.rs'
with open(p, 'r', encoding='utf-8', newline='') as f:
    lines = f.readlines()

from collections import Counter

for i, ln in enumerate(lines):
    if 'replace_inline' in ln and 'sed s///' in ln and 'kind' in ln and i + 1 != 2250:
        runs = []
        cur = 0
        for ch in ln:
            if ch == '\\':
                cur += 1
            else:
                if cur:
                    runs.append(cur)
                cur = 0
        if cur:
            runs.append(cur)
        print('LINE', i + 1, 'backslash-run lengths:', Counter(runs))
        j = ln.find('kind')
        print('around kind:', repr(ln[j - 10:j + 50]))
        break
