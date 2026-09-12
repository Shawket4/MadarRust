#!/usr/bin/env bash
#
# Re-stamp one migration's recorded checksum on the DEMO database.
#
# WHY THIS EXISTS
#
# `20260912050000_a_delivery_order_freezes_its_tax.sql` was edited after the
# demo box had already applied it. It had to be: the original asserted that no
# existing row records a road distance without a provenance — true of the dev
# copy it was written against, false of production, where the deploy failed on
# `23514`. The fix backfills `distance_source = 'legacy'` before adding the
# constraint.
#
# sqlx records a SHA-384 of each migration's text and refuses to boot against a
# recorded one that no longer matches. That is the protection working. Prod
# never applied the original, so it took the corrected file cleanly; the demo
# box had, so it has been failing every deploy since.
#
# WHAT THIS DOES, AND WHAT IT REFUSES TO DO
#
# Re-stamping a checksum is recording "this migration ran". That must only be
# said when it is TRUE, so this checks the file's whole effect is already
# present before touching anything:
#
#   1. both constraints exist, and
#   2. no row is left that the backfill would have changed.
#
# If either check fails it does the backfill (1) or stops (2) rather than
# writing a checksum for work that did not happen.
#
# It touches exactly one version and only ever that one. It is idempotent: run
# against an already-repaired database it reports that and exits 0.
#
# Usage:  DATABASE_URL=postgres://…/madar_demo scripts/repair-demo-migration-checksum.sh
set -euo pipefail

VERSION=20260912050000
FILE="migrations/${VERSION}_a_delivery_order_freezes_its_tax.sql"

: "${DATABASE_URL:?set DATABASE_URL to the demo database}"
[ -f "$FILE" ] || { echo "::error::$FILE not found — run from the repo root"; exit 1; }

q() { psql "$DATABASE_URL" -tAc "$1"; }

recorded=$(q "SELECT encode(checksum,'hex') FROM _sqlx_migrations WHERE version = $VERSION")
if [ -z "$recorded" ]; then
  echo "Migration $VERSION is not recorded on this database — nothing to repair."
  echo "It will simply be applied on the next boot."
  exit 0
fi

want=$(sha384sum "$FILE" | cut -d' ' -f1)
if [ "$recorded" = "$want" ]; then
  echo "✅ Already matches ($want) — nothing to do."
  exit 0
fi

echo "Recorded: $recorded"
echo "File:     $want"
echo

# 1. Is the effect actually present?
constraints=$(q "SELECT count(*) FROM pg_constraint
                  WHERE conrelid = 'delivery_orders'::regclass
                    AND conname IN ('delivery_orders_distance_source_is_known',
                                    'delivery_orders_distance_has_a_source')")
if [ "$constraints" != "2" ]; then
  echo "::error::Only $constraints of 2 constraints present. This database did not"
  echo "::error::apply the migration's effect, so its checksum must not be re-stamped."
  echo "::error::Let the migration run instead — delete the row and restart the container."
  exit 1
fi

# 2. Anything the backfill would have touched? Do the backfill rather than
#    record that it happened when it did not.
pending=$(q "SELECT count(*) FROM delivery_orders
              WHERE road_distance_meters IS NOT NULL AND distance_source IS NULL")
if [ "$pending" != "0" ]; then
  echo "Backfilling $pending row(s) the corrected migration would have set to 'legacy'…"
  psql "$DATABASE_URL" -c "UPDATE delivery_orders SET distance_source = 'legacy'
                            WHERE road_distance_meters IS NOT NULL
                              AND distance_source IS NULL"
fi

psql "$DATABASE_URL" -c "UPDATE _sqlx_migrations
                            SET checksum = decode('$want','hex')
                          WHERE version = $VERSION"
echo "✅ Re-stamped $VERSION. The demo backend will boot and apply what is queued behind it."
