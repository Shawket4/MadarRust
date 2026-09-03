#!/usr/bin/env bash
# Fail when the Sentry redaction lists drift apart across the three surfaces.
#
# The denylist, the exact short-form list and the allowlist exist three times —
# once per language — because each SDK scrubs in its own process. They are a
# COMPLIANCE control: the published privacy policy says error reports exclude
# personal data, and that has to hold on the wire from the backend, the web
# dashboard and the till alike.
#
# Three copies of anything drift. They drift silently, and the failure mode is
# the worst kind: two surfaces keep their promise and one quietly stops, with no
# error anywhere to say so. This script is what makes that a build failure.
#
#   src/observability/scrub.rs                     (Rust backend)
#   ../MadarDashboard/src/lib/sentry-scrub.ts      (web dashboard)
#   ../madar/apps/madar/lib/app/observability.dart (Flutter cashier app)
#
# Override the sibling paths with MADAR_DASHBOARD_DIR / MADAR_FLUTTER_DIR when
# the repos are not checked out side by side. A surface that is not present is
# SKIPPED with a warning rather than failing — CI for one repo should not depend
# on another being cloned — but a surface that IS present must match exactly.
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dashboard_dir="${MADAR_DASHBOARD_DIR:-$here/../MadarDashboard}"
flutter_dir="${MADAR_FLUTTER_DIR:-$here/../madar}"

rust_file="$here/src/observability/scrub.rs"
ts_file="$dashboard_dir/src/lib/sentry-scrub.ts"
dart_file="$flutter_dir/apps/madar/lib/app/observability.dart"

python3 - "$rust_file" "$ts_file" "$dart_file" <<'PY'
import re, sys, os

rust_file, ts_file, dart_file = sys.argv[1:4]

# Each surface names the same three lists in its own idiom.
NAMES = {
    "denylist":  ("PII_KEY_DENYLIST",  "PII_KEY_DENYLIST",  "piiKeyDenylist"),
    "exact":     ("PII_KEY_EXACT",     "PII_KEY_EXACT",     "piiKeyExact"),
    "allowlist": ("PII_KEY_ALLOWLIST", "PII_KEY_ALLOWLIST", "piiKeyAllowlist"),
}

def extract(path, symbol):
    """Pull the string literals out of `symbol`'s array/list initialiser.

    Deliberately literal-scraping rather than parsing: the point is to compare
    what is actually written in each file, and a real parser per language would
    be three more things to keep in step.

    The symbol is matched at its DECLARATION, not at its first mention. Every
    one of these files references its own lists from a doc comment
    ("see [`PII_KEY_DENYLIST`]"), and taking the first occurrence lands on the
    doc link, whose `[...]` closes immediately — which extracts an empty list
    and reports three surfaces as identical because all three are empty.
    """
    src = open(path, encoding="utf-8").read()
    start = None
    for m in re.finditer(rf"\b{re.escape(symbol)}\b", src):
        tail = src[m.end() : m.end() + 200]
        # A declaration has an `=` after the name; a doc-comment reference does
        # not. The initialiser's bracket is the first one AFTER that `=` —
        # taking the first bracket outright lands inside the TYPE
        # (`&[&str]`, `readonly string[]`), not the value.
        eq = tail.find("=")
        if eq == -1:
            continue
        bracket = tail.find("[", eq)
        if bracket != -1:
            start = m.end() + bracket
            break
    if start is None:
        raise SystemExit(
            f"FAIL {os.path.basename(path)}: no declaration of `{symbol}` found"
        )
    depth, i = 0, start
    while i < len(src):
        if src[i] == "[":
            depth += 1
        elif src[i] == "]":
            depth -= 1
            if depth == 0:
                break
        i += 1
    body = src[start : i + 1]
    # Strip line comments so a commented-out entry never counts.
    body = re.sub(r"//[^\n]*", "", body)
    return [m.group(1) for m in re.finditer(r"['\"]([^'\"]+)['\"]", body)]

surfaces = [("rust", rust_file, 0), ("dashboard", ts_file, 1), ("flutter", dart_file, 2)]
present, missing = [], []
for name, path, slot in surfaces:
    (present if os.path.exists(path) else missing).append((name, path, slot))

for name, path, _ in missing:
    print(f"SKIP  {name}: {path} not found")

if len(present) < 2:
    print("SKIP  fewer than two surfaces available; nothing to compare")
    raise SystemExit(0)

failed = False
for list_name, symbols in NAMES.items():
    extracted = {}
    for name, path, slot in present:
        try:
            extracted[name] = extract(path, symbols[slot])
        except SystemExit as e:
            print(e)
            failed = True
    if len(extracted) < 2:
        continue

    # An empty extraction means the scraper broke, not that the lists agree.
    # Without this an extraction bug reports "identical" and the drift guard
    # silently stops guarding anything.
    for name, values in extracted.items():
        if not values:
            failed = True
            print(f"FAIL  {list_name} extracted 0 entries from {name} — the "
                  f"parity check itself is broken, not the lists")

    reference_name, reference = next(iter(extracted.items()))
    for name, values in extracted.items():
        if name == reference_name:
            continue
        # Order is not compared — only membership. A reordered list is still
        # the same control; a different set is not.
        only_ref = sorted(set(reference) - set(values))
        only_other = sorted(set(values) - set(reference))
        if only_ref or only_other:
            failed = True
            print(f"FAIL  {list_name} differs between {reference_name} and {name}")
            if only_ref:
                print(f"      only in {reference_name}: {', '.join(only_ref)}")
            if only_other:
                print(f"      only in {name}: {', '.join(only_other)}")
        # A duplicate is harmless at runtime but means someone edited one copy
        # by hand; flag it before the lists diverge for real.
        if len(values) != len(set(values)):
            dupes = sorted({v for v in values if values.count(v) > 1})
            failed = True
            print(f"FAIL  {list_name} has duplicates in {name}: {', '.join(dupes)}")
        # Case-insensitive comparison is the contract everywhere; an uppercase
        # entry silently stops matching.
        bad_case = [v for v in values if v != v.lower()]
        if bad_case:
            failed = True
            print(f"FAIL  {list_name} has non-lowercase entries in {name}: {', '.join(bad_case)}")

    if not failed:
        print(f"OK    {list_name}: {len(reference)} entries, identical across "
              f"{', '.join(extracted)}")

raise SystemExit(1 if failed else 0)
PY
