#!/usr/bin/env python3
"""Convert runtime sqlx queries to the compile-time macros, driven by the compiler.

  sqlx::query("lit").bind(a).bind(b)                 -> sqlx::query!("lit", a, b)
  sqlx::query_as::<_, T>("lit").bind(a)              -> sqlx::query_as!(T, "lit", a)
  sqlx::query_scalar::<_, T>("lit").bind(a)          -> sqlx::query_scalar!("lit", a)

Only sites whose first argument is a single string literal are touched.  Everything
after the bind chain (.fetch_one(..) etc.) is left alone.  Then a loop runs
`cargo check --all-targets`, maps each error to the converted site that caused it
(inside the macro span, or the nearest result-producing site in the enclosing fn)
and reverts that site to its original text, until the crate compiles.  Nothing is
ever "fixed" automatically: a site either compiles unchanged-in-meaning or goes back.

Usage (DATABASE_URL must point at a DB migrated from scratch):
  DATABASE_URL=postgres://.../schema_db scripts/sqlx_codemod.py [--exclude m1,m2] [--only m1,m2]
        [--report /path/report.json] [--convert-only] [--max-iter N]
Afterwards:  cargo sqlx prepare -- --all-targets   and commit .sqlx/

Module = first path component under src/ (file stem for top-level files).
"""
import argparse, json, os, re, subprocess, sys, collections

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_EXCLUDE = "authz,sync,orders,tills,inventory,menu,recipes,costing,menu_unification"

# ---------------------------------------------------------------- lexing helpers

def skip_nontoken(s, i):
    """If s[i] starts a comment/string/char literal, return index after it, else i."""
    n = len(s)
    c = s[i]
    if c == '/' and i + 1 < n and s[i + 1] == '/':
        j = s.find('\n', i)
        return n if j < 0 else j
    if c == '/' and i + 1 < n and s[i + 1] == '*':
        depth, j = 1, i + 2
        while j < n and depth:
            if s.startswith('/*', j): depth += 1; j += 2
            elif s.startswith('*/', j): depth -= 1; j += 2
            else: j += 1
        return j
    m = re.compile(r'b?r(#*)"').match(s, i) if c in 'rb' else None
    if m and (i == 0 or not (s[i - 1].isalnum() or s[i - 1] == '_')):
        end = '"' + m.group(1)
        j = s.find(end, m.end())
        return n if j < 0 else j + len(end)
    if c == '"' or (c == 'b' and s.startswith('b"', i) and (i == 0 or not (s[i-1].isalnum() or s[i-1]=='_'))):
        j = i + (2 if c == 'b' else 1)
        while j < n:
            if s[j] == '\\': j += 2
            elif s[j] == '"': return j + 1
            else: j += 1
        return n
    if c == "'":
        if i + 1 < n and s[i + 1] == '\\':
            j = s.find("'", i + 2)
            return j + 1
        if i + 2 < n and s[i + 2] == "'":
            return i + 3
        return i  # lifetime
    return i

def code_scan(s):
    """Yield indices of code characters (not in comments / strings)."""
    i, n = 0, len(s)
    while i < n:
        j = skip_nontoken(s, i)
        if j != i:
            i = j
            continue
        yield i
        i += 1

def match_paren(s, i):
    """s[i] == '('; return index of the matching ')'."""
    assert s[i] == '('
    depth, n = 0, len(s)
    while i < n:
        j = skip_nontoken(s, i)
        if j != i:
            i = j; continue
        c = s[i]
        if c in '([{': depth += 1
        elif c in ')]}':
            depth -= 1
            if depth == 0: return i
        i += 1
    raise ValueError("unbalanced")

def split_top(s, sep=','):
    out, depth, cur, i, n = [], 0, 0, 0, len(s)
    while i < n:
        j = skip_nontoken(s, i)
        if j != i: i = j; continue
        c = s[i]
        if c in '([{<': depth += 1
        elif c in ')]}>':
            if not (c == '>' and i and s[i-1] == '-'): depth -= 1
        elif c == sep and depth == 0:
            out.append(s[cur:i]); cur = i + 1
        i += 1
    out.append(s[cur:])
    return out

def skip_ws(s, i):
    n = len(s)
    while i < n:
        if s[i].isspace(): i += 1; continue
        j = skip_nontoken(s, i)
        if j != i and (s.startswith('//', i) or s.startswith('/*', i)): i = j; continue
        break
    return i

# Nullability hazards.  sqlx infers nullability from EXPLAIN, and it can call an
# outer-joined / subselect / set-operation column NOT NULL.  A `T` or `Option<T>` field
# then compiles but fails at runtime ("unexpected null").  Such SQL stays runtime.
RISKY = re.compile(r'\b(LEFT|RIGHT|FULL)\s+(OUTER\s+)?JOIN\b|\bLATERAL\b|\bUNION\b|\bINTERSECT\b|\bEXCEPT\b|\bWITH\s+(RECURSIVE\s+)?\w+\s+AS\b', re.I)
SUBSEL = re.compile(r'\(\s*SELECT\b', re.I)

def risky_sql(lit):
    if RISKY.search(lit): return True
    for m in SUBSEL.finditer(lit):
        if not re.search(r'\b(EXISTS|IN|ANY|ALL|SOME)\s*$', lit[:m.start()], re.I):
            return True
    return False

STR_LIT = re.compile(r'^(?:r(#*)".*"\1|"(?:[^"\\]|\\.)*")$', re.S)
CALL = re.compile(r'sqlx::(query_as|query_scalar|query)(::<)?(?=\(|::<)')

# ---------------------------------------------------------------- site discovery

class Site:
    def __init__(self, path, start, end, orig, new, kind):
        self.path, self.start, self.end, self.orig, self.new, self.kind = path, start, end, orig, new, kind
        self.active = True
        self.reason = None
        self.lines = (0, 0)

def module_of(rel):
    parts = rel.split(os.sep)
    p = parts[1]
    return p[:-3] if p.endswith('.rs') else p

def find_sites(path, src):
    code = set()
    sites = []
    for m in CALL.finditer(src):
        i = m.start()
        # must be real code, not comment/string: check by re-lexing from line start is costly; use mask
        sites.append(m)
    mask = bytearray(len(src))
    for i in code_scan(src): mask[i] = 1
    out = []
    for m in sites:
        if not mask[m.start()]: continue
        kind = m.group(1)
        j = m.end()
        turbo = None
        if src.startswith('::<', j):
            # find matching '>'
            depth, k = 0, j + 2
            while True:
                if src[k] == '<': depth += 1
                elif src[k] == '>' and src[k-1] != '-':
                    depth -= 1
                    if depth == 0: break
                k += 1
            turbo = src[j + 3:k]
            j = k + 1
        if j >= len(src) or src[j] != '(': continue
        try:
            close = match_paren(src, j)
        except ValueError:
            continue
        args = split_top(src[j + 1:close])
        if args and not args[-1].strip(): args = args[:-1]
        if len(args) != 1: continue
        lit = args[0].strip()
        if not STR_LIT.match(lit): continue
        # bind chain
        binds, end, k = [], close + 1, close + 1
        while True:
            k2 = skip_ws(src, k)
            if src.startswith('.bind', k2):
                k3 = skip_ws(src, k2 + 5)
                if k3 < len(src) and src[k3] == '(':
                    try: c2 = match_paren(src, k3)
                    except ValueError: break
                    a = src[k3 + 1:c2].strip()
                    if a.endswith(','): a = a[:-1].strip()
                    binds.append(a)
                    end = c2 + 1; k = end
                    continue
            break
        nxt = skip_ws(src, end)
        if src.startswith('.map(', nxt) or src.startswith('.try_map(', nxt):
            continue
        if risky_sql(lit): continue
        # placeholder count must equal bind count
        ph = [int(x) for x in re.findall(r'\$(\d+)', re.sub(r"--[^\n]*", "", lit))]
        if (max(ph) if ph else 0) != len(binds): continue
        if kind == 'query_as':
            if turbo is None: continue
            parts = split_top(turbo)
            if len(parts) != 2: continue
            ty = parts[1].strip()
            if ty.startswith('(') or ty.startswith('['): continue
            first = [ty]
        else:
            if kind == 'query' and turbo is not None: continue
            first = []
        line_start = src.rfind('\n', 0, m.start()) + 1
        indent = re.match(r'\s*', src[line_start:]).group(0)
        name = {'query': 'query', 'query_as': 'query_as', 'query_scalar': 'query_scalar'}[kind]
        items = first + [lit] + binds
        spread = '\n' in src[j:end].split(lit, 1)[0] or '\n' in lit or '\n' in src[close + 1:end]
        if spread:
            new = f'sqlx::{name}!(\n' + ''.join(f'{indent}    {it},\n' for it in items) + indent + ')'
        else:
            new = f'sqlx::{name}!(' + ', '.join(items) + ')'
        out.append(Site(path, m.start(), end, src[m.start():end], new, kind))
    # drop sites nested inside another site's range (bind args containing queries)
    out.sort(key=lambda s: s.start)
    res, last = [], -1
    for s in out:
        if s.start < last: continue
        res.append(s); last = s.end
    return res

class File:
    def __init__(self, path):
        self.path = path
        self.src = open(path).read()
        self.sites = find_sites(path, self.src)
    def render(self):
        out, pos, line = [], 0, 1
        for s in self.sites:
            seg = self.src[pos:s.start]
            out.append(seg); line += seg.count('\n')
            txt = s.new if s.active else s.orig
            s.lines = (line, line + txt.count('\n'))
            out.append(txt); line += txt.count('\n')
            pos = s.end
            # the original text's newlines were counted by txt, nothing else to do
        out.append(self.src[pos:])
        return ''.join(out)
    def write(self):
        text = self.render()
        with open(self.path, 'w') as f: f.write(text)
        self.text = text

# ---------------------------------------------------------------- compiler loop

def span_in_src(sp):
    """Walk macro-expansion chain to the outermost span in our own sources."""
    best = None
    while sp:
        fn = sp['file_name']
        if fn.startswith('src/') or fn.startswith('./src/'):
            best = sp
        exp = sp.get('expansion')
        sp = exp['span'] if exp else None
    return best

def run_check(env):
    p = subprocess.run(['cargo', 'check', '--all-targets', '--message-format=json', '-j', '3'],
                       cwd=ROOT, env=env, capture_output=True, text=True)
    errs = []
    for ln in p.stdout.splitlines():
        try: d = json.loads(ln)
        except Exception: continue
        if d.get('reason') != 'compiler-message': continue
        msg = d['message']
        if msg['level'] != 'error': continue
        prim = [sp for sp in msg['spans'] if sp['is_primary']] or msg['spans']
        sp = span_in_src(prim[0]) if prim else None
        errs.append((msg['message'], sp, msg.get('rendered', '')))
    return p.returncode, errs, p.stderr

LET = re.compile(r'let\s+(?:mut\s+)?([A-Za-z_]\w*|\([^=]*?\))\s*(?::[^=]*)?=\s*$', re.S)

def fn_bounds(lines, ln):
    """Approximate enclosing fn line range (1-based) of line ln."""
    a = ln - 1
    pat = re.compile(r'^\s*(pub(\([^)]*\))?\s+)?(async\s+)?(unsafe\s+)?fn\s')
    while a > 0 and not pat.match(lines[a]): a -= 1
    b = ln
    while b < len(lines) and not pat.match(lines[b]): b += 1
    return a + 1, b

def attribute(files, err):
    msg, sp, rendered = err
    if not sp: return None, 'no-span'
    path = os.path.normpath(os.path.join(ROOT, sp['file_name']))
    f = files.get(path)
    if not f: return None, 'outside-converted-file'
    ls, le = sp['line_start'], sp['line_end']
    for s in f.sites:
        if s.active and s.lines[0] <= le and ls <= s.lines[1]:
            return s, 'direct'
    # consumer-side error: find a site in the same fn
    lines = f.text.split('\n')
    a, b = fn_bounds(lines, ls)
    cands = [s for s in f.sites if s.active and a <= s.lines[0] <= b and s.lines[0] <= ls]
    if not cands:
        cands = [s for s in f.sites if s.active and a <= s.lines[0] <= b]
    if not cands: return None, 'consumer-no-site'
    errline = ' '.join(lines[ls - 1:le])
    for s in reversed(cands):
        pre = f.text.split('\n')
        before = '\n'.join(pre[max(0, s.lines[0] - 3):s.lines[0] - 1]) + '\n' + pre[s.lines[0] - 1][:len(pre[s.lines[0] - 1]) - len(pre[s.lines[0] - 1].lstrip())]
        m = LET.search(before.split(';')[-1].split('{')[-1].strip() + ' ') or LET.search(before[-120:])
        if m:
            names = re.findall(r'[A-Za-z_]\w*', m.group(1))
            if any(re.search(r'\b' + re.escape(n) + r'\b', errline) for n in names if n not in ('mut', 'Some', 'Ok')):
                return s, 'consumer-bound'
    return cands[-1], 'consumer-nearest'

def short(msg):
    msg = re.sub(r'`[^`]*`', '`_`', msg)
    return msg[:90]

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--exclude', default=DEFAULT_EXCLUDE)
    ap.add_argument('--only', default='')
    ap.add_argument('--report', default=os.path.join(ROOT, 'sqlx_codemod_report.json'))
    ap.add_argument('--convert-only', action='store_true')
    ap.add_argument('--max-iter', type=int, default=60)
    a = ap.parse_args()
    excl = set(filter(None, a.exclude.split(',')))
    only = set(filter(None, a.only.split(',')))
    env = dict(os.environ)
    env.pop('SQLX_OFFLINE', None)
    if 'DATABASE_URL' not in env: sys.exit('DATABASE_URL required')

    files = {}
    for dp, _, fns in os.walk(os.path.join(ROOT, 'src')):
        for fn in fns:
            if not fn.endswith('.rs'): continue
            p = os.path.join(dp, fn)
            rel = os.path.relpath(p, ROOT)
            mod = module_of(rel)
            if mod in excl or (only and mod not in only): continue
            f = File(p)
            if f.sites: files[p] = f
    total = sum(len(f.sites) for f in files.values())
    print(f'{total} candidate sites in {len(files)} files', flush=True)
    for f in files.values(): f.write()
    if a.convert_only: return

    reasons = collections.Counter()
    log = []
    for it in range(a.max_iter):
        rc, errs, stderr = run_check(env)
        print(f'iter {it}: rc={rc} errors={len(errs)}', flush=True)
        if rc == 0: break
        if not errs:
            print(stderr[-3000:]); sys.exit('cargo failed without errors')
        reverted = {}
        for e in errs:
            s, how = attribute(files, e)
            if s is None:
                print('UNATTRIBUTED', how, e[0], e[1] and e[1]['file_name'], e[1] and e[1]['line_start'])
                continue
            if id(s) not in reverted:
                s.active = False
                s.reason = (how, short(e[0]))
                reverted[id(s)] = s
        if not reverted:
            print(errs[0][2]); sys.exit('no progress: unattributable errors')
        for p in {s.path for s in reverted.values()}: files[p].write()
        print(f'  reverted {len(reverted)}', flush=True)
    else:
        sys.exit('max iterations')

    rep = {}
    for p, f in files.items():
        mod = module_of(os.path.relpath(p, ROOT))
        r = rep.setdefault(mod, {'converted': 0, 'reverted': 0, 'reverted_sites': []})
        for s in f.sites:
            if s.active: r['converted'] += 1
            else:
                r['reverted'] += 1
                line = f.src[:s.start].count('\n') + 1
                r['reverted_sites'].append({'file': os.path.relpath(p, ROOT), 'line': line, 'how': s.reason[0], 'why': s.reason[1]})
    json.dump(rep, open(a.report, 'w'), indent=1)
    for mod, r in sorted(rep.items()): print(f"{mod}: converted {r['converted']}, reverted {r['reverted']}")
    c = collections.Counter(x['why'] for r in rep.values() for x in r['reverted_sites'])
    for why, n in c.most_common(15): print(n, why)

if __name__ == '__main__':
    main()
