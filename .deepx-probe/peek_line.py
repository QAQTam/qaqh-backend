p = 'crates/deepx-workspace/src/file_edit_v2.rs'
with open(p, 'r', encoding='utf-8', newline='') as f:
    lines = f.readlines()
for i, ln in enumerate(lines):
    if 'kind' in ln and 'replace_inline' in ln and 'anchor' in ln and i + 1 != 96:
        print('LINE', i + 1, 'len', len(ln))
        j = ln.find('replace_inline')
        print('SEG:', repr(ln[j-60:j+380]))
        break
