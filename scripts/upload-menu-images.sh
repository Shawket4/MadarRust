#!/usr/bin/env bash
# Upload menu item images to Madar through the real pipeline
# (POST /uploads/menu-items/{id} → background WebP conversion → image_group_id).
#
#   scripts/upload-menu-images.sh --api URL --org <org-uuid> (--email E [--password P] | --token JWT)
#                                 [--map drops/menu_images.csv] [--dry-run] [--no-wait]
#
#   --api       base URL the routes hang off, e.g. http://localhost:8080 or
#               https://madar-pos.cloud/api
#   --map       CSV with header madar_item_name,source,image_url (exact item names)
#   --dry-run   resolve names and download images, but don't upload
#   --no-wait   don't poll the conversion jobs
# Password comes from --password or $MADAR_PASSWORD, else it is prompted for.
# Re-running is safe: the backend dedups identical uploads per org.
set -euo pipefail

api="" org="" email="" token="" password="${MADAR_PASSWORD:-}"
map="drops/menu_images.csv" dry=0 wait=1

usage() { sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-1}"; }
die() { echo "error: $*" >&2; exit 1; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --api) api="${2%/}"; shift 2 ;;
    --org) org="$2"; shift 2 ;;
    --email) email="$2"; shift 2 ;;
    --password) password="$2"; shift 2 ;;
    --token) token="$2"; shift 2 ;;
    --map) map="$2"; shift 2 ;;
    --dry-run) dry=1; shift ;;
    --no-wait) wait=0; shift ;;
    -h|--help) usage 0 ;;
    *) echo "unknown argument: $1" >&2; usage ;;
  esac
done

for bin in curl jq; do command -v "$bin" >/dev/null || die "$bin not found"; done
[[ -n "$api" && -n "$org" && ( -n "$email" || -n "$token" ) ]] || usage
[[ -f "$map" ]] || die "missing $map"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# ── Login ──────────────────────────────────────────────────────────────────
if [[ -z "$token" ]]; then
  if [[ -z "$password" ]]; then read -r -s -p "Password for $email: " password; echo; fi
  # No org_id: login filters users by it, which would exclude super admins
  # (org_id NULL) — the only accounts left after a --reset-org.
  login_body="$(jq -n --arg e "$email" --arg p "$password" '{email: $e, password: $p}')"
  login_resp="$(curl -sS -X POST "$api/auth/login" -H 'Content-Type: application/json' -d "$login_body")"
  token="$(jq -r '.token // empty' <<<"$login_resp" 2>/dev/null || true)"
  [[ -n "$token" ]] || die "login failed for $email: $login_resp"
fi
# X-Org-Id scopes a super-admin token to the target org; org tokens ignore it.
auth=(-H "Authorization: Bearer $token" -H "X-Org-Id: $org")

# ── Resolve item names → ids ───────────────────────────────────────────────
curl -sS --fail-with-body "${auth[@]}" "$api/menu-items?org_id=$org" > "$work/items.json" \
  || die "could not list menu items: $(cat "$work/items.json")"
echo "Org has $(jq length "$work/items.json") menu items."

# Plain comma split: the map's names and URLs contain no commas or quotes.
tail -n +2 "$map" | tr -d '\r' > "$work/map.csv"

ok=0 failed=0 missing=0
jobs_file="$work/jobs.tsv"; : > "$jobs_file"

while IFS=, read -r name source url; do
  [[ -n "$name" ]] || continue
  id="$(jq -r --arg n "$name" '[.[] | select(.name == $n) | .id] | first // empty' "$work/items.json")"
  if [[ -z "$id" ]]; then
    echo "  MISSING  $name (no menu item with that exact name)"
    missing=$((missing + 1)); continue
  fi

  img="$work/$id.img"
  if ! curl -sSL --fail --retry 3 --retry-all-errors --max-time 60 -o "$img" "$url"; then
    echo "  FAILED   $name — download error: $url"
    failed=$((failed + 1)); continue
  fi

  if [[ $dry -eq 1 ]]; then
    echo "  DRY      $name ← $source ($(wc -c < "$img" | tr -d ' ') bytes)"
    ok=$((ok + 1)); continue
  fi

  resp="$(curl -sS "${auth[@]}" -F "image=@$img;filename=$(basename "${url%%\?*}")" \
    "$api/uploads/menu-items/$id")" || resp=""
  job="$(jq -r '.asset_job_id // empty' <<<"$resp" 2>/dev/null || true)"
  if [[ -z "$job" ]]; then
    echo "  FAILED   $name — upload: ${resp:-no response}"
    failed=$((failed + 1)); continue
  fi
  printf '%s\t%s\n' "$job" "$name" >> "$jobs_file"
  echo "  QUEUED   $name ← $source"
  ok=$((ok + 1))
done < "$work/map.csv"

echo
echo "Uploaded/queued: $ok  failed: $failed  missing items: $missing"
[[ $dry -eq 1 || $wait -eq 0 || ! -s "$jobs_file" ]] && exit $(( failed + missing > 0 ))

# ── Wait for conversion (worker processes jobs sequentially by default) ────
echo "Waiting for WebP conversion…"
pending="$(wc -l < "$jobs_file" | tr -d ' ')"
deadline=$((SECONDS + 900))
while [[ $pending -gt 0 && $SECONDS -lt $deadline ]]; do
  sleep 3
  : > "$work/still.tsv"
  while IFS=$'\t' read -r job name; do
    status="$(curl -sS "${auth[@]}" "$api/assets/jobs/$job" | jq -r '.status // "unknown"')"
    case "$status" in
      done) echo "  DONE     $name" ;;
      failed) echo "  FAILED   $name — $(curl -sS "${auth[@]}" "$api/assets/jobs/$job" | jq -r '.error')"
              failed=$((failed + 1)) ;;
      *) printf '%s\t%s\n' "$job" "$name" >> "$work/still.tsv" ;;
    esac
  done < "$jobs_file"
  mv "$work/still.tsv" "$jobs_file"
  pending="$(wc -l < "$jobs_file" | tr -d ' ')"
done

[[ $pending -eq 0 ]] || echo "Timed out with $pending jobs still processing (they continue server-side)."
exit $(( failed + missing > 0 ))
