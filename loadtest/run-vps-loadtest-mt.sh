#!/usr/bin/env bash
# MULTITENANT throwaway load test on the VPS. Same isolation/teardown guarantees
# as run-vps-loadtest.sh, but seeds N self-contained orgs (each its own branch +
# OPEN shift → different advisory-lock keys → writes PARALLELIZE) and spreads load
# across a token list (tenants.json) so it models many branches, not one hotspot.
#
#   LT_IMAGE=<img> TENANTS_N=1000 ./run-vps-loadtest-mt.sh
# tenants.json (a JSON array of {org,branch,shift,item,token}) must sit alongside.
set -uo pipefail
cd "$(dirname "$0")"
COMPOSE=(docker compose -f madar-loadtest.vps.yml)
NET=madar-loadtest_default
: "${LT_IMAGE:?set LT_IMAGE}"; TENANTS_N="${TENANTS_N:-1000}"
[ -f tenants.json ] || { echo "✗ tenants.json missing"; exit 1; }
TENANTS="$(cat tenants.json)"
RESULTS="$PWD/results"; mkdir -p "$RESULTS"
note(){ printf '\n\033[1;36m▶ %s\033[0m\n' "$*"; }

cleanup(){
  note "TEARDOWN: removing throwaway stack + test images"
  "${COMPOSE[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
  docker rmi -f postgres:15 grafana/k6:latest curlimages/curl:latest >/dev/null 2>&1 || true
  echo "✓ torn down. Prod still up:"; docker ps --format '   {{.Names}}' | grep -i madar-rust || true
}
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT

grep -q 'loadtest-db:5432/madar_loadtest' madar-loadtest.vps.yml || { echo "✗ not the throwaway DB — ABORT"; exit 1; }

note "starting throwaway Postgres"
"${COMPOSE[@]}" up -d loadtest-db
for _ in $(seq 1 40); do "${COMPOSE[@]}" exec -T loadtest-db pg_isready -U madar -d madar_loadtest >/dev/null 2>&1 && break; sleep 2; done

note "starting backend ($LT_IMAGE) → throwaway DB"
"${COMPOSE[@]}" up -d loadtest-backend
note "waiting for /health (migrations on boot)…"
ok=0; for _ in $(seq 1 90); do docker run --rm --network "$NET" curlimages/curl:latest -fsS http://loadtest-backend:8081/health >/dev/null 2>&1 && { ok=1; break; }; sleep 2; done
[ "$ok" = 1 ] || { echo "✗ backend unhealthy"; "${COMPOSE[@]}" logs --tail 40 loadtest-backend; exit 1; }
echo "✓ healthy"

note "seeding base fixture + $TENANTS_N tenants"
"${COMPOSE[@]}" exec -T loadtest-db psql -U madar -d madar_loadtest -v ON_ERROR_STOP=1 -q < seed_fuzz.sql     || exit 1
"${COMPOSE[@]}" exec -T loadtest-db psql -U madar -d madar_loadtest -v ON_ERROR_STOP=1 -q < seed_loadtest.sql || exit 1
"${COMPOSE[@]}" exec -T loadtest-db psql -U madar -d madar_loadtest -v ON_ERROR_STOP=1 -v tenants="$TENANTS_N" -q < seed_multitenant.sql || exit 1
seeded_branches=$(docker exec madar-loadtest-loadtest-db-1 psql -U madar -d madar_loadtest -tAc "SELECT count(*) FROM branches" 2>/dev/null)
echo "✓ seeded — $seeded_branches branches in the throwaway DB"

run_k6(){ # name profile [extra -e ...]
  local name="$1" profile="$2"; shift 2
  note "k6: $name (profile=$profile $*)  → results/$name.txt"
  docker run --rm --network "$NET" \
    -e BASE_URL=http://loadtest-backend:8081 -e PROFILE="$profile" \
    -e WRITE_RATIO="${WRITE_RATIO:-0.15}" -e TENANTS="$TENANTS" "$@" \
    -v "$PWD/k6/load.js:/load.js:ro" \
    grafana/k6:latest run --no-color /load.js 2>&1 | tee "$RESULTS/$name.txt"
  sleep 4
}

run_k6 smoke.mt         smoke
run_k6 ramp.mt          ramp
run_k6 posday-85.mt     pos-day  -e RATE=85  -e DURATION=2m -e MAX_VUS=300
run_k6 posday-170.mt    pos-day  -e RATE=170 -e DURATION=2m -e MAX_VUS=400

note "DONE — key metrics"
for n in smoke.mt ramp.mt posday-85.mt posday-170.mt; do
  echo "── $n ──"
  grep -aE "http_reqs|http_req_duration|http_req_failed|orders_created|order_create_ms|vus_max" "$RESULTS/$n.txt" | sed -E "s/\x1b\[[0-9;]*m//g" | grep -avE "http_req_duration$|http_req_failed$"
done
echo "✓ multitenant load test complete"
