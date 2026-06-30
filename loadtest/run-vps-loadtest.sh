#!/usr/bin/env bash
# Runs the THROWAWAY load test on the VPS. Container-only: nothing is installed
# on the host, nothing is published on a host port, and the prod Postgres/`madar`
# DB is never referenced. Always tears the stack down on exit (trap), and removes
# the test-only images to reclaim storage. Prod containers are untouched.
#
#   LT_IMAGE=<prod image> TOKEN=<jwt> ./run-vps-loadtest.sh [profile...]
#   profiles default to: smoke ramp soak spike pos-day   (KEEP=1 skips teardown)
set -uo pipefail
cd "$(dirname "$0")"
COMPOSE=(docker compose -f madar-loadtest.vps.yml)
NET=madar-loadtest_default
PROFILES=("${@:-}"); [ -z "${PROFILES[*]}" ] && PROFILES=(smoke ramp soak spike pos-day)
: "${LT_IMAGE:?set LT_IMAGE}"; : "${TOKEN:?set TOKEN}"
RESULTS="$PWD/results"; mkdir -p "$RESULTS"
note(){ printf '\n\033[1;36m▶ %s\033[0m\n' "$*"; }

cleanup(){
  note "TEARDOWN: removing throwaway stack + test images"
  "${COMPOSE[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
  # remove test-only images (prod uses host Postgres + the madarrust image — kept)
  docker rmi -f postgres:15 grafana/k6:latest curlimages/curl:latest >/dev/null 2>&1 || true
  echo "✓ torn down. Prod containers still up:"; docker ps --format '   {{.Names}} ({{.Status}})' | grep -i madar-rust || true
}
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT

# ── 0. SAFETY ASSERT: compose must point at the throwaway DB, not prod ─────────
grep -q 'loadtest-db:5432/madar_loadtest' madar-loadtest.vps.yml || { echo "✗ compose DB target is not the throwaway DB — ABORT"; exit 1; }

# ── 1. throwaway Postgres ─────────────────────────────────────────────────────
note "starting throwaway Postgres (capped 384m, no host port)"
LT_IMAGE="$LT_IMAGE" "${COMPOSE[@]}" up -d loadtest-db
for _ in $(seq 1 40); do "${COMPOSE[@]}" exec -T loadtest-db pg_isready -U madar -d madar_loadtest >/dev/null 2>&1 && break; sleep 2; done
"${COMPOSE[@]}" exec -T loadtest-db pg_isready -U madar -d madar_loadtest >/dev/null 2>&1 || { echo "✗ db not ready"; exit 1; }

# ── 2. backend on the prod image → migrates the throwaway DB on boot ──────────
note "starting backend ($LT_IMAGE) against the throwaway DB"
LT_IMAGE="$LT_IMAGE" "${COMPOSE[@]}" up -d loadtest-backend
note "waiting for /health (runs migrations on first boot)…"
healthy=0
for _ in $(seq 1 90); do
  if docker run --rm --network "$NET" curlimages/curl:latest -fsS http://loadtest-backend:8081/health >/dev/null 2>&1; then healthy=1; break; fi
  sleep 2
done
[ "$healthy" = 1 ] || { echo "✗ backend not healthy — logs:"; "${COMPOSE[@]}" logs --tail 40 loadtest-backend; exit 1; }
echo "✓ backend healthy"

# ── 3. seed the throwaway DB (schema exists now) ──────────────────────────────
note "seeding fixtures into madar_loadtest"
"${COMPOSE[@]}" exec -T loadtest-db psql -U madar -d madar_loadtest -v ON_ERROR_STOP=1 -q < seed_fuzz.sql      || { echo "✗ seed_fuzz failed"; exit 1; }
"${COMPOSE[@]}" exec -T loadtest-db psql -U madar -d madar_loadtest -v ON_ERROR_STOP=1 -q < seed_loadtest.sql  || { echo "✗ seed_loadtest failed"; exit 1; }
echo "✓ seeded"

# ── 4. run k6 per profile (container on the private net) ──────────────────────
for p in "${PROFILES[@]}"; do
  note "k6 profile: $p  → results/$p.txt"
  docker run --rm --network "$NET" \
    -e BASE_URL=http://loadtest-backend:8081 -e TOKEN="$TOKEN" -e PROFILE="$p" \
    -e WRITE_RATIO="${WRITE_RATIO:-0.15}" \
    -v "$PWD/k6/load.js:/load.js:ro" \
    grafana/k6:latest run --no-color /load.js 2>&1 | tee "$RESULTS/$p.txt"
  sleep 3
done

note "ALL PROFILES DONE — key metrics"
for p in "${PROFILES[@]}"; do
  echo "── $p ──"
  grep -aE "http_reqs|http_req_duration|http_req_failed|iterations|orders_created|vus_max|✓|✗" "$RESULTS/$p.txt" | grep -aviE "scenario|default fn" | head -12
done
echo "✓ load test complete"
