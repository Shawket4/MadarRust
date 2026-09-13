#!/usr/bin/env bash
# Capture golden JSON of the backend's CURRENT shifts/tills API into
# tests/fixtures/legacy_till_api/ (see the README there).
#
# Builds a backend from a git ref (default HEAD) in a temp worktree, boots it on
# a DISPOSABLE copy of madar_prodcopy (createdb -T; madar_prodcopy is never
# written), with every sweep / outbound integration disabled, runs capture.py,
# then stops the server and drops the copy.
#
# Usage: scripts/legacy_till_golden/capture.sh [git-ref]
#   PORT=8090 SRC_DB=madar_prodcopy GOLDEN_DB=madar_golden KEEP_DB=1 ...
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
REF="${1:-HEAD}"
PORT="${PORT:-8090}"
SRC_DB="${SRC_DB:-madar_prodcopy}"
GOLDEN_DB="${GOLDEN_DB:-madar_golden}"
PGUSER="${PGUSER:-shawket}"
OUT="$REPO/tests/fixtures/legacy_till_api"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/legacy_till_golden.XXXXXX")"
SERVER_PID=""

cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
  git -C "$REPO" worktree remove --force "$WORK/wt" >/dev/null 2>&1 || true
  [ "${KEEP_DB:-0}" = 1 ] || dropdb -U "$PGUSER" --force --if-exists "$GOLDEN_DB" || true
}
trap cleanup EXIT

git -C "$REPO" worktree add --detach "$WORK/wt" "$REF" >/dev/null
(cd "$WORK/wt" && CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$REPO/target/legacy_golden}" cargo build -q --bin madar-rust)
BIN="${CARGO_TARGET_DIR:-$REPO/target/legacy_golden}/debug/madar-rust"

dropdb -U "$PGUSER" --force --if-exists "$GOLDEN_DB"
createdb -U "$PGUSER" -T "$SRC_DB" "$GOLDEN_DB"

set -a; source "$REPO/.env"; set +a
export DATABASE_URL="postgres://$PGUSER@localhost:5432/$GOLDEN_DB"
unset READ_DATABASE_URL DEV_DATABASE_URL SENTRY_DSN
export BIND_ADDR="127.0.0.1:$PORT" SENTRY_ENVIRONMENT=local
export WHATSAPP_SERVICE_URL=http://127.0.0.1:9 SHLINK_BASE_URL=http://127.0.0.1:9
export ATTENDANCE_SWEEP_ENABLED=false BOOKINGS_SWEEP_ENABLED=false DELIVERY_SWEEP_ENABLED=false \
       LOYALTY_BIRTHDAY_SWEEP_ENABLED=false LOYALTY_WINBACK_SWEEP_ENABLED=false \
       LOYALTY_PASS_REFRESH_ENABLED=false MADAR_DISABLE_AUTO_TRANSLATION=true
(cd "$WORK/wt" && exec "$BIN") >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 120); do
  curl -s -o /dev/null "http://127.0.0.1:$PORT/" && break
  kill -0 "$SERVER_PID" 2>/dev/null || { tail -30 "$WORK/server.log"; exit 1; }
  sleep 1
done

rm -f "$OUT"/*.json
BASE="http://127.0.0.1:$PORT" DB="$GOLDEN_DB" OUT="$OUT" PGUSER="$PGUSER" \
  python3 "$REPO/scripts/legacy_till_golden/capture.py"
echo "captured into $OUT (backend $(git -C "$REPO" rev-parse --short "$REF"))"
