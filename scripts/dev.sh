#!/usr/bin/env bash
# Usage:
#   ./scripts/dev.sh             — build (if needed) + start all containers
#   ./scripts/dev.sh build       — force rebuild backend image
#   ./scripts/dev.sh down        — stop and remove containers
#   ./scripts/dev.sh logs        — follow all logs  (add service name to filter)
#   ./scripts/dev.sh setup-osrm  — download + pre-process Egypt map data (one-time, ~10 min)
#   ./scripts/dev.sh <cmd>       — pass through to docker compose (ps, restart, exec …)
set -euo pipefail

OSRM_DATA="$(cd "$(dirname "$0")/.." && pwd)/dev-data/osrm"

# ── Enable OSRM profile only when map data is present ────────────────────────
OSRM_PROFILE=""
if [ -f "$OSRM_DATA/egypt-latest.osrm" ]; then
  OSRM_PROFILE="--profile osrm"
fi

COMPOSE_FILES="-f docker-compose.yml -f docker-compose.dev.yml"
DC="docker compose $COMPOSE_FILES $OSRM_PROFILE"

# ── Ensure Colima ARM is running ──────────────────────────────────────────────
if ! colima status --profile arm 2>/dev/null | grep -q "Running"; then
  echo "▶ Starting Colima ARM profile…"
  colima start --profile arm --cpu 4 --memory 8 --arch aarch64 --vm-type vz --vz-rosetta
  docker context use colima-arm
fi

# ── Subcommands ───────────────────────────────────────────────────────────────
CMD="${1:-up}"

case "$CMD" in
  up)
    $DC build
    $DC up -d --no-build
    echo ""
    $DC ps
    if [ -z "$OSRM_PROFILE" ]; then
      echo "  ℹ  OSRM not started (no map data). Run ./scripts/dev.sh setup-osrm to enable routing."
    fi
    ;;
  build)
    shift
    $DC build "$@"
    ;;
  down)
    $DC down
    ;;
  logs)
    shift || true
    $DC logs -f "$@"
    ;;
  setup-osrm)
    mkdir -p "$OSRM_DATA"

    PBF="$OSRM_DATA/egypt-latest.osm.pbf"
    if [ -f "$PBF" ]; then
      echo "▶ Found $PBF — skipping download."
    else
      echo "▶ Downloading Egypt OSM extract (~500 MB)…"
      curl -L --fail --progress-bar -o "$PBF" \
        https://download.geofabrik.de/africa/egypt-latest.osm.pbf
      SIZE=$(wc -c < "$PBF")
      if [ "$SIZE" -lt 10000000 ]; then
        echo "✗ Download looks wrong (${SIZE} bytes — likely a proxy error page)." >&2
        echo "  Download manually from https://download.geofabrik.de/africa/egypt-latest.osm.pbf" >&2
        echo "  and place it at: $PBF" >&2
        echo "  Then re-run this command." >&2
        rm -f "$PBF"
        exit 1
      fi
    fi
    echo "▶ Extracting…"
    docker run --rm --platform linux/amd64 \
      -v "$OSRM_DATA:/data" osrm/osrm-backend \
      osrm-extract -p /opt/car.lua /data/egypt-latest.osm.pbf
    echo "▶ Partitioning…"
    docker run --rm --platform linux/amd64 \
      -v "$OSRM_DATA:/data" osrm/osrm-backend \
      osrm-partition /data/egypt-latest.osrm
    echo "▶ Customizing…"
    docker run --rm --platform linux/amd64 \
      -v "$OSRM_DATA:/data" osrm/osrm-backend \
      osrm-customize /data/egypt-latest.osrm

    echo ""
    echo "✅ OSRM data ready. Run ./scripts/dev.sh up to start with routing enabled."
    ;;
  *)
    $DC "$@"
    ;;
esac
