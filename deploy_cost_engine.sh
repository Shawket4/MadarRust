#!/usr/bin/env bash
# ═══════════════════════════════════════════════════════════════════
# Sufrix cost-engine deploy — run ON THE VPS from the repo root.
#
#   ./deploy_cost_engine.sh "postgres://user:pass@localhost:5432/sufrix"
#   (or export DATABASE_URL and run with no args)
#
# What it does, in order:
#   1. pg_dump custom-format backup next to this script
#   2. Applies migrations/20260610090000_cost_engine.sql in one
#      transaction (cost columns + epoch/history baselines + backfill
#      of historical order lines + drops branch_menu_overrides)
#   3. Repairs sqlx migration bookkeeping so future `sqlx migrate run`
#      agrees with the shipped migrations/ directory:
#        - removes the retired 20260531130918 row (file renumbered to
#          20260601000000 and made idempotent)
#        - upserts a row per migration file with the file's SHA-384
#          checksum (sqlx's checksum algorithm)
#
# It does NOT restart the service — swap the binary and restart as
# usual after this succeeds.
#
# Idempotent: safe to re-run. The migration itself uses IF NOT EXISTS
# guards and only backfills rows whose cost columns are still NULL.
# ═══════════════════════════════════════════════════════════════════
set -euo pipefail

DB="${1:-${DATABASE_URL:-}}"
if [ -z "$DB" ]; then
    echo "usage: $0 <DATABASE_URL>   (or export DATABASE_URL)" >&2
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MIG_DIR="$SCRIPT_DIR/migrations"
COST_MIG="$MIG_DIR/20260610090000_cost_engine.sql"
[ -f "$COST_MIG" ] || { echo "missing $COST_MIG — run from the repo root" >&2; exit 1; }

STAMP="$(date +%Y%m%d_%H%M%S)"
BACKUP="$SCRIPT_DIR/sufrix_pre_cost_engine_${STAMP}.dump"

echo "── 1/3 backup → $BACKUP"
pg_dump "$DB" -Fc -f "$BACKUP"
echo "    done ($(du -h "$BACKUP" | cut -f1))"

echo "── 2/3 applying cost-engine migration (single transaction)"
ALREADY="$(psql "$DB" -tAc "SELECT count(*) FROM information_schema.columns WHERE table_name='order_items' AND column_name='line_cost'")"
psql "$DB" -v ON_ERROR_STOP=1 --single-transaction -q -f "$COST_MIG"
if [ "$ALREADY" = "0" ]; then
    echo "    applied fresh"
else
    echo "    re-applied (columns existed — guards made it a no-op + backfill of remaining NULLs)"
fi

BACKFILLED="$(psql "$DB" -tAc "SELECT count(*) FROM order_items WHERE line_cost IS NOT NULL")"
MISSING="$(psql "$DB" -tAc "SELECT count(*) FROM order_items WHERE cost_missing")"
echo "    order lines with cost: $BACKFILLED · cost_missing: $MISSING"

echo "── 3/3 sqlx migration bookkeeping"
HAS_TABLE="$(psql "$DB" -tAc "SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")"
if [ "$HAS_TABLE" != "t" ]; then
    echo "    _sqlx_migrations not present — skipping (nothing to reconcile)"
else
    # Retired version: file renumbered to 20260601000000 (it sorted before
    # the full_schema baseline it depends on, breaking fresh databases).
    psql "$DB" -v ON_ERROR_STOP=1 -q -c \
        "DELETE FROM _sqlx_migrations WHERE version = 20260531130918;"

    for f in "$MIG_DIR"/*.sql; do
        base="$(basename "$f")"
        ver="${base%%_*}"
        desc="${base#*_}"; desc="${desc%.sql}"; desc="${desc//_/ }"
        # sqlx checksum = SHA-384 of the file bytes
        sum="$(openssl dgst -sha384 -binary "$f" | od -An -v -tx1 | tr -d ' \n')"
        psql "$DB" -v ON_ERROR_STOP=1 -q -c "
            INSERT INTO _sqlx_migrations
                (version, description, installed_on, success, checksum, execution_time)
            VALUES
                ($ver, '$desc', now(), true, decode('$sum','hex'), 0)
            ON CONFLICT (version) DO UPDATE
                SET checksum = EXCLUDED.checksum,
                    description = EXCLUDED.description,
                    success = true;"
        echo "    reconciled $base"
    done
fi

echo "── all done. Swap the binary and restart the service."
