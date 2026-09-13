#!/usr/bin/env bash
# Capture golden JSON of a backend's shifts/tills API into
# tests/fixtures/legacy_till_api/ (see the README there).
#
# Builds the backend from a git ref (default 097a653, the last pre-rename
# commit) in a temp worktree, boots it on a DISPOSABLE empty database on the
# throwaway test cluster (:5433), which it migrates on boot; loads seed.sql;
# runs scenario.json via capture.py; stops the server and drops the database.
#
# Usage: scripts/legacy_till_golden/capture.sh [git-ref]
#   PORT=8090 PGPORT=5433 GOLDEN_DB=madar_golden TARGET=<cargo target dir> KEEP_DB=1 ...
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
REF="${1:-097a653}"
PORT="${PORT:-8090}"
PGPORT="${PGPORT:-5433}"
GOLDEN_DB="${GOLDEN_DB:-madar_golden}"
PGUSER="${PGUSER:-shawket}"
OUT="$REPO/tests/fixtures/legacy_till_api"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/legacy_till_golden.XXXXXX")"
TARGET="${TARGET:-$WORK/target}"
SERVER_PID=""

cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
  git -C "$REPO" worktree remove --force "$WORK/wt" >/dev/null 2>&1 || true
  [ "${KEEP_DB:-0}" = 1 ] || dropdb -p "$PGPORT" -U "$PGUSER" --force --if-exists "$GOLDEN_DB" || true
}
trap cleanup EXIT

dropdb -p "$PGPORT" -U "$PGUSER" --force --if-exists "$GOLDEN_DB"
createdb -p "$PGPORT" -U "$PGUSER" -T template0 "$GOLDEN_DB"
export DATABASE_URL="postgres://$PGUSER@localhost:$PGPORT/$GOLDEN_DB"

git -C "$REPO" worktree add --detach "$WORK/wt" "$REF" >/dev/null
# The sqlx compile-time checks need the ref's schema.
(cd "$WORK/wt" && sqlx migrate run >/dev/null && CARGO_TARGET_DIR="$TARGET" cargo build -q --bin madar-rust)
BIN="$TARGET/debug/madar-rust"

set -a; source "$REPO/.env"; set +a
export DATABASE_URL="postgres://$PGUSER@localhost:$PGPORT/$GOLDEN_DB"
export JWT_SECRET="legacy-golden-capture-secret"
unset READ_DATABASE_URL DEV_DATABASE_URL SENTRY_DSN
export BIND_ADDR="127.0.0.1:$PORT" SENTRY_ENVIRONMENT=local
export WHATSAPP_SERVICE_URL=http://127.0.0.1:9 SHLINK_BASE_URL=http://127.0.0.1:9
export ATTENDANCE_SWEEP_ENABLED=false BOOKINGS_SWEEP_ENABLED=false DELIVERY_SWEEP_ENABLED=false \
       LOYALTY_BIRTHDAY_SWEEP_ENABLED=false LOYALTY_WINBACK_SWEEP_ENABLED=false \
       LOYALTY_PASS_REFRESH_ENABLED=false MADAR_DISABLE_AUTO_TRANSLATION=1 MADAR_DISABLE_RATE_LIMIT=1
(cd "$WORK/wt" && exec "$BIN") >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 120); do
  curl -s -o /dev/null "http://127.0.0.1:$PORT/" && break
  kill -0 "$SERVER_PID" 2>/dev/null || { tail -30 "$WORK/server.log"; exit 1; }
  sleep 1
done

psql -p "$PGPORT" -U "$PGUSER" -d "$GOLDEN_DB" -v ON_ERROR_STOP=1 -q -f "$REPO/scripts/legacy_till_golden/seed.sql"

rm -f "$OUT"/*.json
BASE="http://127.0.0.1:$PORT" OUT="$OUT" python3 "$REPO/scripts/legacy_till_golden/capture.py" \
  || { tail -40 "$WORK/server.log"; exit 1; }
echo "captured into $OUT (backend $(git -C "$REPO" rev-parse --short "$REF"))"
