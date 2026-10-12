#!/usr/bin/env bash
# Create a new organization (org, its roles and tenders, first branch, owner login)
# through POST /orgs/provision, signed in as a super admin. Same thing the
# dashboard's Organizations → create wizard does.
#
#   scripts/org-setup/create-org.sh --name "Pachamama" --slug pachamama --branch "Main" \
#       --owner-name "…" --owner-email … [--owner-password …] [--owner-pin 123456] \
#       --admin-email … [--admin-password …] [--template cafe] [--api URL]
#
# Run it with NO arguments to be asked for everything, then shown a summary to
# confirm. A password left off is asked for without echo (out of shell history).
# POS module only, EGP, Africa/Cairo, tax 0: change those in the dashboard after.
# Prints the new org id: pass it to import-foodics-menu.sh --org.
set -euo pipefail

api="https://api.madar-pos.cloud"
template="cafe"
name="" slug="" branch="" owner_name="" owner_email="" owner_password="" owner_pin=""
admin_email="" admin_password=""

usage() { sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-1}"; }
die() { echo "error: $*" >&2; exit 1; }

wizard=0
[[ $# -eq 0 ]] && wizard=1
while [[ $# -gt 0 ]]; do
  case "$1" in
    --name) name="$2"; shift 2 ;;
    --slug) slug="$2"; shift 2 ;;
    --branch) branch="$2"; shift 2 ;;
    --owner-name) owner_name="$2"; shift 2 ;;
    --owner-email) owner_email="$2"; shift 2 ;;
    --owner-password) owner_password="$2"; shift 2 ;;
    --owner-pin) owner_pin="$2"; shift 2 ;;
    --admin-email) admin_email="$2"; shift 2 ;;
    --admin-password) admin_password="$2"; shift 2 ;;
    --template) template="$2"; shift 2 ;;
    --api) api="${2%/}"; shift 2 ;;
    -h|--help) usage 0 ;;
    *) echo "unknown argument: $1" >&2; usage ;;
  esac
done

command -v jq >/dev/null || die "jq not found"

# No arguments: ask for everything, then confirm a summary.
if [[ $wizard -eq 1 ]]; then
  . "$(cd "$(dirname "$0")" && pwd)/lib.sh"
  ask name "Organization name"
  ask slug "Short name (its web address)" "$(tr '[:upper:]' '[:lower:]' <<<"$name" | sed -E 's/[^a-z0-9]+/-/g; s/^-+|-+$//g')"
  ask branch "First branch name" "Main"
  ask owner_name "Owner's name"
  ask owner_email "Owner's email (their sign-in)"
  while :; do
    ask_secret owner_password "Owner's password (8+ characters)"
    [[ ${#owner_password} -ge 8 ]] || { echo "  at least 8 characters" >&2; continue; }
    ask_secret again "Type it again"
    [[ $owner_password == "$again" ]] && break
    echo "  they don't match" >&2
  done
  ask_optional owner_pin "Owner's 6-digit till PIN"
  ask template "Template (cafe or restaurant)" "$template"
  ask admin_email "Your super-admin email"
  ask_secret admin_password "Your super-admin password"
  ask api "API" "$api"
  echo
  echo "Create \"$name\" ($slug.madar-pos.cloud), branch \"$branch\", owner $owner_name <$owner_email>${owner_pin:+ with a till PIN}, template $template, on $api."
  confirm "Go ahead?" || { echo "Nothing created."; exit 0; }
fi

for v in name slug branch owner_name owner_email admin_email; do
  [[ -n "${!v}" ]] || die "--${v//_/-} is required"
done
[[ -n "$admin_password" ]] || { read -r -s -p "Super-admin password for $admin_email: " admin_password; echo; }
[[ -n "$owner_password" ]] || { read -r -s -p "New owner password for $owner_email (8+ chars): " owner_password; echo; }

# POSTs JSON; prints the body, or the server's error and exits.
post() {
  local path="$1" body="$2" out code
  out="$(mktemp)"
  code="$(curl -sS -o "$out" -w '%{http_code}' -X POST "$api$path" \
    -H 'Content-Type: application/json' ${token:+-H "Authorization: Bearer $token"} \
    --data-binary @- <<<"$body")" || { rm -f "$out"; die "cannot reach $api"; }
  if [[ "$code" != 2* ]]; then
    echo "error: $path → HTTP $code: $(cat "$out")" >&2; rm -f "$out"; exit 1
  fi
  cat "$out"; rm -f "$out"
}

token=""
token="$(post /auth/login "$(jq -n --arg e "$admin_email" --arg p "$admin_password" \
  '{email: $e, password: $p}')" | jq -r '.token // empty')"
[[ -n "$token" ]] || die "login returned no token"

body="$(jq -n --arg name "$name" --arg slug "$slug" --arg template "$template" \
  --arg branch "$branch" --arg on "$owner_name" --arg oe "$owner_email" \
  --arg op "$owner_password" --arg pin "$owner_pin" '{
    name: $name, slug: $slug, template: $template,
    branch: {name: $branch},
    owner: ({name: $on, email: $oe, password: $op} + (if $pin == "" then {} else {pin: $pin} end))
  }')"

res="$(post /orgs/provision "$body")"
echo "Created: $(jq -r '.org.name' <<<"$res") (slug $(jq -r '.org.slug' <<<"$res"))"
echo "  org id:    $(jq -r '.org.id' <<<"$res")"
echo "  branch id: $(jq -r '.branch_id' <<<"$res")"
echo "  owner id:  $(jq -r '.owner_id' <<<"$res")"
echo "Next, load its menu: scripts/org-setup/import-foodics-menu.sh (no arguments)."
