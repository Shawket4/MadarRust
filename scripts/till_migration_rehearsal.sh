#!/usr/bin/env bash
# Tills-rework migration rehearsal on a COPY of madar_prodcopy.
#
#   1. createdb -T madar_prodcopy madar_rehearsal_<ts>   (the source is never written)
#   2. baseline invariants (scripts/till_migration/invariants_before.sql, OLD names)
#   3. apply pending migrations of the CURRENT checkout:
#        --migrate-with binary  (default) boot target/debug/madar-rust, which runs
#                               sqlx::migrate! on start, and stop it once serving
#        --migrate-with sqlx    `sqlx migrate run --source migrations`
#   4. post invariants (scripts/till_migration/invariants_after.sql, NEW names —
#      TODO placeholders until the migration agent fills them) and diff with 2
#   5. --down FILE: run the down script, then re-run invariants_before.sql and
#      diff against the baseline (round trip must be lossless)
#
# Usage:
#   scripts/till_migration_rehearsal.sh [--down scripts/till_migration/down.sql]
#        [--migrate-with binary|sqlx] [--keep] [--no-build] [--port 8091]
# Output: $OUT_DIR (default target/till_rehearsal/<ts>/) holds before.txt,
# after.txt, after.diff, [down.txt, down.diff], migrate.log. Exit 1 on any diff.
# The rehearsal DB is dropped at the end unless --keep.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SRC_DB="${SRC_DB:-madar_prodcopy}"
PGUSER="${PGUSER:-shawket}"
PGHOST="${PGHOST:-localhost}"
TS="$(date +%Y%m%d%H%M%S)"
DB="madar_rehearsal_$TS"
DOWN=""
MIGRATE_WITH=binary
KEEP=0
BUILD=1
PORT=8091

while [ $# -gt 0 ]; do
  case "$1" in
    --down) DOWN="$(cd "$(dirname "$2")" && pwd)/$(basename "$2")"; shift 2 ;;
    --migrate-with) MIGRATE_WITH="$2"; shift 2 ;;
    --keep) KEEP=1; shift ;;
    --no-build) BUILD=0; shift ;;
    --port) PORT="$2"; shift 2 ;;
    -h|--help) sed -n 2,22p "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

OUT_DIR="${OUT_DIR:-$REPO/target/till_rehearsal/$TS}"
mkdir -p "$OUT_DIR"
URL="postgres://$PGUSER@$PGHOST:5432/$DB"
PSQL=(psql -X -q -U "$PGUSER" -h "$PGHOST" -v ON_ERROR_STOP=1)
SERVER_PID=""

cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
  if [ "$KEEP" = 1 ]; then echo "kept database $DB"; else dropdb -U "$PGUSER" -h "$PGHOST" --force --if-exists "$DB" || true; fi
}
trap cleanup EXIT

invariants() { # <sql file> <out file>
  "${PSQL[@]}" -d "$DB" -f "$1" >"$2"
  echo "  $(wc -l <"$2" | tr -d ' ') invariant lines -> $2"
}

echo "==> cloning $SRC_DB -> $DB"
createdb -U "$PGUSER" -h "$PGHOST" -T "$SRC_DB" "$DB"

echo "==> baseline invariants"
invariants "$REPO/scripts/till_migration/invariants_before.sql" "$OUT_DIR/before.txt"
"${PSQL[@]}" -d "$DB" -Atc "SELECT version FROM _sqlx_migrations ORDER BY version" >"$OUT_DIR/migrations_before.txt"

echo "==> migrating ($MIGRATE_WITH)"
case "$MIGRATE_WITH" in
  sqlx)
    (cd "$REPO" && DATABASE_URL="$URL" sqlx migrate run --source migrations) >"$OUT_DIR/migrate.log" 2>&1
    ;;
  binary)
    if [ "$BUILD" = 1 ]; then (cd "$REPO" && cargo build -q --bin madar-rust); fi
    BIN="${CARGO_TARGET_DIR:-$REPO/target}/debug/madar-rust"
    (
      set -a; source "$REPO/.env"; set +a
      export DATABASE_URL="$URL" BIND_ADDR="127.0.0.1:$PORT" SENTRY_ENVIRONMENT=local
      unset READ_DATABASE_URL DEV_DATABASE_URL SENTRY_DSN
      export WHATSAPP_SERVICE_URL=http://127.0.0.1:9 SHLINK_BASE_URL=http://127.0.0.1:9
      export ATTENDANCE_SWEEP_ENABLED=false BOOKINGS_SWEEP_ENABLED=false DELIVERY_SWEEP_ENABLED=false \
             LOYALTY_BIRTHDAY_SWEEP_ENABLED=false LOYALTY_WINBACK_SWEEP_ENABLED=false \
             LOYALTY_PASS_REFRESH_ENABLED=false MADAR_DISABLE_AUTO_TRANSLATION=true
      cd "$REPO" && exec "$BIN"
    ) >"$OUT_DIR/migrate.log" 2>&1 &
    SERVER_PID=$!
    for _ in $(seq 1 180); do
      grep -q "starting service" "$OUT_DIR/migrate.log" && break
      kill -0 "$SERVER_PID" 2>/dev/null || { echo "backend exited during migration:"; tail -30 "$OUT_DIR/migrate.log"; exit 1; }
      sleep 1
    done
    grep -q "starting service" "$OUT_DIR/migrate.log" || { echo "backend never came up"; exit 1; }
    kill "$SERVER_PID"; wait "$SERVER_PID" 2>/dev/null || true; SERVER_PID=""
    ;;
  *) echo "--migrate-with must be binary|sqlx" >&2; exit 2 ;;
esac
"${PSQL[@]}" -d "$DB" -Atc "SELECT version FROM _sqlx_migrations ORDER BY version" >"$OUT_DIR/migrations_after.txt"
echo "  applied: $(comm -13 "$OUT_DIR/migrations_before.txt" "$OUT_DIR/migrations_after.txt" | tr '\n' ' ')"

status=0
echo "==> post-migration invariants"
invariants "$REPO/scripts/till_migration/invariants_after.sql" "$OUT_DIR/after.txt"
if diff -u "$OUT_DIR/before.txt" "$OUT_DIR/after.txt" >"$OUT_DIR/after.diff"; then
  echo "  OK: invariants identical"
else
  echo "  FAIL: invariants differ ($(grep -c '^[-+][^-+]' "$OUT_DIR/after.diff") lines) -> $OUT_DIR/after.diff"; status=1
fi

if [ -n "$DOWN" ]; then
  echo "==> down script $DOWN"
  "${PSQL[@]}" -d "$DB" -1 -f "$DOWN" >"$OUT_DIR/down.log" 2>&1 || { echo "  FAIL: down script errored"; tail -20 "$OUT_DIR/down.log"; exit 1; }
  invariants "$REPO/scripts/till_migration/invariants_before.sql" "$OUT_DIR/down.txt"
  if diff -u "$OUT_DIR/before.txt" "$OUT_DIR/down.txt" >"$OUT_DIR/down.diff"; then
    echo "  OK: round trip lossless"
  else
    echo "  FAIL: down differs from baseline -> $OUT_DIR/down.diff"; status=1
  fi
fi

echo "==> artifacts in $OUT_DIR"
exit $status
