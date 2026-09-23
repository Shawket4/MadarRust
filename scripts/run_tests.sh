#!/usr/bin/env bash
# Run the backend test suite.
#
# Why this exists rather than `cargo nextest run`:
#
# 1. MADAR_FAST_TEST_POOLS. Every suite is an integration test, and an
#    integration test binary links the library WITHOUT `cfg(test)`. The tenant
#    connection pool therefore takes its production idle timeout (30s), and a
#    connection opened while serving a request outlives the test — so
#    `#[sqlx::test]`'s teardown cannot drop the throwaway database and each test
#    that serves a request waits ~5s. See `src/db.rs`. This variable restores
#    the short reaping; it is also gated on `debug_assertions`, so no release
#    build can be talked into it.
#
# 2. One suite at a time, sweeping between them. A single run over the whole
#    suite creates enough per-test databases to fill the 12 GiB RAM cluster, and
#    whichever suite happens to be running when the volume fills fails with
#    PoolTimedOut — which reads as broken code, not a full disk. The cluster
#    cannot simply grow: the machine has 24 GiB of RAM in total.
#
# Usage:
#   scripts/run_tests.sh                 # everything
#   scripts/run_tests.sh orders menu     # named suites (no `tests/` prefix)
set -uo pipefail

cd "$(dirname "$0")/.."

export DATABASE_URL="${DATABASE_URL:-postgres://shawket@localhost:5433/madar}"
export MADAR_FAST_TEST_POOLS=1
# Sweep the cluster the tests actually run on (a private cluster has its own port).
PGPORT_TESTS="$(printf '%s' "$DATABASE_URL" | sed -nE 's#.*@[^:/]+:([0-9]+)/.*#\1#p')"
PGPORT_TESTS="${PGPORT_TESTS:-5433}"

sweep() {
  psql -h localhost -p "$PGPORT_TESTS" -d postgres -Atc \
    "select 'drop database \"'||datname||'\";' from pg_database where datname like '_sqlx_test%'" \
    2>/dev/null | psql -h localhost -p "$PGPORT_TESTS" -d postgres -q 2>/dev/null || true
}

if [ $# -gt 0 ]; then
  targets=()
  for t in "$@"; do targets+=("--test $t"); done
else
  # `--lib` first: the inline unit tests still live beside the code they cover.
  targets=("--lib")
  for f in tests/*.rs; do
    m="$(basename "$f" .rs)"
    # client_seen's binary is killed on exec by XProtect (its fixtures embed
    # complete browser User-Agent strings, which a signature matches). Rewriting
    # test data to dodge a malware scanner is the owner's call, not this script's
    # — so it is skipped here and named, never silently dropped.
    [ "$m" = "client_seen" ] && continue
    targets+=("--test $m")
  done
fi

passed=0
failed=0
problems=()

for t in "${targets[@]}"; do
  printf '%-34s ' "$t"
  out="$(cargo nextest run $t 2>&1)"
  line="$(printf '%s' "$out" | grep -E '^ +Summary' | tail -1)"
  if [ -z "$line" ]; then
    echo "NO SUMMARY (see below)"
    problems+=("$t: produced no summary")
    printf '%s\n' "$out" | tail -5
  else
    echo "$line"
    p="$(printf '%s' "$line" | sed -nE 's/.* ([0-9]+) tests run: ([0-9]+) passed.*/\2/p')"
    f="$(printf '%s' "$line" | sed -nE 's/.*passed.*, ([0-9]+) failed.*/\1/p')"
    passed=$((passed + ${p:-0}))
    failed=$((failed + ${f:-0}))
    [ -n "${f:-}" ] && problems+=("$t: $f failed")
  fi
  sweep
done

echo
echo "TOTAL passed=$passed failed=$failed"
for p in "${problems[@]:-}"; do [ -n "$p" ] && echo "  !! $p"; done
[ "$failed" -eq 0 ] && [ "${#problems[@]}" -eq 0 ]
