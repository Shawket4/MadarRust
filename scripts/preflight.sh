#!/usr/bin/env bash
#
# Local pre-push robustness gate for sufrix-rust — run the same checks CI would,
# on your machine, before you `git push`.
#
#   scripts/preflight.sh                # FAST gate: fmt + clippy + cargo test --lib
#   scripts/preflight.sh --mutants      # + cargo-mutants on the lines you changed (--in-diff)
#   scripts/preflight.sh --schemathesis # + Schemathesis API fuzz (throwaway sufrix_fuzz DB)
#   scripts/preflight.sh --fuzz         # + cargo-fuzz smoke on the money fns (nightly, ~30s each)
#   scripts/preflight.sh --full-mutants # + full money-engine mutation sweep (~15 min)
#   scripts/preflight.sh --restler      # + RESTler stateful fuzz (needs the x86_64 colima VM; slow)
#   scripts/preflight.sh --all          # everything above
#
# Env:
#   DATABASE_URL   Postgres for tests + mutants (default: dev DB on :5432)
#   STRICT=1       make fmt/clippy block the push too (default: they warn only)
#
# Exit code is non-zero if any GATE stage fails — wire it as a git pre-push hook:
#   ln -sf ../../scripts/preflight.sh .git/hooks/pre-push
set -uo pipefail
cd "$(dirname "$0")/.."
REPO="$PWD"
export PATH="$HOME/.cargo/bin:$PATH"
export DATABASE_URL="${DATABASE_URL:-postgres://shawket@localhost:5432/sufrix_dev}"
# Money-engine unit tests used to keep mutation runs fast (each mutant reruns only these).
FAST_TESTS='test(/units::tests::/) | test(/osrm::tests::/) | test(/cost_math::tests::/) | test(/service::unit_tests::/) | test(/calc_discount_tests::/) | test(/select_zone/) | test(/zone_fee/)'
MONEY_FILES=(-f src/costing/service.rs -f src/orders/cost_math.rs -f src/units.rs -f src/geo/osrm.rs -f src/discounts/handlers.rs -f src/delivery/public.rs)

# ── flags ───────────────────────────────────────────────────────────────────
M=0 S=0 F=0 FM=0 R=0
for a in "$@"; do case "$a" in
  --mutants) M=1;; --schemathesis) S=1;; --fuzz) F=1;; --full-mutants) FM=1;; --restler) R=1;;
  --all) M=1; S=1; F=1; FM=1; R=1;;
  -h|--help) sed -n '2,22p' "$0"; exit 0;;
  *) echo "unknown flag: $a (try --help)" >&2; exit 2;;
esac; done

FAILED=(); WARNED=(); SKIPPED=()
hdr(){ printf '\n\033[1m── %s ──\033[0m\n' "$1"; }
have(){ command -v "$1" >/dev/null 2>&1; }
pg_up(){ pg_isready -d "$DATABASE_URL" >/dev/null 2>&1; }

# ── fast gate ────────────────────────────────────────────────────────────────
hdr "rustfmt --check"
if cargo fmt --all --check; then echo "✓ formatted"
elif [ "${STRICT:-0}" = 1 ]; then FAILED+=("fmt"); else WARNED+=("fmt (run: cargo fmt)"); fi

hdr "clippy"
if cargo clippy --all-targets 2>&1 | tail -15; [ "${PIPESTATUS[0]}" = 0 ]; then echo "✓ clippy"
elif [ "${STRICT:-0}" = 1 ]; then FAILED+=("clippy"); else WARNED+=("clippy"); fi

hdr "cargo test --lib  (GATE)"
if ! pg_up; then
  echo "✗ Postgres not reachable at \$DATABASE_URL ($DATABASE_URL)"; FAILED+=("test: no DB")
elif cargo test --lib; then echo "✓ tests pass"
else FAILED+=("test"); fi

# ── opt-in: mutation testing on changed lines (adaptive) ──────────────────────
if [ $M = 1 ]; then
  hdr "cargo-mutants --in-diff (changed lines)"
  if ! have cargo-mutants; then SKIPPED+=("mutants: cargo-mutants not installed (cargo install cargo-mutants)")
  else
    base="$(git merge-base HEAD origin/main 2>/dev/null || git rev-parse HEAD~1 2>/dev/null || echo HEAD)"
    git diff "$base"...HEAD > /tmp/preflight.diff 2>/dev/null || git diff HEAD > /tmp/preflight.diff
    if [ ! -s /tmp/preflight.diff ]; then SKIPPED+=("mutants: no diff vs $base")
    elif cargo mutants --in-diff /tmp/preflight.diff --jobs 2 -- --lib; then echo "✓ no surviving mutants in the diff"
    else WARNED+=("mutants: surviving mutants in changed code — see mutants.out/missed.txt"); fi
  fi
fi

# ── opt-in: full money-engine mutation sweep ──────────────────────────────────
if [ $FM = 1 ]; then
  hdr "full money-engine mutation sweep (~15 min)"
  if ! have cargo-mutants || ! have cargo-nextest; then SKIPPED+=("full-mutants: needs cargo-mutants + cargo-nextest")
  elif cargo mutants --jobs 3 "${MONEY_FILES[@]}" \
        --re '(round_piastres|blend_weighted_cost|convert|normalize_to_base|is_valid_unit|calc_discount|haversine_meters|select_zone_fee|summarize_line_costs)' \
        -- -E "$FAST_TESTS"; then echo "✓ money engine mutation-clean"
  else WARNED+=("full-mutants: survivors — see mutants.out/missed.txt"); fi
fi

# ── opt-in: cargo-fuzz smoke (nightly) ────────────────────────────────────────
if [ $F = 1 ]; then
  hdr "cargo-fuzz smoke (30s/target, nightly)  (GATE on crash)"
  if ! have rustup || ! rustup toolchain list 2>/dev/null | grep -q nightly; then
    SKIPPED+=("fuzz: nightly toolchain not installed (rustup toolchain install nightly)")
  else
    fz_fail=0
    for t in round_piastres calc_discount convert_units haversine select_zone_fee blend_weighted_cost summarize_line_costs; do
      echo "  fuzzing $t…"
      cargo +nightly fuzz run "$t" -- -max_total_time=30 >/dev/null 2>&1 || { echo "  ✗ $t produced a crash"; fz_fail=1; }
    done
    [ $fz_fail = 0 ] && echo "✓ no crashes" || FAILED+=("fuzz")
  fi
fi

# ── opt-in: Schemathesis API fuzz (throwaway DB) ──────────────────────────────
if [ $S = 1 ]; then
  hdr "Schemathesis API fuzz (throwaway sufrix_fuzz DB)  (GATE on 5xx)"
  if [ ! -x ./.fuzzvenv/bin/st ] && ! have st; then SKIPPED+=("schemathesis: not installed (python3 -m venv .fuzzvenv && .fuzzvenv/bin/pip install schemathesis)")
  else
    if bash scripts/api-fuzz.sh >/tmp/preflight-apifuzz.log 2>&1; then :; fi
    if grep -qiE "Server error:" /tmp/preflight-apifuzz.log fuzz-out/*.log 2>/dev/null; then
      echo "✗ Schemathesis found server errors (5xx) — see fuzz-out/*.log"; FAILED+=("schemathesis: 5xx")
    else echo "✓ no 5xx across the API"; fi
  fi
fi

# ── opt-in: RESTler stateful (x86_64 colima VM) ───────────────────────────────
if [ $R = 1 ]; then
  hdr "RESTler stateful fuzz (x86_64 colima VM)"
  if ! have docker || ! docker info >/dev/null 2>&1; then SKIPPED+=("restler: docker/colima x86_64 VM not running (colima start --arch x86_64 --vm-type qemu)")
  elif [ ! -f restler_work/Compile/grammar.py ]; then SKIPPED+=("restler: no grammar — see scripts/restler-run.sh header to compile")
  else
    bash scripts/restler-run.sh test >/tmp/preflight-restler.log 2>&1 || true
    if grep -qiE "bug_buckets.*: \{[^}]" /tmp/preflight-restler.log; then WARNED+=("restler: bugs found — see restler_work/Test"); else echo "✓ RESTler: no bugs"; fi
  fi
fi

# ── summary ──────────────────────────────────────────────────────────────────
hdr "preflight summary"
for w in "${WARNED[@]:-}";  do [ -n "$w" ] && printf '  \033[33m⚠ %s\033[0m\n' "$w"; done
for s in "${SKIPPED[@]:-}"; do [ -n "$s" ] && printf '  \033[2m• skipped: %s\033[0m\n' "$s"; done
if [ "${#FAILED[@]}" -gt 0 ] && [ -n "${FAILED[0]:-}" ]; then
  for f in "${FAILED[@]}"; do printf '  \033[31m✗ %s\033[0m\n' "$f"; done
  printf '\n\033[1;31mPREFLIGHT FAILED — push blocked.\033[0m\n'; exit 1
fi
printf '\n\033[1;32mPREFLIGHT PASSED.\033[0m\n'
