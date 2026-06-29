#!/usr/bin/env bash
#
# Local schema-driven API fuzzing with Schemathesis.
#
# Boots the REAL server binary against a DISPOSABLE `madar_fuzz` database, seeds
# a minimal fixture, mints a JWT, re-exports the current OpenAPI spec, and fuzzes
# every endpoint for 5xx / schema-conformance violations. The fuzz DB is dropped
# and recreated each run, and external integrations (OSRM, WhatsApp) are left
# UNSET so no real outbound calls happen.
#
# Prereqs: schemathesis (`pipx install schemathesis` or `uv tool install schemathesis`),
# sqlx-cli, a local Postgres. Usage: scripts/api-fuzz.sh
set -euo pipefail
cd "$(dirname "$0")/.."
REPO="$PWD"

# ── Config (override via env) ────────────────────────────────────────────────
PG_HOST="${PG_HOST:-localhost}"
PG_PORT="${PG_PORT:-5432}"
PG_USER="${PG_USER:-shawket}"
FUZZ_DB="${FUZZ_DB:-madar_fuzz}"
FUZZ_DATABASE_URL="${FUZZ_DATABASE_URL:-postgres://${PG_USER}@${PG_HOST}:${PG_PORT}/${FUZZ_DB}}"
ADMIN_DATABASE_URL="${ADMIN_DATABASE_URL:-postgres://${PG_USER}@${PG_HOST}:${PG_PORT}/postgres}"
JWT_SECRET="${JWT_SECRET:-fuzz-secret-not-for-prod}"
BIND_ADDR="${BIND_ADDR:-127.0.0.1:8099}"
BASE_URL="http://${BIND_ADDR}"
MAX_EXAMPLES="${MAX_EXAMPLES:-50}"
OUT="${OUT:-$REPO/fuzz-out}"

# ── Guardrail: refuse anything that isn't the throwaway fuzz DB ───────────────
case "$FUZZ_DATABASE_URL" in
  *madar_fuzz*) : ;;
  *) echo "REFUSING: FUZZ_DATABASE_URL must point at a 'madar_fuzz' DB, got: $FUZZ_DATABASE_URL" >&2; exit 1 ;;
esac
case "$FUZZ_DATABASE_URL" in
  *madar_dev*|*madar_prod*|*@*prod*) echo "REFUSING: looks like a real DB: $FUZZ_DATABASE_URL" >&2; exit 1 ;;
esac

# Resolve the Schemathesis CLI: PATH first, then a local ./.fuzzvenv (created by
# `python3 -m venv .fuzzvenv && .fuzzvenv/bin/pip install schemathesis`).
ST="${ST:-$(command -v st || true)}"
[ -z "$ST" ] && [ -x "$REPO/.fuzzvenv/bin/st" ] && ST="$REPO/.fuzzvenv/bin/st"
[ -n "$ST" ] && [ -x "$ST" ] || { echo "schemathesis not found — 'pipx install schemathesis' or create ./.fuzzvenv" >&2; exit 1; }
pg_isready -h "$PG_HOST" -p "$PG_PORT" >/dev/null 2>&1 || { echo "Postgres not reachable at $PG_HOST:$PG_PORT" >&2; exit 1; }

mkdir -p "$OUT"
SERVER_PID=""
RUN_DIR="$(mktemp -d)"   # run the server here so its dotenv() finds no real .env
cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
  # Belt-and-suspenders: make sure no fuzz server survives to hold the DB open.
  pkill -f "target/debug/madar-rust" 2>/dev/null || true
  rm -rf "$RUN_DIR"
  psql "$ADMIN_DATABASE_URL" -c "DROP DATABASE IF EXISTS ${FUZZ_DB} WITH (FORCE);" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "▶ (re)creating $FUZZ_DB"
psql "$ADMIN_DATABASE_URL" -c "DROP DATABASE IF EXISTS ${FUZZ_DB} WITH (FORCE);" >/dev/null
psql "$ADMIN_DATABASE_URL" -c "CREATE DATABASE ${FUZZ_DB};" >/dev/null

# Pre-rebrand migrations GRANT to a legacy 'sufrix' role (cluster-global). Create
# it (idempotent, NOLOGIN) or `sqlx migrate run` aborts with SQLSTATE 42704.
echo "▶ ensuring legacy 'sufrix' grant-target role exists"
psql "$ADMIN_DATABASE_URL" -qtAc "DO \$do\$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname='sufrix') THEN CREATE ROLE sufrix NOLOGIN; END IF; END \$do\$;" >/dev/null 2>&1 || true

echo "▶ applying migrations"
DATABASE_URL="$FUZZ_DATABASE_URL" sqlx migrate run --source "$REPO/migrations" >/dev/null

echo "▶ seeding fixture"
psql "$FUZZ_DATABASE_URL" -v ON_ERROR_STOP=1 -f "$REPO/scripts/seed_fuzz.sql" >/dev/null

echo "▶ building server + token bin (debug — release uses slow LTO and isn't needed here)"
cargo build --bin madar-rust --bin fuzz-token >/dev/null 2>&1
ORG_TOKEN="$(JWT_SECRET="$JWT_SECRET" "$REPO/target/debug/fuzz-token" org-admin)"
SUPER_TOKEN="$(JWT_SECRET="$JWT_SECRET" "$REPO/target/debug/fuzz-token" super-admin)"

echo "▶ exporting current OpenAPI spec (adapts to the live endpoint surface)"
SPEC="$OUT/openapi.json"
cargo run --quiet --bin export-openapi "$SPEC" >/dev/null

echo "▶ booting server on $BASE_URL (rate limiting OFF, external integrations UNSET)"
# Run from RUN_DIR so dotenvy() loads no real .env; pass only fuzz-safe env.
# OSRM_URL / WHATSAPP_SERVICE_URL / WHATSAPP_AUTH_HEADER intentionally absent.
# `exec` so SERVER_PID is the real binary (not the subshell); redirect its output
# to a log so the background server never holds this script's stdout pipe open.
( cd "$RUN_DIR" && exec env -i \
    PATH="$PATH" HOME="$HOME" \
    DATABASE_URL="$FUZZ_DATABASE_URL" \
    JWT_SECRET="$JWT_SECRET" \
    BIND_ADDR="$BIND_ADDR" \
    UPLOADS_DIR="$RUN_DIR/uploads" \
    MADAR_DISABLE_RATE_LIMIT=1 \
    MADAR_DISABLE_AUTO_TRANSLATION=1 \
    "$REPO/target/debug/madar-rust" ) >"$OUT/server.log" 2>&1 &
SERVER_PID=$!

echo "▶ waiting for /health"
for _ in $(seq 1 60); do
  if curl -fsS "$BASE_URL/health" >/dev/null 2>&1; then break; fi
  sleep 0.5
done
curl -fsS "$BASE_URL/health" >/dev/null || { echo "server did not become healthy" >&2; exit 1; }

# Common skips: the SSE stream (hangs the fuzzer); the WhatsApp relay (needs the
# external gateway — returns 503 by design when WHATSAPP_SERVICE_URL is unset, as
# it is here); and the multipart upload endpoints (Schemathesis can't generate
# `bytes` bodies). DELETE is excluded from the read/write passes and fuzzed
# separately last (it tears the fixture down).
EXCLUDE=(
  --exclude-path "/delivery-orders/stream"
  --exclude-path "/menu-items/{id}/image"
  --exclude-path "/orgs/{id}/logo"
  # External-dependency endpoints that correctly return 503 when their service is
  # UNSET (as it deliberately is here): the WhatsApp gateway and the Shlink/QR
  # stack. not_a_server_error counts 503 as a 5xx, so they'd be false positives.
  # Listed as EXACT paths — Schemathesis v4's --exclude-path-regex did not reliably
  # match these, but exact --exclude-path does.
  --exclude-path "/whatsapp/status"
  --exclude-path "/whatsapp/pair"
  --exclude-path "/whatsapp/logout"
  --exclude-path "/whatsapp/pause"
  --exclude-path "/qr/links"
  --exclude-path "/orgs/{id}/qr"
  --exclude-path "/branches/{id}/qr"
  --exclude-path "/branches/{id}/tables/{tid}/qr"
  --exclude-path "/delivery-orders/{id}/qr"
)

CHECKS="not_a_server_error,status_code_conformance,content_type_conformance,response_schema_conformance"
run_pass() {
  local name="$1"; shift
  echo "▶ schemathesis pass: $name"
  "$ST" run "$SPEC" --url "$BASE_URL" "$@" "${EXCLUDE[@]}" \
    --checks "$CHECKS" --max-examples "$MAX_EXAMPLES" --workers 1 --continue-on-failure \
    --report junit --report-junit-path "$OUT/$name.junit.xml" \
    2>&1 | tee "$OUT/$name.log" || true
}

# Non-destructive passes first (skip DELETE so the fixture survives).
run_pass org-admin   --exclude-method DELETE -H "Authorization: Bearer $ORG_TOKEN"
run_pass super-admin --exclude-method DELETE -H "Authorization: Bearer $SUPER_TOKEN" -H "X-Org-Id: 00000000-0000-0000-0000-000000000001"

# Dedicated DELETE pass LAST (destructive). Reseed so the fixture exists, then
# fuzz only DELETE — any 5xx here is a real bug.
echo "▶ reseeding for DELETE pass"
psql "$FUZZ_DATABASE_URL" -v ON_ERROR_STOP=1 -f "$REPO/scripts/seed_fuzz.sql" >/dev/null 2>&1 || true
run_pass delete --include-method DELETE -H "Authorization: Bearer $SUPER_TOKEN" -H "X-Org-Id: 00000000-0000-0000-0000-000000000001"

echo "✓ done — reports in $OUT/"
