#!/usr/bin/env bash
# One-command verification of a DB architecture phase on a copy of a source
# database (DB_ARCHITECTURE_AUDIT_2026-10-16 §9.1, §10).
#
#   scripts/db_verify_phase.sh <phase> <source-db> [out-dir]
#
#   phase      2 | 3 | 5
#   source-db  a database on the server PGHOST/PGPORT/PGUSER point at (a
#              restored prod dump, any migration level up to the phase). It is
#              NEVER written: the script clones it with `createdb -T`.
#
# Steps:
#   1. clone <source-db> -> <source-db>_verify_p<phase>; migrate the clone up to
#      the migration just before the phase (whatever the source lacked), ANALYZE;
#   2. BEFORE: db_reconcile.sql, db_rls_visibility.sql, explain_hot_paths.sql;
#   3. apply the phase's migrations (timed, one line per migration);
#   4. run the phase's backfills twice (the second run must change 0 rows), and
#      for Phase 2 reconcile every report switched to the rollups (2.6);
#   5. AFTER: the same three; diff reconcile (must be empty except the phase's
#      expected new metrics), diff visible rows (must be empty), plan table;
#   6. the phase's check script (must print nothing).
# Exit 0 only when every gate passes. Everything lands in out-dir (default
# ./db-verify-p<phase>-<timestamp>). The clone is kept for inspection; drop it
# with `dropdb <source-db>_verify_p<phase>`.
#
# Needs: psql, createdb, dropdb, sqlx (0.8.x) on PATH; PGPASSWORD etc. as usual;
# cargo (Phase 2 runs the sales-rollup-backfill and report-rollup-reconcile bins).
set -euo pipefail
cd "$(dirname "$0")/.."

phase="${1:?phase (2|3|5)}"
renames=""   # old=new table renames a phase makes (the same rows, a new name)
src="${2:?source database}"
out="${3:-db-verify-p${phase}-$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$out"

# Migration ranges: (last version BEFORE the phase, last version OF the phase).
case "$phase" in
  2) before=20261017010100; last=20261018999999; check=scripts/db_phase2_check.sql ;;
  3) before=20261018999999; last=20261019999999; check=scripts/db_phase3_check.sql ;;
  5) before=20261019999999; last=20261020999999; check=scripts/db_phase5_check.sql
     renames="loyalty_customers=loyalty_memberships" ;;
  *) echo "unknown phase $phase" >&2; exit 2 ;;
esac

clone="${src}_verify_p${phase}"
host_args=()
[ -n "${PGHOST:-}" ] && host_args+=(-h "$PGHOST")
[ -n "${PGPORT:-}" ] && host_args+=(-p "$PGPORT")
[ -n "${PGUSER:-}" ] && host_args+=(-U "$PGUSER")
url="postgres://${PGUSER:-$USER}${PGPASSWORD:+:$PGPASSWORD}@${PGHOST:-localhost}:${PGPORT:-5432}/$clone"
q() { psql "$url" -X -At -F $'\t' -v ON_ERROR_STOP=1 "$@"; }

# The newest version at or below a bound, among migrations/*.sql.
version_at_or_below() {
  ls migrations/*.sql | sed -E 's#.*/([0-9]+)_.*#\1#' | sort -n \
    | awk -v b="$1" '$1 <= b { v = $1 } END { print v }'
}

echo "== clone $src -> $clone"
dropdb "${host_args[@]}" --if-exists "$clone"
createdb "${host_args[@]}" -T "$src" "$clone"

pre=$(version_at_or_below "$before")
echo "== bring the clone up to $pre (the state before Phase $phase)"
sqlx migrate run --target-version "$pre" --database-url "$url" | tee "$out/migrate_pre.log" | tail -1
q -c "VACUUM ANALYZE" >/dev/null

echo "== BEFORE"
q -f scripts/db_reconcile.sql      > "$out/reconcile_before.tsv"
q -f scripts/db_rls_visibility.sql > "$out/visible_before.tsv"
q -f scripts/explain_hot_paths.sql > "$out/plans_before.tsv"

end=$(version_at_or_below "$last")
echo "== apply Phase $phase (up to $end)"
sqlx migrate run --target-version "$end" --database-url "$url" > "$out/migrate_phase.log"
grep Applied "$out/migrate_phase.log" | sed -E 's/^Applied ([0-9]+)\/migrate (.*) \((.*)\)$/\3\t\1 \2/' \
  | sort -t$'\t' -k1,1 -h -r > "$out/migration_times.tsv" || true
echo "   $(grep -c Applied "$out/migrate_phase.log" || true) migrations; slowest:"
head -5 "$out/migration_times.tsv" | sed 's/^/     /'

# Phase backfills (each idempotent; the second run must report 0 changes).
backfill() {
  case "$phase" in
    2) # 2.6: the report rollups' history, twice (the second must do nothing),
       # then every switched report old vs new, per org per day.
       DATABASE_URL="$url" cargo run -q --bin sales-rollup-backfill > "$out/backfill_run1.tsv"
       DATABASE_URL="$url" cargo run -q --bin sales-rollup-backfill > "$out/backfill_run2.tsv"
       case "$(cat "$out/backfill_run2.tsv")" in
         "covered 0 new days, recomputed 0 days"*) echo "OK rollup backfill idempotent ($(cat "$out/backfill_run1.tsv"))" ;;
         *) echo "!! second rollup backfill did work: $(cat "$out/backfill_run2.tsv")"; return 1 ;;
       esac
       if DATABASE_URL="$url" cargo run -q --bin report-rollup-reconcile > "$out/report_reconcile.txt" 2>&1; then
         echo "OK report reconciliation: $(tail -1 "$out/report_reconcile.txt")"
       else
         echo "!! report reconciliation failed:"; tail -20 "$out/report_reconcile.txt"; return 1
       fi ;;
    5) for i in 1 2; do q -f scripts/db_phase5_backfill.sql > "$out/backfill_run$i.tsv"; done
       if grep -qv $'\t0$' "$out/backfill_run2.tsv"; then
         echo "!! second backfill run changed rows:"; cat "$out/backfill_run2.tsv"; return 1
       fi
       echo "OK backfills idempotent ($(wc -l < "$out/backfill_run2.tsv") backfills, second run changed 0 rows)" ;;
    *) : ;;
  esac
}
backfill || exit 1

q -c "ANALYZE" >/dev/null
echo "== AFTER"
q -f scripts/db_reconcile.sql      > "$out/reconcile_after.tsv"
q -f scripts/db_rls_visibility.sql > "$out/visible_after.tsv"
q -f scripts/explain_hot_paths.sql > "$out/plans_after.tsv"

fail=0
# Metrics a phase adds on purpose (new columns/tables it reconciles) are
# listed in scripts/db_reconcile_expected_p<phase>.txt (one metric name per
# line); every other line must be byte-identical.
exp="scripts/db_reconcile_expected_p${phase}.txt"
filter() { if [ -f "$exp" ]; then grep -v -F -f "$exp"; else cat; fi; }
if diff <(filter < "$out/reconcile_before.tsv") <(filter < "$out/reconcile_after.tsv") > "$out/reconcile.diff"; then
  echo "OK reconcile: identical ($(wc -l < "$out/reconcile_after.tsv") rows)"
else
  echo "!! reconcile differs (see $out/reconcile.diff)"; fail=1
fi
# Tables the phase CREATES have no "before": compare the tables that existed,
# and list the new ones (with what each org sees in them) for the record.
# A renamed table is compared under its new name.
cp "$out/visible_before.tsv" "$out/visible_before_named.tsv"
for r in $renames; do
  awk -F'\t' -v OFS='\t' -v o="${r%%=*}" -v n="${r##*=}" '$2 == o { $2 = n } 1' \
    "$out/visible_before_named.tsv" | sort > "$out/visible_before_named.tmp"
  mv "$out/visible_before_named.tmp" "$out/visible_before_named.tsv"
done
sort -o "$out/visible_before_named.tsv" "$out/visible_before_named.tsv"
cut -f2 "$out/visible_before_named.tsv" | sort -u > "$out/tables_before.txt"
awk -F'\t' 'NR==FNR { t[$1]; next } ($2 in t)' "$out/tables_before.txt" "$out/visible_after.tsv" | sort > "$out/visible_after_existing.tsv"
awk -F'\t' 'NR==FNR { t[$1]; next } !($2 in t)' "$out/tables_before.txt" "$out/visible_after.tsv" > "$out/visible_new_tables.tsv"
if diff "$out/visible_before_named.tsv" "$out/visible_after_existing.tsv" > "$out/visible.diff"; then
  echo "OK visible rows per org: identical ($(wc -l < "$out/visible_before.tsv") org x table${renames:+; renamed: $renames})"
else
  echo "!! visible rows changed (see $out/visible.diff)"; fail=1
fi
if [ -s "$out/visible_new_tables.tsv" ]; then
  echo "   new RLS tables: $(cut -f2 "$out/visible_new_tables.tsv" | sort -u | tr '\n' ' ')(per-org rows in visible_new_tables.tsv)"
fi
paste "$out/plans_before.tsv" "$out/plans_after.tsv" \
  | awk -F'\t' '{ r = ($3 > 0) ? $6 / $3 : 1;
                  flag = ($6 - $3 > 1 && r > 1.25) ? "  <-- slower" : "";
                  printf "%-32s %-10s %9s -> %9s%s\n", $1, $2, $3, $6, flag }' > "$out/plans.txt"
cat "$out/plans.txt"
if grep -q "slower" "$out/plans.txt"; then echo "!! a plan regressed by > 25% and > 1 ms"; fail=1; fi

if [ -f "$check" ]; then
  q -f "$check" > "$out/check.tsv"
  if [ -s "$out/check.tsv" ]; then echo "!! $check found problems:"; cat "$out/check.tsv"; fail=1
  else echo "OK $check: nothing to report"; fi
fi
q -f scripts/db_phase1_check.sql > "$out/phase1_check.tsv"
if [ -s "$out/phase1_check.tsv" ]; then echo "!! db_phase1_check:"; cat "$out/phase1_check.tsv"; fail=1; fi

echo "== evidence in $out"
exit "$fail"
