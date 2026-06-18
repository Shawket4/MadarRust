#!/usr/bin/env bash
#
# RESTler stateful API fuzzing against a DISPOSABLE sufrix_fuzz DB.
#
# Prereqs (one-time): a colima x86_64 VM (RESTler's amd64 .NET segfaults under
# Rosetta, so the VM must be x86_64), the RESTler image, and a compiled grammar:
#   colima start --arch x86_64 --vm-type qemu --cpu 4 --memory 6
#   docker pull mcr.microsoft.com/restlerfuzzer/restler
#   cargo run --bin export-openapi restler_work/openapi.json
#   python3 scripts/openapi_31_to_30.py restler_work/openapi.json restler_work/openapi_30.json   # RESTler can't parse OpenAPI 3.1
#   docker run --rm -v "$PWD/restler_work:/work" -w /work mcr.microsoft.com/restlerfuzzer/restler \
#       dotnet /RESTler/restler/Restler.dll compile --api_spec /work/openapi_30.json
#
# Usage: scripts/restler-run.sh [test|fuzz-lean|fuzz]   (default: test)
set -euo pipefail
cd "$(dirname "$0")/.."
REPO="$PWD"
PG_ADMIN="${PG_ADMIN:-postgres://shawket@localhost:5432/postgres}"
FUZZ_DB_URL="${FUZZ_DB_URL:-postgres://shawket@localhost:5432/sufrix_fuzz}"
JWT_SECRET="${JWT_SECRET:-fuzz-secret-not-for-prod}"
IMG="mcr.microsoft.com/restlerfuzzer/restler"
MODE="${1:-test}"

case "$FUZZ_DB_URL" in *sufrix_fuzz*) : ;; *) echo "REFUSING: not a sufrix_fuzz DB" >&2; exit 1;; esac
docker info >/dev/null 2>&1 || { echo "docker/colima not running" >&2; exit 1; }
[ -f restler_work/Compile/grammar.py ] || { echo "no grammar — run the compile step (see header)" >&2; exit 1; }

echo "▶ (re)seed sufrix_fuzz"
psql "$PG_ADMIN" -c "DROP DATABASE IF EXISTS sufrix_fuzz WITH (FORCE);" >/dev/null
psql "$PG_ADMIN" -c "CREATE DATABASE sufrix_fuzz;" >/dev/null
DATABASE_URL="$FUZZ_DB_URL" sqlx migrate run --source ./migrations >/dev/null
psql "$FUZZ_DB_URL" -v ON_ERROR_STOP=1 -f scripts/seed_fuzz.sql >/dev/null

echo "▶ build server + mint token"
cargo build --quiet --bin sufrix-rust --bin fuzz-token
JWT_SECRET="$JWT_SECRET" ./target/debug/fuzz-token super-admin > restler_work/token.txt
# RESTler token-refresh contract: a metadata line, then the auth header line(s).
cat > restler_work/token.sh <<'EOF'
#!/bin/sh
# RESTler CMD token format: a metadata dict naming the app, then the header(s).
# An empty {} is rejected ("Authentication failed"); it needs a named app.
echo "{u'app1': {}}"
echo "Authorization: Bearer $(cat /work/token.txt)"
EOF

echo "▶ boot server on 0.0.0.0:8099 (reachable from the VM via host.docker.internal)"
RUN_DIR="$(mktemp -d)"; mkdir -p "$RUN_DIR/uploads"
( cd "$RUN_DIR" && exec env -i PATH="$PATH" HOME="$HOME" \
    DATABASE_URL="$FUZZ_DB_URL" JWT_SECRET="$JWT_SECRET" BIND_ADDR="0.0.0.0:8099" \
    UPLOADS_DIR="$RUN_DIR/uploads" SUFRIX_DISABLE_RATE_LIMIT=1 SUFRIX_DISABLE_AUTO_TRANSLATION=1 \
    "$REPO/target/debug/sufrix-rust" ) > "$RUN_DIR/server.log" 2>&1 &
SRV=$!
cleanup() {
  kill "$SRV" 2>/dev/null || true
  pkill -f "target/debug/sufrix-rust" 2>/dev/null || true
  rm -rf "$RUN_DIR"
  psql "$PG_ADMIN" -c "DROP DATABASE IF EXISTS sufrix_fuzz WITH (FORCE);" >/dev/null 2>&1 || true
}
trap cleanup EXIT
for _ in $(seq 1 60); do curl -fsS http://127.0.0.1:8099/health >/dev/null 2>&1 && break; sleep 0.5; done
curl -fsS http://127.0.0.1:8099/health >/dev/null || { echo "server not healthy"; tail "$RUN_DIR/server.log"; exit 1; }
echo "  server healthy"

echo "▶ RESTler $MODE (host.docker.internal:8099)"
docker run --rm -v "$REPO/restler_work:/work" -w /work "$IMG" \
  dotnet /RESTler/restler/Restler.dll "$MODE" \
    --grammar_file /work/Compile/grammar.py \
    --dictionary_file /work/Compile/dict.json \
    --settings /work/Compile/engine_settings.json \
    --no_ssl --host host.docker.internal --target_port 8099 \
    --token_refresh_interval 3600 --token_refresh_command "sh /work/token.sh" 2>&1 | tail -45 || true

echo "▶ RESTler results (bug buckets + spec coverage):"
find restler_work -path "*RestlerResults*" \( -name "bug_buckets.txt" -o -name "testing_summary.json" \) 2>/dev/null | while read -r f; do
  echo "--- $f ---"; head -40 "$f"
done
