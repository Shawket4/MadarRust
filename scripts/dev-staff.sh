#!/usr/bin/env bash
# One command to get the whole staff module running locally with data in it.
#
#   scripts/dev-staff.sh            # migrate, seed, start the backend
#   scripts/dev-staff.sh --no-seed  # keep the existing demo data
#
# What it does, in order:
#   1. applies migrations to the local dev database
#   2. seeds (or keeps) the isolated "Madar Demo" org
#   3. starts the backend
#   4. generates + marks paid last month's payroll THROUGH THE REAL ENDPOINT, so
#      the payslips you look at were produced by the rules rather than typed into
#      a seed file
#   5. prints the credentials and the exact commands for the dashboard + staff app
#
# LOCAL ONLY. The seed deletes and recreates its org by slug; never point this at
# anything but a development database.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

DB_URL="${DATABASE_URL:-postgres://shawket@localhost:5432/madar}"
PORT="${PORT:-8082}"
API="http://localhost:${PORT}"
ADMIN_EMAIL="admin@demo.madar"
ADMIN_PASSWORD="Demo1234!"
SEED=1
[[ "${1:-}" == "--no-seed" ]] && SEED=0

# Refuse to run against anything that looks like production. The seed is
# destructive by design, so this guard is not optional.
case "$DB_URL" in
  *prod*|*amazonaws*|*187.124.33.153*|*madar-pos.cloud*)
    echo "!! DATABASE_URL looks like production — refusing. ($DB_URL)" >&2
    exit 1 ;;
esac

echo "── 1/5  migrations"
DATABASE_URL="$DB_URL" sqlx migrate run >/dev/null
echo "        up to date"

if [[ $SEED -eq 1 ]]; then
  echo "── 2/5  seeding the demo org"
  DATABASE_URL="$DB_URL" cargo run --quiet --bin seed-staff-demo
else
  echo "── 2/5  skipping the seed (--no-seed)"
fi

echo "── 3/5  starting the backend on :${PORT}"
LOG="$(mktemp -t madar-staff-backend)"
DATABASE_URL="$DB_URL" PORT="$PORT" cargo run --quiet --bin madar-rust >"$LOG" 2>&1 &
BACKEND_PID=$!
# Leave the server running when this script exits — the point is to hand you a
# working environment — but tear it down if the *startup* fails.
trap 'kill $BACKEND_PID 2>/dev/null || true' ERR

for _ in $(seq 1 60); do
  curl -sf -o /dev/null "${API}/health" && break
  sleep 1
done
if ! curl -sf -o /dev/null "${API}/health"; then
  echo "!! backend did not come up. Last lines:" >&2
  tail -20 "$LOG" >&2
  exit 1
fi
echo "        up (pid ${BACKEND_PID}, log: ${LOG})"

echo "── 4/5  running last month's payroll through the API"
TOKEN=$(curl -sf -X POST "${API}/auth/login" -H 'Content-Type: application/json' \
  -d "{\"email\":\"${ADMIN_EMAIL}\",\"password\":\"${ADMIN_PASSWORD}\"}" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["token"])')

# The oldest DRAFT period is last month's; the newer one stays draft so there is
# something left to press in the dashboard.
PERIOD=$(curl -sf "${API}/staff/payroll/periods" -H "Authorization: Bearer $TOKEN" \
  | python3 -c '
import json, sys
periods = sorted(json.load(sys.stdin), key=lambda p: p["start_date"])
print(periods[0]["id"] if periods else "")')

if [[ -n "$PERIOD" ]]; then
  curl -sf -X POST "${API}/staff/payroll/periods/${PERIOD}/generate" \
    -H "Authorization: Bearer $TOKEN" >/dev/null
  curl -sf -X PATCH "${API}/staff/payroll/periods/${PERIOD}/status" \
    -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    -d '{"status":"paid"}' >/dev/null
  NET=$(curl -sf "${API}/staff/payroll/periods/${PERIOD}/payslips" \
    -H "Authorization: Bearer $TOKEN" \
    | python3 -c '
import json, sys
slips = json.load(sys.stdin)
total = sum(s["net_piastres"] for s in slips) / 100
print(f"{len(slips)} payslips, {total:,.0f} EGP total")')
  echo "        ${NET}"
fi

echo "── 5/5  ready"
cat <<EOF

  API              ${API}
  Sign in          ${ADMIN_EMAIL} / ${ADMIN_PASSWORD}
                   (employees: nour@ omar@ salma@ youssef@ mariam@ demo.madar,
                    same password — omar is the habitually late one)

  Dashboard
    echo 'VITE_API_URL=${API}' > ~/Desktop/MadarDashboard/.env.local
    cd ~/Desktop/MadarDashboard && npm run dev
    → Team ▸ Employees / Attendance / Work shifts / Requests / Payroll

  Staff app (iOS simulator)
    cd ~/Desktop/madar/apps/staff
    flutter run --dart-define=MADAR_API=${API} --dart-define=MADAR_ENV=dev
    Drive the geofence with:
      xcrun simctl location booted set 30.0444,31.2357   # at Downtown
      xcrun simctl location booted set 30.0600,31.2357   # ~1.7 km away

  Backend log       ${LOG}
  Stop the backend  kill ${BACKEND_PID}

EOF
