#!/usr/bin/env bash
# Fill a test or review org's menu with the mock café (seed-mock-cafe-menu.sql).
# Asks for the database and the org, dry-runs, then offers the real run (backup
# first on prod). The SQL refuses an org that already has a menu, unless you
# pick "from scratch", which it refuses for an org that has sold.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
. "$here/lib.sh"

pick_db
pick_org
replace=0
confirm "From scratch (first clears this org's menu, ingredients, suppliers and kitchen stations)?" && replace=1

seed() { psql "$db" -X -v org="$org" -v replace="$replace" "$@" -f "$here/seed-mock-cafe-menu.sql"; }
seed -v dry_run=1
echo
confirm "Do it for real on \"$org_name\"?" || { echo "Nothing changed."; exit 0; }
[[ $is_prod -eq 1 ]] && backup_prod "seed-mock-menu"
seed
