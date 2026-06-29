#!/usr/bin/env bash
#
# Local load test that mimics the production VPS (1 vCPU / 4 GB, Postgres
# co-resident) using Docker + k6. See docker-compose.loadtest.yml for the
# fidelity model and loadtest/README.md for the full story.
#
#   scripts/loadtest.sh                       # smoke (single tenant)
#   scripts/loadtest.sh ramp                  # ramp to the capacity knee
#   scripts/loadtest.sh all                   # every profile, in turn
#   scripts/loadtest.sh ramp --multitenant    # spread load across N seeded orgs
#   scripts/loadtest.sh ramp --multitenant=50 # …with 50 tenants
#   scripts/loadtest.sh ramp --cache          # turn on the per-org menu cache (#5)
#   scripts/loadtest.sh ramp --pgbouncer      # front Postgres with PgBouncer (#3)
#   scripts/loadtest.sh ramp --multitenant --cache   # combine
#   scripts/loadtest.sh ramp --keep           # leave the stack up afterwards
#   scripts/loadtest.sh down                  # tear the stack down
#
# Env knobs (all optional):
#   BACKEND_CPUS / DB_CPUS   throttle the shared core (default 1.0; lower → weaker vCPU)
#   DB_MAX_CONNECTIONS       app pool size (default 10; multitenant bumps to 40)
#   MENU_CACHE_TTL_SECS      menu cache TTL when --cache (default 15)
#   WRITE_RATIO              fraction of iters that POST an order (default 0.15)
#   RATE / VUS / DURATION / MAX_VUS   per-profile overrides (see load.js)
set -uo pipefail
cd "$(dirname "$0")/.."
REPO="$PWD"

COMPOSE_BASE=(docker compose -f docker-compose.loadtest.yml --profile pgbouncer)
DB_URL="postgres://madar:madar@localhost:55432/madar_loadtest" # direct Postgres (host)
BASE_URL="http://localhost:8085"
SECRET="${LOADTEST_JWT_SECRET:-loadtest-secret-not-for-prod}"
HOST_DB_URL="${DATABASE_URL:-postgres://shawket@localhost:5432/madar}" # build-time only (sqlx macros)
RESULTS="$REPO/loadtest/results"
PROFILES_ALL=(smoke ramp soak spike pos-day)

note(){ printf '\n\033[1;36m▶ %s\033[0m\n' "$*"; }
die(){ printf '\033[1;31m✗ %s\033[0m\n' "$*" >&2; exit 1; }
teardown(){ note "tearing down"; "${COMPOSE_BASE[@]}" down -v --remove-orphans >/dev/null 2>&1 || true; }

# ── subcommand: down ──────────────────────────────────────────────────────────
if [ "${1:-}" = "down" ]; then teardown; echo "✓ stack down"; exit 0; fi

# ── args ──────────────────────────────────────────────────────────────────────
PROFILE="${1:-smoke}"
KEEP=0; BUILD=1; MT=0; MT_N=25; PGB=0; CACHE=0
for a in "${@:2}"; do case "$a" in
  --keep) KEEP=1;; --no-build) BUILD=0;;
  --multitenant) MT=1;; --multitenant=*) MT=1; MT_N="${a#*=}";;
  --pgbouncer) PGB=1;; --cache) CACHE=1;;
  *) die "unknown flag: $a";;
esac; done
if [ "$PROFILE" != "all" ] && ! printf '%s\n' "${PROFILES_ALL[@]}" | grep -qx "$PROFILE"; then
  die "unknown profile '$PROFILE' (smoke|ramp|soak|spike|pos-day|all)"
fi

command -v docker >/dev/null || die "docker not found"
docker info >/dev/null 2>&1 || die "docker daemon not reachable (start Docker / colima)"
command -v k6 >/dev/null || die "k6 not found (brew install k6)"
command -v psql >/dev/null || die "psql not found"

# ── perf env (substituted into the backend service by docker compose) ─────────
USER_POOL="${DB_MAX_CONNECTIONS:-}" # remember whether the user set it explicitly
export DB_MAX_CONNECTIONS="${DB_MAX_CONNECTIONS:-10}"
export DB_STATEMENT_CACHE_CAPACITY="${DB_STATEMENT_CACHE_CAPACITY:-100}"
export MENU_CACHE_TTL_SECS="${MENU_CACHE_TTL_SECS:-0}"
export LT_DB_HOST=loadtest-db LT_DB_PORT=5432
# Multitenant defaults to a larger pool — but only when the user didn't pin one.
[ "$MT" = 1 ]    && [ -z "$USER_POOL" ] && export DB_MAX_CONNECTIONS=40
[ "$CACHE" = 1 ] && [ "$MENU_CACHE_TTL_SECS" = 0 ] && export MENU_CACHE_TTL_SECS=15
if [ "$PGB" = 1 ]; then
  export LT_DB_HOST=loadtest-pgbouncer LT_DB_PORT=6432 DB_STATEMENT_CACHE_CAPACITY=0
  command -v sqlx >/dev/null || die "--pgbouncer needs sqlx-cli to pre-migrate (cargo install sqlx-cli)"
fi
note "config: profile=$PROFILE multitenant=$([ $MT = 1 ] && echo $MT_N || echo no) pgbouncer=$([ $PGB = 1 ] && echo yes || echo no) cache=$([ $CACHE = 1 ] && echo ${MENU_CACHE_TTL_SECS}s || echo no) pool=$DB_MAX_CONNECTIONS"

# ── 1. build the backend image ────────────────────────────────────────────────
if [ "$BUILD" = 1 ]; then
  note "building backend image (first build compiles release — several minutes)"
  "${COMPOSE_BASE[@]}" build loadtest-backend || die "image build failed"
fi
# fuzz-token mints per-org tokens; rebuild (it changed for multitenant args)
note "building token minter"
DATABASE_URL="$HOST_DB_URL" cargo build -q --bin fuzz-token || die "fuzz-token build failed"

# ── 2. start Postgres, wait healthy ───────────────────────────────────────────
note "starting Postgres (capped: 1 shared core)"
"${COMPOSE_BASE[@]}" up -d loadtest-db
for _ in $(seq 1 60); do pg_isready -h localhost -p 55432 -U madar >/dev/null 2>&1 && break; sleep 1; done
pg_isready -h localhost -p 55432 -U madar >/dev/null 2>&1 || die "Postgres not reachable on :55432"

# ── 2b. PgBouncer (opt-in): start it + PRE-MIGRATE directly (sqlx's migration
#        advisory lock is session-scoped and would misbehave through txn pooling).
if [ "$PGB" = 1 ]; then
  note "starting PgBouncer + pre-migrating directly against Postgres"
  "${COMPOSE_BASE[@]}" up -d loadtest-pgbouncer
  DATABASE_URL="$DB_URL" sqlx migrate run --source "$REPO/migrations" >/dev/null 2>&1 || die "pre-migration failed"
  for _ in $(seq 1 30); do pg_isready -h localhost -p 56432 >/dev/null 2>&1 && break; sleep 1; done
fi

# ── 3. start backend, wait for /health ────────────────────────────────────────
note "starting backend (DATABASE_URL → ${LT_DB_HOST}:${LT_DB_PORT})"
"${COMPOSE_BASE[@]}" up -d loadtest-backend
for _ in $(seq 1 90); do curl -fsS "$BASE_URL/health" >/dev/null 2>&1 && break; sleep 1; done
curl -fsS "$BASE_URL/health" >/dev/null 2>&1 || { "${COMPOSE_BASE[@]}" logs --tail 40 loadtest-backend; die "backend did not become healthy"; }

# ── 4. seed fixtures (schema now exists) ──────────────────────────────────────
note "seeding fixture"
psql "$DB_URL" -v ON_ERROR_STOP=1 -q -f "$REPO/scripts/seed_fuzz.sql" || die "seed_fuzz failed"
psql "$DB_URL" -v ON_ERROR_STOP=1 -q -f "$REPO/loadtest/seed_loadtest.sql" || die "seed_loadtest failed"
if [ "$MT" = 1 ]; then
  note "seeding $MT_N tenants"
  psql "$DB_URL" -v ON_ERROR_STOP=1 -v tenants="$MT_N" -q -f "$REPO/loadtest/seed_multitenant.sql" || die "multitenant seed failed"
fi

# ── 5. mint token(s) ──────────────────────────────────────────────────────────
TOKEN="$(JWT_SECRET="$SECRET" "$REPO/target/debug/fuzz-token" org-admin)"
[ -n "$TOKEN" ] || die "failed to mint token"
TENANTS=""
if [ "$MT" = 1 ]; then
  note "minting $MT_N per-org tokens + building tenant list"
  TENANTS="["
  for i in $(seq 1 "$MT_N"); do
    sfx="$(printf '%012d' "$i")"
    org="0a000000-0000-0000-0000-$sfx"; branch="0b000000-0000-0000-0000-$sfx"
    shift_id="0d000000-0000-0000-0000-$sfx"; item="0f000000-0000-0000-0000-$sfx"
    user="0c000000-0000-0000-0000-$sfx"
    tok="$(JWT_SECRET="$SECRET" "$REPO/target/debug/fuzz-token" org-admin "$user" "$org")"
    [ "$i" -gt 1 ] && TENANTS+=","
    TENANTS+="{\"org\":\"$org\",\"branch\":\"$branch\",\"shift\":\"$shift_id\",\"item\":\"$item\",\"token\":\"$tok\"}"
  done
  TENANTS+="]"
fi

# ── 6. run k6 for the chosen profile(s) ───────────────────────────────────────
mkdir -p "$RESULTS"
TAG="$([ $MT = 1 ] && echo .mt || echo "")$([ $PGB = 1 ] && echo .pgb || echo "")$([ $CACHE = 1 ] && echo .cache || echo "")"
run_one(){
  local p="$1"
  note "k6 profile: $p$TAG   (results → loadtest/results/$p$TAG.txt)"
  k6 run \
    -e BASE_URL="$BASE_URL" -e TOKEN="$TOKEN" -e PROFILE="$p" \
    -e WRITE_RATIO="${WRITE_RATIO:-0.15}" \
    ${TENANTS:+-e TENANTS="$TENANTS"} \
    ${RATE:+-e RATE="$RATE"} ${VUS:+-e VUS="$VUS"} \
    ${DURATION:+-e DURATION="$DURATION"} ${MAX_VUS:+-e MAX_VUS="$MAX_VUS"} \
    "$REPO/loadtest/k6/load.js" 2>&1 | tee "$RESULTS/$p$TAG.txt"
}
if [ "$PROFILE" = all ]; then
  for p in "${PROFILES_ALL[@]}"; do run_one "$p"; sleep 3; done
else
  run_one "$PROFILE"
fi

# ── 7. teardown unless --keep ─────────────────────────────────────────────────
if [ "$KEEP" = 1 ]; then
  note "stack left UP (--keep). Tear down with: scripts/loadtest.sh down"
else
  teardown
fi
echo "✓ load test complete"
