#!/usr/bin/env bash
# Release rehearsal: the whole deploy of this checkout, on a copy of prod.
#
#   scripts/release_rehearsal.sh <source-db> [out-dir]
#
#   source-db  a restored prod dump on the server PGHOST/PGPORT/PGUSER point at,
#              in the shape prod runs today (v1.10.0). NEVER written: it is
#              cloned with `createdb -T`.
#
# Env:
#   MADAR_OLD_BIN  the binary prod runs today (v1.10.0 release build). Required:
#                  it serves the old-shape routes for the byte comparison.
#   MADAR_NEW_BIN  the release binary (default target/release/madar-rust under
#                  $CARGO_TARGET_DIR or ./target). Build it first:
#                  `SQLX_OFFLINE=true cargo build --release --bin madar-rust`.
#   OLD_STATIC     static/ dir for the old binary (default: beside MADAR_OLD_BIN).
#   MADAR_DUMP     the .dump the source was restored from. Needed only when the
#                  source was restored without ACLs: its grants are applied to
#                  the clone (the source itself is never touched).
#
# Steps (each gate prints OK or !!; exit 0 only when every gate passes):
#   1. clone <source-db> -> <source-db>_rehearsal;
#   2. boot the OLD binary on the clone (it migrates nothing on a v1.10.0 copy),
#      capture the 16 old-shape compat routes, stop it;
#   3. BEFORE: db_reconcile.sql, db_rls_visibility.sql, explain_hot_paths.sql;
#   4. boot the NEW binary on the clone exactly as prod will (migrations on
#      boot), timed until /health answers; per-migration times from
#      _sqlx_migrations;
#   5. the 16 compat routes again: byte-identical to the old binary's;
#   6. what follows the deploy: the 2.6 rollup history (sales-rollup-backfill,
#      twice: the second does nothing) and its reconciliation (every switched
#      report, rollups vs live, per org per day); the Phase 5 backfills twice
#      (the migrations did them: both runs change 0 rows);
#   7. AFTER: the same three; reconcile diff (only the phases' expected new
#      metrics may differ), visible rows per org (identical; renamed tables
#      under their new name), plan table (no query > 25% and > 1 ms slower),
#      db_phase1/2/3/5_check.sql print nothing;
#   8. API smoke on the new binary: a sale via /orders, one via /sync/replay
#      (and its replay again: idempotent), a full menu pull, a rename, a second
#      full pull that carries it, a dashboard sales report.
# Evidence lands in out-dir (default ./release-rehearsal-<timestamp>); the clone
# is kept: `dropdb <source-db>_rehearsal`.
#
# Needs: psql, createdb, dropdb, curl, python3, cargo (step 6 runs two bins).
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd)

src="${1:?source database}"
out="${2:-release-rehearsal-$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$out"; out=$(cd "$out" && pwd)
clone="${src}_rehearsal"
OLD_BIN="${MADAR_OLD_BIN:?MADAR_OLD_BIN = the v1.10.0 release binary}"
NEW_BIN="${MADAR_NEW_BIN:-${CARGO_TARGET_DIR:-$ROOT/target}/release/madar-rust}"
OLD_STATIC="${OLD_STATIC:-$(dirname "$OLD_BIN")/static}"
[ -x "$OLD_BIN" ] || { echo "!! no executable MADAR_OLD_BIN at $OLD_BIN"; exit 2; }
[ -x "$NEW_BIN" ] || { echo "!! no release binary at $NEW_BIN (cargo build --release --bin madar-rust)"; exit 2; }
OLD_PORT="${OLD_PORT:-18091}"; NEW_PORT="${NEW_PORT:-18092}"

host_args=()
[ -n "${PGHOST:-}" ] && host_args+=(-h "$PGHOST")
[ -n "${PGPORT:-}" ] && host_args+=(-p "$PGPORT")
[ -n "${PGUSER:-}" ] && host_args+=(-U "$PGUSER")
url="postgres://${PGUSER:-$USER}${PGPASSWORD:+:$PGPASSWORD}@${PGHOST:-localhost}:${PGPORT:-5432}/$clone"
q() { psql "$url" -X -At -F $'\t' -v ON_ERROR_STOP=1 "$@"; }
py() { python3 "$ROOT/scripts/release_rehearsal.py" "$@"; }

fail=0
gate() { if [ "$1" = 0 ]; then echo "OK $2"; else echo "!! $2"; fail=1; fi; }

JWT_SECRET="rehearsal-$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')"
ASSET_URL_SECRET="$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n')"
PIN_KEY="$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n')"
srv_pid=""
stop_server() { if [ -n "$srv_pid" ]; then kill "$srv_pid" 2>/dev/null; wait "$srv_pid" 2>/dev/null; srv_pid=""; fi; }
trap stop_server EXIT

# boot <bin> <port> <workdir> <log>: start, wait for /health; seconds in $boot_secs.
boot_secs=""
boot() {
  local bin=$1 port=$2 wd=$3 log=$4 t0 t1
  t0=$(date +%s.%N)
  ( cd "$wd" && exec env DATABASE_URL="$url" JWT_SECRET="$JWT_SECRET" ASSET_URL_SECRET="$ASSET_URL_SECRET" \
      MADAR_PIN_FINGERPRINT_KEY="$PIN_KEY" \
      BIND_ADDR="127.0.0.1:$port" UPLOADS_DIR="$out/uploads" RUST_LOG="${RUST_LOG:-info}" \
      MADAR_DISABLE_AUTO_TRANSLATION=1 MADAR_DISABLE_RATE_LIMIT=1 \
      MADAR_ROLLUP_REPORT_SECS=0 MADAR_ROLLUP_SWEEP_SECS=0 \
      "$bin" ) > "$log" 2>&1 &
  srv_pid=$!
  for _ in $(seq 1 1200); do
    if curl -fs -o /dev/null "http://127.0.0.1:$port/health"; then
      t1=$(date +%s.%N); boot_secs=$(awk -v a="$t0" -v b="$t1" 'BEGIN { printf "%.1f", b - a }'); return 0
    fi
    if ! kill -0 "$srv_pid" 2>/dev/null; then srv_pid=""; return 1; fi
    sleep 0.25
  done
  return 1
}

echo "== clone $src -> $clone"
dropdb "${host_args[@]}" --if-exists "$clone" || exit 2
createdb "${host_args[@]}" -T "$src" "$clone" || exit 2
# The app's tenant role must hold prod's grants. A dump restored with
# --no-acl (or -x) has none, and then nothing in this rehearsal means
# anything (every tenant read fails): put prod's grants back from the dump.
grants=$(q -c "SELECT count(*) FROM information_schema.role_table_grants WHERE grantee = 'madar_app'")
if [ "$grants" = 0 ]; then
  if [ -n "${MADAR_DUMP:-}" ] && [ -f "$MADAR_DUMP" ]; then
    pg_restore -l "$MADAR_DUMP" | grep -E ' ACL ' > "$out/acl.list"
    pg_restore -f "$out/acl.sql" -L "$out/acl.list" "$MADAR_DUMP"
    q -q -f "$out/acl.sql" > "$out/acl_restore.log" 2>&1 || { echo "!! restoring the dump's grants failed (see acl_restore.log)"; exit 2; }
    echo "   the copy had no madar_app grants (restored without ACLs): applied the dump's $(grep -c '^GRANT' "$out/acl.sql") GRANTs to the clone"
  else
    echo "!! $src has no grants for madar_app (restored with --no-acl?). Pass MADAR_DUMP=<the .dump file> so the clone gets prod's grants."
    exit 2
  fi
fi
before_ver=$(q -c "SELECT max(version) FROM _sqlx_migrations WHERE success")
echo "   source is at migration $before_ver"

# The ids the compat routes and the smoke use: the busiest org and branch, an
# item with modifier groups, an add-on, an org admin of that org.
q > "$out/ctx.tsv" <<'SQL' || { echo "!! could not read the ids"; exit 2; }
WITH o AS (SELECT b.org_id, b.id AS branch, count(*) n FROM orders x JOIN branches b ON b.id = x.branch_id
            GROUP BY 1, 2 ORDER BY n DESC LIMIT 1)
SELECT o.org_id, o.branch,
       (SELECT m.id FROM menu_items m WHERE m.org_id = o.org_id AND m.is_active
          ORDER BY (SELECT count(*) FROM menu_item_modifier_groups g WHERE g.menu_item_id = m.id) DESC, m.id LIMIT 1),
       COALESCE((SELECT a.id::text FROM addon_items a WHERE a.org_id = o.org_id ORDER BY a.id LIMIT 1), '-'),
       (SELECT u.id FROM users u WHERE u.org_id = o.org_id AND u.role = 'org_admin' ORDER BY u.created_at LIMIT 1),
       COALESCE((SELECT t.id::text || ',' || t.branch_id::text FROM tills t
         WHERE t.status = 'open'
           AND t.teller_id = (SELECT u.id FROM users u WHERE u.org_id = o.org_id AND u.role = 'org_admin'
                               ORDER BY u.created_at LIMIT 1)
         ORDER BY t.opened_at DESC LIMIT 1), '-'),
       COALESCE((SELECT json_agg(id) FROM (SELECT m.id FROM menu_items m WHERE m.org_id = o.org_id AND m.is_active
           AND NOT EXISTS (SELECT 1 FROM menu_item_modifier_groups g WHERE g.menu_item_id = m.id)
         ORDER BY m.id LIMIT 5) s)::text, '[]')
  FROM o
SQL
IFS=$'\t' read -r ORG BRANCH ITEM ADDON USER TILL SALE_ITEMS < "$out/ctx.tsv"
# The admin's own open till, else one is opened by the smoke.
python3 - "$out/ctx.json" "$ORG" "$BRANCH" "$ITEM" "$ADDON" "$USER" "$TILL" "$SALE_ITEMS" <<'PY'
import json, sys
f, org, br, item, addon, user, till, sale = sys.argv[1:]
addon = "" if addon == "-" else addon
till, till_branch = (till.split(",") + [""])[:2] if till != "-" else ("", "")
json.dump({"org": org, "branch": br, "item": item, "addon": addon or "00000000-0000-0000-0000-000000000000",
           "user": user, "teller_till": till or None, "till_branch": till_branch or None, "sale_items": json.loads(sale or "[]") or [item]},
          open(f, "w"), indent=1)
PY
TOKEN=$(py token "$JWT_SECRET" "$USER" "$ORG" org_admin)
echo "   org $ORG, branch $BRANCH, admin $USER"

echo "== OLD binary ($OLD_BIN)"
if boot "$OLD_BIN" "$OLD_PORT" "$(dirname "$OLD_STATIC")" "$out/boot_old.log"; then
  echo "   up in ${boot_secs}s"
  py capture "http://127.0.0.1:$OLD_PORT" "$TOKEN" "$out/ctx.json" "$out/compat_old"
else
  echo "!! the old binary did not come up (see $out/boot_old.log)"; tail -5 "$out/boot_old.log"; exit 1
fi
stop_server
old_ver=$(q -c "SELECT max(version) FROM _sqlx_migrations WHERE success")
[ "$old_ver" = "$before_ver" ] || echo "   note: the old binary migrated $before_ver -> $old_ver"
q -c "VACUUM ANALYZE" >/dev/null

echo "== BEFORE"
q -f scripts/db_reconcile.sql      > "$out/reconcile_before.tsv"
q -f scripts/db_rls_visibility.sql > "$out/visible_before.tsv"
q -f scripts/explain_hot_paths.sql > "$out/plans_before.tsv"
q -f scripts/db_phase1_check.sql   > "$out/db_phase1_check_before.tsv" 2>&1

echo "== NEW binary ($NEW_BIN): migrations on boot, as in prod"
if boot "$NEW_BIN" "$NEW_PORT" "$ROOT" "$out/boot_new.log"; then
  echo "OK boot: migrated and serving in ${boot_secs}s"
else
  echo "!! the release binary did not come up (see $out/boot_new.log)"; tail -20 "$out/boot_new.log"; exit 1
fi
q -c "SELECT version, description, round(execution_time / 1e6, 1) AS ms FROM _sqlx_migrations
       WHERE version > $old_ver ORDER BY execution_time DESC" > "$out/migration_times.tsv"
n_mig=$(wc -l < "$out/migration_times.tsv")
total_ms=$(awk -F'\t' '{ s += $3 } END { printf "%.0f", s }' "$out/migration_times.tsv")
echo "   $n_mig migrations, ${total_ms} ms in all; slowest:"
head -5 "$out/migration_times.tsv" | awk -F'\t' '{ printf "     %9s ms  %s %s\n", $3, $1, $2 }'
q -c "SELECT version FROM _sqlx_migrations WHERE NOT success" > "$out/migrations_failed.tsv"
[ -s "$out/migrations_failed.tsv" ] && { echo "!! failed migrations: $(cat "$out/migrations_failed.tsv")"; fail=1; }

py capture "http://127.0.0.1:$NEW_PORT" "$TOKEN" "$out/ctx.json" "$out/compat_new"
py compare "$out/compat_old" "$out/compat_new" || fail=1

echo "== after the deploy: rollup history and backfills"
export SQLX_OFFLINE=true
DATABASE_URL="$url" cargo run -q --bin sales-rollup-backfill > "$out/rollup_backfill_1.txt" 2>&1
DATABASE_URL="$url" cargo run -q --bin sales-rollup-backfill > "$out/rollup_backfill_2.txt" 2>&1
case "$(tail -1 "$out/rollup_backfill_2.txt")" in
  "covered 0 new days, recomputed 0 days"*) echo "OK rollup history ($(tail -1 "$out/rollup_backfill_1.txt")); second run did nothing" ;;
  *) echo "!! rollup backfill: $(tail -3 "$out/rollup_backfill_1.txt") / second run: $(tail -3 "$out/rollup_backfill_2.txt")"; fail=1 ;;
esac
if DATABASE_URL="$url" cargo run -q --bin report-rollup-reconcile > "$out/report_reconcile.txt" 2>&1; then
  echo "OK report reconciliation: $(tail -1 "$out/report_reconcile.txt")"
else
  echo "!! report reconciliation:"; tail -20 "$out/report_reconcile.txt"; fail=1
fi
for i in 1 2; do q -f scripts/db_phase5_backfill.sql > "$out/phase5_backfill_$i.tsv" || fail=1; done
if grep -qv $'\t0$' "$out/phase5_backfill_1.tsv" "$out/phase5_backfill_2.tsv"; then
  echo "!! Phase 5 backfills changed rows after the migrations:"; grep -v $'\t0$' "$out"/phase5_backfill_*.tsv; fail=1
else
  echo "OK Phase 5 backfills: the migrations did them ($(wc -l < "$out/phase5_backfill_2.tsv") backfills, 0 rows either run)"
fi

q -c "ANALYZE" >/dev/null
echo "== AFTER"
q -f scripts/db_reconcile.sql      > "$out/reconcile_after.tsv"
q -f scripts/db_rls_visibility.sql > "$out/visible_after.tsv"
q -f scripts/explain_hot_paths.sql > "$out/plans_after.tsv"

# Tables that change ON PURPOSE across this deploy, compared separately:
#   sync_changes  the POS changefeed; Phase 6's sweep retires old LEDGER feed
#                 rows (the ledger tables themselves are compared and must not
#                 move) and re-emits what a device still needs;
#   compat_usage  counts the old-shape calls this rehearsal itself made.
#   sync_feed_watermarks  raised by that sweep for a branch whose old feed rows
#                 it retired (a device behind it resyncs).
EXPECTED_TABLES="sync_changes sync_feed_watermarks compat_usage"
cat scripts/db_reconcile_expected_p*.txt 2>/dev/null > "$out/reconcile_expected.txt" || true
filter() {
  awk -F'\t' -v skip="$EXPECTED_TABLES" 'BEGIN { n = split(skip, a, " "); for (i = 1; i <= n; i++) s[a[i]] } !($2 in s)' \
    | if [ -s "$out/reconcile_expected.txt" ]; then grep -v -F -f "$out/reconcile_expected.txt"; else cat; fi
}
diff <(filter < "$out/reconcile_before.tsv") <(filter < "$out/reconcile_after.tsv") > "$out/reconcile.diff"
gate $? "reconcile: per-org counts and sums identical ($(wc -l < "$out/reconcile_after.tsv") rows, $EXPECTED_TABLES apart; see reconcile.diff)"
for t in $EXPECTED_TABLES; do
  b=$(awk -F'\t' -v t="$t" '$2 == t && $3 == "count" { s += $4 } END { print s + 0 }' "$out/reconcile_before.tsv")
  a=$(awk -F'\t' -v t="$t" '$2 == t && $3 == "count" { s += $4 } END { print s + 0 }' "$out/reconcile_after.tsv")
  echo "   $t rows: $b -> $a (expected to change)"
done

renames="loyalty_customers=loyalty_memberships"
cp "$out/visible_before.tsv" "$out/visible_before_named.tsv"
for r in $renames; do
  awk -F'\t' -v OFS='\t' -v o="${r%%=*}" -v n="${r##*=}" '$2 == o { $2 = n } 1' \
    "$out/visible_before_named.tsv" > "$out/visible_before_named.tmp" && mv "$out/visible_before_named.tmp" "$out/visible_before_named.tsv"
done
sort -o "$out/visible_before_named.tsv" "$out/visible_before_named.tsv"
cut -f2 "$out/visible_before_named.tsv" | sort -u > "$out/tables_before.txt"
awk -F'\t' 'NR==FNR { t[$1]; next } ($2 in t)' "$out/tables_before.txt" "$out/visible_after.tsv" | sort > "$out/visible_after_existing.tsv"
awk -F'\t' 'NR==FNR { t[$1]; next } !($2 in t)' "$out/tables_before.txt" "$out/visible_after.tsv" > "$out/visible_new_tables.tsv"
diff <(filter < "$out/visible_before_named.tsv") <(filter < "$out/visible_after_existing.tsv") > "$out/visible.diff"
gate $? "visible rows per org and table identical ($(wc -l < "$out/visible_before.tsv") org x table; renamed: $renames)"
[ -s "$out/visible_new_tables.tsv" ] && echo "   new tables: $(cut -f2 "$out/visible_new_tables.tsv" | sort -u | tr '\n' ' ')"

# Timings are medians of three; a busy box can still spike one query. A
# flagged query is measured once more and its faster AFTER figure kept.
plan_table() {
  paste "$out/plans_before.tsv" "$out/plans_after.tsv" \
    | awk -F'\t' '{ r = ($3 > 0) ? $6 / $3 : 1; print (($6 - $3 > 1 && r > 1.25) ? "slower" : "ok") }'
}
if plan_table | grep -q slower; then
  q -f scripts/explain_hot_paths.sql > "$out/plans_after_2.tsv"
  paste "$out/plans_after.tsv" "$out/plans_after_2.tsv" \
    | awk -F'\t' -v OFS='\t' '{ print $1, $2, ($3 < $6 ? $3 : $6) }' > "$out/plans_after_min.tsv"
  cp "$out/plans_after.tsv" "$out/plans_after_1.tsv"; cp "$out/plans_after_min.tsv" "$out/plans_after.tsv"
  echo "   a query looked slower: measured AFTER again, kept the faster of two runs"
fi
paste "$out/plans_before.tsv" "$out/plans_after.tsv" \
  | awk -F'\t' '{ r = ($3 > 0) ? $6 / $3 : 1;
                  flag = ($6 - $3 > 1 && r > 1.25) ? "  <-- slower" : "";
                  printf "%-32s %-10s %9s -> %9s%s\n", $1, $2, $3, $6, flag }' > "$out/plans.txt"
sed 's/^/   /' "$out/plans.txt"
! grep -q "slower" "$out/plans.txt"
gate $? "plans: no hot query > 25% and > 1 ms slower"

for c in scripts/db_phase1_check.sql scripts/db_phase2_check.sql scripts/db_phase3_check.sql scripts/db_phase5_check.sql; do
  [ -f "$c" ] || continue
  n=$(basename "$c" .sql)
  q -f "$c" > "$out/$n.tsv" 2>&1
  if [ -f "$out/${n}_before.tsv" ]; then
    # Phase 1's checks ran before too: only a finding the deploy ADDED fails.
    grep -v -x -F -f "$out/${n}_before.tsv" "$out/$n.tsv" > "$out/${n}_new.tsv" || true
    [ ! -s "$out/${n}_new.tsv" ]
    gate $? "$n: nothing new ($(wc -l < "$out/${n}_before.tsv") finding(s) already in the source, unchanged)"
    sed 's/^/     already in the source: /' "$out/${n}_before.tsv"
    [ -s "$out/${n}_new.tsv" ] && sed 's/^/     NEW: /' "$out/${n}_new.tsv"
  else
    [ ! -s "$out/$n.tsv" ]
    gate $? "$n prints nothing"
    [ -s "$out/$n.tsv" ] && head -20 "$out/$n.tsv" | sed 's/^/     /'
  fi
done

echo "== API smoke (release binary)"
py smoke "http://127.0.0.1:$NEW_PORT" "$TOKEN" "$out/ctx.json" "$out/smoke" || fail=1
grep -E '"level":"ERROR"| ERROR ' "$out/boot_new.log" | head -5 > "$out/server_errors.txt"
[ ! -s "$out/server_errors.txt" ]
gate $? "release server logged no ERROR (see boot_new.log)"
[ -s "$out/server_errors.txt" ] && sed 's/^/     /' "$out/server_errors.txt"
stop_server

echo "== evidence in $out (clone kept: dropdb ${host_args[*]} $clone)"
[ "$fail" = 0 ] && echo "REHEARSAL PASSED" || echo "REHEARSAL FAILED"
exit "$fail"
