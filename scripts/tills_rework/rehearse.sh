#!/usr/bin/env bash
# Tills-rework rehearsal (TILLS_CONTRACT.md §7.3 steps 1–4 + 7 schema part).
# Clones madar_prodcopy (never written), snapshots invariants, applies the
# pending migrations, checks invariants_after == invariants_before, runs
# scripts/tills_rework/invariants.sql, then down.sql and checks the round trip.
#   scripts/tills_rework/rehearse.sh [--migrate-with sqlx|binary] [--keep]
# Default --migrate-with sqlx (the shared tree's binary may be mid-rework).
set -euo pipefail
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
MODE=sqlx; EXTRA=()
while [ $# -gt 0 ]; do
  case "$1" in
    --migrate-with) MODE="$2"; shift 2 ;;
    *) EXTRA+=("$1"); shift ;;
  esac
done
export OUT_DIR="${OUT_DIR:-$REPO/target/till_rehearsal/$(date +%Y%m%d%H%M%S)}"
mkdir -p "$OUT_DIR"
# Post-migration standalone check runs between migrate and down via a psql hook:
# the driver keeps the DB, so we run it with --keep and do down ourselves.
set +e
"$REPO/scripts/till_migration_rehearsal.sh" --migrate-with "$MODE" --keep ${EXTRA[@]+"${EXTRA[@]}"} | tee "$OUT_DIR/driver.log"
status=${PIPESTATUS[0]}
set -e
DB="$(grep -o 'madar_rehearsal_[0-9]*' "$OUT_DIR/driver.log" | head -1)"
PSQL=(psql -X -q -v ON_ERROR_STOP=1 -h "${PGHOST:-localhost}" -U "${PGUSER:-shawket}" -d "$DB")
echo "==> standalone invariants ($DB)"
"${PSQL[@]}" -f "$REPO/scripts/tills_rework/invariants.sql" >"$OUT_DIR/invariants.txt" 2>&1 || { tail -5 "$OUT_DIR/invariants.txt"; status=1; }
grep -c PASS "$OUT_DIR/invariants.txt" | sed 's/^/  PASS checks: /'
echo "==> down + round trip"
"${PSQL[@]}" -1 -f "$REPO/scripts/tills_rework/down.sql" >"$OUT_DIR/down.log" 2>&1 || { tail -5 "$OUT_DIR/down.log"; status=1; }
"${PSQL[@]}" -f "$REPO/scripts/till_migration/invariants_before.sql" >"$OUT_DIR/down.txt"
if diff -u "$OUT_DIR/before.txt" "$OUT_DIR/down.txt" >"$OUT_DIR/down.diff"; then echo "  OK: round trip lossless"; else echo "  FAIL: $OUT_DIR/down.diff"; status=1; fi
echo "==> re-apply after down"
(cd "$REPO" && DATABASE_URL="postgres://${PGUSER:-shawket}@${PGHOST:-localhost}:5432/$DB" sqlx migrate run --source migrations) >"$OUT_DIR/reapply.log" 2>&1 \
  && echo "  OK: re-applied" || { tail -5 "$OUT_DIR/reapply.log"; status=1; }
dropdb -h "${PGHOST:-localhost}" -U "${PGUSER:-shawket}" --force "$DB"
echo "==> artifacts in $OUT_DIR (exit $status)"
exit $status
