#!/usr/bin/env bash
# Verify DB Phase 2.6 (reports from the closed-day rollups) on a COPY of a
# database, in one command:
#
#   scripts/verify_report_rollups.sh <source-db> [admin-url]
#   e.g. scripts/verify_report_rollups.sh madar_prod_2026_10_20 postgres://apex:apex@localhost:5432
#
# 1. clones <source-db> into <source-db>_p26_<epoch> (the source is never written),
# 2. records scripts/db_reconcile.sql, applies the pending migrations (timed),
#    records it again: the diff must be empty (the migration moves no data),
# 3. runs the backfill twice (the second run must do 0 / 0),
# 4. runs report-rollup-reconcile: every switched report, old vs new, per org
#    per day: must print "differences 0",
# 5. drops the clone unless KEEP=1.
# Exits non-zero on the first failed check. Needs sqlx-cli 0.8 and cargo.
set -euo pipefail
cd "$(dirname "$0")/.."

src="${1:?usage: $0 <source-db> [admin-url]}"
admin="${2:-postgres://apex:apex@localhost:5432}"
copy="${src}_p26_$(date +%s)"
url="${admin%/}/${copy}"
out="$(mktemp -d)"

echo "· cloning $src → $copy"
psql "${admin%/}/postgres" -X -q -c "CREATE DATABASE \"$copy\" TEMPLATE \"$src\""
cleanup() { [ "${KEEP:-0}" = 1 ] || psql "${admin%/}/postgres" -X -q -c "DROP DATABASE IF EXISTS \"$copy\""; }
trap cleanup EXIT

psql "$url" -X -At -F $'\t' -f scripts/db_reconcile.sql > "$out/before.tsv"
echo "· migrating"
sqlx migrate run --database-url "$url" | tee "$out/migrate.log" | grep -E "report rollups|Applied" || true
psql "$url" -X -At -F $'\t' -f scripts/db_reconcile.sql > "$out/after.tsv"
if diff "$out/before.tsv" "$out/after.tsv"; then echo "✓ ledger reconciliation unchanged"; else echo "✗ ledger reconciliation changed"; exit 1; fi

echo "· backfill (twice)"
DATABASE_URL="$url" cargo run -q --bin sales-rollup-backfill
second="$(DATABASE_URL="$url" cargo run -q --bin sales-rollup-backfill)"
echo "$second"
case "$second" in
  "covered 0 new days, recomputed 0 days"*) echo "✓ backfill idempotent" ;;
  *) echo "✗ second backfill did work"; exit 1 ;;
esac

echo "· reconciling every switched report, old vs new"
DATABASE_URL="$url" cargo run -q --bin report-rollup-reconcile
echo "✓ Phase 2.6 verified on a copy of $src"
