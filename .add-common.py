import sys
# Insert `mod common;` after the leading inner attributes / //! doc block,
# which must stay at the very top of the file.
for p in sys.argv[1:]:
    lines = open(p).read().split('\n')
    if any(l.strip() == 'mod common;' for l in lines[:40]):
        lines = [l for l in lines if l.strip() != 'mod common;']
    i = 0
    while i < len(lines):
        s = lines[i].strip()
        if s.startswith('//!') or s.startswith('#![') or s == '':
            i += 1
        else:
            break
    lines.insert(i, 'mod common;\n')
    open(p, 'w').write('\n'.join(lines))
    print(f"{p}: mod common at line {i+1}")
